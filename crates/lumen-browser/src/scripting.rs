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

use crate::{Page, ResourceLoader, ResourceRequest, Session, resolve as resolve_url};
use boa_engine::object::ObjectInitializer;
use boa_engine::object::builtins::{JsPromise, JsProxyBuilder};
use boa_engine::property::Attribute;
use boa_engine::{Context, JsObject, JsResult, JsValue, NativeFunction, Source, js_string};
use lumen_html::NodeId;
use std::cell::RefCell;
use std::collections::HashMap;

/// The page state scripts operate on while a call is in flight.
#[derive(Default)]
struct Bridge {
    page: Option<Page>,
    form_values: HashMap<NodeId, String>,
    /// Set when a mutation changed something render-visible.
    dirty: bool,
    /// Registrations collected during the call.
    pending_listeners: Vec<(NodeId, String, JsObject)>,
    pending_timers: Vec<(u64, f64, Option<f64>, JsObject)>,
    cleared_timers: Vec<u64>,
    next_timer_id: u64,
    now_ms: f64,
    /// `event.preventDefault()` was called during the dispatch.
    prevented: bool,
    /// `event.stopPropagation()` was called (stops the bubble walk).
    stopped: bool,
    /// fetch() calls made during the entry, resolved between entries.
    pending_fetches: Vec<(String, JsObject, JsObject)>,
    /// The page URL (feeds location.href reads).
    url: String,
    /// A navigation a script requested (location.href = ..., reload()).
    pending_navigation: Option<String>,
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

fn prevent_default(_this: &JsValue, _args: &[JsValue], _context: &mut Context) -> JsResult<JsValue> {
    with_bridge(|bridge| bridge.prevented = true);
    Ok(JsValue::undefined())
}

fn stop_propagation(_this: &JsValue, _args: &[JsValue], _context: &mut Context) -> JsResult<JsValue> {
    with_bridge(|bridge| bridge.stopped = true);
    Ok(JsValue::undefined())
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
    listeners: Vec<(NodeId, String, JsObject)>,
    timers: Vec<Timer>,
    /// fetch() calls awaiting their network round-trip.
    pending_fetches: Vec<(String, JsObject, JsObject)>,
    /// A navigation requested by a script, for the shell to perform.
    navigation: Option<String>,
    now_ms: f64,
}

impl PageScripts {
    /// Builds the script world for the current page and runs its
    /// `<script>` elements (inline text and `src=` fetched against the
    /// page URL) in document order. `None` when the page has no scripts.
    pub fn new<L: ResourceLoader>(session: &mut Session<L>) -> Option<Self> {
        let sources = script_sources(session);
        if sources.is_empty() {
            return None;
        }
        let mut context = Context::default();
        install_globals(&mut context);
        let mut scripts = Self {
            context,
            listeners: Vec::new(),
            timers: Vec::new(),
            pending_fetches: Vec::new(),
            navigation: None,
            now_ms: 0.0,
        };
        for source in sources {
            scripts.enter(session, |context| {
                if let Err(error) = context.eval(Source::from_bytes(source.as_bytes())) {
                    eprintln!("[js] script error: {error}");
                }
            });
        }
        scripts.pump_fetches(session);
        // The document is ready: fire the lifecycle events on the root.
        scripts.dispatch(session, 0, "DOMContentLoaded");
        scripts.dispatch(session, 0, "load");
        Some(scripts)
    }

    /// Whether any listener is registered for (node, event).
    #[must_use]
    pub fn has_listener(&self, node: NodeId, event: &str) -> bool {
        self.listeners
            .iter()
            .any(|(target, kind, _)| *target == node && kind == event)
    }

    /// Dispatches an event at `node`, bubbling to its ancestors.
    pub fn dispatch<L: ResourceLoader>(
        &mut self,
        session: &mut Session<L>,
        node: NodeId,
        event: &str,
    ) -> DispatchOutcome {
        self.dispatch_with_key(session, node, event, None)
    }

