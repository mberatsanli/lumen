//! Page scripting on the Boa JavaScript engine.
//!
//! [`PageScripts`] owns a `boa_engine::Context` — one JS world per page.
//! Boa's GC handles are not `Send`, while a [`Session`] rides a loader
//! thread, so the script world lives OUTSIDE the session (the desktop
//! shell keeps it on the main thread) and every entry point borrows the
//! session for the duration of the call.
//!
//! DOM natives reach the page through a thread-local [`Bridge`]: before
//! running any script the caller moves the page and live form values
//! into the bridge, the natives mutate them there, and afterwards they
//! move back (relayouting once if anything render-visible changed).
//! Element wrappers carry their engine node id in a hidden `__node`
//! property; `textContent`/`value` are accessor properties reading and
//! writing the live page.
//!
//! Script-initiated network (fetch, XHR, runtime `import()`) never
//! blocks the caller: requests are PREPARED against read-only session
//! state (URL resolution, the `file://` gate, the same-origin policy,
//! the Cookie header), EXECUTED by the page's [`NetworkQueue`] (worker
//! threads, or inline for deterministic embedders), and APPLIED back on
//! this thread (Set-Cookie into the jar, the CORS read gate,
//! promise/XHR settlement) by the pump —
//! which the shell re-enters through [`PageScripts::pump_network`]
//! when the queue's wake hook fires.

use crate::network::{ModuleSlot, NetworkQueue};
use crate::{
    CorsCheck, Page, ResourceLoader, ScriptCredentials, Session, resolve as resolve_url,
    same_origin,
};
use boa_engine::builtins::promise::PromiseState;
use boa_engine::context::time::JsInstant;
use boa_engine::job::{GenericJob, Job, JobExecutor, NativeAsyncJob, PromiseJob, TimeoutJob};
use boa_engine::module::{ModuleLoader, Referrer};
use boa_engine::object::builtins::{JsArray, JsPromise, JsProxyBuilder};
use boa_engine::object::{FunctionObjectBuilder, ObjectInitializer};
use boa_engine::property::Attribute;
use boa_engine::{
    Context, JsNativeError, JsObject, JsResult, JsString, JsValue, Module, NativeFunction, Source,
    js_string,
};
use futures_concurrency::future::FutureGroup;
use futures_lite::{StreamExt, future};
use lumen_html::NodeId;
use lumen_platform::{ResourceRequest, Url};
use std::cell::{Cell, RefCell};
use std::collections::{BTreeMap, HashMap, VecDeque};
use std::path::Path;
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

/// The page state scripts operate on while a call is in flight.
#[derive(Default)]
struct Bridge {
    page: Option<Page>,
    form_values: HashMap<NodeId, String>,
    /// Set when a mutation changed something render-visible.
    dirty: bool,
    /// Registrations collected during the call.
    pending_listeners: Vec<(NodeId, String, Listener)>,
    /// removeEventListener calls collected during the call.
    pending_removals: Vec<(NodeId, String, JsObject)>,
    pending_timers: Vec<(u64, f64, Option<f64>, JsObject)>,
    cleared_timers: Vec<u64>,
    next_timer_id: u64,
    now_ms: f64,
    /// `event.preventDefault()` was called during the dispatch.
    prevented: bool,
    /// `event.stopPropagation()` was called (stops the bubble walk).
    stopped: bool,
    /// fetch() calls made during the entry, submitted to the network
    /// queue at the next pump (target, credentials-include, resolve,
    /// reject).
    pending_fetches: Vec<(String, bool, JsObject, JsObject)>,
    /// XMLHttpRequest.send() calls made during the entry.
    pending_xhrs: Vec<XhrRequest>,
    /// The page URL (feeds location.href reads).
    url: String,
    /// The Cookie header for the page URL (document.cookie reads).
    cookie_header: String,
    /// document.cookie writes, stored into the jar after the entry.
    pending_cookies: Vec<String>,
    /// A navigation a script requested (location.href = ..., reload()).
    pending_navigation: Option<String>,
    /// A focus change a script requested: focus(Some) / blur(None).
    pending_focus: Option<Option<NodeId>>,
    /// localStorage of the page origin, hydrated at check-in and
    /// persisted at checkout when `storage_dirty`.
    local_storage: BTreeMap<String, String>,
    /// Set by a localStorage write (set/remove/clear/proxy set).
    storage_dirty: bool,
    /// sessionStorage of this script world (memory only).
    session_storage: BTreeMap<String, String>,
    /// history.length / history.state feeds.
    history_length: usize,
    history_state: Option<String>,
    /// A history operation a script requested.
    pending_history: Option<HistoryOp>,
}

/// One `xhr.send()` queued for the between-entries pump. Custom
/// headers are recorded but only Content-Type reaches the wire —
/// [`crate::Session::fetch_resource`] has no header channel; they DO
/// drive the CORS preflight decision. `with_credentials` is read at
/// send() time, so a script may set `xhr.withCredentials` any time
/// after open().
struct XhrRequest {
    method: String,
    url: String,
    headers: Vec<(String, String)>,
    body: Option<String>,
    with_credentials: bool,
    xhr: JsObject,
}

/// One addEventListener registration: the callback plus the `once`
/// option (the rest of the options object — capture, passive — is
/// tolerated but not modeled).
#[derive(Clone)]
struct Listener {
    callback: JsObject,
    once: bool,
}

/// Who a finished (or refused) script read settles: a fetch promise's
/// resolvers or an XHR object.
enum SettleTarget {
    Fetch(JsObject, JsObject),
    Xhr(JsObject),
}

/// A script read waiting on its CORS preflight probe: the actual
/// request only goes out once the grant passes.
struct PendingPreflight {
    request: ResourceRequest,
    check: CorsCheck,
    target: SettleTarget,
}

/// A `history.*` call made during an entry, applied at checkout.
enum HistoryOp {
    /// pushState: new entry, no reload (url is already resolved).
    Push {
        url: String,
        state: Option<String>,
    },
    /// replaceState: rewrite the current entry.
    Replace {
        url: String,
        state: Option<String>,
    },
    /// back()/forward(): handed to the shell as a navigation.
    Back,
    Forward,
}

thread_local! {
    static BRIDGE: RefCell<Bridge> = RefCell::new(Bridge::default());
}

fn with_bridge<R>(action: impl FnOnce(&mut Bridge) -> R) -> R {
    BRIDGE.with(|bridge| action(&mut bridge.borrow_mut()))
}

/// What an event dispatch did.
#[derive(Debug, Clone, Copy)]
pub struct DispatchOutcome {
    /// At least one handler ran (the page may have re-rendered).
    pub handled: bool,
    /// A handler called `event.preventDefault()` — skip the default
    /// action (navigation, submit, toggle...).
    pub prevented: bool,
}

fn prevent_default(
    _this: &JsValue,
    _args: &[JsValue],
    _context: &mut Context,
) -> JsResult<JsValue> {
    with_bridge(|bridge| bridge.prevented = true);
    Ok(JsValue::undefined())
}

fn default_prevented_get(
    _this: &JsValue,
    _args: &[JsValue],
    _context: &mut Context,
) -> JsResult<JsValue> {
    Ok(JsValue::from(with_bridge(|bridge| bridge.prevented)))
}

/// One callback in a dispatch: an `addEventListener` registration or
/// the compiled form of an inline `on*` attribute.
enum Handler {
    Js(Listener),
    Inline(String),
}

fn stop_propagation(
    _this: &JsValue,
    _args: &[JsValue],
    _context: &mut Context,
) -> JsResult<JsValue> {
    with_bridge(|bridge| bridge.stopped = true);
    Ok(JsValue::undefined())
}

/// Drops `callback` from the (node, event) listener list — the
/// removeEventListener primitive, also used for `once` listeners that
/// just ran.
fn remove_listener(
    listeners: &mut HashMap<NodeId, HashMap<String, Vec<Listener>>>,
    node: NodeId,
    event: &str,
    callback: &JsObject,
) {
    if let Some(callbacks) = listeners
        .get_mut(&node)
        .and_then(|by_event| by_event.get_mut(event))
    {
        callbacks.retain(|listener| !JsObject::equals(&listener.callback, callback));
    }
}

/// A pending `setTimeout`/`setInterval`.
struct Timer {
    id: u64,
    due_ms: f64,
    interval_ms: Option<f64>,
    callback: JsObject,
}

/// One page's JavaScript world: the Boa context plus the listener and
/// timer registries. Lives on the shell's thread, next to the window.
pub struct PageScripts {
    context: Context,
    /// Event listeners indexed by (node, event kind) so dispatch does
    /// not scan every registration for every bubble target. Each inner
    /// Vec keeps registration order.
    listeners: HashMap<NodeId, HashMap<String, Vec<Listener>>>,
    /// Compiled inline `on*` attribute handlers, keyed by (node, event)
    /// with the attribute text they were compiled from — a changed
    /// attribute recompiles on the next dispatch.
    inline_handlers: HashMap<(NodeId, String), (String, JsObject)>,
    timers: Vec<Timer>,
    /// fetch() calls queued by scripts, not yet submitted to the network.
    pending_fetches: Vec<(String, bool, JsObject, JsObject)>,
    /// fetch() calls submitted to the network queue, by request id.
    in_flight_fetches: HashMap<u64, (JsObject, JsObject)>,
    /// CORS gate per in-flight cross-origin script read, by request id
    /// (same-origin reads never land here — their responses stay
    /// readable unconditionally).
    cors_checks: HashMap<u64, CorsCheck>,
    /// XMLHttpRequests queued by scripts, not yet submitted.
    pending_xhrs: Vec<XhrRequest>,
    /// XMLHttpRequests submitted to the network queue, by request id.
    in_flight_xhrs: HashMap<u64, JsObject>,
    /// Script reads waiting on their CORS preflight probe, by probe id.
    in_flight_preflights: HashMap<u64, PendingPreflight>,
    /// The page's network queue: script-initiated loads execute here
    /// (worker threads, or inline for deterministic embedders), so a
    /// slow server never blocks the shell's thread.
    network: Rc<NetworkQueue>,
    /// sessionStorage of this page (lives and dies with the world).
    session_storage: BTreeMap<String, String>,
    /// A navigation requested by a script, for the shell to perform.
    navigation: Option<String>,
    /// A focus change requested by a script, for the shell to apply.
    focus_request: Option<Option<NodeId>>,
    now_ms: f64,
}

