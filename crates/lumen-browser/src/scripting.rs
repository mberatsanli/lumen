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

use crate::{Page, ResourceLoader, ResourceRequest, Session, resolve};
use boa_engine::object::ObjectInitializer;
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
    pending_timers: Vec<(f64, Option<f64>, JsObject)>,
    now_ms: f64,
}

thread_local! {
    static BRIDGE: RefCell<Bridge> = RefCell::new(Bridge::default());
}

fn with_bridge<R>(action: impl FnOnce(&mut Bridge) -> R) -> R {
    BRIDGE.with(|bridge| action(&mut bridge.borrow_mut()))
}

/// A pending `setTimeout`/`setInterval`.
struct Timer {
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
            now_ms: 0.0,
        };
        for source in sources {
            scripts.enter(session, |context| {
                if let Err(error) = context.eval(Source::from_bytes(source.as_bytes())) {
                    eprintln!("[js] script error: {error}");
                }
            });
        }
        Some(scripts)
    }

    /// Whether any listener is registered for (node, event).
    #[must_use]
    pub fn has_listener(&self, node: NodeId, event: &str) -> bool {
        self.listeners
            .iter()
            .any(|(target, kind, _)| *target == node && kind == event)
    }

    /// Dispatches an event at `node`, bubbling to its ancestors. Returns
    /// whether any handler ran (the page may have re-rendered).
    pub fn dispatch<L: ResourceLoader>(
        &mut self,
        session: &mut Session<L>,
        node: NodeId,
        event: &str,
    ) -> bool {
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
        if handlers.is_empty() {
            return false;
        }
        let event_name = event.to_string();
        for (target, callback) in handlers {
            self.enter(session, |context| {
                let target = element_object(target, context);
                let event_object = ObjectInitializer::new(context)
                    .property(
                        js_string!("type"),
                        js_string!(event_name.as_str()),
                        Attribute::all(),
                    )
                    .property(js_string!("target"), target, Attribute::all())
                    .build();
                if let Err(error) =
                    callback.call(&JsValue::undefined(), &[event_object.into()], context)
                {
                    eprintln!("[js] script error: {error}");
                }
            });
        }
        true
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
        ran
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
        with_bridge(|bridge| {
            bridge.page = session.page.take();
            bridge.form_values = std::mem::take(&mut session.form_values);
            bridge.dirty = false;
            bridge.now_ms = now_ms;
        });
        action(&mut self.context);
        let dirty = with_bridge(|bridge| {
            session.page = bridge.page.take();
            session.form_values = std::mem::take(&mut bridge.form_values);
            for (node, event, callback) in bridge.pending_listeners.drain(..) {
                self.listeners.push((node, event, callback));
            }
            for (delay, interval, callback) in bridge.pending_timers.drain(..) {
                self.timers.push(Timer {
                    due_ms: now_ms + delay,
                    interval_ms: interval,
                    callback,
                });
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
            (element.tag_name == "script").then(|| {
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
                let url = resolve(&base, &src).ok()?;
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

    let document = ObjectInitializer::new(context)
        .function(
            NativeFunction::from_fn_ptr(get_element_by_id),
            js_string!("getElementById"),
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
        .build();
    context
        .register_global_property(js_string!("document"), document, Attribute::all())
        .expect("fresh context");

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
    with_bridge(|bridge| {
        let Some(page) = bridge.page.as_ref() else {
            return Vec::new();
        };
        let document = &page.document;
        document
            .descendants(document.root())
            .filter(|node| selector_matches(document, *node, selector))
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
    let count = with_bridge(|bridge| {
        bridge.pending_timers.push((
            delay,
            interval.then_some(delay.max(1.0)),
            callback.clone(),
        ));
        bridge.pending_timers.len()
    });
    Ok(JsValue::from(count as f64))
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
        .build()
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