    /// [`Self::dispatch`] with a `key` property on the event (keydown /
    /// keyup).
    pub fn dispatch_with_key<L: ResourceLoader>(
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
        let handlers: Vec<(NodeId, JsObject)> = targets
            .iter()
            .flat_map(|target| {
                self.listeners
                    .iter()
                    .filter(|(node, kind, _)| node == target && kind == event)
                    .map(|(node, _, callback)| (*node, callback.clone()))
                    .collect::<Vec<_>>()
            })
            .collect();
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
        for (target, callback) in handlers {
            let key = key.clone();
            self.enter(session, |context| {
                let target = element_object(target, context);
                let mut initializer = ObjectInitializer::new(context);
                initializer
                    .property(
                        js_string!("type"),
                        js_string!(event_name.as_str()),
                        Attribute::all(),
                    )
                    .property(js_string!("target"), target, Attribute::all())
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
                if let Err(error) =
                    callback.call(&JsValue::undefined(), &[event_object.into()], context)
                {
                    eprintln!("[js] script error: {error}");
                }
            });
            let stopped = with_bridge(|bridge| bridge.stopped);
            if stopped {
                break;
            }
        }
        outcome.prevented = with_bridge(|bridge| bridge.prevented);
        self.pump_fetches(session);
        outcome
    }

    /// Runs timers due at `now_ms`. Returns whether anything ran.
    pub fn tick<L: ResourceLoader>(&mut self, session: &mut Session<L>, now_ms: f64) -> bool {
        self.now_ms = now_ms;
        let mut ran = false;
        loop {
            let Some(index) = self.timers.iter().position(|timer| timer.due_ms <= now_ms) else {
                break;
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
                    eprintln!("[js] script error: {error}");
                }
            });
            ran = true;
        }
        if ran {
            self.pump_fetches(session);
        }
        ran
    }

    /// A navigation a script requested (location.href / reload), if any.
    /// The shell performs it; `"::reload"` means refresh.
    pub fn take_navigation(&mut self) -> Option<String> {
        self.navigation.take()
    }

    /// Performs queued fetch() round-trips and resolves their promises,
    /// looping because continuations may fetch again.
    fn pump_fetches<L: ResourceLoader>(&mut self, session: &mut Session<L>) {
        for _ in 0..8 {
            let requests = std::mem::take(&mut self.pending_fetches);
            if requests.is_empty() {
                return;
            }
            let results: Vec<(Result<String, String>, JsObject, JsObject)> = requests
                .into_iter()
                .map(|(target, resolve, reject)| {
                    let response = session
                        .current_url()
                        .cloned()
                        .ok_or_else(|| "no page".to_string())
                        .and_then(|base| {
                            resolve_url(&base, &target).map_err(|error| error.to_string())
                        })
                        .and_then(|url| {
                            session
                                .loader
                                .load(&ResourceRequest { url })
                                .map(|response| response.text())
                                .map_err(|error| error.to_string())
                        });
                    (response, resolve, reject)
                })
                .collect();
            self.enter(session, |context| {
                for (result, resolve, reject) in results {
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
                        eprintln!("[js] script error: {error}");
                    }
                }
            });
        }
        eprintln!("[js] fetch chain ran too deep; dropping the rest");
        self.pending_fetches.clear();
    }

    /// Whether timers are pending (the shell keeps frames coming).
    #[must_use]
    pub fn has_timers(&self) -> bool {
        !self.timers.is_empty()
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
        let url = session
            .current_url()
            .map(ToString::to_string)
            .unwrap_or_default();
        with_bridge(|bridge| {
            bridge.page = session.page.take();
            bridge.form_values = std::mem::take(&mut session.form_values);
            bridge.dirty = false;
            bridge.now_ms = now_ms;
            bridge.url = url;
        });
        action(&mut self.context);
        // Drain the microtask queue (promise .then/await continuations)
        // while the page is still checked in.
        if let Err(error) = self.context.run_jobs() {
            eprintln!("[js] script error: {error}");
        }
        let dirty = with_bridge(|bridge| {
            session.page = bridge.page.take();
            session.form_values = std::mem::take(&mut bridge.form_values);
            for (node, event, callback) in bridge.pending_listeners.drain(..) {
                self.listeners.push((node, event, callback));
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
            if let Some(target) = bridge.pending_navigation.take() {
                self.navigation = Some(target);
            }
            bridge.dirty
        });
        if dirty {
            session.relayout();
        }
    }
}