impl PageScripts {
    /// Builds the script world for the current page and runs its
    /// `<script>` elements (inline text and `src=` fetched against the
    /// page URL) in document order. `None` when the page has no scripts.
    ///
    /// Script-initiated network loads (fetch, XHR, runtime `import()`)
    /// execute INLINE on the caller's thread — the legacy, deterministic
    /// behavior. Embedders with an event loop should use
    /// [`Self::new_with_network`] with [`NetworkQueue::threaded`] so a
    /// slow server never blocks the UI.
    ///
    /// Classic scripts (including `async` — the pipeline is synchronous
    /// and everything already runs after parsing, so keeping document
    /// order is the most deterministic interpretation) run first; then
    /// the post-parse queue: `defer` scripts and `type="module"` scripts
    /// in document order, per spec. Modules share this context's global
    /// with the classic scripts.
    pub fn new<L: ResourceLoader + Send + Sync + 'static>(
        session: &mut Session<L>,
    ) -> Option<Self> {
        Self::new_with_network(session, NetworkQueue::inline())
    }

    /// [`Self::new`] with an explicit network execution strategy for
    /// script-initiated loads. The loader itself stays the session's;
    /// only prepared requests cross to workers.
    pub fn new_with_network<L: ResourceLoader + Send + Sync + 'static>(
        session: &mut Session<L>,
        network: NetworkQueue,
    ) -> Option<Self> {
        let entries = collect_scripts(session);
        if entries.is_empty() && !has_inline_handlers(session) {
            return None;
        }
        let network = Rc::new(network);
        network.set_loader(session.shared_loader());
        let loader = Rc::new(PageModuleLoader::new(network.clone()));
        let mut context = Context::builder()
            .module_loader(loader.clone())
            .job_executor(Rc::new(BoundedJobExecutor::with_activity(
                network.module_activity(),
            )))
            .build()
            .expect("fresh context");
        // A runaway loop (while(true){}) becomes a JS error instead of
        // hanging the shell. Boa already caps recursion (512 frames)
        // and stack size (10 KiB) by default.
        context
            .runtime_limits_mut()
            .set_loop_iteration_limit(LOOP_ITERATION_BUDGET);
        install_globals(&mut context);
        let mut scripts = Self {
            context,
            listeners: HashMap::new(),
            inline_handlers: HashMap::new(),
            timers: Vec::new(),
            pending_fetches: Vec::new(),
            in_flight_fetches: HashMap::new(),
            cors_checks: HashMap::new(),
            pending_xhrs: Vec::new(),
            in_flight_xhrs: HashMap::new(),
            in_flight_preflights: HashMap::new(),
            network,
            session_storage: BTreeMap::new(),
            navigation: None,
            focus_request: None,
            now_ms: 0.0,
        };
        let page_path = session
            .current_url()
            .map(|url| url.to_string())
            .unwrap_or_default();
        let mut deferred: Vec<ScriptEntry> = Vec::new();
        for entry in entries {
            match entry {
                ScriptEntry::Now(source) => scripts.eval_classic(session, &source, &page_path),
                post_parse => deferred.push(post_parse),
            }
        }
        // Document parse is done: deferred scripts and modules run in
        // document order, before DOMContentLoaded.
        for entry in deferred {
            match entry {
                ScriptEntry::Deferred(source) => {
                    scripts.eval_classic(session, &source, &page_path);
                }
                ScriptEntry::Module { key, source } => {
                    scripts.run_module(session, &loader, &key, &source);
                }
                ScriptEntry::Now(_) => unreachable!("Now entries ran above"),
            }
        }
        // From here on the static module graph is settled: a runtime
        // dynamic import() of a never-fetched URL goes through the
        // network queue instead of rejecting.
        loader.enable_queue_fetch();
        scripts.pump_fetches(session);
        // The document is ready: fire the lifecycle events on the root.
        scripts.dispatch(session, 0, "DOMContentLoaded");
        scripts.dispatch(session, 0, "load");
        // A back()/forward() navigation re-runs the page in a fresh
        // world; the traversal surfaces as popstate after load.
        if session.take_traversed() {
            scripts.dispatch(session, 0, "popstate");
        }
        Some(scripts)
    }

    /// Runs one classic script, reporting errors without aborting the
    /// page. The source carries the page path so a dynamic `import()`
    /// inside it resolves relative to the page URL.
    fn eval_classic<L: ResourceLoader + Send + Sync + 'static>(
        &mut self,
        session: &mut Session<L>,
        source: &str,
        page_path: &str,
    ) {
        self.enter(session, |context| {
            let source = Source::from_bytes(source.as_bytes()).with_path(Path::new(page_path));
            if let Err(error) = context.eval(source) {
                report_script_error(&error);
            }
        });
    }

    /// Loads, links and evaluates one module. The loader only serves
    /// sources it was given up front, so this runs a fetch-retry loop:
    /// each round the loader reports the import URLs it missed, those
    /// get fetched through the session (the file:// gate included), and
    /// the next round resolves one more level of the import graph.
    /// A failure is reported and the page moves on to the next script.
    fn run_module<L: ResourceLoader + Send + Sync + 'static>(
        &mut self,
        session: &mut Session<L>,
        loader: &Rc<PageModuleLoader>,
        key: &str,
        source: &str,
    ) {
        loader.give_source(key, source);
        for _ in 0..MAX_MODULE_ROUNDS {
            let mut promise = None;
            let mut parse_error = false;
            self.enter(session, |context| {
                let source = Source::from_bytes(source.as_bytes()).with_path(Path::new(key));
                match Module::parse(source, None, context) {
                    Ok(module) => promise = Some(module.load_link_evaluate(context)),
                    Err(error) => {
                        eprintln!("[js] module parse error ({key}): {error}");
                        parse_error = true;
                    }
                }
            });
            if parse_error {
                return;
            }
            let misses = loader.take_misses();
            if !misses.is_empty() {
                // Fetch the next graph level and retry; a failed fetch
                // sinks this module but not the page.
                let mut failed = false;
                for miss in misses {
                    let fetched = Url::parse(&miss)
                        .map_err(|error| error.to_string())
                        .and_then(|url| {
                            session
                                .fetch_resource(url, None)
                                .map(|response| response.text())
                                .map_err(|error| error.to_string())
                        });
                    match fetched {
                        Ok(source) => loader.give_source(&miss, &source),
                        Err(error) => {
                            eprintln!("[js] module fetch failed ({miss}): {error}");
                            failed = true;
                        }
                    }
                }
                if failed {
                    return;
                }
                continue;
            }
            let Some(promise) = promise else {
                return;
            };
            if let PromiseState::Rejected(reason) = promise.state() {
                self.enter(session, |context| {
                    let message = reason
                        .to_string(context)
                        .map(|text| text.to_std_string_escaped())
                        .unwrap_or_else(|_| "?".to_string());
                    eprintln!("[js] module error ({key}): {message}");
                });
            }
            // Pending is usually fine (the import graph settled), but
            // a top-level await on the page's own fetch() never
            // resolves: the parked future cannot outlive this entry
            // (see BoundedJobExecutor's DOCUMENTED LIMIT).
            return;
        }
        eprintln!("[js] module graph too deep ({key}); giving up");
    }

    /// Whether any listener is registered for (node, event).
    #[must_use]
    pub fn has_listener(&self, node: NodeId, event: &str) -> bool {
        self.listeners
            .get(&node)
            .and_then(|by_event| by_event.get(event))
            .is_some_and(|callbacks| !callbacks.is_empty())
    }

    /// Dispatches an event at `node`, bubbling to its ancestors.
    pub fn dispatch<L: ResourceLoader + Send + Sync + 'static>(
        &mut self,
        session: &mut Session<L>,
        node: NodeId,
        event: &str,
    ) -> DispatchOutcome {
        self.dispatch_with_key(session, node, event, None)
    }

    /// [`Self::dispatch`] with a `key` property on the event (keydown /
    /// keyup).
    ///
    /// Per bubble target the inline `on*` attribute handler (if any)
    /// runs first — the attribute was set when the HTML was parsed,
    /// before any `addEventListener` call — with `this` bound to that
    /// element; registered listeners follow in registration order.
    pub fn dispatch_with_key<L: ResourceLoader + Send + Sync + 'static>(
        &mut self,
        session: &mut Session<L>,
        node: NodeId,
        event: &str,
        key: Option<&str>,
    ) -> DispatchOutcome {
        let mut targets: Vec<NodeId> = vec![node];
        if let Some(page) = session.page() {
            targets.extend(page.document.ancestors(node));
        }
        let attribute = format!("on{event}");
        let inline = |node: NodeId| {
            session
                .page()
                .and_then(|page| page.document.element(node))
                .and_then(|element| element.attributes.get(&attribute))
                .map(str::to_string)
        };
        let mut handlers: Vec<(NodeId, Handler)> = Vec::new();
        // window-level lifecycle events (load/DOMContentLoaded land on
        // the document root) also honor `<body on*>` — the classic way
        // pages hook them.
        if node == 0
            && let Some(body) = session.page().and_then(|page| {
                let document = &page.document;
                document.descendants(document.root()).find(|node| {
                    document
                        .element(*node)
                        .is_some_and(|element| element.tag_name == "body")
                })
            })
            && let Some(source) = inline(body)
        {
            handlers.push((body, Handler::Inline(source)));
        }
        for target in targets {
            if let Some(source) = inline(target) {
                handlers.push((target, Handler::Inline(source)));
            }
            if let Some(callbacks) = self
                .listeners
                .get(&target)
                .and_then(|by_event| by_event.get(event))
            {
                handlers.extend(
                    callbacks
                        .iter()
                        .map(|listener| (target, Handler::Js(listener.clone()))),
                );
            }
        }
        let mut outcome = DispatchOutcome {
            handled: !handlers.is_empty(),
            prevented: false,
        };
        if handlers.is_empty() {
            return outcome;
        }
        with_bridge(|bridge| {
            bridge.prevented = false;
            bridge.stopped = false;
        });
        let event_name = event.to_string();
        let key = key.map(str::to_string);
        for (target, handler) in handlers {
            // `this`: the element for inline handlers (spec), undefined
            // for addEventListener callbacks (existing behavior).
            let resolved = match handler {
                Handler::Js(listener) => Some((listener.callback, None, listener.once)),
                Handler::Inline(source) => self
                    .inline_handler(session, target, &event_name, &source)
                    .map(|callback| (callback, Some(target), false)),
            };
            let Some((callback, this_node, once)) = resolved else {
                continue;
            };
            let key = key.clone();
            self.enter(session, |context| {
                let target = element_object(target, context);
                let this = match this_node {
                    Some(node) => JsValue::from(element_object(node, context)),
                    None => JsValue::undefined(),
                };
                let default_prevented = NativeFunction::from_fn_ptr(default_prevented_get)
                    .to_js_function(context.realm());
                let mut initializer = ObjectInitializer::new(context);
                initializer
                    .property(
                        js_string!("type"),
                        js_string!(event_name.as_str()),
                        Attribute::all(),
                    )
                    .property(js_string!("target"), target, Attribute::all())
                    .accessor(
                        js_string!("defaultPrevented"),
                        Some(default_prevented),
                        None,
                        Attribute::all(),
                    )
                    .function(
                        NativeFunction::from_fn_ptr(prevent_default),
                        js_string!("preventDefault"),
                        0,
                    )
                    .function(
                        NativeFunction::from_fn_ptr(stop_propagation),
                        js_string!("stopPropagation"),
                        0,
                    );
                if let Some(key) = &key {
                    initializer.property(
                        js_string!("key"),
                        js_string!(key.as_str()),
                        Attribute::all(),
                    );
                }
                let event_object = initializer.build();
                if let Err(error) = callback.call(&this, &[event_object.into()], context) {
                    report_script_error(&error);
                }
            });
            let stopped = with_bridge(|bridge| bridge.stopped);
            if once {
                remove_listener(&mut self.listeners, target, &event_name, &callback);
            }
            if stopped {
                break;
            }
        }
        outcome.prevented = with_bridge(|bridge| bridge.prevented);
        self.pump_fetches(session);
        outcome
    }

    /// Compiles an inline `on*` attribute into `function (event) { .. }`
    /// and caches it per (node, event); a changed attribute recompiles.
    /// A handler that does not parse is reported once and skipped.
    fn inline_handler<L: ResourceLoader>(
        &mut self,
        session: &mut Session<L>,
        node: NodeId,
        event: &str,
        source: &str,
    ) -> Option<JsObject> {
        let key = (node, event.to_string());
        if let Some((compiled_from, callback)) = self.inline_handlers.get(&key)
            && compiled_from == source
        {
            return Some(callback.clone());
        }
        let wrapped = format!("(function (event) {{\n{source}\n}})");
        let mut callback = None;
        self.enter(session, |context| {
            match context.eval(Source::from_bytes(wrapped.as_bytes())) {
                Ok(value) => match value.as_object() {
                    Some(object) => callback = Some(object),
                    None => eprintln!("[js] inline handler is not a function ({event})"),
                },
                Err(error) => eprintln!("[js] inline handler error ({event}): {error}"),
            }
        });
        let callback = callback?;
        self.inline_handlers
            .insert(key, (source.to_string(), callback.clone()));
        Some(callback)
    }

    /// Runs timers due at `now_ms`. Returns whether anything ran.
    ///
    /// The due set is snapshotted up front: timers a callback registers
    /// wait for the next tick, so a `setTimeout(f, 0)` chain cannot spin
    /// this call forever. A callback can still clearTimeout a later
    /// sibling of the same snapshot — that one is then skipped.
    pub fn tick<L: ResourceLoader + Send + Sync + 'static>(
        &mut self,
        session: &mut Session<L>,
        now_ms: f64,
    ) -> bool {
        self.now_ms = now_ms;
        let due: Vec<u64> = self
            .timers
            .iter()
            .filter(|timer| timer.due_ms <= now_ms)
            .map(|timer| timer.id)
            .collect();
        let mut ran = false;
        for id in due {
            let Some(index) = self.timers.iter().position(|timer| timer.id == id) else {
                continue; // cleared by an earlier callback of this tick
            };
            let timer = self.timers.remove(index);
            if let Some(interval) = timer.interval_ms {
                self.timers.push(Timer {
                    id: timer.id,
                    due_ms: now_ms + interval,
                    interval_ms: Some(interval),
                    callback: timer.callback.clone(),
                });
            }
            self.enter(session, |context| {
                if let Err(error) = timer.callback.call(&JsValue::undefined(), &[], context) {
                    report_script_error(&error);
                }
            });
            ran = true;
        }
        if ran {
            self.pump_fetches(session);
        }
        ran
    }

    /// A navigation a script requested (location.href / reload /
    /// history traversal). The shell performs it; `"::reload"` means
    /// refresh, `"::back"`/`"::forward"` a history traversal.
    pub fn take_navigation(&mut self) -> Option<String> {
        self.navigation.take()
    }

    /// A focus change a script requested (el.focus() / el.blur()).
    pub fn take_focus_request(&mut self) -> Option<Option<NodeId>> {
        self.focus_request.take()
    }

    /// Drains completed network results and settles their promises and
    /// XHRs. The shell calls this when the network queue's wake hook
    /// fires (a threaded completion landed); entry points call it
    /// through `pump_fetches` after running scripts.
    pub fn pump_network<L: ResourceLoader + Send + Sync + 'static>(
        &mut self,
        session: &mut Session<L>,
    ) {
        self.pump_fetches(session);
    }

    /// Registers the hook the network queue calls (from a worker
    /// thread) when a threaded load completes — typically an event-loop
    /// proxy that schedules a [`Self::pump_network`] on the UI thread.
    pub fn set_network_wake(&self, wake: impl Fn() + Send + Sync + 'static) {
        self.network.set_wake_hook(wake);
    }

    /// Submits queued fetch()/XHR round-trips to the network queue and
    /// settles completed ones, looping because continuations may fetch
    /// again. Never blocks: with a threaded queue the loop returns with
    /// promises pending and the completion wake re-enters through
    /// [`Self::pump_network`]; with an inline queue the results are
    /// already waiting at the next drain, preserving the legacy
    /// resolve-within-the-entry behavior.
    fn pump_fetches<L: ResourceLoader + Send + Sync + 'static>(
        &mut self,
        session: &mut Session<L>,
    ) {
        // The session's loader never changes, but (re)publishing it is
        // cheap and keeps the invariant obvious: submits always execute
        // against the current session's loader.
        self.network.set_loader(session.shared_loader());
        for _ in 0..8 {
            // APPLY for module loads: their Set-Cookie headers (parked
            // on the queue by the loader future, which resolves inside
            // the job executor beyond the session's reach) land in the
            // jar here, on the session's thread.
            for (url, headers) in self.network.take_module_cookies() {
                for header in headers {
                    session.cookies.store(&url, &header);
                }
            }
            let mut settled_fetches: Vec<(Result<String, String>, JsObject, JsObject)> = Vec::new();
            let mut settled_xhrs: Vec<(Result<String, String>, JsObject)> = Vec::new();

            // Submit what scripts queued since the last pump. A
            // preparation failure (bad URL, the file:// gate) settles
            // synchronously, like the legacy synchronous pump.
            let requests = std::mem::take(&mut self.pending_fetches);
            let xhrs = std::mem::take(&mut self.pending_xhrs);
            for (target, include, resolve, reject) in requests {
                let credentials = if include {
                    ScriptCredentials::Include
                } else {
                    ScriptCredentials::SameOrigin
                };
                // fetch() is always a plain GET without custom headers
                // here — a CORS "simple request", so no preflight.
                match prepare_fetch(session, &target, None, credentials, "GET", &[]) {
                    Ok((request, check)) => {
                        self.submit_read(request, check, SettleTarget::Fetch(resolve, reject));
                    }
                    Err(error) => settled_fetches.push((Err(error), resolve, reject)),
                }
            }
            for request in xhrs {
                let xhr = request.xhr.clone();
                let credentials = if request.with_credentials {
                    // xhr.withCredentials = true: the fetch API's
                    // credentials: "include" flow — cookies ride
                    // cross-origin, and ACAO '*' no longer grants.
                    ScriptCredentials::Include
                } else {
                    ScriptCredentials::SameOrigin
                };
                match prepare_fetch(
                    session,
                    &request.url,
                    xhr_body(&request),
                    credentials,
                    &request.method,
                    &request.headers,
                ) {
                    Ok((prepared, check)) => {
                        self.submit_read(prepared, check, SettleTarget::Xhr(xhr));
                    }
                    Err(error) => settled_xhrs.push((Err(error), xhr)),
                }
            }

            // Drain preflight probes: a passing grant is cached on the
            // session and the actual request goes out (its response is
            // still CORS-gated at drain); a refused probe fails the
            // read like a network error, and the request never leaves.
            for (id, result) in self.network.drain_preflights() {
                let Some(pending) = self.in_flight_preflights.remove(&id) else {
                    continue;
                };
                let granted =
                    matches!(&result, Ok(grant) if !pending.check.preflight_blocks(grant));
                if granted {
                    session.cache_preflight(pending.check.page_origin(), &pending.request.url);
                    let mut check = pending.check;
                    // The grant answered the probe: the resubmitted
                    // request must not probe again (it would loop).
                    check.take_preflight();
                    self.submit_read(pending.request, Some(check), pending.target);
                    continue;
                }
                let error = match result {
                    Err(error) => error.to_string(),
                    Ok(_) => "CORS preflight refused the request".to_string(),
                };
                match pending.target {
                    SettleTarget::Fetch(resolve, reject) => {
                        settled_fetches.push((Err(error), resolve, reject));
                    }
                    SettleTarget::Xhr(xhr) => settled_xhrs.push((Err(error), xhr)),
                }
            }

            // Drain completions: cookies land in the jar here, on the
            // session's thread, before the result reaches JS. A
            // cross-origin read only reaches JS when its CORS grant
            // allows it; otherwise the promise/XHR settles as a
            // network error, like a real browser.
            for (id, result) in self.network.drain() {
                let check = self.cors_checks.remove(&id);
                let result = match result {
                    Ok(response) => {
                        session.store_response_cookies(&response);
                        if check.is_some_and(|check| check.blocks(&response)) {
                            Err("CORS: cross-origin response is not readable by this page"
                                .to_string())
                        } else {
                            Ok(response.text())
                        }
                    }
                    Err(error) => Err(error.to_string()),
                };
                if let Some((resolve, reject)) = self.in_flight_fetches.remove(&id) {
                    settled_fetches.push((result, resolve, reject));
                } else if let Some(xhr) = self.in_flight_xhrs.remove(&id) {
                    settled_xhrs.push((result, xhr));
                }
            }

            if settled_fetches.is_empty() && settled_xhrs.is_empty() {
                // Nothing to settle: either quiescent, or requests are
                // in flight on workers and the wake hook will re-enter.
                return;
            }
            self.enter(session, |context| {
                for (result, resolve, reject) in settled_fetches {
                    let call = match result {
                        Ok(body) => {
                            let response = response_object(&body, context);
                            resolve.call(&JsValue::undefined(), &[response.into()], context)
                        }
                        Err(error) => reject.call(
                            &JsValue::undefined(),
                            &[JsValue::from(js_string!(error.as_str()))],
                            context,
                        ),
                    };
                    if let Err(error) = call {
                        report_script_error(&error);
                    }
                }
                for (result, xhr) in settled_xhrs {
                    settle_xhr(result, &xhr, context);
                }
            });
        }
        eprintln!("[js] fetch chain ran too deep; dropping the rest");
        self.pending_fetches.clear();
        self.pending_xhrs.clear();
    }

    /// Submits a prepared script read to the network queue — or, when
    /// its CORS check still carries a preflight probe, submits the
    /// PROBE and parks the read until the grant lands.
    fn submit_read(
        &mut self,
        request: ResourceRequest,
        check: Option<CorsCheck>,
        target: SettleTarget,
    ) {
        if let Some(probe) = check.as_ref().and_then(CorsCheck::preflight).cloned() {
            let id = self.network.submit_preflight(probe);
            self.in_flight_preflights.insert(
                id,
                PendingPreflight {
                    request,
                    check: check.expect("a probe implies a check"),
                    target,
                },
            );
            return;
        }
        let id = self.network.submit(request);
        if let Some(check) = check {
            self.cors_checks.insert(id, check);
        }
        match target {
            SettleTarget::Fetch(resolve, reject) => {
                self.in_flight_fetches.insert(id, (resolve, reject));
            }
            SettleTarget::Xhr(xhr) => {
                self.in_flight_xhrs.insert(id, xhr);
            }
        }
    }

    /// Whether any script-initiated network load is still in flight.
    #[must_use]
    pub fn has_pending_network(&self) -> bool {
        !self.in_flight_fetches.is_empty()
            || !self.in_flight_xhrs.is_empty()
            || !self.in_flight_preflights.is_empty()
    }

    /// Whether timers are pending (the shell keeps frames coming).
    #[must_use]
    pub fn has_timers(&self) -> bool {
        !self.timers.is_empty()
    }

    /// The nearest pending timer's due time, if any — the shell sleeps
    /// until then instead of spinning frames while a timer waits.
    #[must_use]
    pub fn next_timer_due_ms(&self) -> Option<f64> {
        self.timers
            .iter()
            .map(|timer| timer.due_ms)
            .reduce(f64::min)
    }

    /// Runs `action` with the session's page checked into the bridge,
    /// then checks it back out, collecting registrations and relayouting
    /// if the DOM changed.
    fn enter<L: ResourceLoader>(
        &mut self,
        session: &mut Session<L>,
        action: impl FnOnce(&mut Context),
    ) {
        let now_ms = self.now_ms;
        let url = session.current_url().cloned();
        let cookie_header = url
            .as_ref()
            .and_then(|url| session.cookies.header_for(url))
            .unwrap_or_default();
        let storage_origin = url.as_ref().map(crate::storage::origin_key);
        let local_storage = storage_origin
            .as_ref()
            .map(|origin| session.storage.load(origin))
            .unwrap_or_default();
        let history_length = session.history_len();
        let history_state = session.history_state();
        with_bridge(|bridge| {
            bridge.page = session.page.take();
            bridge.form_values = std::mem::take(&mut session.form_values);
            bridge.dirty = false;
            bridge.now_ms = now_ms;
            bridge.url = url.as_ref().map(ToString::to_string).unwrap_or_default();
            bridge.cookie_header = cookie_header;
            bridge.local_storage = local_storage;
            bridge.storage_dirty = false;
            bridge.session_storage = std::mem::take(&mut self.session_storage);
            bridge.history_length = history_length;
            bridge.history_state = history_state;
        });
        action(&mut self.context);
        // Drain the microtask queue (promise .then/await continuations)
        // while the page is still checked in.
        if let Err(error) = self.context.run_jobs() {
            report_script_error(&error);
        }
        let dirty = with_bridge(|bridge| {
            session.page = bridge.page.take();
            session.form_values = std::mem::take(&mut bridge.form_values);
            for (node, event, callback) in bridge.pending_listeners.drain(..) {
                self.listeners
                    .entry(node)
                    .or_default()
                    .entry(event)
                    .or_default()
                    .push(callback);
            }
            for (node, event, callback) in bridge.pending_removals.drain(..) {
                remove_listener(&mut self.listeners, node, &event, &callback);
            }
            for (id, delay, interval, callback) in bridge.pending_timers.drain(..) {
                self.timers.push(Timer {
                    id,
                    due_ms: now_ms + delay,
                    interval_ms: interval,
                    callback,
                });
            }
            for cleared in bridge.cleared_timers.drain(..) {
                self.timers.retain(|timer| timer.id != cleared);
            }
            self.pending_fetches.append(&mut bridge.pending_fetches);
            self.pending_xhrs.append(&mut bridge.pending_xhrs);
            self.session_storage = std::mem::take(&mut bridge.session_storage);
            if let Some(target) = bridge.pending_navigation.take() {
                self.navigation = Some(target);
            }
            if let Some(target) = bridge.pending_focus.take() {
                self.focus_request = Some(target);
            }
            let pending_cookies = std::mem::take(&mut bridge.pending_cookies);
            let local_storage = std::mem::take(&mut bridge.local_storage);
            (
                bridge.dirty,
                pending_cookies,
                bridge.storage_dirty,
                local_storage,
                bridge.pending_history.take(),
            )
        });
        let (dirty, pending_cookies, storage_dirty, local_storage, history_op) = dirty;
        if let Some(url) = &url {
            for header in pending_cookies {
                session.cookies.store(url, &header);
            }
        }
        if storage_dirty && let Some(origin) = &storage_origin {
            session.storage.save(origin, &local_storage);
        }
        match history_op {
            Some(HistoryOp::Push { url, state }) => {
                if let Ok(url) = Url::parse(&url) {
                    session.push_state(url, state);
                }
            }
            Some(HistoryOp::Replace { url, state }) => {
                if let Ok(url) = Url::parse(&url) {
                    session.replace_state(url, state);
                }
            }
            // A traversal becomes a shell navigation unless the script
            // already asked for one in the same entry.
            Some(HistoryOp::Back) if self.navigation.is_none() => {
                self.navigation = Some("::back".to_string());
            }
            Some(HistoryOp::Forward) if self.navigation.is_none() => {
                self.navigation = Some("::forward".to_string());
            }
            _ => {}
        }
        if dirty {
            session.relayout();
        }
    }
}

/// One `<script>` element, classified by how the spec schedules it.
enum ScriptEntry {
    /// Runs in document order as encountered: classic inline scripts,
    /// classic `src=` scripts and `async` ones (see [`PageScripts::new`]
    /// for why async keeps document order here).
    Now(String),
    /// `<script defer>`: after the document parse, in document order
    /// together with modules. Inline `defer` (no `src`) is meaningless
    /// per spec and stays a `Now` entry.
    Deferred(String),
    /// `<script type="module">`: always deferred, in document order
    /// together with `defer` scripts. `key` is the module's canonical
    /// URL — the resolved `src`, or a synthetic fragment on the page
    /// URL for inline modules so their relative imports still resolve.
    Module { key: String, source: String },
}

/// The page's `<script>` elements in document order, with `src=`
/// bodies fetched up front (a failed fetch drops just that script).
fn collect_scripts<L: ResourceLoader>(session: &mut Session<L>) -> Vec<ScriptEntry> {
    let Some(base) = session.current_url().cloned() else {
        return Vec::new();
    };
    enum Kind {
        Classic { defer: bool },
        Module,
    }
    // Phase 1: read the document (immutable borrow of the session).
    let found: Vec<(Kind, Option<String>, String)> = {
        let Some(page) = session.page.as_ref() else {
            return Vec::new();
        };
        let document = &page.document;
        document
            .descendants(document.root())
            .filter_map(|node| {
                let element = document.element(node)?;
                if element.tag_name != "script" {
                    return None;
                }
                // Classic JavaScript and modules run; JSON-LD, templates
                // and import maps are skipped.
                let kind = element.attributes.get("type").unwrap_or("").trim();
                let module = kind == "module";
                let classic = matches!(
                    kind,
                    "" | "text/javascript" | "application/javascript" | "application/ecmascript"
                );
                if !module && !classic {
                    return None;
                }
                let kind = if module {
                    Kind::Module
                } else {
                    Kind::Classic {
                        defer: element.attributes.contains("defer"),
                    }
                };
                Some((
                    kind,
                    element.attributes.get("src").map(str::to_string),
                    document.text_content(node),
                ))
            })
            .collect()
    };
    // Phase 2: fetch src= bodies and classify (mutable borrow).
    let mut inline_modules = 0u32;
    found
        .into_iter()
        .filter_map(|(kind, src, inline)| {
            // A src= that failed to resolve or fetch drops the script.
            let (url, source) = match src {
                Some(src) => {
                    let url = resolve_url(&base, &src).ok()?;
                    let text = session
                        .fetch_resource(url.clone(), None)
                        .ok()
                        .map(|response| response.text())?;
                    (Some(url), text)
                }
                None => (None, inline),
            };
            match kind {
                Kind::Module => {
                    let key = match &url {
                        Some(url) => url.to_string(),
                        None => {
                            let key = format!("{base}#inline-module-{inline_modules}");
                            inline_modules += 1;
                            key
                        }
                    };
                    Some(ScriptEntry::Module { key, source })
                }
                // Inline `defer` (no src) is meaningless per spec and
                // runs right away like any other inline script.
                Kind::Classic { defer } if defer && url.is_some() => {
                    Some(ScriptEntry::Deferred(source))
                }
                Kind::Classic { .. } => Some(ScriptEntry::Now(source)),
            }
        })
        .collect()
}

// ---- modules ----

/// Whether any element carries an inline `on*` handler attribute — such
/// a page needs a script world even without a single `<script>` tag.
fn has_inline_handlers<L: ResourceLoader>(session: &Session<L>) -> bool {
    session.page().is_some_and(|page| {
        let document = &page.document;
        document.descendants(document.root()).any(|node| {
            document.element(node).is_some_and(|element| {
                element
                    .attributes
                    .iter()
                    .any(|(name, _)| name.starts_with("on"))
            })
        })
    })
}

/// Import graphs deeper than this many fetch rounds give up (each
/// round resolves one level; cycles are served from the cache).
const MAX_MODULE_ROUNDS: u32 = 64;

/// Serves `import` specifiers from pre-fetched sources — never from
/// the filesystem. During page load [`PageScripts::run_module`] drives
/// it in a fetch-retry loop: the loader records the URLs it was asked
/// for but has no source for (`misses`), the caller fetches them
/// through the session and retries. Once the static graph is settled
/// (`enable_queue_fetch`), a runtime dynamic `import()` of a
/// never-fetched URL instead goes through the page's network queue:
/// the load runs on a worker and the loader future awaits the result,
/// so the `import()` promise stays pending instead of rejecting.
struct PageModuleLoader {
    /// Resolved URL -> fetched source text.
    sources: RefCell<HashMap<String, String>>,
    /// Resolved URL -> parsed module (one instance per URL, so cyclic
    /// imports and duplicate tags share a single evaluation).
    cache: RefCell<HashMap<String, Module>>,
    /// URLs requested without a known source since the last drain.
    misses: RefCell<Vec<String>>,
    /// The page's network queue, for runtime dynamic-import fetches.
    network: Rc<NetworkQueue>,
    /// Runtime mode: misses are fetched through the queue instead of
    /// being reported for the page-load retry loop.
    queue_fetch: Cell<bool>,
}

impl PageModuleLoader {
    fn new(network: Rc<NetworkQueue>) -> Self {
        Self {
            sources: RefCell::new(HashMap::new()),
            cache: RefCell::new(HashMap::new()),
            misses: RefCell::new(Vec::new()),
            network,
            queue_fetch: Cell::new(false),
        }
    }

    fn enable_queue_fetch(&self) {
        self.queue_fetch.set(true);
    }

    fn give_source(&self, url: &str, source: &str) {
        self.sources
            .borrow_mut()
            .insert(url.to_string(), source.to_string());
    }

    fn take_misses(&self) -> Vec<String> {
        std::mem::take(&mut *self.misses.borrow_mut())
    }

    /// Runtime fetch of a module the static graph never loaded:
    /// submits a prepared request to the network queue and awaits the
    /// worker's slot. The job executor keeps polling the future (the
    /// queue's module-activity counter tells it progress is coming),
    /// so this resolves the import without blocking the shell thread
    /// when the queue is threaded. The response's Set-Cookie headers
    /// are parked on the queue — the next pump's APPLY phase stores
    /// them in the session's jar.
    async fn load_over_network(
        self: &Rc<Self>,
        key: &str,
        context: &RefCell<&mut Context>,
    ) -> JsResult<Module> {
        let url = Url::parse(key).map_err(|error| {
            JsNativeError::typ().with_message(format!("bad module URL '{key}': {error}"))
        })?;
        let request = prepare_module_request(&url)?;
        let slot: ModuleSlot = self.network.submit_module(request);
        let payload = loop {
            if let Some(outcome) = slot.lock().expect("module slot").take() {
                break outcome;
            }
            future::yield_now().await;
        };
        let payload = payload.map_err(|error| {
            JsNativeError::typ().with_message(format!("module fetch failed ({key}): {error}"))
        })?;
        self.network
            .note_module_cookies(payload.final_url, payload.set_cookies);
        let source = payload.source;
        let module = Module::parse(
            Source::from_bytes(source.as_bytes()).with_path(Path::new(key)),
            None,
            &mut context.borrow_mut(),
        )?;
        self.give_source(key, &source);
        self.cache
            .borrow_mut()
            .insert(key.to_string(), module.clone());
        Ok(module)
    }
}

/// The PREPARE phase of a runtime module fetch. The loader runs inside
/// the script job executor — no session is reachable there — so it
/// prepares from the bridge snapshot instead: the file:// gate mirrors
/// the session's, and the Cookie header rides only to same-origin URLs
/// (the full per-URL jar lookup that fetch/XHR get on the UI thread is
/// unavailable here; `Set-Cookie` on module responses still reaches
/// the jar, one pump later, via the queue's APPLY detour).
fn prepare_module_request(url: &Url) -> JsResult<ResourceRequest> {
    let (page_url, cookie) =
        with_bridge(|bridge| (bridge.url.clone(), bridge.cookie_header.clone()));
    let page = Url::parse(&page_url).ok();
    if url.scheme() == "file"
        && page
            .as_ref()
            .is_some_and(|page| matches!(page.scheme(), "http" | "https"))
    {
        return Err(JsNativeError::typ()
            .with_message(format!(
                "module fetch blocked (file from a remote page): {url}"
            ))
            .into());
    }
    let same_origin = page
        .as_ref()
        .is_some_and(|page| page.origin() == url.origin());
    Ok(ResourceRequest {
        cookie: (same_origin && !cookie.is_empty()).then_some(cookie),
        url: url.clone(),
        body: None,
    })
}

impl ModuleLoader for PageModuleLoader {
    async fn load_imported_module(
        self: Rc<Self>,
        referrer: Referrer,
        specifier: JsString,
        context: &RefCell<&mut Context>,
    ) -> JsResult<Module> {
        let specifier = specifier.to_std_string_escaped();
        let base = referrer
            .path()
            .and_then(|path| path.to_str())
            .ok_or_else(|| JsNativeError::typ().with_message("import without a referrer URL"))?;
        let url = Url::parse(base)
            .map_err(|error| {
                JsNativeError::typ().with_message(format!("bad referrer '{base}': {error}"))
            })
            .and_then(|base| {
                resolve_url(&base, &specifier).map_err(|error| {
                    JsNativeError::typ().with_message(format!("bad import '{specifier}': {error}"))
                })
            })?;
        let key = url.to_string();
        if let Some(module) = self.cache.borrow().get(&key) {
            return Ok(module.clone());
        }
        // Bound the immutable borrow to this statement: the else path
        // may fetch over the network and re-enter `sources` mutably.
        let known = self.sources.borrow().get(&key).cloned();
        let Some(source) = known else {
            // Runtime dynamic import() of a URL the static graph never
            // fetched: load it through the network queue and await the
            // worker — the import() promise stays pending meanwhile.
            if self.queue_fetch.get() {
                return self.load_over_network(&key, context).await;
            }
            let mut misses = self.misses.borrow_mut();
            if !misses.contains(&key) {
                misses.push(key.clone());
            }
            return Err(JsNativeError::typ()
                .with_message(format!("module not fetched: {key}"))
                .into());
        };
        let source = Source::from_bytes(source.as_bytes()).with_path(Path::new(&key));
        let module = Module::parse(source, None, &mut context.borrow_mut())?;
        self.cache.borrow_mut().insert(key, module.clone());
        Ok(module)
    }
}

// ---- job budget ----

/// A runaway loop throws after this many iterations (per loop), so
/// `while (true) {}` becomes a catchable JS error instead of a hang.
const LOOP_ITERATION_BUDGET: u64 = 1_000_000;

/// Jobs a single [`Context::run_jobs`] call may run before yielding
/// the rest to the next entry — a promise that re-schedules itself
/// (`Promise.resolve().then(f)` where f enqueues f) otherwise keeps
/// the drain loop spinning forever.
const MAX_JOBS_PER_TICK: usize = 10_000;

/// Boa's default `SimpleJobExecutor` drains the queues until empty,
/// which self-scheduling promises keep non-empty forever. This
/// executor mirrors boa's drain loop (boe_engine 0.21 `src/job.rs`,
/// MIT/Apache-2.0) but caps the jobs per call: whatever is left stays
/// queued and continues on the next script entry.
///
/// Async jobs that park (a dynamic `import()` awaiting its network
/// fetch) cannot just spin: while the network queue reports a module
/// load in flight the loop naps (1 ms) and keeps polling — progress
/// arrives from the worker thread. A parked job with no network
/// activity (a top-level await on the page's own fetch pump, which
/// only runs between script entries) cannot progress inside this call;
/// the executor breaks instead of spinning forever, and the parked
/// future is dropped — its module evaluation never completes.
///
/// DOCUMENTED LIMIT: that dropped future is why a module whose
/// top-level await waits on the page's own `fetch()` promise never
/// settles. In boa 0.21, `NativeAsyncJob::call` consumes the job and
/// returns a future borrowing the `&RefCell<&mut Context>` of THIS
/// `run_jobs` call, so the future can neither be re-created (the
/// closure is `FnOnce` — calling again would restart the async fn)
/// nor stored across script entries without a self-referential,
/// unsafe structure. Top-level awaits on module-internal promises
/// (imports, already-resolved values) DO settle; only awaits that
/// depend on the between-entries fetch pump hit this limit.
struct BoundedJobExecutor {
    promise_jobs: RefCell<VecDeque<PromiseJob>>,
    async_jobs: RefCell<VecDeque<NativeAsyncJob>>,
    timeout_jobs: RefCell<BTreeMap<JsInstant, TimeoutJob>>,
    generic_jobs: RefCell<VecDeque<GenericJob>>,
    /// Module loads in flight on the network queue (shared counter).
    network_activity: Arc<AtomicUsize>,
}

impl BoundedJobExecutor {
    fn with_activity(network_activity: Arc<AtomicUsize>) -> Self {
        Self {
            promise_jobs: RefCell::new(VecDeque::new()),
            async_jobs: RefCell::new(VecDeque::new()),
            timeout_jobs: RefCell::new(BTreeMap::new()),
            generic_jobs: RefCell::new(VecDeque::new()),
            network_activity,
        }
    }

    fn clear(&self) {
        self.promise_jobs.borrow_mut().clear();
        self.async_jobs.borrow_mut().clear();
        self.timeout_jobs.borrow_mut().clear();
        self.generic_jobs.borrow_mut().clear();
    }
}

impl std::fmt::Debug for BoundedJobExecutor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BoundedJobExecutor").finish_non_exhaustive()
    }
}

impl JobExecutor for BoundedJobExecutor {
    fn enqueue_job(self: Rc<Self>, job: Job, context: &mut Context) {
        match job {
            Job::PromiseJob(job) => self.promise_jobs.borrow_mut().push_back(job),
            Job::AsyncJob(job) => self.async_jobs.borrow_mut().push_back(job),
            Job::TimeoutJob(job) => {
                let now = context.clock().now();
                self.timeout_jobs
                    .borrow_mut()
                    .insert(now + job.timeout(), job);
            }
            Job::GenericJob(job) => self.generic_jobs.borrow_mut().push_back(job),
            // Non-exhaustive: future job kinds have no queue here.
            _ => {}
        }
    }

    fn run_jobs(self: Rc<Self>, context: &mut Context) -> JsResult<()> {
        future::block_on(self.run_jobs_async(&RefCell::new(context)))
    }

    async fn run_jobs_async(self: Rc<Self>, context: &RefCell<&mut Context>) -> JsResult<()> {
        let mut group = FutureGroup::new();
        let mut budget = MAX_JOBS_PER_TICK;
        loop {
            let drained = std::mem::take(&mut *self.async_jobs.borrow_mut());
            let mut progressed = !drained.is_empty();
            for job in drained {
                group.insert(job.call(context));
            }

            // There are no timeout jobs to run IIF there are no jobs to
            // execute right now.
            let no_timeout_jobs_to_run = {
                let now = context.borrow().clock().now();
                !self.timeout_jobs.borrow().iter().any(|(t, _)| &now >= t)
            };

            let idle = self.promise_jobs.borrow().is_empty()
                && self.async_jobs.borrow().is_empty()
                && self.generic_jobs.borrow().is_empty()
                && no_timeout_jobs_to_run
                && group.is_empty();
            if idle || budget == 0 {
                break;
            }

            match future::poll_once(group.next()).await {
                Some(Some(Err(error))) => {
                    self.clear();
                    return Err(error);
                }
                Some(Some(Ok(_))) => progressed = true,
                Some(None) | None => {}
            }

            {
                let now = context.borrow().clock().now();
                let mut timeouts_borrow = self.timeout_jobs.borrow_mut();
                let mut jobs_to_keep = timeouts_borrow.split_off(&now);
                jobs_to_keep.retain(|_, job| !job.is_cancelled());
                let jobs_to_run = std::mem::replace(&mut *timeouts_borrow, jobs_to_keep);
                drop(timeouts_borrow);

                for job in jobs_to_run.into_values() {
                    budget = budget.saturating_sub(1);
                    progressed = true;
                    if let Err(error) = job.call(&mut context.borrow_mut()) {
                        self.clear();
                        return Err(error);
                    }
                }
            }

            let jobs = std::mem::take(&mut *self.promise_jobs.borrow_mut());
            for job in jobs {
                budget = budget.saturating_sub(1);
                progressed = true;
                if let Err(error) = job.call(&mut context.borrow_mut()) {
                    self.clear();
                    return Err(error);
                }
            }

            let jobs = std::mem::take(&mut *self.generic_jobs.borrow_mut());
            for job in jobs {
                budget = budget.saturating_sub(1);
                progressed = true;
                if let Err(error) = job.call(&mut context.borrow_mut()) {
                    self.clear();
                    return Err(error);
                }
            }
            context.borrow_mut().clear_kept_objects();
            if !progressed {
                // Only parked async jobs remain. With a module load in
                // flight the worker's result is what unblocks them —
                // nap and poll again. Otherwise nothing can progress
                // inside this call (a top-level await on the page's own
                // fetch pump settles between entries); break rather
                // than spin forever.
                if self.network_activity.load(Ordering::Relaxed) == 0 {
                    break;
                }
                std::thread::sleep(Duration::from_millis(1));
            }
            future::yield_now().await;
        }

        Ok(())
    }
}

// ---- globals ----

fn install_globals(context: &mut Context) {
    let log = NativeFunction::from_fn_ptr(console_log).to_js_function(context.realm());
    let console = ObjectInitializer::new(context)
        .property(js_string!("log"), log.clone(), Attribute::all())
        .property(js_string!("warn"), log.clone(), Attribute::all())
        .property(js_string!("error"), log, Attribute::all())
        .build();
    context
        .register_global_property(js_string!("console"), console, Attribute::all())
        .expect("fresh context");

    let body_get = NativeFunction::from_fn_ptr(document_body_get).to_js_function(context.realm());
    let head_get = NativeFunction::from_fn_ptr(document_head_get).to_js_function(context.realm());
    let root_get =
        NativeFunction::from_fn_ptr(document_element_get).to_js_function(context.realm());
    let ready_state_get =
        NativeFunction::from_fn_ptr(document_ready_state_get).to_js_function(context.realm());
    let cookie_get = NativeFunction::from_fn_ptr(cookie_get_).to_js_function(context.realm());
    let cookie_set = NativeFunction::from_fn_ptr(cookie_set_).to_js_function(context.realm());
    let document = ObjectInitializer::new(context)
        .accessor(
            js_string!("cookie"),
            Some(cookie_get),
            Some(cookie_set),
            Attribute::all(),
        )
        .property(js_string!("__node"), JsValue::from(0.0), Attribute::empty())
        .function(
            NativeFunction::from_fn_ptr(add_event_listener),
            js_string!("addEventListener"),
            2,
        )
        .function(
            NativeFunction::from_fn_ptr(remove_event_listener),
            js_string!("removeEventListener"),
            2,
        )
        .accessor(js_string!("body"), Some(body_get), None, Attribute::all())
        .accessor(js_string!("head"), Some(head_get), None, Attribute::all())
        .accessor(
            js_string!("documentElement"),
            Some(root_get),
            None,
            Attribute::all(),
        )
        .accessor(
            js_string!("readyState"),
            Some(ready_state_get),
            None,
            Attribute::all(),
        )
        .function(
            NativeFunction::from_fn_ptr(get_element_by_id),
            js_string!("getElementById"),
            1,
        )
        .function(
            NativeFunction::from_fn_ptr(create_element),
            js_string!("createElement"),
            1,
        )
        .function(
            NativeFunction::from_fn_ptr(query_selector),
            js_string!("querySelector"),
            1,
        )
        .function(
            NativeFunction::from_fn_ptr(query_selector_all),
            js_string!("querySelectorAll"),
            1,
        )
        .function(
            NativeFunction::from_fn_ptr(get_elements_by_class_name),
            js_string!("getElementsByClassName"),
            1,
        )
        .function(
            NativeFunction::from_fn_ptr(get_elements_by_tag_name),
            js_string!("getElementsByTagName"),
            1,
        )
        .build();
    context
        .register_global_property(js_string!("document"), document, Attribute::all())
        .expect("fresh context");
    // window.addEventListener (also reachable bare) registers on the
    // document root, so load/DOMContentLoaded and bubbled events reach it.
    context
        .register_global_builtin_callable(
            js_string!("addEventListener"),
            2,
            NativeFunction::from_fn_ptr(window_add_event_listener),
        )
        .expect("fresh context");
    context
        .register_global_builtin_callable(
            js_string!("removeEventListener"),
            2,
            NativeFunction::from_fn_ptr(window_remove_event_listener),
        )
        .expect("fresh context");
    install_stubs(context);

    context
        .register_global_builtin_callable(
            js_string!("setTimeout"),
            2,
            NativeFunction::from_fn_ptr(set_timeout),
        )
        .expect("fresh context");
    context
        .register_global_builtin_callable(
            js_string!("setInterval"),
            2,
            NativeFunction::from_fn_ptr(set_interval),
        )
        .expect("fresh context");
    for name in [js_string!("clearTimeout"), js_string!("clearInterval")] {
        context
            .register_global_builtin_callable(name, 1, NativeFunction::from_fn_ptr(clear_timer))
            .expect("fresh context");
    }
    context
        .register_global_builtin_callable(
            js_string!("fetch"),
            1,
            NativeFunction::from_fn_ptr(fetch_),
        )
        .expect("fresh context");

    let href_get = NativeFunction::from_fn_ptr(location_href_get).to_js_function(context.realm());
    let href_set = NativeFunction::from_fn_ptr(location_href_set).to_js_function(context.realm());
    let location = ObjectInitializer::new(context)
        .accessor(
            js_string!("href"),
            Some(href_get),
            Some(href_set),
            Attribute::all(),
        )
        .function(
            NativeFunction::from_fn_ptr(location_reload),
            js_string!("reload"),
            0,
        )
        .build();
    context
        .register_global_property(js_string!("location"), location, Attribute::all())
        .expect("fresh context");

    let xhr_ctor = FunctionObjectBuilder::new(
        context.realm(),
        NativeFunction::from_fn_ptr(xml_http_request),
    )
    .name(js_string!("XMLHttpRequest"))
    .length(0)
    .constructor(true)
    .build();
    context
        .register_global_property(js_string!("XMLHttpRequest"), xhr_ctor, Attribute::all())
        .expect("fresh context");

    let history_length_get =
        NativeFunction::from_fn_ptr(history_length_get_).to_js_function(context.realm());
    let history_state_get =
        NativeFunction::from_fn_ptr(history_state_get_).to_js_function(context.realm());
    let history = ObjectInitializer::new(context)
        .function(
            NativeFunction::from_fn_ptr(history_push_state),
            js_string!("pushState"),
            3,
        )
        .function(
            NativeFunction::from_fn_ptr(history_replace_state),
            js_string!("replaceState"),
            3,
        )
        .function(
            NativeFunction::from_fn_ptr(history_back_),
            js_string!("back"),
            0,
        )
        .function(
            NativeFunction::from_fn_ptr(history_forward_),
            js_string!("forward"),
            0,
        )
        .function(
            NativeFunction::from_fn_ptr(history_go_),
            js_string!("go"),
            1,
        )
        .accessor(
            js_string!("length"),
            Some(history_length_get),
            None,
            Attribute::all(),
        )
        .accessor(
            js_string!("state"),
            Some(history_state_get),
            None,
            Attribute::all(),
        )
        .build();
    context
        .register_global_property(js_string!("history"), history, Attribute::all())
        .expect("fresh context");

    let languages = JsArray::from_iter(
        [
            JsValue::from(js_string!("tr-TR")),
            JsValue::from(js_string!("en")),
        ],
        context,
    );
    let navigator = ObjectInitializer::new(context)
        .property(
            js_string!("userAgent"),
            js_string!(USER_AGENT),
            Attribute::all(),
        )
        .property(
            js_string!("language"),
            js_string!("tr-TR"),
            Attribute::all(),
        )
        .property(js_string!("languages"), languages, Attribute::all())
        .property(
            js_string!("platform"),
            js_string!(platform()),
            Attribute::all(),
        )
        .property(js_string!("onLine"), true, Attribute::all())
        .build();
    context
        .register_global_property(js_string!("navigator"), navigator, Attribute::all())
        .expect("fresh context");

    let local_storage = storage_object(StorageKind::Local, context);
    context
        .register_global_property(js_string!("localStorage"), local_storage, Attribute::all())
        .expect("fresh context");
    let session_storage = storage_object(StorageKind::Session, context);
    context
        .register_global_property(
            js_string!("sessionStorage"),
            session_storage,
            Attribute::all(),
        )
        .expect("fresh context");
    // window is the global object itself (enough for window.location,
    // window.setTimeout and friends).
    let global = context.global_object();
    context
        .register_global_property(js_string!("window"), global, Attribute::all())
        .expect("fresh context");
}

fn cookie_get_(_this: &JsValue, _args: &[JsValue], _context: &mut Context) -> JsResult<JsValue> {
    let header = with_bridge(|bridge| bridge.cookie_header.clone());
    Ok(JsValue::from(js_string!(header.as_str())))
}

fn cookie_set_(_this: &JsValue, args: &[JsValue], context: &mut Context) -> JsResult<JsValue> {
    let header = string_arg(args, 0, context);
    with_bridge(|bridge| {
        // Reads within the same entry see the write too; rewriting a
        // name replaces its earlier pair instead of duplicating it.
        let pair = header.split(';').next().unwrap_or("").trim().to_string();
        if !pair.is_empty() {
            let name = pair.split('=').next().unwrap_or("").trim();
            let mut pairs: Vec<&str> = bridge
                .cookie_header
                .split(';')
                .map(str::trim)
                .filter(|existing| {
                    !existing.is_empty() && existing.split('=').next().unwrap_or("").trim() != name
                })
                .collect();
            pairs.push(&pair);
            bridge.cookie_header = pairs.join("; ");
        }
        bridge.pending_cookies.push(header);
    });
    Ok(JsValue::undefined())
}

fn document_body_get(
    _this: &JsValue,
    _args: &[JsValue],
    context: &mut Context,
) -> JsResult<JsValue> {
    // Pages without an explicit <body> fall back to the root, so
    // document.body.appendChild and friends still work.
    let body = with_bridge(|bridge| {
        let page = bridge.page.as_ref()?;
        let document = &page.document;
        document
            .descendants(document.root())
            .find(|node| {
                document
                    .element(*node)
                    .is_some_and(|element| element.tag_name == "body")
            })
            .or_else(|| Some(document.root()))
    });
    Ok(match body {
        Some(node) => element_object(node, context).into(),
        None => JsValue::null(),
    })
}

/// The first element with `tag` in document order (document.head and
/// friends), as a JS wrapper or null.
fn first_element_by_tag(tag: &str, context: &mut Context) -> JsValue {
    let found = with_bridge(|bridge| {
        let page = bridge.page.as_ref()?;
        let document = &page.document;
        document.descendants(document.root()).find(|node| {
            document
                .element(*node)
                .is_some_and(|element| element.tag_name == tag)
        })
    });
    match found {
        Some(node) => element_object(node, context).into(),
        None => JsValue::null(),
    }
}

fn document_head_get(
    _this: &JsValue,
    _args: &[JsValue],
    context: &mut Context,
) -> JsResult<JsValue> {
    Ok(first_element_by_tag("head", context))
}

fn document_element_get(
    _this: &JsValue,
    _args: &[JsValue],
    context: &mut Context,
) -> JsResult<JsValue> {
    // Pages without an explicit <html> fall back to the root, like
    // document.body does.
    let value = first_element_by_tag("html", context);
    if value.is_null() {
        let root = with_bridge(|bridge| bridge.page.as_ref().map(|page| page.document.root()));
        return Ok(match root {
            Some(node) => element_object(node, context).into(),
            None => JsValue::null(),
        });
    }
    Ok(value)
}

fn document_ready_state_get(
    _this: &JsValue,
    _args: &[JsValue],
    _context: &mut Context,
) -> JsResult<JsValue> {
    // Scripts run after the document parse finishes, so the readyState
    // they can ever observe is the post-load one.
    Ok(JsValue::from(js_string!("complete")))
}

fn window_add_event_listener(
    _this: &JsValue,
    args: &[JsValue],
    context: &mut Context,
) -> JsResult<JsValue> {
    // The window listens on the document root (node 0).
    register_listener(args, context, 0);
    Ok(JsValue::undefined())
}

fn window_remove_event_listener(
    _this: &JsValue,
    args: &[JsValue],
    context: &mut Context,
) -> JsResult<JsValue> {
    unregister_listener(args, context, 0);
    Ok(JsValue::undefined())
}

/// Cheap stubs that keep common site boot code from crashing:
/// matchMedia, requestAnimationFrame and getComputedStyle. (Storage,
/// navigator, history and XHR are real natives, not stubs.)
fn install_stubs(context: &mut Context) {
    let source = r#"
        function matchMedia(query) {
            return { matches: false, media: query,
                     addListener() {}, removeListener() {},
                     addEventListener() {}, removeEventListener() {} };
        }
        function requestAnimationFrame(callback) { return setTimeout(callback, 16); }
        function cancelAnimationFrame(id) { clearTimeout(id); }
        function getComputedStyle() { return { getPropertyValue() { return ""; } }; }
    "#;
    if let Err(error) = context.eval(Source::from_bytes(source)) {
        eprintln!("[js] stub install error: {error}");
    }
}

// ---- fetch ----

/// The PREPARE phase of a script fetch, on the caller's thread:
/// resolves the target against the page URL, applies the file:// gate,
/// the same-origin policy and the credentials mode (see
/// [`Session::prepare_script_read`]). The returned [`CorsCheck`] (Some
/// for cross-origin reads only) gates the response at drain time — and
/// may still carry a preflight probe for non-simple requests.
fn prepare_fetch<L: ResourceLoader>(
    session: &Session<L>,
    target: &str,
    body: Option<(String, Vec<u8>)>,
    credentials: ScriptCredentials,
    method: &str,
    headers: &[(String, String)],
) -> Result<(ResourceRequest, Option<CorsCheck>), String> {
    let base = session
        .current_url()
        .cloned()
        .ok_or_else(|| "no page".to_string())?;
    let url = resolve_url(&base, target).map_err(|error| error.to_string())?;
    session
        .prepare_script_read(url, body, credentials, method, headers)
        .map_err(|error| error.to_string())
}

fn fetch_(_this: &JsValue, args: &[JsValue], context: &mut Context) -> JsResult<JsValue> {
    let target = string_arg(args, 0, context);
    // fetch(url, { credentials: 'include' }) — the only read option;
    // the default (and 'same-origin'/'omit') keeps cookies same-origin.
    let include = args
        .get(1)
        .and_then(JsValue::as_object)
        .and_then(|options| options.get(js_string!("credentials"), context).ok())
        .and_then(|value| value.to_string(context).ok())
        .is_some_and(|value| value.to_std_string_escaped() == "include");
    let (promise, resolvers) = JsPromise::new_pending(context);
    with_bridge(|bridge| {
        bridge.pending_fetches.push((
            target,
            include,
            resolvers.resolve.into(),
            resolvers.reject.into(),
        ));
    });
    Ok(promise.into())
}

/// Builds the object a fetch resolves with: `ok`/`status` plus `text()`
/// and `json()` returning already-resolved promises.
fn response_object(body: &str, context: &mut Context) -> JsObject {
    ObjectInitializer::new(context)
        .property(js_string!("ok"), true, Attribute::all())
        .property(js_string!("status"), 200, Attribute::all())
        .property(js_string!("__body"), js_string!(body), Attribute::empty())
        .function(
            NativeFunction::from_fn_ptr(response_text),
            js_string!("text"),
            0,
        )
        .function(
            NativeFunction::from_fn_ptr(response_json),
            js_string!("json"),
            0,
        )
        .build()
}

fn response_body(this: &JsValue, context: &mut Context) -> JsValue {
    this.as_object()
        .and_then(|object| object.get(js_string!("__body"), context).ok())
        .unwrap_or_default()
}