/// The page's `<script>` sources in document order.
fn script_sources<L: ResourceLoader>(session: &mut Session<L>) -> Vec<String> {
    let Some(base) = session.current_url().cloned() else {
        return Vec::new();
    };
    let Some(page) = session.page.as_ref() else {
        return Vec::new();
    };
    let document = &page.document;
    let scripts: Vec<(Option<String>, String)> = document
        .descendants(document.root())
        .filter_map(|node| {
            let element = document.element(node)?;
            if element.tag_name != "script" {
                return None;
            }
            // Only classic JavaScript runs: JSON-LD, templates, import
            // maps and modules (no import support) are skipped.
            let kind = element.attributes.get("type").unwrap_or("").trim();
            let classic = matches!(
                kind,
                "" | "text/javascript" | "application/javascript" | "application/ecmascript"
            );
            classic.then(|| {
                (
                    element.attributes.get("src").map(str::to_string),
                    document.text_content(node),
                )
            })
        })
        .collect();
    scripts
        .into_iter()
        .filter_map(|(src, inline)| match src {
            Some(src) => {
                let url = resolve_url(&base, &src).ok()?;
                session
                    .loader
                    .load(&ResourceRequest { url })
                    .ok()
                    .map(|response| response.text())
            }
            None => Some(inline),
        })
        .collect()
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
    let document = ObjectInitializer::new(context)
        .property(js_string!("__node"), JsValue::from(0.0), Attribute::empty())
        .function(
            NativeFunction::from_fn_ptr(add_event_listener),
            js_string!("addEventListener"),
            2,
        )
        .accessor(js_string!("body"), Some(body_get), None, Attribute::all())
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
        .register_global_builtin_callable(js_string!("fetch"), 1, NativeFunction::from_fn_ptr(fetch_))
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
    // window is the global object itself (enough for window.location,
    // window.setTimeout and friends).
    let global = context.global_object();
    context
        .register_global_property(js_string!("window"), global, Attribute::all())
        .expect("fresh context");
}

fn document_body_get(_this: &JsValue, _args: &[JsValue], context: &mut Context) -> JsResult<JsValue> {
    let body = with_bridge(|bridge| {
        let page = bridge.page.as_ref()?;
        let document = &page.document;
        document.descendants(document.root()).find(|node| {
            document
                .element(*node)
                .is_some_and(|element| element.tag_name == "body")
        })
    });
    Ok(match body {
        Some(node) => element_object(node, context).into(),
        None => JsValue::null(),
    })
}

fn window_add_event_listener(
    _this: &JsValue,
    args: &[JsValue],
    context: &mut Context,
) -> JsResult<JsValue> {
    let event = string_arg(args, 0, context);
    let Some(callback) = args.get(1).and_then(JsValue::as_object) else {
        return Ok(JsValue::undefined());
    };
    with_bridge(|bridge| {
        // The window listens on the document root (node 0).
        bridge.pending_listeners.push((0, event, callback.clone()));
    });
    Ok(JsValue::undefined())
}