fn response_text(this: &JsValue, _args: &[JsValue], context: &mut Context) -> JsResult<JsValue> {
    let body = response_body(this, context);
    Ok(JsPromise::resolve(body, context).into())
}

fn response_json(this: &JsValue, _args: &[JsValue], context: &mut Context) -> JsResult<JsValue> {
    let body = response_body(this, context);
    // Parse through the engine's own JSON.parse.
    let json = context.global_object().get(js_string!("JSON"), context)?;
    let parse = json
        .as_object()
        .ok_or_else(|| boa_engine::JsNativeError::typ().with_message("JSON missing"))?
        .get(js_string!("parse"), context)?;
    let parsed = parse
        .as_callable()
        .ok_or_else(|| boa_engine::JsNativeError::typ().with_message("parse missing"))?
        .call(&JsValue::undefined(), &[body], context);
    Ok(match parsed {
        Ok(value) => JsPromise::resolve(value, context).into(),
        Err(error) => JsPromise::reject(error, context).into(),
    })
}

// ---- XMLHttpRequest ----

/// `new XMLHttpRequest()`: a plain object carrying readyState/status/
/// responseText as data properties; the natives below mutate it.
fn xml_http_request(
    _this: &JsValue,
    _args: &[JsValue],
    context: &mut Context,
) -> JsResult<JsValue> {
    let headers = JsArray::new(context);
    Ok(ObjectInitializer::new(context)
        .property(js_string!("readyState"), 0, Attribute::all())
        .property(js_string!("status"), 0, Attribute::all())
        .property(js_string!("responseText"), js_string!(""), Attribute::all())
        .property(js_string!("response"), js_string!(""), Attribute::all())
        // Settable any time after open(); read at send().
        .property(js_string!("withCredentials"), false, Attribute::all())
        .property(
            js_string!("__headers"),
            JsValue::from(headers),
            Attribute::empty(),
        )
        .function(NativeFunction::from_fn_ptr(xhr_open), js_string!("open"), 2)
        .function(
            NativeFunction::from_fn_ptr(xhr_set_request_header),
            js_string!("setRequestHeader"),
            2,
        )
        .function(NativeFunction::from_fn_ptr(xhr_send), js_string!("send"), 1)
        .build()
        .into())
}

/// Reads a string own property of the xhr object.
fn xhr_prop(object: &JsObject, name: &str, context: &mut Context) -> String {
    object
        .get(js_string!(name), context)
        .ok()
        .and_then(|value| value.to_string(context).ok())
        .map(|value| value.to_std_string_escaped())
        .unwrap_or_default()
}

fn xhr_open(this: &JsValue, args: &[JsValue], context: &mut Context) -> JsResult<JsValue> {
    let Some(object) = this.as_object() else {
        return Err(boa_engine::JsNativeError::typ()
            .with_message("XMLHttpRequest.open called on a non-xhr")
            .into());
    };
    let method = string_arg(args, 0, context).to_ascii_uppercase();
    let url = string_arg(args, 1, context);
    // The async flag (arg 2) is accepted: every request completes in a
    // later pump either way (inline queue: the pump right after this
    // entry; threaded queue: the pump the completion wake triggers).
    object.set(
        js_string!("__method"),
        js_string!(method.as_str()),
        false,
        context,
    )?;
    object.set(
        js_string!("__url"),
        js_string!(url.as_str()),
        false,
        context,
    )?;
    object.set(
        js_string!("__headers"),
        JsValue::from(JsArray::new(context)),
        false,
        context,
    )?;
    object.set(js_string!("readyState"), 1, false, context)?;
    Ok(JsValue::undefined())
}

fn xhr_set_request_header(
    this: &JsValue,
    args: &[JsValue],
    context: &mut Context,
) -> JsResult<JsValue> {
    let Some(object) = this.as_object() else {
        return Ok(JsValue::undefined());
    };
    let headers = object.get(js_string!("__headers"), context)?;
    let Some(headers) = headers.as_object() else {
        return Ok(JsValue::undefined());
    };
    let Ok(array) = JsArray::from_object(headers) else {
        return Ok(JsValue::undefined());
    };
    array.push(
        JsValue::from(js_string!(string_arg(args, 0, context).as_str())),
        context,
    )?;
    array.push(
        JsValue::from(js_string!(string_arg(args, 1, context).as_str())),
        context,
    )?;
    Ok(JsValue::undefined())
}

fn xhr_send(this: &JsValue, args: &[JsValue], context: &mut Context) -> JsResult<JsValue> {
    let Some(object) = this.as_object() else {
        return Err(boa_engine::JsNativeError::typ()
            .with_message("XMLHttpRequest.send called on a non-xhr")
            .into());
    };
    let url = xhr_prop(&object, "__url", context);
    if url.is_empty() {
        return Err(boa_engine::JsNativeError::typ()
            .with_message("XMLHttpRequest.send called before open")
            .into());
    }
    let method = xhr_prop(&object, "__method", context);
    let mut headers: Vec<(String, String)> = Vec::new();
    if let Ok(value) = object.get(js_string!("__headers"), context)
        && let Some(array) = value.as_object()
        && let Ok(array) = JsArray::from_object(array)
    {
        let length = array.length(context).unwrap_or(0);
        for index in 0..length {
            let item = array
                .get(index, context)
                .ok()
                .and_then(|value| value.to_string(context).ok())
                .map(|value| value.to_std_string_escaped())
                .unwrap_or_default();
            if index % 2 == 0 {
                headers.push((item, String::new()));
            } else if let Some(last) = headers.last_mut() {
                last.1 = item;
            }
        }
    }
    let body = args
        .first()
        .filter(|value| !value.is_null_or_undefined())
        .map(|_| string_arg(args, 0, context));
    let with_credentials = object
        .get(js_string!("withCredentials"), context)
        .ok()
        .is_some_and(|value| value.to_boolean());
    with_bridge(|bridge| {
        bridge.pending_xhrs.push(XhrRequest {
            method,
            url,
            headers,
            body,
            with_credentials,
            xhr: object.clone(),
        });
    });
    Ok(JsValue::undefined())
}

/// The request body for the wire: GET/HEAD never carry one; the
/// Content-Type header (if set via setRequestHeader) becomes the
/// body's content type.
fn xhr_body(request: &XhrRequest) -> Option<(String, Vec<u8>)> {
    if matches!(request.method.as_str(), "" | "GET" | "HEAD") {
        return None;
    }
    let content_type = request
        .headers
        .iter()
        .find(|(name, _)| name.eq_ignore_ascii_case("content-type"))
        .map(|(_, value)| value.clone())
        .unwrap_or_else(|| "text/plain;charset=UTF-8".to_string());
    Some((
        content_type,
        request.body.clone().unwrap_or_default().into_bytes(),
    ))
}

/// Applies a completed XHR round-trip: fills status/responseText and
/// fires onreadystatechange (state 4) plus onload / onerror.
fn settle_xhr(result: Result<String, String>, xhr: &JsObject, context: &mut Context) {
    let mut set = |name: &str, value: JsValue| {
        let _ = xhr.set(js_string!(name), value, false, context);
    };
    let handler = match result {
        Ok(text) => {
            set("status", JsValue::from(200));
            set("responseText", JsValue::from(js_string!(text.as_str())));
            set("response", JsValue::from(js_string!(text.as_str())));
            "onload"
        }
        Err(_) => {
            set("status", JsValue::from(0));
            "onerror"
        }
    };
    set("readyState", JsValue::from(4));
    xhr_fire(xhr, "onreadystatechange", "readystatechange", context);
    xhr_fire(xhr, handler, &handler[2..], context);
}

/// Calls `xhr[handler](event)` when it is callable, with `this` = xhr.
fn xhr_fire(xhr: &JsObject, handler: &str, event_type: &str, context: &mut Context) {
    let Ok(callback) = xhr.get(js_string!(handler), context) else {
        return;
    };
    let Some(callback) = callback.as_callable() else {
        return;
    };
    let event = ObjectInitializer::new(context)
        .property(js_string!("type"), js_string!(event_type), Attribute::all())
        .property(js_string!("target"), xhr.clone(), Attribute::all())
        .build();
    if let Err(error) = callback.call(&xhr.clone().into(), &[event.into()], context) {
        report_script_error(&error);
    }
}

// ---- history ----

/// `history.pushState` / `history.replaceState`: resolves the optional
/// URL against the page, enforces the same-origin rule (a JS error,
/// like the spec's SecurityError) and queues the entry update — no
/// reload happens, that is the whole point of the API.
fn history_update(args: &[JsValue], replace: bool, context: &mut Context) -> JsResult<JsValue> {
    let state = args
        .first()
        .filter(|value| !value.is_null_or_undefined())
        .and_then(|value| json_stringify(value, context));
    let target = args
        .get(2)
        .filter(|value| !value.is_null_or_undefined())
        .map(|_| string_arg(args, 2, context));
    let applied = with_bridge(|bridge| {
        let base = Url::parse(&bridge.url).ok();
        let resolved = match (&base, &target) {
            (Some(base), Some(target)) => resolve_url(base, target).ok(),
            (Some(base), None) => Some(base.clone()),
            _ => None,
        };
        let Some(url) = resolved else {
            return false;
        };
        if base.as_ref().is_none_or(|base| !same_origin(base, &url)) {
            return false;
        }
        let url = url.to_string();
        bridge.url = url.clone();
        bridge.history_state = state.clone();
        if replace {
            bridge.pending_history = Some(HistoryOp::Replace { url, state });
        } else {
            bridge.history_length += 1;
            bridge.pending_history = Some(HistoryOp::Push { url, state });
        }
        true
    });
    if applied {
        Ok(JsValue::undefined())
    } else {
        Err(boa_engine::JsNativeError::typ()
            .with_message("SecurityError: history URL must be same-origin with the page")
            .into())
    }
}

fn history_push_state(
    _this: &JsValue,
    args: &[JsValue],
    context: &mut Context,
) -> JsResult<JsValue> {
    history_update(args, false, context)
}

fn history_replace_state(
    _this: &JsValue,
    args: &[JsValue],
    context: &mut Context,
) -> JsResult<JsValue> {
    history_update(args, true, context)
}

fn history_back_(_this: &JsValue, _args: &[JsValue], _context: &mut Context) -> JsResult<JsValue> {
    with_bridge(|bridge| bridge.pending_history = Some(HistoryOp::Back));
    Ok(JsValue::undefined())
}

fn history_forward_(
    _this: &JsValue,
    _args: &[JsValue],
    _context: &mut Context,
) -> JsResult<JsValue> {
    with_bridge(|bridge| bridge.pending_history = Some(HistoryOp::Forward));
    Ok(JsValue::undefined())
}

fn history_go_(_this: &JsValue, args: &[JsValue], context: &mut Context) -> JsResult<JsValue> {
    let delta = args
        .first()
        .and_then(|value| value.to_number(context).ok())
        .unwrap_or(0.0);
    with_bridge(|bridge| {
        bridge.pending_history = match delta.signum() as i32 {
            -1 => Some(HistoryOp::Back),
            1 => Some(HistoryOp::Forward),
            _ => None,
        };
    });
    Ok(JsValue::undefined())
}

fn history_length_get_(
    _this: &JsValue,
    _args: &[JsValue],
    _context: &mut Context,
) -> JsResult<JsValue> {
    Ok(JsValue::from(
        with_bridge(|bridge| bridge.history_length) as f64
    ))
}

fn history_state_get_(
    _this: &JsValue,
    _args: &[JsValue],
    context: &mut Context,
) -> JsResult<JsValue> {
    let state = with_bridge(|bridge| bridge.history_state.clone());
    Ok(state
        .and_then(|state| json_parse(&state, context))
        .unwrap_or(JsValue::null()))
}

/// JSON.stringify through the engine's own JSON; `None` when the
/// value does not serialize (functions, cycles).
fn json_stringify(value: &JsValue, context: &mut Context) -> Option<String> {
    let json = context
        .global_object()
        .get(js_string!("JSON"), context)
        .ok()?;
    let stringify = json
        .as_object()?
        .get(js_string!("stringify"), context)
        .ok()?;
    let result = stringify
        .as_callable()?
        .call(&JsValue::undefined(), std::slice::from_ref(value), context)
        .ok()?;
    result.as_string().map(|text| text.to_std_string_escaped())
}

/// JSON.parse through the engine's own JSON; `None` on bad input.
fn json_parse(text: &str, context: &mut Context) -> Option<JsValue> {
    let json = context
        .global_object()
        .get(js_string!("JSON"), context)
        .ok()?;
    let parse = json.as_object()?.get(js_string!("parse"), context).ok()?;
    parse
        .as_callable()?
        .call(
            &JsValue::undefined(),
            &[JsValue::from(js_string!(text))],
            context,
        )
        .ok()
}

// ---- navigator ----

/// The UA string navigator.userAgent reports (kept in one place so a
/// future network User-Agent header can share it).
const USER_AGENT: &str = "Lumen/0.1 (educational)";

/// navigator.platform, following the classic (frozen) web values.
fn platform() -> &'static str {
    match std::env::consts::OS {
        "macos" => "MacIntel",
        "windows" => "Win32",
        "linux" => "Linux x86_64",
        _ => "",
    }
}

// ---- Web Storage ----

/// Which backing map a storage object talks to — each kind gets its
/// own fn-ptr set, so the methods work unbound
/// (`const {getItem} = localStorage; getItem("k")`).
#[derive(Clone, Copy)]
enum StorageKind {
    Local,
    Session,
}

/// A `localStorage`/`sessionStorage` global: methods on the target,
/// named-property access (`storage.x`) through a proxy whose get trap
/// falls through to the backing map (like the style/dataset proxies).
fn storage_object(kind: StorageKind, context: &mut Context) -> JsObject {
    type Native = fn(&JsValue, &[JsValue], &mut Context) -> JsResult<JsValue>;
    let (get_item, set_item, remove_item, clear, length, get_trap, set_trap): (
        Native,
        Native,
        Native,
        Native,
        Native,
        Native,
        Native,
    ) = match kind {
        StorageKind::Local => (
            storage_get_item_local,
            storage_set_item_local,
            storage_remove_item_local,
            storage_clear_local,
            storage_length_local,
            storage_get_trap_local,
            storage_set_trap_local,
        ),
        StorageKind::Session => (
            storage_get_item_session,
            storage_set_item_session,
            storage_remove_item_session,
            storage_clear_session,
            storage_length_session,
            storage_get_trap_session,
            storage_set_trap_session,
        ),
    };
    let length_get = NativeFunction::from_fn_ptr(length).to_js_function(context.realm());
    let target = ObjectInitializer::new(context)
        .function(
            NativeFunction::from_fn_ptr(get_item),
            js_string!("getItem"),
            1,
        )
        .function(
            NativeFunction::from_fn_ptr(set_item),
            js_string!("setItem"),
            2,
        )
        .function(
            NativeFunction::from_fn_ptr(remove_item),
            js_string!("removeItem"),
            1,
        )
        .function(NativeFunction::from_fn_ptr(clear), js_string!("clear"), 0)
        .accessor(
            js_string!("length"),
            Some(length_get),
            None,
            Attribute::all(),
        )
        .build();
    JsProxyBuilder::new(target)
        .get(get_trap)
        .set(set_trap)
        .build(context)
        .into()
}

fn storage_get_item_local(
    _this: &JsValue,
    args: &[JsValue],
    context: &mut Context,
) -> JsResult<JsValue> {
    let key = string_arg(args, 0, context);
    Ok(
        with_bridge(|bridge| bridge.local_storage.get(&key).cloned())
            .map_or(JsValue::null(), |value| {
                JsValue::from(js_string!(value.as_str()))
            }),
    )
}

fn storage_set_item_local(
    _this: &JsValue,
    args: &[JsValue],
    context: &mut Context,
) -> JsResult<JsValue> {
    let key = string_arg(args, 0, context);
    let value = string_arg(args, 1, context);
    with_bridge(|bridge| {
        bridge.local_storage.insert(key, value);
        bridge.storage_dirty = true;
    });
    Ok(JsValue::undefined())
}

fn storage_remove_item_local(
    _this: &JsValue,
    args: &[JsValue],
    context: &mut Context,
) -> JsResult<JsValue> {
    let key = string_arg(args, 0, context);
    with_bridge(|bridge| {
        bridge.local_storage.remove(&key);
        bridge.storage_dirty = true;
    });
    Ok(JsValue::undefined())
}

fn storage_clear_local(
    _this: &JsValue,
    _args: &[JsValue],
    _context: &mut Context,
) -> JsResult<JsValue> {
    with_bridge(|bridge| {
        bridge.local_storage.clear();
        bridge.storage_dirty = true;
    });
    Ok(JsValue::undefined())
}

fn storage_length_local(
    _this: &JsValue,
    _args: &[JsValue],
    _context: &mut Context,
) -> JsResult<JsValue> {
    Ok(JsValue::from(
        with_bridge(|bridge| bridge.local_storage.len()) as f64,
    ))
}

fn storage_get_item_session(
    _this: &JsValue,
    args: &[JsValue],
    context: &mut Context,
) -> JsResult<JsValue> {
    let key = string_arg(args, 0, context);
    Ok(
        with_bridge(|bridge| bridge.session_storage.get(&key).cloned())
            .map_or(JsValue::null(), |value| {
                JsValue::from(js_string!(value.as_str()))
            }),
    )
}

fn storage_set_item_session(
    _this: &JsValue,
    args: &[JsValue],
    context: &mut Context,
) -> JsResult<JsValue> {
    let key = string_arg(args, 0, context);
    let value = string_arg(args, 1, context);
    with_bridge(|bridge| {
        bridge.session_storage.insert(key, value);
    });
    Ok(JsValue::undefined())
}

fn storage_remove_item_session(
    _this: &JsValue,
    args: &[JsValue],
    context: &mut Context,
) -> JsResult<JsValue> {
    let key = string_arg(args, 0, context);
    with_bridge(|bridge| {
        bridge.session_storage.remove(&key);
    });
    Ok(JsValue::undefined())
}

fn storage_clear_session(
    _this: &JsValue,
    _args: &[JsValue],
    _context: &mut Context,
) -> JsResult<JsValue> {
    with_bridge(|bridge| bridge.session_storage.clear());
    Ok(JsValue::undefined())
}

fn storage_length_session(
    _this: &JsValue,
    _args: &[JsValue],
    _context: &mut Context,
) -> JsResult<JsValue> {
    Ok(JsValue::from(
        with_bridge(|bridge| bridge.session_storage.len()) as f64,
    ))
}

/// Proxy get trap for one storage map: methods and `length` resolve
/// on the target first, anything else reads the map as a named
/// property (`localStorage.x`). Trap args: [target, key, receiver].
fn storage_get(
    args: &[JsValue],
    map: fn(&mut Bridge) -> &BTreeMap<String, String>,
    context: &mut Context,
) -> JsResult<JsValue> {
    let Some(target) = args.first().and_then(JsValue::as_object) else {
        return Ok(JsValue::undefined());
    };
    let Some(key) = args
        .get(1)
        .and_then(|value| value.to_string(context).ok())
        .map(|value| value.to_std_string_escaped())
    else {
        return Ok(JsValue::undefined());
    };
    let property = js_string!(key.as_str());
    if target.has_property(property.clone(), context)? {
        return target.get(property, context);
    }
    Ok(
        with_bridge(|bridge| map(bridge).get(&key).cloned())
            .map_or(JsValue::undefined(), |value| {
                JsValue::from(js_string!(value.as_str()))
            }),
    )
}

/// Proxy set trap: every named write goes into the map
/// (`localStorage.x = "1"`). Returns true per the trap contract.
fn storage_set(
    args: &[JsValue],
    map: fn(&mut Bridge) -> &mut BTreeMap<String, String>,
    dirty: bool,
    context: &mut Context,
) -> JsResult<JsValue> {
    let Some(key) = args
        .get(1)
        .and_then(|value| value.to_string(context).ok())
        .map(|value| value.to_std_string_escaped())
    else {
        return Ok(JsValue::from(true));
    };
    let value = args
        .get(2)
        .and_then(|value| value.to_string(context).ok())
        .map(|value| value.to_std_string_escaped())
        .unwrap_or_default();
    with_bridge(|bridge| {
        map(bridge).insert(key, value);
        bridge.storage_dirty |= dirty;
    });
    Ok(JsValue::from(true))
}

fn storage_get_trap_local(
    _this: &JsValue,
    args: &[JsValue],
    context: &mut Context,
) -> JsResult<JsValue> {
    storage_get(args, |bridge| &bridge.local_storage, context)
}

fn storage_set_trap_local(
    _this: &JsValue,
    args: &[JsValue],
    context: &mut Context,
) -> JsResult<JsValue> {
    storage_set(args, |bridge| &mut bridge.local_storage, true, context)
}

fn storage_get_trap_session(
    _this: &JsValue,
    args: &[JsValue],
    context: &mut Context,
) -> JsResult<JsValue> {
    storage_get(args, |bridge| &bridge.session_storage, context)
}

fn storage_set_trap_session(
    _this: &JsValue,
    args: &[JsValue],
    context: &mut Context,
) -> JsResult<JsValue> {
    storage_set(args, |bridge| &mut bridge.session_storage, false, context)
}

// ---- location ----

fn location_href_get(
    _this: &JsValue,
    _args: &[JsValue],
    _context: &mut Context,
) -> JsResult<JsValue> {
    let url = with_bridge(|bridge| bridge.url.clone());
    Ok(JsValue::from(js_string!(url.as_str())))
}

fn location_href_set(
    _this: &JsValue,
    args: &[JsValue],
    context: &mut Context,
) -> JsResult<JsValue> {
    let target = string_arg(args, 0, context);
    with_bridge(|bridge| bridge.pending_navigation = Some(target));
    Ok(JsValue::undefined())
}

fn location_reload(
    _this: &JsValue,
    _args: &[JsValue],
    _context: &mut Context,
) -> JsResult<JsValue> {
    with_bridge(|bridge| bridge.pending_navigation = Some("::reload".to_string()));
    Ok(JsValue::undefined())
}

fn console_log(_this: &JsValue, args: &[JsValue], context: &mut Context) -> JsResult<JsValue> {
    let message = args
        .iter()
        .map(|value| {
            value
                .to_string(context)
                .map_or_else(|_| "?".to_string(), |s| s.to_std_string_escaped())
        })
        .collect::<Vec<_>>()
        .join(" ");
    eprintln!("[js] {message}");
    Ok(JsValue::undefined())
}

// ---- error flood control ----

/// How many times one distinct error message prints before suppression
/// kicks in — a misfiring handler retried every frame would otherwise
/// drown the terminal in thousands of identical lines.
const ERROR_REPORT_BUDGET: usize = 3;

thread_local! {
    /// Per-message occurrence counts (one script world lives per
    /// thread, so a thread-local suffices).
    static ERROR_COUNTS: RefCell<HashMap<String, usize>> = RefCell::new(HashMap::new());
}

/// What to do with one more occurrence of an error message.
#[derive(Debug, PartialEq, Eq)]
enum ErrorReport {
    /// Print it (within the budget).
    Print,
    /// Print it once more with a "suppressed from here on" note.
    LastOne,
    /// Swallow it.
    Silent,
}

/// Tallies one occurrence of `message` and decides its fate.
fn tally_error(message: &str) -> ErrorReport {
    ERROR_COUNTS.with(|counts| {
        let mut counts = counts.borrow_mut();
        // Bound the map: a page generating endless DISTINCT messages
        // resets the tally rather than growing it forever.
        if counts.len() >= 1024 && !counts.contains_key(message) {
            counts.clear();
        }
        let count = counts.entry(message.to_string()).or_insert(0);
        *count += 1;
        match *count {
            n if n <= ERROR_REPORT_BUDGET => ErrorReport::Print,
            n if n == ERROR_REPORT_BUDGET + 1 => ErrorReport::LastOne,
            _ => ErrorReport::Silent,
        }
    })
}

/// Prints a runtime script error, deduplicated: the first few
/// occurrences of each distinct message print as-is, the next one
/// prints with a suppression note, and the rest are silent.
fn report_script_error(error: &boa_engine::JsError) {
    let message = error.to_string();
    match tally_error(&message) {
        ErrorReport::Print => eprintln!("[js] script error: {message}"),
        ErrorReport::LastOne => {
            eprintln!("[js] script error: {message} (repeated; further occurrences suppressed)");
        }
        ErrorReport::Silent => {}
    }
}

// ---- document ----

fn string_arg(args: &[JsValue], index: usize, context: &mut Context) -> String {
    args.get(index)
        .and_then(|value| value.to_string(context).ok())
        .map(|value| value.to_std_string_escaped())
        .unwrap_or_default()
}

fn get_element_by_id(
    _this: &JsValue,
    args: &[JsValue],
    context: &mut Context,
) -> JsResult<JsValue> {
    let id = string_arg(args, 0, context);
    let found = with_bridge(|bridge| {
        let page = bridge.page.as_ref()?;
        let document = &page.document;
        document.descendants(document.root()).find(|node| {
            document
                .element(*node)
                .is_some_and(|element| element.attributes.get("id") == Some(id.as_str()))
        })
    });
    Ok(match found {
        Some(node) => element_object(node, context).into(),
        None => JsValue::null(),
    })
}

/// The practical selector subset scripts actually use: `#id`, `.class`,
/// `tag` and `tag.class`.
fn selector_matches(document: &lumen_html::Document, node: NodeId, selector: &str) -> bool {
    let Some(element) = document.element(node) else {
        return false;
    };
    if let Some(id) = selector.strip_prefix('#') {
        return element.attributes.get("id") == Some(id);
    }
    let has_class = |class: &str| {
        element
            .attributes
            .get("class")
            .is_some_and(|classes| classes.split_whitespace().any(|c| c == class))
    };
    if let Some(class) = selector.strip_prefix('.') {
        return has_class(class);
    }
    match selector.split_once('.') {
        Some((tag, class)) => element.tag_name == tag && has_class(class),
        None => element.tag_name == selector,
    }
}

fn query_nodes(selector: &str) -> Vec<NodeId> {
    query_nodes_scoped(selector, None)
}

fn query_nodes_scoped(selector: &str, scope: Option<NodeId>) -> Vec<NodeId> {
    with_bridge(|bridge| {
        let Some(page) = bridge.page.as_ref() else {
            return Vec::new();
        };
        let document = &page.document;
        let root = scope.unwrap_or_else(|| document.root());
        document
            .descendants(root)
            .filter(|node| *node != root && selector_matches(document, *node, selector))
            .collect()
    })
}

fn query_selector(_this: &JsValue, args: &[JsValue], context: &mut Context) -> JsResult<JsValue> {
    let selector = string_arg(args, 0, context);
    Ok(match query_nodes(selector.trim()).first() {
        Some(node) => element_object(*node, context).into(),
        None => JsValue::null(),
    })
}

fn elements_matching(selector: String, context: &mut Context) -> JsResult<JsValue> {
    let elements: Vec<JsValue> = query_nodes(selector.trim())
        .into_iter()
        .map(|node| element_object(node, context).into())
        .collect();
    Ok(boa_engine::object::builtins::JsArray::from_iter(elements, context).into())
}

fn get_elements_by_class_name(
    _this: &JsValue,
    args: &[JsValue],
    context: &mut Context,
) -> JsResult<JsValue> {
    let class = string_arg(args, 0, context);
    elements_matching(format!(".{class}"), context)
}

fn get_elements_by_tag_name(
    _this: &JsValue,
    args: &[JsValue],
    context: &mut Context,
) -> JsResult<JsValue> {
    let tag = string_arg(args, 0, context);
    elements_matching(tag, context)
}

fn query_selector_all(
    _this: &JsValue,
    args: &[JsValue],
    context: &mut Context,
) -> JsResult<JsValue> {
    let selector = string_arg(args, 0, context);
    let elements: Vec<JsValue> = query_nodes(selector.trim())
        .into_iter()
        .map(|node| element_object(node, context).into())
        .collect();
    Ok(boa_engine::object::builtins::JsArray::from_iter(elements, context).into())
}

fn create_element(_this: &JsValue, args: &[JsValue], context: &mut Context) -> JsResult<JsValue> {
    let tag = string_arg(args, 0, context);
    let node = with_bridge(|bridge| {
        bridge
            .page
            .as_mut()
            .map(|page| page.document.create_element(&tag))
    });
    Ok(match node {
        Some(node) => element_object(node, context).into(),
        None => JsValue::null(),
    })
}

// ---- timers ----

fn queue_timer(args: &[JsValue], interval: bool, context: &mut Context) -> JsResult<JsValue> {
    let Some(callback) = args.first().and_then(JsValue::as_object) else {
        return Ok(JsValue::undefined());
    };
    let delay = args
        .get(1)
        .and_then(|value| value.to_number(context).ok())
        .unwrap_or(0.0)
        .max(0.0);
    let id = with_bridge(|bridge| {
        bridge.next_timer_id += 1;
        let id = bridge.next_timer_id;
        bridge.pending_timers.push((
            id,
            delay,
            interval.then_some(delay.max(1.0)),
            callback.clone(),
        ));
        id
    });
    Ok(JsValue::from(id as f64))
}

fn clear_timer(_this: &JsValue, args: &[JsValue], context: &mut Context) -> JsResult<JsValue> {
    if let Some(id) = args.first().and_then(|value| value.to_number(context).ok()) {
        with_bridge(|bridge| bridge.cleared_timers.push(id as u64));
    }
    Ok(JsValue::undefined())
}

fn set_timeout(_this: &JsValue, args: &[JsValue], context: &mut Context) -> JsResult<JsValue> {
    queue_timer(args, false, context)
}

fn set_interval(_this: &JsValue, args: &[JsValue], context: &mut Context) -> JsResult<JsValue> {
    queue_timer(args, true, context)
}

// ---- elements ----

/// The engine node id an element wrapper points at. A forged `__node`
/// outside the document arena is a TypeError, not an indexing panic.
fn this_node(this: &JsValue, context: &mut Context) -> JsResult<Option<NodeId>> {
    let Some(object) = this.as_object() else {
        return Ok(None);
    };
    let Ok(value) = object.get(js_string!("__node"), context) else {
        return Ok(None);
    };
    let Some(number) = value.as_number() else {
        return Ok(None);
    };
    let node = number as NodeId;
    let valid = number.is_finite()
        && number >= 0.0
        && number.fract() == 0.0
        && with_bridge(|bridge| {
            bridge
                .page
                .as_ref()
                .is_none_or(|page| node < page.document.nodes().len())
        });
    if valid {
        Ok(Some(node))
    } else {
        Err(boa_engine::JsNativeError::typ()
            .with_message("element handle points outside the document")
            .into())
    }
}

/// Builds the JS wrapper for a DOM node: methods plus live accessor
/// properties backed by the bridge.
fn element_object(node: NodeId, context: &mut Context) -> JsObject {
    let text_get = NativeFunction::from_fn_ptr(text_content_get).to_js_function(context.realm());
    let text_set = NativeFunction::from_fn_ptr(text_content_set).to_js_function(context.realm());
    let value_get = NativeFunction::from_fn_ptr(value_get_).to_js_function(context.realm());
    let value_set = NativeFunction::from_fn_ptr(value_set_).to_js_function(context.realm());
    let id_get = NativeFunction::from_fn_ptr(id_get_).to_js_function(context.realm());
    let class_get = NativeFunction::from_fn_ptr(class_name_get).to_js_function(context.realm());
    let html_get = NativeFunction::from_fn_ptr(inner_html_get).to_js_function(context.realm());
    let html_set = NativeFunction::from_fn_ptr(inner_html_set).to_js_function(context.realm());
    let parent_get =
        NativeFunction::from_fn_ptr(parent_element_get).to_js_function(context.realm());
    let parent_node_get =
        NativeFunction::from_fn_ptr(parent_node_get_).to_js_function(context.realm());
    let children_get = NativeFunction::from_fn_ptr(children_get_).to_js_function(context.realm());
    let child_nodes_get =
        NativeFunction::from_fn_ptr(child_nodes_get_).to_js_function(context.realm());
    let first_child_get =
        NativeFunction::from_fn_ptr(first_child_get_).to_js_function(context.realm());
    let last_child_get =
        NativeFunction::from_fn_ptr(last_child_get_).to_js_function(context.realm());
    let next_sibling_get =
        NativeFunction::from_fn_ptr(next_sibling_get_).to_js_function(context.realm());
    let previous_sibling_get =
        NativeFunction::from_fn_ptr(previous_sibling_get_).to_js_function(context.realm());
    let null_frame_get =
        NativeFunction::from_fn_ptr(frame_browsing_context_get).to_js_function(context.realm());
    let style = node_proxy(node, style_get_trap, style_set_trap, context);
    let dataset = node_proxy(node, dataset_get_trap, dataset_set_trap, context);
    let class_set = NativeFunction::from_fn_ptr(class_name_set).to_js_function(context.realm());
    let class_list = ObjectInitializer::new(context)
        .property(
            js_string!("__node"),
            JsValue::from(node as f64),
            Attribute::empty(),
        )
        .function(NativeFunction::from_fn_ptr(class_add), js_string!("add"), 1)
        .function(
            NativeFunction::from_fn_ptr(class_remove),
            js_string!("remove"),
            1,
        )
        .function(
            NativeFunction::from_fn_ptr(class_toggle),
            js_string!("toggle"),
            1,
        )
        .function(
            NativeFunction::from_fn_ptr(class_contains),
            js_string!("contains"),
            1,
        )
        .build();
    ObjectInitializer::new(context)
        .property(
            js_string!("__node"),
            JsValue::from(node as f64),
            Attribute::empty(),
        )
        .function(
            NativeFunction::from_fn_ptr(add_event_listener),
            js_string!("addEventListener"),
            2,
        )
        .function(
            NativeFunction::from_fn_ptr(remove_event_listener),
            js_string!("removeEventListener"),
            2,
        )
        .function(
            NativeFunction::from_fn_ptr(get_attribute),
            js_string!("getAttribute"),
            1,
        )
        .function(
            NativeFunction::from_fn_ptr(set_attribute),
            js_string!("setAttribute"),
            2,
        )
        .function(
            NativeFunction::from_fn_ptr(append_child),
            js_string!("appendChild"),
            1,
        )
        .function(
            NativeFunction::from_fn_ptr(insert_before_),
            js_string!("insertBefore"),
            2,
        )
        .function(
            NativeFunction::from_fn_ptr(remove_child),
            js_string!("removeChild"),
            1,
        )
        .function(
            NativeFunction::from_fn_ptr(closest_),
            js_string!("closest"),
            1,
        )
        .function(
            NativeFunction::from_fn_ptr(remove_node),
            js_string!("remove"),
            0,
        )
        .function(
            NativeFunction::from_fn_ptr(focus_element),
            js_string!("focus"),
            0,
        )
        .function(
            NativeFunction::from_fn_ptr(blur_element),
            js_string!("blur"),
            0,
        )
        .accessor(
            js_string!("textContent"),
            Some(text_get.clone()),
            Some(text_set.clone()),
            Attribute::all(),
        )
        .accessor(
            js_string!("innerText"),
            Some(text_get),
            Some(text_set),
            Attribute::all(),
        )
        .accessor(
            js_string!("value"),
            Some(value_get),
            Some(value_set),
            Attribute::all(),
        )
        .accessor(js_string!("id"), Some(id_get), None, Attribute::all())
        .accessor(
            js_string!("className"),
            Some(class_get),
            Some(class_set),
            Attribute::all(),
        )
        .property(js_string!("classList"), class_list, Attribute::all())
        .accessor(
            js_string!("innerHTML"),
            Some(html_get),
            Some(html_set),
            Attribute::all(),
        )
        .accessor(
            js_string!("parentElement"),
            Some(parent_get),
            None,
            Attribute::all(),
        )
        .accessor(
            js_string!("parentNode"),
            Some(parent_node_get),
            None,
            Attribute::all(),
        )
        .accessor(
            js_string!("children"),
            Some(children_get),
            None,
            Attribute::all(),
        )
        .accessor(
            js_string!("childNodes"),
            Some(child_nodes_get),
            None,
            Attribute::all(),
        )
        .accessor(
            js_string!("firstChild"),
            Some(first_child_get),
            None,
            Attribute::all(),
        )
        .accessor(
            js_string!("lastChild"),
            Some(last_child_get),
            None,
            Attribute::all(),
        )
        .accessor(
            js_string!("nextSibling"),
            Some(next_sibling_get),
            None,
            Attribute::all(),
        )
        .accessor(
            js_string!("previousSibling"),
            Some(previous_sibling_get),
            None,
            Attribute::all(),
        )
        // An iframe's browsing context is not implemented: null lets
        // feature-detecting scripts (Cloudflare's challenge) bail out
        // cleanly instead of crashing on undefined.
        .accessor(
            js_string!("contentDocument"),
            Some(null_frame_get.clone()),
            None,
            Attribute::all(),
        )
        .accessor(
            js_string!("contentWindow"),
            Some(null_frame_get),
            None,
            Attribute::all(),
        )
        .property(js_string!("style"), style, Attribute::all())
        .property(js_string!("dataset"), dataset, Attribute::all())
        .function(
            NativeFunction::from_fn_ptr(element_query_selector),
            js_string!("querySelector"),
            1,
        )
        .function(
            NativeFunction::from_fn_ptr(element_query_selector_all),
            js_string!("querySelectorAll"),
            1,
        )
        .function(
            NativeFunction::from_fn_ptr(bounding_client_rect),
            js_string!("getBoundingClientRect"),
            0,
        )
        .build()
}

/// A proxy whose traps see the element's node id (via the target's
/// hidden `__node`) — powers `el.style.x = ...` and `el.dataset.x`.
type Trap = fn(&JsValue, &[JsValue], &mut Context) -> JsResult<JsValue>;

fn node_proxy(node: NodeId, get: Trap, set: Trap, context: &mut Context) -> JsObject {
    let target = ObjectInitializer::new(context)
        .property(
            js_string!("__node"),
            JsValue::from(node as f64),
            Attribute::empty(),
        )
        .build();
    JsProxyBuilder::new(target)
        .get(get)
        .set(set)
        .build(context)
        .into()
}

/// Proxy traps receive [target, key, (value), receiver].
fn trap_context(args: &[JsValue], context: &mut Context) -> JsResult<Option<(NodeId, String)>> {
    let Some(first) = args.first() else {
        return Ok(None);
    };
    let Some(node) = this_node(first, context)? else {
        return Ok(None);
    };
    let Some(key) = args
        .get(1)
        .and_then(|value| value.to_string(context).ok())
        .map(|value| value.to_std_string_escaped())
    else {
        return Ok(None);
    };
    Ok(Some((node, key)))
}

/// camelCase to kebab-case (backgroundColor -> background-color).
fn kebab(name: &str) -> String {
    let mut output = String::with_capacity(name.len() + 4);
    for character in name.chars() {
        if character.is_ascii_uppercase() {
            output.push('-');
            output.push(character.to_ascii_lowercase());
        } else {
            output.push(character);
        }
    }
    output
}

fn style_decls(attribute: &str) -> Vec<(String, String)> {
    attribute
        .split(';')
        .filter_map(|declaration| {
            let (name, value) = declaration.split_once(':')?;
            Some((name.trim().to_string(), value.trim().to_string()))
        })
        .collect()
}

fn style_get_trap(_this: &JsValue, args: &[JsValue], context: &mut Context) -> JsResult<JsValue> {
    let Some((node, key)) = trap_context(args, context)? else {
        return Ok(JsValue::undefined());
    };
    let property = kebab(&key);
    let value = with_bridge(|bridge| {
        let attribute = bridge
            .page
            .as_ref()
            .and_then(|page| page.document.element(node))
            .and_then(|element| element.attributes.get("style"))
            .unwrap_or_default()
            .to_string();
        if key == "cssText" {
            return attribute;
        }
        style_decls(&attribute)
            .into_iter()
            .rev()
            .find(|(name, _)| *name == property)
            .map(|(_, value)| value)
            .unwrap_or_default()
    });
    Ok(JsValue::from(js_string!(value.as_str())))
}

fn style_set_trap(_this: &JsValue, args: &[JsValue], context: &mut Context) -> JsResult<JsValue> {
    let Some((node, key)) = trap_context(args, context)? else {
        return Ok(JsValue::from(true));
    };
    let value = args
        .get(2)
        .and_then(|value| value.to_string(context).ok())
        .map(|value| value.to_std_string_escaped())
        .unwrap_or_default();
    let property = kebab(&key);
    with_bridge(|bridge| {
        let Some(page) = bridge.page.as_mut() else {
            return;
        };
        let merged = if key == "cssText" {
            value.clone()
        } else {
            let attribute = page
                .document
                .element(node)
                .and_then(|element| element.attributes.get("style"))
                .unwrap_or_default()
                .to_string();
            let mut declarations: Vec<(String, String)> = style_decls(&attribute)
                .into_iter()
                .filter(|(name, _)| *name != property)
                .collect();
            if !value.is_empty() {
                declarations.push((property.clone(), value.clone()));
            }
            declarations
                .into_iter()
                .map(|(name, value)| format!("{name}: {value}"))
                .collect::<Vec<_>>()
                .join("; ")
        };
        page.document.set_attribute(node, "style", &merged);
        bridge.dirty = true;
    });
    Ok(JsValue::from(true))
}

fn dataset_get_trap(_this: &JsValue, args: &[JsValue], context: &mut Context) -> JsResult<JsValue> {
    let Some((node, key)) = trap_context(args, context)? else {
        return Ok(JsValue::undefined());
    };
    let attribute = format!("data-{}", kebab(&key));
    let value = with_bridge(|bridge| {
        bridge
            .page
            .as_ref()
            .and_then(|page| page.document.element(node))
            .and_then(|element| element.attributes.get(&attribute))
            .map(str::to_string)
    });
    Ok(value.map_or(JsValue::undefined(), |value| {
        JsValue::from(js_string!(value.as_str()))
    }))
}

fn dataset_set_trap(_this: &JsValue, args: &[JsValue], context: &mut Context) -> JsResult<JsValue> {
    let Some((node, key)) = trap_context(args, context)? else {
        return Ok(JsValue::from(true));
    };
    let value = args
        .get(2)
        .and_then(|value| value.to_string(context).ok())
        .map(|value| value.to_std_string_escaped())
        .unwrap_or_default();
    let attribute = format!("data-{}", kebab(&key));
    with_bridge(|bridge| {
        if let Some(page) = bridge.page.as_mut() {
            page.document.set_attribute(node, &attribute, &value);
            bridge.dirty = true;
        }
    });
    Ok(JsValue::from(true))
}

fn parent_element_get(
    this: &JsValue,
    _args: &[JsValue],
    context: &mut Context,
) -> JsResult<JsValue> {
    let Some(node) = this_node(this, context)? else {
        return Ok(JsValue::null());
    };
    let parent = with_bridge(|bridge| {
        let page = bridge.page.as_ref()?;
        let document = &page.document;
        document
            .ancestors(node)
            .find(|ancestor| document.element(*ancestor).is_some())
    });
    Ok(match parent {
        Some(parent) => element_object(parent, context).into(),
        None => JsValue::null(),
    })
}

/// Wraps `Some(node)` as an element object, `None` as null.
fn optional_element_object(node: Option<NodeId>, context: &mut Context) -> JsValue {
    match node {
        Some(node) => element_object(node, context).into(),
        None => JsValue::null(),
    }
}

/// Runs `lookup` against the live document for this wrapper's node,
/// wrapping the resulting node id (or null).
fn node_lookup(
    this: &JsValue,
    context: &mut Context,
    lookup: impl FnOnce(&lumen_html::Document, NodeId) -> Option<NodeId>,
) -> JsResult<JsValue> {
    let Some(node) = this_node(this, context)? else {
        return Ok(JsValue::null());
    };
    let found = with_bridge(|bridge| {
        let page = bridge.page.as_ref()?;
        lookup(&page.document, node)
    });
    Ok(optional_element_object(found, context))
}

fn parent_node_get_(this: &JsValue, _args: &[JsValue], context: &mut Context) -> JsResult<JsValue> {
    node_lookup(this, context, |document, node| document.parent(node))
}

fn first_child_get_(this: &JsValue, _args: &[JsValue], context: &mut Context) -> JsResult<JsValue> {
    node_lookup(this, context, |document, node| {
        document.children(node).first().copied()
    })
}

fn last_child_get_(this: &JsValue, _args: &[JsValue], context: &mut Context) -> JsResult<JsValue> {
    node_lookup(this, context, |document, node| {
        document.children(node).last().copied()
    })
}

/// The node's position among its parent's children, with the parent id.
fn sibling_position(document: &lumen_html::Document, node: NodeId) -> Option<(NodeId, usize)> {
    let parent = document.parent(node)?;
    let index = document
        .children(parent)
        .iter()
        .position(|child| *child == node)?;
    Some((parent, index))
}

fn next_sibling_get_(
    this: &JsValue,
    _args: &[JsValue],
    context: &mut Context,
) -> JsResult<JsValue> {
    node_lookup(this, context, |document, node| {
        let (parent, index) = sibling_position(document, node)?;
        document.children(parent).get(index + 1).copied()
    })
}

fn previous_sibling_get_(
    this: &JsValue,
    _args: &[JsValue],
    context: &mut Context,
) -> JsResult<JsValue> {
    node_lookup(this, context, |document, node| {
        let (parent, index) = sibling_position(document, node)?;
        index
            .checked_sub(1)
            .and_then(|previous| document.children(parent).get(previous).copied())
    })
}

/// An iframe's contentDocument/contentWindow: always null (no nested
/// browsing contexts), so scripts feature-detecting frames bail out.
fn frame_browsing_context_get(
    _this: &JsValue,
    _args: &[JsValue],
    _context: &mut Context,
) -> JsResult<JsValue> {
    Ok(JsValue::null())
}