/// Cheap stubs that keep common site boot code from crashing:
/// localStorage/sessionStorage (in-memory via plain objects driven by
/// JS), matchMedia, navigator and requestAnimationFrame.
fn install_stubs(context: &mut Context) {
    let source = r#"
        var localStorage = {
            __data: {},
            getItem(key) { return Object.hasOwn(this.__data, key) ? this.__data[key] : null; },
            setItem(key, value) { this.__data[key] = String(value); },
            removeItem(key) { delete this.__data[key]; },
            clear() { this.__data = {}; },
        };
        var sessionStorage = {
            __data: {},
            getItem(key) { return Object.hasOwn(this.__data, key) ? this.__data[key] : null; },
            setItem(key, value) { this.__data[key] = String(value); },
            removeItem(key) { delete this.__data[key]; },
            clear() { this.__data = {}; },
        };
        var navigator = { userAgent: "Lumen/0.1 (educational)", language: "tr-TR", languages: ["tr-TR", "en"] };
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

fn fetch_(_this: &JsValue, args: &[JsValue], context: &mut Context) -> JsResult<JsValue> {
    let target = string_arg(args, 0, context);
    let (promise, resolvers) = JsPromise::new_pending(context);
    with_bridge(|bridge| {
        bridge
            .pending_fetches
            .push((target, resolvers.resolve.into(), resolvers.reject.into()));
    });
    Ok(promise.into())
}

/// Builds the object a fetch resolves with: `ok`/`status` plus `text()`
/// and `json()` returning already-resolved promises.
fn response_object(body: &str, context: &mut Context) -> JsObject {
    ObjectInitializer::new(context)
        .property(js_string!("ok"), true, Attribute::all())
        .property(js_string!("status"), 200, Attribute::all())
        .property(
            js_string!("__body"),
            js_string!(body),
            Attribute::empty(),
        )
        .function(NativeFunction::from_fn_ptr(response_text), js_string!("text"), 0)
        .function(NativeFunction::from_fn_ptr(response_json), js_string!("json"), 0)
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
    let json = context
        .global_object()
        .get(js_string!("JSON"), context)?;
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

// ---- location ----

fn location_href_get(_this: &JsValue, _args: &[JsValue], _context: &mut Context) -> JsResult<JsValue> {
    let url = with_bridge(|bridge| bridge.url.clone());
    Ok(JsValue::from(js_string!(url.as_str())))
}

fn location_href_set(_this: &JsValue, args: &[JsValue], context: &mut Context) -> JsResult<JsValue> {
    let target = string_arg(args, 0, context);
    with_bridge(|bridge| bridge.pending_navigation = Some(target));
    Ok(JsValue::undefined())
}

fn location_reload(_this: &JsValue, _args: &[JsValue], _context: &mut Context) -> JsResult<JsValue> {
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

// ---- document ----

fn string_arg(args: &[JsValue], index: usize, context: &mut Context) -> String {
    args.get(index)
        .and_then(|value| value.to_string(context).ok())
        .map(|value| value.to_std_string_escaped())
        .unwrap_or_default()
}

fn get_element_by_id(_this: &JsValue, args: &[JsValue], context: &mut Context) -> JsResult<JsValue> {
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

/// The engine node id an element wrapper points at.
fn this_node(this: &JsValue, context: &mut Context) -> Option<NodeId> {
    let object = this.as_object()?;
    let value = object.get(js_string!("__node"), context).ok()?;
    value.as_number().map(|number| number as NodeId)
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
    let parent_get = NativeFunction::from_fn_ptr(parent_element_get).to_js_function(context.realm());
    let children_get = NativeFunction::from_fn_ptr(children_get_).to_js_function(context.realm());
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
        .function(NativeFunction::from_fn_ptr(remove_node), js_string!("remove"), 0)
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
            js_string!("children"),
            Some(children_get),
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
fn trap_context(args: &[JsValue], context: &mut Context) -> Option<(NodeId, String)> {
    let node = this_node(args.first()?, context)?;
    let key = args
        .get(1)
        .and_then(|value| value.to_string(context).ok())
        .map(|value| value.to_std_string_escaped())?;
    Some((node, key))
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
    let Some((node, key)) = trap_context(args, context) else {
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
    let Some((node, key)) = trap_context(args, context) else {
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
    let Some((node, key)) = trap_context(args, context) else {
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
    let Some((node, key)) = trap_context(args, context) else {
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

fn parent_element_get(this: &JsValue, _args: &[JsValue], context: &mut Context) -> JsResult<JsValue> {
    let Some(node) = this_node(this, context) else {
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

fn children_get_(this: &JsValue, _args: &[JsValue], context: &mut Context) -> JsResult<JsValue> {
    let Some(node) = this_node(this, context) else {
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

fn element_query_selector(this: &JsValue, args: &[JsValue], context: &mut Context) -> JsResult<JsValue> {
    let Some(node) = this_node(this, context) else {
        return Ok(JsValue::null());
    };
    let selector = string_arg(args, 0, context);
    Ok(match query_nodes_scoped(selector.trim(), Some(node)).first() {
        Some(found) => element_object(*found, context).into(),
        None => JsValue::null(),
    })
}

fn element_query_selector_all(
    this: &JsValue,
    args: &[JsValue],
    context: &mut Context,
) -> JsResult<JsValue> {
    let Some(node) = this_node(this, context) else {
        return Ok(JsValue::undefined());
    };
    let selector = string_arg(args, 0, context);
    let items: Vec<JsValue> = query_nodes_scoped(selector.trim(), Some(node))
        .into_iter()
        .map(|found| element_object(found, context).into())
        .collect();
    Ok(boa_engine::object::builtins::JsArray::from_iter(items, context).into())
}

fn bounding_client_rect(this: &JsValue, _args: &[JsValue], context: &mut Context) -> JsResult<JsValue> {
    let Some(node) = this_node(this, context) else {
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
    let Some(node) = this_node(this, context) else {
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
    let Some(node) = this_node(this, context) else {
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
    let Some(node) = this_node(this, context) else {
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
    let Some(node) = this_node(this, context) else {
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
    let Some(node) = this_node(this, context) else {
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
    let Some(node) = this_node(this, context) else {
        return Ok(JsValue::undefined());
    };
    let class = string_arg(args, 0, context);
    let found = with_bridge(|bridge| {
        bridge
            .page
            .as_ref()
            .and_then(|page| page.document.element(node))
            .is_some_and(|element| element.attributes.get("class").is_some_and(|classes| {
                classes.split_whitespace().any(|c| c == class)
            }))
    });
    Ok(JsValue::from(found))
}

fn text_content_get(this: &JsValue, _args: &[JsValue], context: &mut Context) -> JsResult<JsValue> {
    let Some(node) = this_node(this, context) else {
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
    let Some(node) = this_node(this, context) else {
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
    let Some(node) = this_node(this, context) else {
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
    let Some(node) = this_node(this, context) else {
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
            let display = if value.is_empty() { " " } else { value.as_str() };
            page.document.upsert_generated_text(node, true, display);
        }
        bridge.dirty = true;
    });
    Ok(JsValue::undefined())
}

fn id_get_(this: &JsValue, _args: &[JsValue], context: &mut Context) -> JsResult<JsValue> {
    let Some(node) = this_node(this, context) else {
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

fn add_event_listener(this: &JsValue, args: &[JsValue], context: &mut Context) -> JsResult<JsValue> {
    let Some(node) = this_node(this, context) else {
        return Ok(JsValue::undefined());
    };
    let event = string_arg(args, 0, context);
    let Some(callback) = args.get(1).and_then(JsValue::as_object) else {
        return Ok(JsValue::undefined());
    };
    with_bridge(|bridge| {
        bridge.pending_listeners.push((node, event, callback.clone()));
    });
    Ok(JsValue::undefined())
}

fn append_child(this: &JsValue, args: &[JsValue], context: &mut Context) -> JsResult<JsValue> {
    let Some(parent) = this_node(this, context) else {
        return Ok(JsValue::undefined());
    };
    let child = args
        .first()
        .cloned()
        .map(|value| this_node(&value, context));
    let Some(Some(child)) = child else {
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

fn remove_node(this: &JsValue, _args: &[JsValue], context: &mut Context) -> JsResult<JsValue> {
    let Some(node) = this_node(this, context) else {
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
    let Some(node) = this_node(this, context) else {
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
    let Some(node) = this_node(this, context) else {
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