fn children_get_(this: &JsValue, _args: &[JsValue], context: &mut Context) -> JsResult<JsValue> {
    let Some(node) = this_node(this, context)? else {
        return Ok(JsValue::undefined());
    };
    let children = with_bridge(|bridge| {
        bridge
            .page
            .as_ref()
            .map(|page| {
                page.document
                    .children(node)
                    .iter()
                    .copied()
                    .filter(|child| page.document.element(*child).is_some())
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default()
    });
    let items: Vec<JsValue> = children
        .into_iter()
        .map(|child| element_object(child, context).into())
        .collect();
    Ok(boa_engine::object::builtins::JsArray::from_iter(items, context).into())
}

fn child_nodes_get_(this: &JsValue, _args: &[JsValue], context: &mut Context) -> JsResult<JsValue> {
    let Some(node) = this_node(this, context)? else {
        return Ok(JsValue::undefined());
    };
    let children = with_bridge(|bridge| {
        bridge
            .page
            .as_ref()
            .map(|page| page.document.children(node).to_vec())
            .unwrap_or_default()
    });
    let items: Vec<JsValue> = children
        .into_iter()
        .map(|child| element_object(child, context).into())
        .collect();
    Ok(boa_engine::object::builtins::JsArray::from_iter(items, context).into())
}

fn insert_before_(this: &JsValue, args: &[JsValue], context: &mut Context) -> JsResult<JsValue> {
    let Some(parent) = this_node(this, context)? else {
        return Ok(JsValue::undefined());
    };
    let Some(argument) = args.first() else {
        return Ok(JsValue::undefined());
    };
    let Some(child) = this_node(argument, context)? else {
        return Ok(JsValue::undefined());
    };
    // insertBefore(new, null) appends, per spec.
    let reference = match args.get(1) {
        Some(value) if !value.is_null_or_undefined() => this_node(value, context)?,
        _ => None,
    };
    with_bridge(|bridge| {
        if let Some(page) = bridge.page.as_mut() {
            page.document.insert_child_before(parent, child, reference);
            bridge.dirty = true;
        }
    });
    Ok(args.first().cloned().unwrap_or_default())
}

fn remove_child(this: &JsValue, args: &[JsValue], context: &mut Context) -> JsResult<JsValue> {
    let Some(parent) = this_node(this, context)? else {
        return Ok(JsValue::undefined());
    };
    let Some(argument) = args.first() else {
        return Ok(JsValue::undefined());
    };
    let Some(child) = this_node(argument, context)? else {
        return Ok(JsValue::undefined());
    };
    with_bridge(|bridge| {
        let Some(page) = bridge.page.as_mut() else {
            return;
        };
        // Only detach what is actually a child here (a stale or foreign
        // node is a no-op instead of a NotFoundError — tolerated, like
        // the rest of the bindings' error policy).
        if page.document.parent(child) == Some(parent) {
            page.document.detach(child);
            bridge.dirty = true;
        }
    });
    Ok(args.first().cloned().unwrap_or_default())
}

fn closest_(this: &JsValue, args: &[JsValue], context: &mut Context) -> JsResult<JsValue> {
    let Some(node) = this_node(this, context)? else {
        return Ok(JsValue::null());
    };
    let selector = string_arg(args, 0, context);
    let found = with_bridge(|bridge| {
        let page = bridge.page.as_ref()?;
        let document = &page.document;
        std::iter::once(node)
            .chain(document.ancestors(node))
            .find(|candidate| selector_matches(document, *candidate, selector.trim()))
    });
    Ok(optional_element_object(found, context))
}

fn element_query_selector(
    this: &JsValue,
    args: &[JsValue],
    context: &mut Context,
) -> JsResult<JsValue> {
    let Some(node) = this_node(this, context)? else {
        return Ok(JsValue::null());
    };
    let selector = string_arg(args, 0, context);
    Ok(
        match query_nodes_scoped(selector.trim(), Some(node)).first() {
            Some(found) => element_object(*found, context).into(),
            None => JsValue::null(),
        },
    )
}

fn element_query_selector_all(
    this: &JsValue,
    args: &[JsValue],
    context: &mut Context,
) -> JsResult<JsValue> {
    let Some(node) = this_node(this, context)? else {
        return Ok(JsValue::undefined());
    };
    let selector = string_arg(args, 0, context);
    let items: Vec<JsValue> = query_nodes_scoped(selector.trim(), Some(node))
        .into_iter()
        .map(|found| element_object(found, context).into())
        .collect();
    Ok(boa_engine::object::builtins::JsArray::from_iter(items, context).into())
}

fn bounding_client_rect(
    this: &JsValue,
    _args: &[JsValue],
    context: &mut Context,
) -> JsResult<JsValue> {
    let Some(node) = this_node(this, context)? else {
        return Ok(JsValue::undefined());
    };
    let rect = with_bridge(|bridge| {
        bridge
            .page
            .as_ref()
            .and_then(|page| page.layout.find_by_node(node))
            .map(|laid| laid.border_box())
    })
    .unwrap_or(lumen_engine::Rect {
        x: 0.0,
        y: 0.0,
        width: 0.0,
        height: 0.0,
    });
    Ok(ObjectInitializer::new(context)
        .property(js_string!("x"), rect.x, Attribute::all())
        .property(js_string!("y"), rect.y, Attribute::all())
        .property(js_string!("left"), rect.x, Attribute::all())
        .property(js_string!("top"), rect.y, Attribute::all())
        .property(js_string!("width"), rect.width, Attribute::all())
        .property(js_string!("height"), rect.height, Attribute::all())
        .property(js_string!("right"), rect.x + rect.width, Attribute::all())
        .property(js_string!("bottom"), rect.y + rect.height, Attribute::all())
        .build()
        .into())
}

fn inner_html_get(this: &JsValue, _args: &[JsValue], context: &mut Context) -> JsResult<JsValue> {
    let Some(node) = this_node(this, context)? else {
        return Ok(JsValue::undefined());
    };
    let html = with_bridge(|bridge| {
        bridge
            .page
            .as_ref()
            .map(|page| page.document.inner_html(node))
            .unwrap_or_default()
    });
    Ok(JsValue::from(js_string!(html.as_str())))
}

fn inner_html_set(this: &JsValue, args: &[JsValue], context: &mut Context) -> JsResult<JsValue> {
    let Some(node) = this_node(this, context)? else {
        return Ok(JsValue::undefined());
    };
    let html = string_arg(args, 0, context);
    with_bridge(|bridge| {
        if let Some(page) = bridge.page.as_mut() {
            page.document.set_inner_html(node, &html);
            bridge.dirty = true;
        }
    });
    Ok(JsValue::undefined())
}

fn class_name_get(this: &JsValue, _args: &[JsValue], context: &mut Context) -> JsResult<JsValue> {
    let Some(node) = this_node(this, context)? else {
        return Ok(JsValue::undefined());
    };
    let classes = with_bridge(|bridge| {
        bridge
            .page
            .as_ref()
            .and_then(|page| page.document.element(node))
            .and_then(|element| element.attributes.get("class"))
            .unwrap_or_default()
            .to_string()
    });
    Ok(JsValue::from(js_string!(classes.as_str())))
}

fn class_name_set(this: &JsValue, args: &[JsValue], context: &mut Context) -> JsResult<JsValue> {
    let Some(node) = this_node(this, context)? else {
        return Ok(JsValue::undefined());
    };
    let classes = string_arg(args, 0, context);
    with_bridge(|bridge| {
        if let Some(page) = bridge.page.as_mut() {
            page.document.set_attribute(node, "class", &classes);
            bridge.dirty = true;
        }
    });
    Ok(JsValue::undefined())
}

/// Applies one classList operation, returning the op's result value.
fn class_list_op(
    this: &JsValue,
    args: &[JsValue],
    context: &mut Context,
    op: fn(&mut Vec<String>, &str) -> bool,
) -> JsResult<JsValue> {
    let Some(node) = this_node(this, context)? else {
        return Ok(JsValue::undefined());
    };
    let class = string_arg(args, 0, context);
    let result = with_bridge(|bridge| {
        let Some(page) = bridge.page.as_mut() else {
            return false;
        };
        let mut classes: Vec<String> = page
            .document
            .element(node)
            .and_then(|element| element.attributes.get("class"))
            .unwrap_or_default()
            .split_whitespace()
            .map(str::to_string)
            .collect();
        let result = op(&mut classes, &class);
        page.document
            .set_attribute(node, "class", &classes.join(" "));
        bridge.dirty = true;
        result
    });
    Ok(JsValue::from(result))
}

fn class_add(this: &JsValue, args: &[JsValue], context: &mut Context) -> JsResult<JsValue> {
    class_list_op(this, args, context, |classes, class| {
        if !classes.iter().any(|c| c == class) {
            classes.push(class.to_string());
        }
        true
    })
}

fn class_remove(this: &JsValue, args: &[JsValue], context: &mut Context) -> JsResult<JsValue> {
    class_list_op(this, args, context, |classes, class| {
        classes.retain(|c| c != class);
        true
    })
}

fn class_toggle(this: &JsValue, args: &[JsValue], context: &mut Context) -> JsResult<JsValue> {
    class_list_op(this, args, context, |classes, class| {
        if classes.iter().any(|c| c == class) {
            classes.retain(|c| c != class);
            false
        } else {
            classes.push(class.to_string());
            true
        }
    })
}

fn class_contains(this: &JsValue, args: &[JsValue], context: &mut Context) -> JsResult<JsValue> {
    let Some(node) = this_node(this, context)? else {
        return Ok(JsValue::undefined());
    };
    let class = string_arg(args, 0, context);
    let found = with_bridge(|bridge| {
        bridge
            .page
            .as_ref()
            .and_then(|page| page.document.element(node))
            .is_some_and(|element| {
                element
                    .attributes
                    .get("class")
                    .is_some_and(|classes| classes.split_whitespace().any(|c| c == class))
            })
    });
    Ok(JsValue::from(found))
}

fn text_content_get(this: &JsValue, _args: &[JsValue], context: &mut Context) -> JsResult<JsValue> {
    let Some(node) = this_node(this, context)? else {
        return Ok(JsValue::undefined());
    };
    let text = with_bridge(|bridge| {
        bridge
            .page
            .as_ref()
            .map(|page| page.document.text_content(node))
            .unwrap_or_default()
    });
    Ok(JsValue::from(js_string!(text.as_str())))
}

fn text_content_set(this: &JsValue, args: &[JsValue], context: &mut Context) -> JsResult<JsValue> {
    let Some(node) = this_node(this, context)? else {
        return Ok(JsValue::undefined());
    };
    let text = string_arg(args, 0, context);
    with_bridge(|bridge| {
        if let Some(page) = bridge.page.as_mut() {
            page.document.set_text_content(node, &text);
            bridge.dirty = true;
        }
    });
    Ok(JsValue::undefined())
}

fn value_get_(this: &JsValue, _args: &[JsValue], context: &mut Context) -> JsResult<JsValue> {
    let Some(node) = this_node(this, context)? else {
        return Ok(JsValue::undefined());
    };
    let value = with_bridge(|bridge| {
        if let Some(value) = bridge.form_values.get(&node) {
            return value.clone();
        }
        bridge
            .page
            .as_ref()
            .and_then(|page| page.document.element(node))
            .and_then(|element| element.attributes.get("value"))
            .unwrap_or_default()
            .to_string()
    });
    Ok(JsValue::from(js_string!(value.as_str())))
}

fn value_set_(this: &JsValue, args: &[JsValue], context: &mut Context) -> JsResult<JsValue> {
    let Some(node) = this_node(this, context)? else {
        return Ok(JsValue::undefined());
    };
    let value = string_arg(args, 0, context);
    with_bridge(|bridge| {
        bridge.form_values.insert(node, value.clone());
        let Some(page) = bridge.page.as_mut() else {
            return;
        };
        let is_textarea = page
            .document
            .element(node)
            .is_some_and(|element| element.tag_name == "textarea");
        if is_textarea {
            page.document.set_text_content(node, &value);
        } else {
            let display = if value.is_empty() {
                " "
            } else {
                value.as_str()
            };
            page.document.upsert_generated_text(node, true, display);
        }
        bridge.dirty = true;
    });
    Ok(JsValue::undefined())
}

fn id_get_(this: &JsValue, _args: &[JsValue], context: &mut Context) -> JsResult<JsValue> {
    let Some(node) = this_node(this, context)? else {
        return Ok(JsValue::undefined());
    };
    let id = with_bridge(|bridge| {
        bridge
            .page
            .as_ref()
            .and_then(|page| page.document.element(node))
            .and_then(|element| element.attributes.get("id"))
            .unwrap_or_default()
            .to_string()
    });
    Ok(JsValue::from(js_string!(id.as_str())))
}

/// The `once` flag of an addEventListener options argument
/// (boolean | { capture, passive, once }); a boolean third argument
/// (capture) and the remaining flags are tolerated and ignored.
fn listener_options_once(args: &[JsValue], context: &mut Context) -> bool {
    let Some(options) = args.get(2) else {
        return false;
    };
    options
        .as_object()
        .and_then(|options| options.get(js_string!("once"), context).ok())
        .is_some_and(|value| value.to_boolean())
}

/// Shared addEventListener body: a null/undefined or non-callable
/// listener is ignored silently (spec behavior for null; tolerance for
/// the rest), and any options argument shape is accepted.
fn register_listener(args: &[JsValue], context: &mut Context, node: NodeId) {
    let event = string_arg(args, 0, context);
    let Some(callback) = args.get(1).and_then(JsValue::as_callable) else {
        return;
    };
    let once = listener_options_once(args, context);
    with_bridge(|bridge| {
        bridge
            .pending_listeners
            .push((node, event, Listener { callback, once }));
    });
}

/// Shared removeEventListener body: queues the removal of the matching
/// registration (identity comparison, like the spec).
fn unregister_listener(args: &[JsValue], context: &mut Context, node: NodeId) {
    let event = string_arg(args, 0, context);
    let Some(callback) = args.get(1).and_then(JsValue::as_callable) else {
        return;
    };
    with_bridge(|bridge| {
        bridge.pending_removals.push((node, event, callback));
    });
}

fn add_event_listener(
    this: &JsValue,
    args: &[JsValue],
    context: &mut Context,
) -> JsResult<JsValue> {
    if let Some(node) = this_node(this, context)? {
        register_listener(args, context, node);
    }
    Ok(JsValue::undefined())
}

fn remove_event_listener(
    this: &JsValue,
    args: &[JsValue],
    context: &mut Context,
) -> JsResult<JsValue> {
    if let Some(node) = this_node(this, context)? {
        unregister_listener(args, context, node);
    }
    Ok(JsValue::undefined())
}

fn append_child(this: &JsValue, args: &[JsValue], context: &mut Context) -> JsResult<JsValue> {
    let Some(parent) = this_node(this, context)? else {
        return Ok(JsValue::undefined());
    };
    let Some(argument) = args.first() else {
        return Ok(JsValue::undefined());
    };
    let Some(child) = this_node(argument, context)? else {
        return Ok(JsValue::undefined());
    };
    with_bridge(|bridge| {
        if let Some(page) = bridge.page.as_mut() {
            page.document.append_child(parent, child);
            bridge.dirty = true;
        }
    });
    // Return the appended child, as the real API does.
    Ok(args.first().cloned().unwrap_or_default())
}

fn focus_element(this: &JsValue, _args: &[JsValue], context: &mut Context) -> JsResult<JsValue> {
    if let Some(node) = this_node(this, context)? {
        with_bridge(|bridge| bridge.pending_focus = Some(Some(node)));
    }
    Ok(JsValue::undefined())
}

fn blur_element(_this: &JsValue, _args: &[JsValue], _context: &mut Context) -> JsResult<JsValue> {
    with_bridge(|bridge| bridge.pending_focus = Some(None));
    Ok(JsValue::undefined())
}

fn remove_node(this: &JsValue, _args: &[JsValue], context: &mut Context) -> JsResult<JsValue> {
    let Some(node) = this_node(this, context)? else {
        return Ok(JsValue::undefined());
    };
    with_bridge(|bridge| {
        if let Some(page) = bridge.page.as_mut() {
            page.document.detach(node);
            bridge.dirty = true;
        }
    });
    Ok(JsValue::undefined())
}

fn get_attribute(this: &JsValue, args: &[JsValue], context: &mut Context) -> JsResult<JsValue> {
    let Some(node) = this_node(this, context)? else {
        return Ok(JsValue::undefined());
    };
    let name = string_arg(args, 0, context);
    let value = with_bridge(|bridge| {
        bridge
            .page
            .as_ref()
            .and_then(|page| page.document.element(node))
            .and_then(|element| element.attributes.get(&name))
            .map(str::to_string)
    });
    Ok(value.map_or(JsValue::null(), |value| {
        JsValue::from(js_string!(value.as_str()))
    }))
}

fn set_attribute(this: &JsValue, args: &[JsValue], context: &mut Context) -> JsResult<JsValue> {
    let Some(node) = this_node(this, context)? else {
        return Ok(JsValue::undefined());
    };
    let name = string_arg(args, 0, context);
    let value = string_arg(args, 1, context);
    with_bridge(|bridge| {
        let Some(page) = bridge.page.as_mut() else {
            return;
        };
        if let Some(property) = name.strip_prefix("style.") {
            // Appended declarations win within the style attribute.
            let existing = page
                .document
                .element(node)
                .and_then(|element| element.attributes.get("style"))
                .unwrap_or_default()
                .to_string();
            let merged = format!("{existing}; {property}: {value}");
            page.document.set_attribute(node, "style", &merged);
        } else {
            page.document.set_attribute(node, &name, &value);
        }
        bridge.dirty = true;
    });
    Ok(JsValue::undefined())
}

#[cfg(test)]
mod tests {
    use super::*;
    use lumen_platform::loader::{CorsGrant, CorsPreflight};
    use lumen_platform::{LoadError, ResourceRequest, ResourceResponse, Url};
    use std::collections::HashMap;

    /// A canned-response loader. Mutex-based (not RefCell) so the
    /// loader is `Send + Sync`: `PageScripts` shares it with network
    /// workers behind an `Arc`.
    struct FakeLoader {
        pages: HashMap<String, Vec<u8>>,
        /// Requested URLs, in order (a reload shows up here).
        loads: std::sync::Mutex<Vec<String>>,
        /// POST bodies per load (None for GETs).
        bodies: std::sync::Mutex<Vec<Option<String>>>,
        /// Cookie header seen per load (None = no Cookie header).
        cookies: std::sync::Mutex<Vec<Option<String>>>,
        /// Access-Control-Allow-Origin value served per URL.
        acao: HashMap<String, String>,
        /// CORS preflight probes seen: (url, method, headers).
        probes: std::sync::Mutex<Vec<(String, String, Vec<String>)>>,
        /// Canned preflight grants per URL (absent = refused probe).
        grants: HashMap<String, CorsGrant>,
        /// Set-Cookie values served per URL.
        serve_cookies: HashMap<String, Vec<String>>,
        /// Artificial per-load delay — a "slow server" for the
        /// non-blocking tests.
        delay: std::sync::Mutex<Option<Duration>>,
    }

    impl FakeLoader {
        fn new(pages: &[(&str, &str)]) -> Self {
            Self {
                pages: pages
                    .iter()
                    .map(|(url, html)| ((*url).to_string(), html.as_bytes().to_vec()))
                    .collect(),
                loads: std::sync::Mutex::new(Vec::new()),
                bodies: std::sync::Mutex::new(Vec::new()),
                cookies: std::sync::Mutex::new(Vec::new()),
                acao: HashMap::new(),
                probes: std::sync::Mutex::new(Vec::new()),
                grants: HashMap::new(),
                serve_cookies: HashMap::new(),
                delay: std::sync::Mutex::new(None),
            }
        }

        fn with_acao(mut self, url: &str, value: &str) -> Self {
            self.acao.insert(url.to_string(), value.to_string());
            self
        }

        fn with_grant(mut self, url: &str, grant: CorsGrant) -> Self {
            self.grants.insert(url.to_string(), grant);
            self
        }

        fn with_cookies(mut self, url: &str, cookies: &[&str]) -> Self {
            self.serve_cookies.insert(
                url.to_string(),
                cookies.iter().map(|c| (*c).to_string()).collect(),
            );
            self
        }
    }

    impl ResourceLoader for FakeLoader {
        fn load(&self, request: &ResourceRequest) -> Result<ResourceResponse, LoadError> {
            if let Some(delay) = *self.delay.lock().unwrap() {
                std::thread::sleep(delay);
            }
            self.loads.lock().unwrap().push(request.url.to_string());
            self.bodies.lock().unwrap().push(
                request
                    .body
                    .as_ref()
                    .map(|(_, bytes)| String::from_utf8_lossy(bytes).into_owned()),
            );
            self.cookies.lock().unwrap().push(request.cookie.clone());
            let body = self
                .pages
                .get(request.url.as_str())
                .ok_or_else(|| LoadError::Http(format!("404: {}", request.url)))?;
            Ok(ResourceResponse {
                final_url: request.url.clone(),
                content_type: None,
                body: body.clone(),
                set_cookies: self
                    .serve_cookies
                    .get(request.url.as_str())
                    .cloned()
                    .unwrap_or_default(),
                access_control_allow_origin: self.acao.get(request.url.as_str()).cloned(),
            })
        }

        fn preflight(&self, probe: &CorsPreflight) -> Result<CorsGrant, LoadError> {
            self.probes.lock().unwrap().push((
                probe.url.to_string(),
                probe.method.clone(),
                probe.headers.clone(),
            ));
            self.grants
                .get(probe.url.as_str())
                .cloned()
                .ok_or_else(|| LoadError::Http(format!("no canned grant: {}", probe.url)))
        }
    }

    fn session_with(html: &str) -> Session<FakeLoader> {
        session_with_files(&[("https://a.test/", html)], "https://a.test/")
    }

    fn session_with_files(files: &[(&str, &str)], url: &str) -> Session<FakeLoader> {
        let mut session = Session::new(
            FakeLoader::new(files),
            lumen_engine::Size {
                width: 800.0,
                height: 600.0,
            },
        );
        session.load(Url::parse(url).unwrap()).unwrap();
        session
    }

    fn out_text(session: &Session<FakeLoader>) -> String {
        let document = &session.page().unwrap().document;
        let out = document.get_element_by_id("out").unwrap();
        document.text_content(out)
    }

    #[test]
    fn zero_delay_timer_chains_run_once_per_tick() {
        let mut session = session_with(
            "<p id='out'>0</p><script>\
             let n = 0;\
             const out = document.getElementById('out');\
             function again() {\
               n++; out.textContent = String(n);\
               if (n < 3) { setTimeout(again, 0); }\
             }\
             setTimeout(again, 0);\
             </script>",
        );
        let mut scripts = PageScripts::new(&mut session).expect("page has scripts");
        // One tick runs only the snapshot: a setTimeout(f, 0) chain must
        // not spin forever inside a single tick.
        assert!(scripts.tick(&mut session, 0.0));
        assert_eq!(out_text(&session), "1");
        assert!(scripts.tick(&mut session, 0.0));
        assert_eq!(out_text(&session), "2");
        assert!(scripts.tick(&mut session, 0.0));
        assert_eq!(out_text(&session), "3");
        assert!(!scripts.has_timers());
    }

    #[test]
    fn clearing_a_due_sibling_inside_a_tick_skips_it() {
        let mut session = session_with(
            "<p id='out'>-</p><script>\
             const out = document.getElementById('out');\
             let second;\
             setTimeout(() => { clearTimeout(second); out.textContent = 'birinci'; }, 0);\
             second = setTimeout(() => { out.textContent = 'ikinci'; }, 0);\
             </script>",
        );
        let mut scripts = PageScripts::new(&mut session).expect("page has scripts");
        assert!(scripts.tick(&mut session, 0.0));
        assert_eq!(out_text(&session), "birinci");
        assert!(!scripts.has_timers());
    }

    #[test]
    fn forged_node_handles_raise_type_errors() {
        let mut session = session_with(
            "<p id='out'>-</p><script>\
             const out = document.getElementById('out');\
             const getter = Object.getOwnPropertyDescriptor(out, 'textContent').get;\
             try {\
               getter.call({__node: 1e9});\
               out.textContent = 'no-throw';\
             } catch (error) {\
               out.textContent = (error instanceof TypeError) ? 'type-error' : 'other-error';\
             }\
             </script>",
        );
        let _scripts = PageScripts::new(&mut session).expect("page has scripts");
        // A fake {__node: 1e9} handle must not panic the arena index.
        assert_eq!(out_text(&session), "type-error");
    }

    #[test]
    fn cookie_writes_replace_the_same_name() {
        let mut session = session_with(
            "<p id='out'>-</p><script>\
             document.cookie = 'a=1';\
             document.cookie = 'b=1';\
             document.cookie = 'a=2';\
             document.getElementById('out').textContent = document.cookie;\
             </script>",
        );
        let _scripts = PageScripts::new(&mut session).expect("page has scripts");
        // The rewrite of `a` replaced its pair instead of duplicating it.
        assert_eq!(out_text(&session), "b=1; a=2");
    }

    #[test]
    fn next_timer_due_reports_the_earliest_deadline() {
        let mut session = session_with(
            "<script>\
             setTimeout(() => {}, 50);\
             setTimeout(() => {}, 10);\
             </script>",
        );
        let mut scripts = PageScripts::new(&mut session).expect("page has scripts");
        assert_eq!(scripts.next_timer_due_ms(), Some(10.0));
        assert!(scripts.tick(&mut session, 20.0));
        assert_eq!(scripts.next_timer_due_ms(), Some(50.0));
        assert!(scripts.tick(&mut session, 60.0));
        assert_eq!(scripts.next_timer_due_ms(), None);
    }

    #[test]
    fn dispatch_bubbles_in_registration_order() {
        let mut session = session_with(
            "<div id='outer'><button id='inner'>x</button></div><p id='out'></p><script>\
             const out = document.getElementById('out');\
             const inner = document.getElementById('inner');\
             const outer = document.getElementById('outer');\
             outer.addEventListener('click', () => { out.textContent += 'o1'; });\
             inner.addEventListener('click', () => { out.textContent += 'i1'; });\
             inner.addEventListener('click', () => { out.textContent += 'i2'; });\
             outer.addEventListener('click', () => { out.textContent += 'o2'; });\
             </script>",
        );
        let mut scripts = PageScripts::new(&mut session).expect("page has scripts");
        let document = &session.page().unwrap().document;
        let inner = document.get_element_by_id("inner").unwrap();
        let outer = document.get_element_by_id("outer").unwrap();
        assert!(scripts.has_listener(inner, "click"));
        assert!(scripts.has_listener(outer, "click"));
        assert!(!scripts.has_listener(inner, "submit"));
        let outcome = scripts.dispatch(&mut session, inner, "click");
        assert!(outcome.handled);
        // Target first (registration order), then the bubbling ancestors.
        assert_eq!(out_text(&session), "i1i2o1o2");
    }

    #[test]
    fn module_graph_loads_links_and_runs() {
        let mut session = session_with_files(
            &[
                (
                    "https://a.test/",
                    "<p id='out'>-</p><script type='module' src='/a.js'></script>",
                ),
                (
                    "https://a.test/a.js",
                    "import { value } from './b.js';\
                     document.getElementById('out').textContent = value;",
                ),
                ("https://a.test/b.js", "export const value = 'mod-ok';"),
            ],
            "https://a.test/",
        );
        let _scripts = PageScripts::new(&mut session).expect("page has scripts");
        assert_eq!(out_text(&session), "mod-ok");
    }

    #[test]
    fn modules_share_the_global_with_classic_scripts() {
        let mut session = session_with_files(
            &[
                (
                    "https://a.test/",
                    "<p id='out'>-</p>\
                     <script>window.answer = 42;</script>\
                     <script type='module'>window.fromModule = 'm' + String(answer);</script>\
                     <script defer src='/after.js'></script>",
                ),
                (
                    "https://a.test/after.js",
                    "document.getElementById('out').textContent = fromModule;",
                ),
            ],
            "https://a.test/",
        );
        let _scripts = PageScripts::new(&mut session).expect("page has scripts");
        // The module saw the classic script's global, and the deferred
        // classic script (running after the module) saw the module's.
        assert_eq!(out_text(&session), "m42");
    }

    #[test]
    fn a_broken_import_spares_the_other_modules() {
        let mut session = session_with_files(
            &[
                (
                    "https://a.test/",
                    "<p id='out'>-</p>\
                     <script type='module' src='/bad.js'></script>\
                     <script type='module'>document.getElementById('out').textContent = 'survived';</script>",
                ),
                ("https://a.test/bad.js", "import './missing.js';"),
            ],
            "https://a.test/",
        );
        let _scripts = PageScripts::new(&mut session).expect("page has scripts");
        assert_eq!(out_text(&session), "survived");
    }

    #[test]
    fn a_runaway_loop_errors_and_later_scripts_run() {
        let mut session = session_with(
            "<p id='out'>-</p>\
             <script>while (true) {}</script>\
             <script>document.getElementById('out').textContent = 'after';</script>",
        );
        let _scripts = PageScripts::new(&mut session).expect("page has scripts");
        assert_eq!(out_text(&session), "after");
    }

    #[test]
    fn a_self_chaining_promise_does_not_block_the_page() {
        let mut session = session_with(
            "<p id='out'>-</p><script>\
             function f() { Promise.resolve().then(f); }\
             f();\
             document.getElementById('out').textContent = 'done';\
             </script>",
        );
        // run_jobs must return instead of draining the chain forever.
        let _scripts = PageScripts::new(&mut session).expect("page has scripts");
        assert_eq!(out_text(&session), "done");
    }

    #[test]
    fn defer_and_modules_run_after_parsing_in_document_order() {
        let mut session = session_with_files(
            &[
                (
                    "https://a.test/",
                    "<p id='out'>-</p>\
                     <script defer src='/d1.js'></script>\
                     <script>document.getElementById('out').textContent += 'A';</script>\
                     <script type='module'>document.getElementById('out').textContent += 'M';</script>\
                     <script defer src='/d2.js'></script>\
                     <script>document.getElementById('out').textContent += 'B';</script>",
                ),
                (
                    "https://a.test/d1.js",
                    "document.getElementById('out').textContent += 'D1';",
                ),
                (
                    "https://a.test/d2.js",
                    "document.getElementById('out').textContent += 'D2';",
                ),
            ],
            "https://a.test/",
        );
        let _scripts = PageScripts::new(&mut session).expect("page has scripts");
        // Immediate classics first (A, B), then the post-parse queue in
        // document order: D1, the module, D2.
        assert_eq!(out_text(&session), "-ABD1MD2");
    }

    #[test]
    fn inline_onclick_runs_with_element_this_and_event() {
        let mut session = session_with(
            "<button id='btn' onclick='document.getElementById(\"out\").textContent = this.id + \":\" + event.type;'>x</button>\
             <p id='out'>-</p>",
        );
        let mut scripts = PageScripts::new(&mut session).expect("page has scripts");
        let button = session
            .page()
            .unwrap()
            .document
            .get_element_by_id("btn")
            .unwrap();
        let outcome = scripts.dispatch(&mut session, button, "click");
        assert!(outcome.handled);
        assert_eq!(out_text(&session), "btn:click");
    }

    #[test]
    fn inline_handler_prevent_default_cancels_the_default_action() {
        let mut session = session_with(
            "<a id='lnk' href='/x' onclick='event.preventDefault(); document.getElementById(\"out\").textContent = String(event.defaultPrevented);'>go</a>\
             <p id='out'>-</p>",
        );
        let mut scripts = PageScripts::new(&mut session).expect("page has scripts");
        let link = session
            .page()
            .unwrap()
            .document
            .get_element_by_id("lnk")
            .unwrap();
        let outcome = scripts.dispatch(&mut session, link, "click");
        // The shell skips navigation when the click is prevented.
        assert!(outcome.handled && outcome.prevented);
        assert_eq!(out_text(&session), "true");
    }

    #[test]
    fn body_onload_fires_on_the_load_event() {
        let mut session = session_with(
            "<body onload='document.getElementById(\"out\").textContent = \"loaded\";'>\
             <p id='out'>-</p></body>",
        );
        let _scripts = PageScripts::new(&mut session).expect("page has scripts");
        assert_eq!(out_text(&session), "loaded");
    }

    #[test]
    fn xhr_loads_text_and_fires_handlers() {
        let mut session = session_with_files(
            &[
                (
                    "https://a.test/",
                    "<p id='out'>-</p><script>\
                     const states = [];\
                     const xhr = new XMLHttpRequest();\
                     xhr.open('GET', '/data.txt');\
                     xhr.onreadystatechange = () => { states.push(xhr.readyState); };\
                     xhr.onload = () => {\
                       document.getElementById('out').textContent =\
                         states.join(',') + '|' + xhr.status + '|' + xhr.readyState + '|' + xhr.responseText;\
                     };\
                     xhr.send();\
                     </script>",
                ),
                ("https://a.test/data.txt", "merhaba"),
            ],
            "https://a.test/",
        );
        let _scripts = PageScripts::new(&mut session).expect("page has scripts");
        assert_eq!(out_text(&session), "4|200|4|merhaba");
    }

    #[test]
    fn xhr_failure_fires_onerror() {
        let mut session = session_with(
            "<p id='out'>-</p><script>\
             const xhr = new XMLHttpRequest();\
             xhr.open('GET', '/missing');\
             xhr.onerror = () => {\
               document.getElementById('out').textContent = 'error:' + xhr.status;\
             };\
             xhr.onload = () => {\
               document.getElementById('out').textContent = 'unexpected';\
             };\
             xhr.send();\
             </script>",
        );
        let _scripts = PageScripts::new(&mut session).expect("page has scripts");
        assert_eq!(out_text(&session), "error:0");
    }

    #[test]
    fn xhr_post_sends_the_body() {
        let mut session = session_with_files(
            &[
                (
                    "https://a.test/",
                    "<p id='out'>-</p><script>\
                     const xhr = new XMLHttpRequest();\
                     xhr.open('POST', '/echo');\
                     xhr.setRequestHeader('Content-Type', 'text/plain');\
                     xhr.onload = () => {\
                       document.getElementById('out').textContent = xhr.responseText;\
                     };\
                     xhr.send('payload');\
                     </script>",
                ),
                ("https://a.test/echo", "ok"),
            ],
            "https://a.test/",
        );
        let _scripts = PageScripts::new(&mut session).expect("page has scripts");
        assert_eq!(out_text(&session), "ok");
        // Page load GET (None) then the XHR POST with its body.
        assert_eq!(
            *session.loader.bodies.lock().unwrap(),
            vec![None, Some("payload".to_string())]
        );
    }

    #[test]
    fn push_state_rewrites_the_url_without_reloading() {
        let mut session = session_with(
            "<p id='out'>-</p><script>\
             history.pushState({ derinlik: 1 }, '', '/yeni');\
             document.getElementById('out').textContent =\
               location.href + '|' + history.length + '|' + history.state.derinlik;\
             </script>",
        );
        let _scripts = PageScripts::new(&mut session).expect("page has scripts");
        assert_eq!(out_text(&session), "https://a.test/yeni|2|1");
        assert_eq!(
            session.current_url().unwrap().as_str(),
            "https://a.test/yeni"
        );
        // No reload: the loader only ever saw the initial page fetch.
        assert_eq!(
            *session.loader.loads.lock().unwrap(),
            vec!["https://a.test/"]
        );
    }

    #[test]
    fn replace_state_keeps_the_history_length() {
        let mut session = session_with(
            "<p id='out'>-</p><script>\
             history.replaceState(null, '', '/rep');\
             document.getElementById('out').textContent = location.href + '|' + history.length;\
             </script>",
        );
        let _scripts = PageScripts::new(&mut session).expect("page has scripts");
        assert_eq!(out_text(&session), "https://a.test/rep|1");
        assert_eq!(
            session.current_url().unwrap().as_str(),
            "https://a.test/rep"
        );
    }

    #[test]
    fn cross_origin_push_state_throws() {
        let mut session = session_with(
            "<p id='out'>-</p><script>\
             try {\
               history.pushState({}, '', 'https://evil.test/');\
               document.getElementById('out').textContent = 'no-throw';\
             } catch (error) {\
               document.getElementById('out').textContent = 'thrown';\
             }\
             </script>",
        );
        let _scripts = PageScripts::new(&mut session).expect("page has scripts");
        assert_eq!(out_text(&session), "thrown");
        assert_eq!(session.current_url().unwrap().as_str(), "https://a.test/");
    }

    #[test]
    fn history_back_traverses_and_fires_popstate() {
        let mut session = session_with_files(
            &[
                (
                    "https://a.test/",
                    "<p id='out'>-</p><script>\
                     window.addEventListener('popstate', () => {\
                       document.getElementById('out').textContent = 'popped';\
                     });\
                     </script>",
                ),
                (
                    "https://a.test/iki",
                    "<p>iki</p><script>history.back();</script>",
                ),
            ],
            "https://a.test/",
        );
        let _first = PageScripts::new(&mut session).expect("page has scripts");
        session
            .load(Url::parse("https://a.test/iki").unwrap())
            .unwrap();
        let mut second = PageScripts::new(&mut session).expect("page has scripts");
        // history.back() surfaces as a traversal request for the shell.
        assert_eq!(second.take_navigation().as_deref(), Some("::back"));
        session.back().unwrap();
        assert_eq!(session.current_url().unwrap().as_str(), "https://a.test/");
        // The fresh world of the traversed-to page gets popstate.
        let _third = PageScripts::new(&mut session).expect("page has scripts");
        assert_eq!(out_text(&session), "popped");
    }

    #[test]
    fn navigator_reports_identity() {
        let mut session = session_with(
            "<p id='out'>-</p><script>\
             document.getElementById('out').textContent =\
               navigator.userAgent + '|' + navigator.language + '|' +\
               navigator.platform + '|' + String(navigator.onLine) + '|' +\
               navigator.languages.join(',');\
             </script>",
        );
        let _scripts = PageScripts::new(&mut session).expect("page has scripts");
        let text = out_text(&session);
        assert!(text.starts_with("Lumen/"), "userAgent: {text}");
        assert!(text.contains("|tr-TR|"), "language: {text}");
        assert!(text.ends_with("|true|tr-TR,en"), "rest: {text}");
    }

    #[test]
    fn local_storage_round_trip() {
        let mut session = session_with(
            "<p id='out'>-</p><script>\
             localStorage.setItem('a', '1');\
             const { getItem } = localStorage;\
             localStorage.x = 'prop';\
             let result = [getItem('a'), localStorage.x, localStorage.length].join('|');\
             localStorage.removeItem('a');\
             result += '|' + String(getItem('a'));\
             localStorage.clear();\
             result += '|' + localStorage.length + '|' + String(getItem('x'));\
             document.getElementById('out').textContent = result;\
             </script>",
        );
        session.set_storage_root(crate::storage::temp_root("roundtrip"));
        let _scripts = PageScripts::new(&mut session).expect("page has scripts");
        // Unbound getItem, named-property access, length all work.
        assert_eq!(out_text(&session), "1|prop|2|null|0|null");
    }

    #[test]
    fn local_storage_persists_between_sessions() {
        let root = crate::storage::temp_root("persist");
        {
            let mut session = session_with("<script>localStorage.setItem('k', 'kalıcı');</script>");
            session.set_storage_root(root.clone());
            let _scripts = PageScripts::new(&mut session).expect("page has scripts");
        }
        // A brand-new session (fresh WebStorage) on the same origin and
        // storage root sees the earlier session's write.
        let mut session = session_with(
            "<p id='out'>-</p><script>\
             document.getElementById('out').textContent = localStorage.getItem('k');\
             </script>",
        );
        session.set_storage_root(root.clone());
        let _scripts = PageScripts::new(&mut session).expect("page has scripts");
        assert_eq!(out_text(&session), "kalıcı");
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn session_storage_lives_in_the_world_only() {
        let root = crate::storage::temp_root("session");
        // Set within one script, read back from a later timer entry of
        // the SAME script world: still there.
        let mut session = session_with(
            "<p id='out'>-</p><script>\
             sessionStorage.setItem('k', 'oturum');\
             setTimeout(() => {\
               document.getElementById('out').textContent = sessionStorage.getItem('k');\
             }, 0);\
             </script>",
        );
        session.set_storage_root(root.clone());
        let mut scripts = PageScripts::new(&mut session).expect("page has scripts");
        assert!(scripts.tick(&mut session, 0.0));
        assert_eq!(out_text(&session), "oturum");
        // But a new session's world starts empty.
        let mut session = session_with(
            "<p id='out'>-</p><script>\
             document.getElementById('out').textContent = String(sessionStorage.getItem('k'));\
             </script>",
        );
        session.set_storage_root(root.clone());
        let _scripts = PageScripts::new(&mut session).expect("page has scripts");
        assert_eq!(out_text(&session), "null");
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn fetch_settles_within_the_entry_with_the_inline_queue() {
        let mut session = session_with_files(
            &[
                (
                    "https://a.test/",
                    "<p id='out'>-</p><script>\
                     fetch('/data.txt')\
                       .then(r => r.text())\
                       .then(t => { document.getElementById('out').textContent = t; });\
                     </script>",
                ),
                ("https://a.test/data.txt", "inline-ok"),
            ],
            "https://a.test/",
        );
        // PageScripts::new uses the inline queue: the legacy
        // resolve-within-the-entry behavior is preserved.
        let _scripts = PageScripts::new(&mut session).expect("page has scripts");
        assert_eq!(out_text(&session), "inline-ok");
    }

    #[test]
    fn fetch_stays_pending_until_the_network_is_driven() {
        let mut session = session_with_files(
            &[
                (
                    "https://a.test/",
                    "<p id='out'>-</p><script>\
                     fetch('/data.txt')\
                       .then(r => r.text())\
                       .then(t => { document.getElementById('out').textContent = t; });\
                     </script>",
                ),
                ("https://a.test/data.txt", "async-ok"),
            ],
            "https://a.test/",
        );
        let mut scripts = PageScripts::new_with_network(&mut session, NetworkQueue::manual())
            .expect("page has scripts");
        // Submitted but parked on the queue: the promise is pending
        // across whole entries — the async behavior the threaded
        // shell sees, made deterministic.
        assert_eq!(out_text(&session), "-");
        assert!(scripts.has_pending_network());
        scripts.network.drive();
        // Driven, but settlement still needs a pump on this thread.
        assert_eq!(out_text(&session), "-");
        scripts.pump_network(&mut session);
        assert_eq!(out_text(&session), "async-ok");
        assert!(!scripts.has_pending_network());
    }

    #[test]
    fn fetch_rejection_reaches_catch_after_the_drive() {
        let mut session = session_with(
            "<p id='out'>-</p><script>\
             fetch('/missing')\
               .then(() => { document.getElementById('out').textContent = 'unexpected'; })\
               .catch(() => { document.getElementById('out').textContent = 'caught'; });\
             </script>",
        );
        let mut scripts = PageScripts::new_with_network(&mut session, NetworkQueue::manual())
            .expect("page has scripts");
        assert_eq!(out_text(&session), "-");
        scripts.network.drive();
        scripts.pump_network(&mut session);
        assert_eq!(out_text(&session), "caught");
    }

    /// A session on https://a.test/ whose loader also answers the given
    /// cross-origin URLs with Access-Control-Allow-Origin values.
    fn session_with_acao(files: &[(&str, &str)], acao: &[(&str, &str)]) -> Session<FakeLoader> {
        let mut loader = FakeLoader::new(files);
        for (url, value) in acao {
            loader = loader.with_acao(url, value);
        }
        let mut session = Session::new(
            loader,
            lumen_engine::Size {
                width: 800.0,
                height: 600.0,
            },
        );
        session
            .load(Url::parse("https://a.test/").unwrap())
            .unwrap();
        session
    }

    #[test]
    fn same_origin_fetch_sends_cookies_and_reads() {
        let mut session = session_with_files(
            &[
                (
                    "https://a.test/",
                    "<p id='out'>-</p><script>\
                     document.cookie = 'sid=1';\
                     fetch('/data.txt')\
                       .then(r => r.text())\
                       .then(t => { document.getElementById('out').textContent = t; });\
                     </script>",
                ),
                ("https://a.test/data.txt", "same-origin-ok"),
            ],
            "https://a.test/",
        );
        let _scripts = PageScripts::new(&mut session).expect("page has scripts");
        assert_eq!(out_text(&session), "same-origin-ok");
        // The page load had no cookie yet; the fetch carried the jar's.
        let cookies = session.loader.cookies.lock().unwrap();
        assert_eq!(cookies.len(), 2);
        assert_eq!(cookies[1].as_deref(), Some("sid=1"));
    }

    #[test]
    fn cross_origin_fetch_goes_cookieless_and_rejects_without_a_grant() {
        // Same host but another scheme+port: cross-origin, yet the
        // jar's cookie WOULD domain-match — so an observed None proves
        // the policy stripped it, not the jar.
        let mut session = session_with_files(
            &[
                (
                    "https://a.test/",
                    "<p id='out'>-</p><script>\
                     document.cookie = 'sid=1';\
                     fetch('http://a.test:8080/data')\
                       .then(() => { document.getElementById('out').textContent = 'unexpected'; })\
                       .catch(() => { document.getElementById('out').textContent = 'caught'; });\
                     </script>",
                ),
                ("http://a.test:8080/data", "secret"),
            ],
            "https://a.test/",
        );
        let _scripts = PageScripts::new(&mut session).expect("page has scripts");
        // No Access-Control-Allow-Origin on the response: a network
        // error for JS, even though the load did go out.
        assert_eq!(out_text(&session), "caught");
        let cookies = session.loader.cookies.lock().unwrap();
        assert_eq!(cookies.len(), 2);
        assert_eq!(cookies[1], None);
    }

    #[test]
    fn cross_origin_fetch_resolves_with_a_wildcard_grant() {
        let mut session = session_with_acao(
            &[
                (
                    "https://a.test/",
                    "<p id='out'>-</p><script>\
                     document.cookie = 'sid=1';\
                     fetch('http://a.test:8080/data')\
                       .then(r => r.text())\
                       .then(t => { document.getElementById('out').textContent = t; });\
                     </script>",
                ),
                ("http://a.test:8080/data", "wild-ok"),
            ],
            &[("http://a.test:8080/data", "*")],
        );
        let _scripts = PageScripts::new(&mut session).expect("page has scripts");
        assert_eq!(out_text(&session), "wild-ok");
        let cookies = session.loader.cookies.lock().unwrap();
        assert_eq!(cookies[1], None);
    }

    #[test]
    fn cross_origin_fetch_resolves_only_with_the_pages_exact_origin() {
        let mut session = session_with_acao(
            &[
                (
                    "https://a.test/",
                    "<p id='out'>-</p><script>\
                     const out = document.getElementById('out');\
                     fetch('http://a.test:8080/exact')\
                       .then(r => r.text())\
                       .then(t => { out.textContent += '|' + t; })\
                       .catch(() => { out.textContent += '|exact-caught'; });\
                     fetch('http://a.test:8080/foreign')\
                       .then(() => { out.textContent += '|foreign-ok'; })\
                       .catch(() => { out.textContent += '|foreign-caught'; });\
                     </script>",
                ),
                ("http://a.test:8080/exact", "exact-ok"),
                ("http://a.test:8080/foreign", "foreign-body"),
            ],
            &[
                ("http://a.test:8080/exact", "https://a.test"),
                ("http://a.test:8080/foreign", "https://evil.test"),
            ],
        );
        let _scripts = PageScripts::new(&mut session).expect("page has scripts");
        // The exact-origin grant resolves; the foreign one rejects
        // (settlement order varies with the promise-chain lengths).
        let text = out_text(&session);
        assert!(text.contains("exact-ok"), "{text}");
        assert!(text.contains("foreign-caught"), "{text}");
        assert!(!text.contains("exact-caught"), "{text}");
        assert!(!text.contains("foreign-ok"), "{text}");
    }

    #[test]
    fn credentials_include_sends_cookies_but_rejects_a_wildcard_grant() {
        let mut session = session_with_acao(
            &[
                (
                    "https://a.test/",
                    "<p id='out'>-</p><script>\
                     document.cookie = 'sid=1';\
                     const out = document.getElementById('out');\
                     fetch('http://a.test:8080/star', { credentials: 'include' })\
                       .then(() => { out.textContent += '|star-ok'; })\
                       .catch(() => { out.textContent += '|star-caught'; });\
                     fetch('http://a.test:8080/exact', { credentials: 'include' })\
                       .then(r => r.text())\
                       .then(t => { out.textContent += '|' + t; })\
                       .catch(() => { out.textContent += '|exact-caught'; });\
                     </script>",
                ),
                ("http://a.test:8080/star", "star-body"),
                ("http://a.test:8080/exact", "inc-ok"),
            ],
            &[
                ("http://a.test:8080/star", "*"),
                ("http://a.test:8080/exact", "https://a.test"),
            ],
        );
        let _scripts = PageScripts::new(&mut session).expect("page has scripts");
        // '*' + credentials is an invalid grant (spec): rejected. The
        // exact-origin grant lets the credentialed read through.
        let text = out_text(&session);
        assert!(text.contains("star-caught"), "{text}");
        assert!(text.contains("inc-ok"), "{text}");
        assert!(!text.contains("star-ok"), "{text}");
        assert!(!text.contains("exact-caught"), "{text}");
        // credentials: include DID send the cookie cross-origin, on both.
        let cookies = session.loader.cookies.lock().unwrap();
        assert_eq!(cookies.len(), 3);
        assert_eq!(cookies[1].as_deref(), Some("sid=1"));
        assert_eq!(cookies[2].as_deref(), Some("sid=1"));
    }

    #[test]
    fn cross_origin_xhr_follows_the_same_policy() {
        let mut session = session_with_acao(
            &[
                (
                    "https://a.test/",
                    "<p id='out'>-</p><script>\
                     document.cookie = 'sid=1';\
                     const out = document.getElementById('out');\
                     const blocked = new XMLHttpRequest();\
                     blocked.open('GET', 'http://a.test:8080/blocked');\
                     blocked.onload = () => { out.textContent += '|blocked-ok'; };\
                     blocked.onerror = () => { out.textContent += '|blocked-err'; };\
                     blocked.send();\
                     const granted = new XMLHttpRequest();\
                     granted.open('GET', 'http://a.test:8080/granted');\
                     granted.onload = () => { out.textContent += '|' + granted.responseText; };\
                     granted.onerror = () => { out.textContent += '|granted-err'; };\
                     granted.send();\
                     </script>",
                ),
                ("http://a.test:8080/blocked", "blocked-body"),
                ("http://a.test:8080/granted", "xhr-ok"),
            ],
            &[("http://a.test:8080/granted", "*")],
        );
        let _scripts = PageScripts::new(&mut session).expect("page has scripts");
        // No grant: onerror (status 0, like a network error). Wildcard
        // grant: onload with the body.
        assert_eq!(out_text(&session), "-|blocked-err|xhr-ok");
        // Both requests went out without the jar's cookie.
        let cookies = session.loader.cookies.lock().unwrap();
        assert_eq!(cookies.len(), 3);
        assert_eq!(cookies[1], None);
        assert_eq!(cookies[2], None);
    }

    #[test]
    fn xhr_with_credentials_sends_cookies_and_rejects_a_wildcard_grant() {
        let mut session = session_with_acao(
            &[
                (
                    "https://a.test/",
                    "<p id='out'>-</p><script>\
                     document.cookie = 'sid=1';\
                     const out = document.getElementById('out');\
                     const star = new XMLHttpRequest();\
                     star.open('GET', 'http://a.test:8080/star');\
                     star.withCredentials = true;\
                     star.onload = () => { out.textContent += '|star-ok'; };\
                     star.onerror = () => { out.textContent += '|star-err'; };\
                     star.send();\
                     const exact = new XMLHttpRequest();\
                     exact.open('GET', 'http://a.test:8080/exact');\
                     exact.withCredentials = true;\
                     exact.onload = () => { out.textContent += '|' + exact.responseText; };\
                     exact.onerror = () => { out.textContent += '|exact-err'; };\
                     exact.send();\
                     </script>",
                ),
                ("http://a.test:8080/star", "star-body"),
                ("http://a.test:8080/exact", "wc-ok"),
            ],
            &[
                ("http://a.test:8080/star", "*"),
                ("http://a.test:8080/exact", "https://a.test"),
            ],
        );
        let _scripts = PageScripts::new(&mut session).expect("page has scripts");
        // '*' + withCredentials is an invalid grant (spec): onerror.
        // The exact-origin grant lets the credentialed read through.
        assert_eq!(out_text(&session), "-|star-err|wc-ok");
        // withCredentials DID send the jar's cookie cross-origin, on both.
        let cookies = session.loader.cookies.lock().unwrap();
        assert_eq!(cookies.len(), 3);
        assert_eq!(cookies[1].as_deref(), Some("sid=1"));
        assert_eq!(cookies[2].as_deref(), Some("sid=1"));
    }

    #[test]
    fn preflight_probes_a_non_simple_request_then_sends_it() {
        let grant = CorsGrant {
            allow_origin: Some("*".to_string()),
            allow_methods: vec!["get".to_string()],
            allow_headers: vec!["x-token".to_string()],
        };
        let mut loader = FakeLoader::new(&[
            (
                "https://a.test/",
                "<p id='out'>-</p><script>\
                 const out = document.getElementById('out');\
                 const ask = (label, next) => {\
                   const xhr = new XMLHttpRequest();\
                   xhr.open('GET', 'http://a.test:8080/data');\
                   xhr.setRequestHeader('X-Token', 'abc');\
                   xhr.onload = () => {\
                     out.textContent += '|' + label + ':' + xhr.responseText;\
                     if (next) { next(); }\
                   };\
                   xhr.onerror = () => { out.textContent += '|' + label + ':err'; };\
                   xhr.send();\
                 };\
                 ask('bir', () => ask('iki', null));\
                 </script>",
            ),
            ("http://a.test:8080/data", "pre-ok"),
        ]);
        loader = loader.with_acao("http://a.test:8080/data", "*");
        loader = loader.with_grant("http://a.test:8080/data", grant);
        let mut session = Session::new(
            loader,
            lumen_engine::Size {
                width: 800.0,
                height: 600.0,
            },
        );
        session
            .load(Url::parse("https://a.test/").unwrap())
            .unwrap();
        let _scripts = PageScripts::new(&mut session).expect("page has scripts");
        // The custom header made both reads non-simple: OPTIONS probe,
        // grant, then the actual request. The second read starts in the
        // first one's onload — a later entry, so the session cache
        // answers it: ONE probe for two requests.
        assert_eq!(out_text(&session), "-|bir:pre-ok|iki:pre-ok");
        let probes = session.loader.probes.lock().unwrap();
        assert_eq!(
            *probes,
            vec![(
                "http://a.test:8080/data".to_string(),
                "GET".to_string(),
                vec!["x-token".to_string()]
            )]
        );
        let loads = session.loader.loads.lock().unwrap();
        assert_eq!(
            *loads,
            vec![
                "https://a.test/",
                "http://a.test:8080/data",
                "http://a.test:8080/data"
            ]
        );
    }

    #[test]
    fn a_refused_preflight_keeps_the_request_from_going_out() {
        // The grant names the page origin but does NOT cover the custom
        // header: the read must fail without the request ever leaving.
        let grant = CorsGrant {
            allow_origin: Some("https://a.test".to_string()),
            allow_methods: vec!["get".to_string()],
            allow_headers: Vec::new(),
        };
        let mut loader = FakeLoader::new(&[
            (
                "https://a.test/",
                "<p id='out'>-</p><script>\
                 const xhr = new XMLHttpRequest();\
                 xhr.open('GET', 'http://a.test:8080/data');\
                 xhr.setRequestHeader('X-Token', 'abc');\
                 xhr.onload = () => { document.getElementById('out').textContent = 'unexpected'; };\
                 xhr.onerror = () => { document.getElementById('out').textContent = 'refused'; };\
                 xhr.send();\
                 </script>",
            ),
            ("http://a.test:8080/data", "secret"),
        ]);
        loader = loader.with_grant("http://a.test:8080/data", grant);
        let mut session = Session::new(
            loader,
            lumen_engine::Size {
                width: 800.0,
                height: 600.0,
            },
        );
        session
            .load(Url::parse("https://a.test/").unwrap())
            .unwrap();
        let _scripts = PageScripts::new(&mut session).expect("page has scripts");
        assert_eq!(out_text(&session), "refused");
        // The probe went out; the actual request never did.
        assert_eq!(session.loader.probes.lock().unwrap().len(), 1);
        assert_eq!(
            *session.loader.loads.lock().unwrap(),
            vec!["https://a.test/"]
        );
    }

    #[test]
    fn runtime_module_fetch_stores_set_cookie_in_the_jar() {
        let loader = FakeLoader::new(&[
            (
                "https://a.test/",
                "<button id='btn'>x</button><p id='out'>-</p><script>\
                 document.getElementById('btn').addEventListener('click', () => {\
                   import('/lazy.js').then(m => {\
                     document.getElementById('out').textContent = m.value;\
                   });\
                 });\
                 </script>",
            ),
            ("https://a.test/lazy.js", "export const value = 'lazy-ok';"),
        ])
        .with_cookies("https://a.test/lazy.js", &["mod=1"]);
        let mut session = Session::new(
            loader,
            lumen_engine::Size {
                width: 800.0,
                height: 600.0,
            },
        );
        session
            .load(Url::parse("https://a.test/").unwrap())
            .unwrap();
        let mut scripts = PageScripts::new(&mut session).expect("page has scripts");
        let button = session
            .page()
            .unwrap()
            .document
            .get_element_by_id("btn")
            .unwrap();
        scripts.dispatch(&mut session, button, "click");
        assert_eq!(out_text(&session), "lazy-ok");
        // The runtime queue fetch's Set-Cookie reached the session jar
        // (the APPLY detour — the loader future itself never sees the
        // session).
        assert_eq!(
            session
                .cookies
                .header_for(&Url::parse("https://a.test/lazy.js").unwrap())
                .as_deref(),
            Some("mod=1")
        );
    }

    #[test]
    fn static_module_fetch_stores_set_cookie_in_the_jar() {
        // The static graph fetches through the session directly, so its
        // Set-Cookie handling was never broken — pinned here.
        let loader = FakeLoader::new(&[
            (
                "https://a.test/",
                "<p id='out'>-</p><script type='module' src='/m.js'></script>",
            ),
            (
                "https://a.test/m.js",
                "document.getElementById('out').textContent = 'm-ok';",
            ),
        ])
        .with_cookies("https://a.test/m.js", &["stat=1"]);
        let mut session = Session::new(
            loader,
            lumen_engine::Size {
                width: 800.0,
                height: 600.0,
            },
        );
        session
            .load(Url::parse("https://a.test/").unwrap())
            .unwrap();
        let _scripts = PageScripts::new(&mut session).expect("page has scripts");
        assert_eq!(out_text(&session), "m-ok");
        assert_eq!(
            session
                .cookies
                .header_for(&Url::parse("https://a.test/m.js").unwrap())
                .as_deref(),
            Some("stat=1")
        );
    }

    #[test]
    fn cross_origin_script_src_still_loads() {
        // Subresource loads are no-cors: a cross-origin <script src>
        // fetches and runs without any Access-Control-Allow-Origin.
        let mut session = session_with_files(
            &[
                (
                    "https://a.test/",
                    "<p id='out'>-</p><script src='https://b.test/x.js'></script>",
                ),
                (
                    "https://b.test/x.js",
                    "document.getElementById('out').textContent = 'cross-script-ok';",
                ),
            ],
            "https://a.test/",
        );
        let _scripts = PageScripts::new(&mut session).expect("page has scripts");
        assert_eq!(out_text(&session), "cross-script-ok");
    }

    #[test]
    fn xhr_waits_for_the_network_like_fetch() {
        let mut session = session_with_files(
            &[
                (
                    "https://a.test/",
                    "<p id='out'>-</p><script>\
                     const xhr = new XMLHttpRequest();\
                     xhr.open('GET', '/data.txt');\
                     xhr.onload = () => {\
                       document.getElementById('out').textContent = xhr.responseText;\
                     };\
                     xhr.send();\
                     </script>",
                ),
                ("https://a.test/data.txt", "xhr-async"),
            ],
            "https://a.test/",
        );
        let mut scripts = PageScripts::new_with_network(&mut session, NetworkQueue::manual())
            .expect("page has scripts");
        assert_eq!(out_text(&session), "-");
        scripts.network.drive();
        scripts.pump_network(&mut session);
        assert_eq!(out_text(&session), "xhr-async");
    }

    #[test]
    fn dynamic_import_fetches_new_urls_at_runtime() {
        let mut session = session_with_files(
            &[
                (
                    "https://a.test/",
                    "<button id='btn'>x</button><p id='out'>-</p><script>\
                     document.getElementById('btn').addEventListener('click', () => {\
                       import('/lazy.js').then(m => {\
                         document.getElementById('out').textContent = m.value;\
                       });\
                     });\
                     </script>",
                ),
                ("https://a.test/lazy.js", "export const value = 'lazy-ok';"),
            ],
            "https://a.test/",
        );
        let mut scripts = PageScripts::new(&mut session).expect("page has scripts");
        let button = session
            .page()
            .unwrap()
            .document
            .get_element_by_id("btn")
            .unwrap();
        scripts.dispatch(&mut session, button, "click");
        // The old limitation — runtime import() of a never-fetched URL
        // rejects — is gone: the module came over the network queue.
        assert_eq!(out_text(&session), "lazy-ok");
    }

    #[test]
    fn a_slow_server_does_not_block_the_tick() {
        let mut session = session_with_files(
            &[
                (
                    "https://a.test/",
                    "<p id='out'>-</p><script>\
                     fetch('/slow.txt')\
                       .then(r => r.text())\
                       .then(t => { document.getElementById('out').textContent = t; });\
                     </script>",
                ),
                ("https://a.test/slow.txt", "slow-ok"),
            ],
            "https://a.test/",
        );
        // From here every load takes 600ms — a stalled server. The
        // loader's 20s timeout lives on the worker, not the UI thread.
        *session.loader.delay.lock().unwrap() = Some(Duration::from_millis(600));
        let wakes = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let queue = NetworkQueue::threaded();
        {
            let wakes = wakes.clone();
            queue.set_wake_hook(move || {
                wakes.fetch_add(1, Ordering::SeqCst);
            });
        }
        let started = std::time::Instant::now();
        let mut scripts =
            PageScripts::new_with_network(&mut session, queue).expect("page has scripts");
        let elapsed = started.elapsed();
        assert!(
            elapsed < Duration::from_millis(400),
            "script setup blocked on the network: {elapsed:?}"
        );
        // The promise is pending; the tick that submitted never waited.
        assert_eq!(out_text(&session), "-");
        assert!(scripts.has_pending_network());
        // The worker lands the result and the wake hook fires; the next
        // pump (what the shell does on the wake event) settles it.
        std::thread::sleep(Duration::from_millis(900));
        assert!(wakes.load(Ordering::SeqCst) >= 1, "wake hook never fired");
        scripts.pump_network(&mut session);
        assert_eq!(out_text(&session), "slow-ok");
        assert!(!scripts.has_pending_network());
    }

    #[test]
    fn gtm_pattern_get_elements_parent_node_insert_before() {
        // The Google Tag Manager bootstrap: find the first <script>,
        // then insertBefore a new one next to it.
        let mut session = session_with(
            "<p id='out'>-</p><script id='f'>\
             const f = document.getElementsByTagName('script')[0];\
             const j = document.createElement('script');\
             j.setAttribute('id', 'j');\
             f.parentNode.insertBefore(j, f);\
             const tail = document.createElement('span');\
             tail.setAttribute('id', 's');\
             f.parentNode.insertBefore(tail, null);\
             document.getElementById('out').textContent = [\
               f.parentNode.childNodes.length,\
               f.previousSibling.id,\
               j.nextSibling.id,\
               f.parentNode.firstChild.id,\
               f.parentNode.lastChild.id\
             ].join('|');\
             </script>",
        );
        let _scripts = PageScripts::new(&mut session).expect("page has scripts");
        // body children: <p>, the inserted <script id=j>, <script id=f>;
        // the null reference appended the span at the end.
        assert_eq!(out_text(&session), "4|j|f|out|s");
    }

    #[test]
    fn parent_node_remove_child_detaches_it() {
        let mut session = session_with(
            "<div id='parent'><span id='kid'>x</span></div><p id='out'>-</p><script>\
             const parent = document.getElementById('parent');\
             const kid = document.getElementById('kid');\
             const removed = parent.removeChild(kid);\
             document.getElementById('out').textContent = [\
               parent.childNodes.length,\
               String(kid.parentNode),\
               removed.id\
             ].join('|');\
             </script>",
        );
        let _scripts = PageScripts::new(&mut session).expect("page has scripts");
        assert_eq!(out_text(&session), "0|null|kid");
    }

    #[test]
    fn checker_pattern_add_event_listener_tolerates_null_and_options() {
        // The WP Rocket browser checker: null listeners and a full
        // options object (capture/passive/once) must not throw.
        let mut session = session_with(
            "<p id='out'>-</p><script>\
             window.addEventListener('test', null, { capture: true, passive: true });\
             window.addEventListener('test', undefined, true);\
             window.removeEventListener('test', null);\
             window.addEventListener('test', () => {\
               document.getElementById('out').textContent += 't';\
             }, { capture: false, passive: true, once: false });\
             </script>",
        );
        let mut scripts = PageScripts::new(&mut session).expect("page has scripts");
        assert!(scripts.has_listener(0, "test"));
        scripts.dispatch(&mut session, 0, "test");
        scripts.dispatch(&mut session, 0, "test");
        assert_eq!(out_text(&session), "-tt");
    }

    #[test]
    fn remove_event_listener_drops_the_registration() {
        let mut session = session_with(
            "<button id='btn'>x</button><p id='out'>-</p><script>\
             const out = document.getElementById('out');\
             const handler = () => { out.textContent += 'h'; };\
             const btn = document.getElementById('btn');\
             btn.addEventListener('click', handler, true);\
             btn.removeEventListener('click', handler);\
             btn.removeEventListener('click', null);\
             </script>",
        );
        let mut scripts = PageScripts::new(&mut session).expect("page has scripts");
        let button = session
            .page()
            .unwrap()
            .document
            .get_element_by_id("btn")
            .unwrap();
        assert!(!scripts.has_listener(button, "click"));
        let outcome = scripts.dispatch(&mut session, button, "click");
        assert!(!outcome.handled);
        assert_eq!(out_text(&session), "-");
    }

    #[test]
    fn once_listeners_run_a_single_time() {
        let mut session = session_with(
            "<button id='btn'>x</button><p id='out'>-</p><script>\
             const out = document.getElementById('out');\
             const btn = document.getElementById('btn');\
             btn.addEventListener('click', () => { out.textContent += '1'; }, { once: true });\
             btn.addEventListener('click', () => { out.textContent += 'k'; });\
             </script>",
        );
        let mut scripts = PageScripts::new(&mut session).expect("page has scripts");
        let button = session
            .page()
            .unwrap()
            .document
            .get_element_by_id("btn")
            .unwrap();
        scripts.dispatch(&mut session, button, "click");
        scripts.dispatch(&mut session, button, "click");
        // The once listener fired only on the first dispatch.
        assert_eq!(out_text(&session), "-1kk");
    }

    #[test]
    fn event_target_closest_walks_the_ancestor_chain() {
        let mut session = session_with(
            "<a id='lnk'><span id='inner'>x</span></a><p id='out'>-</p><script>\
             document.getElementById('lnk').addEventListener('click', (e) => {\
               document.getElementById('out').textContent =\
                 e.target.closest('a').id + '|' + String(e.target.closest('table'));\
             });\
             </script>",
        );
        let mut scripts = PageScripts::new(&mut session).expect("page has scripts");
        let inner = session
            .page()
            .unwrap()
            .document
            .get_element_by_id("inner")
            .unwrap();
        scripts.dispatch(&mut session, inner, "click");
        // e.target is an element wrapper: closest('a') climbs to the
        // link; a selector nothing matches yields null.
        assert_eq!(out_text(&session), "lnk|null");
    }

    #[test]
    fn document_exposes_head_root_and_ready_state() {
        let mut session = session_with(
            "<html><head><title>t</title></head><body><p id='out'>-</p><script>\
             document.getElementById('out').textContent = [\
               document.head !== null,\
               document.documentElement !== null,\
               document.body !== null,\
               document.readyState\
             ].join('|');\
             </script></body></html>",
        );
        let _scripts = PageScripts::new(&mut session).expect("page has scripts");
        assert_eq!(out_text(&session), "true|true|true|complete");
    }

    #[test]
    fn an_iframe_browsing_context_is_null_not_undefined() {
        // Cloudflare's challenge checks `iframe.contentDocument ===
        // null` to bail out; undefined would crash it instead.
        let mut session = session_with(
            "<p id='out'>-</p><script>\
             const frame = document.createElement('iframe');\
             document.body.appendChild(frame);\
             document.getElementById('out').textContent =\
               String(frame.contentDocument) + '|' + String(frame.contentWindow);\
             </script>",
        );
        let _scripts = PageScripts::new(&mut session).expect("page has scripts");
        assert_eq!(out_text(&session), "null|null");
    }

    #[test]
    fn repeated_script_errors_are_throttled_after_the_budget() {
        let message = "TypeError: cannot convert 'null' or 'undefined' to object (throttle test)";
        assert_eq!(tally_error(message), ErrorReport::Print);
        assert_eq!(tally_error(message), ErrorReport::Print);
        assert_eq!(tally_error(message), ErrorReport::Print);
        assert_eq!(tally_error(message), ErrorReport::LastOne);
        assert_eq!(tally_error(message), ErrorReport::Silent);
        assert_eq!(tally_error(message), ErrorReport::Silent);
    }
}
