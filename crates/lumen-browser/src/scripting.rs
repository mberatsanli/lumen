//! Page scripting: wires [`lumen_js`] into a [`Session`].
//!
//! `<script>` elements run after the page first renders; DOM access goes
//! through [`DomHost`], which marks the page dirty so the session
//! relayouts once per batch of mutations. Events bubble from the hit
//! node up through its ancestors.

use crate::{Page, ResourceLoader, ResourceRequest, Session, resolve};
use lumen_html::NodeId;
use lumen_js::{DomNode, Host};

/// The [`lumen_js::Host`] a session exposes to scripts: a mutable window
/// onto the page plus the live form values.
struct DomHost<'a> {
    page: &'a mut Page,
    form_values: &'a mut std::collections::HashMap<NodeId, String>,
    rand_state: &'a mut u64,
    /// Set when a mutation changed something render-visible.
    dirty: &'a mut bool,
}

fn node_id(node: DomNode) -> NodeId {
    node as NodeId
}

impl Host for DomHost<'_> {
    fn console_log(&mut self, message: &str) {
        eprintln!("[js] {message}");
    }

    fn get_element_by_id(&mut self, id: &str) -> Option<DomNode> {
        let document = &self.page.document;
        document
            .descendants(document.root())
            .find(|node| {
                document
                    .element(*node)
                    .is_some_and(|element| element.attributes.get("id") == Some(id))
            })
            .map(|node| node as DomNode)
    }

    /// Supports the practical selector subset scripts actually use:
    /// `#id`, `.class`, `tag` and `tag.class`.
    fn query_selector_all(&mut self, selector: &str) -> Vec<DomNode> {
        let selector = selector.trim();
        let document = &self.page.document;
        let matches = |node: NodeId| -> bool {
            let Some(element) = document.element(node) else {
                return false;
            };
            if let Some(id) = selector.strip_prefix('#') {
                return element.attributes.get("id") == Some(id);
            }
            if let Some(class) = selector.strip_prefix('.') {
                return element
                    .attributes
                    .get("class")
                    .is_some_and(|classes| classes.split_whitespace().any(|c| c == class));
            }
            match selector.split_once('.') {
                Some((tag, class)) => {
                    element.tag_name == tag
                        && element
                            .attributes
                            .get("class")
                            .is_some_and(|classes| classes.split_whitespace().any(|c| c == class))
                }
                None => element.tag_name == selector,
            }
        };
        document
            .descendants(document.root())
            .filter(|node| matches(*node))
            .map(|node| node as DomNode)
            .collect()
    }

    fn get_text(&mut self, node: DomNode) -> String {
        self.page.document.text_content(node_id(node))
    }

    fn set_text(&mut self, node: DomNode, text: &str) {
        self.page.document.set_text_content(node_id(node), text);
        *self.dirty = true;
    }

    fn get_value(&mut self, node: DomNode) -> String {
        let node = node_id(node);
        if let Some(value) = self.form_values.get(&node) {
            return value.clone();
        }
        self.page
            .document
            .element(node)
            .and_then(|element| element.attributes.get("value"))
            .unwrap_or_default()
            .to_string()
    }

    fn set_value(&mut self, node: DomNode, value: &str) {
        let node = node_id(node);
        self.form_values.insert(node, value.to_string());
        let is_textarea = self
            .page
            .document
            .element(node)
            .is_some_and(|element| element.tag_name == "textarea");
        if is_textarea {
            self.page.document.set_text_content(node, value);
        } else {
            let display = if value.is_empty() { " " } else { value };
            self.page.document.upsert_generated_text(node, true, display);
        }
        *self.dirty = true;
    }

    fn get_attribute(&mut self, node: DomNode, name: &str) -> Option<String> {
        self.page
            .document
            .element(node_id(node))
            .and_then(|element| element.attributes.get(name))
            .map(str::to_string)
    }

    fn set_attribute(&mut self, node: DomNode, name: &str, value: &str) {
        self.page.document.set_attribute(node_id(node), name, value);
        *self.dirty = true;
    }

    fn set_style(&mut self, node: DomNode, property: &str, value: &str) {
        // Appended declarations win within the style attribute.
        let existing = self
            .page
            .document
            .element(node_id(node))
            .and_then(|element| element.attributes.get("style"))
            .unwrap_or_default()
            .to_string();
        let merged = format!("{existing}; {property}: {value}");
        self.page
            .document
            .set_attribute(node_id(node), "style", &merged);
        *self.dirty = true;
    }

    fn random(&mut self) -> f64 {
        // xorshift64*: deterministic, dependency-free.
        let mut state = *self.rand_state;
        state ^= state >> 12;
        state ^= state << 25;
        state ^= state >> 27;
        *self.rand_state = state;
        (state.wrapping_mul(0x2545_F491_4F6C_DD1D) >> 11) as f64 / (1u64 << 53) as f64
    }
}

impl<L: ResourceLoader> Session<L> {
    /// Collects and runs the page's `<script>` elements (inline text and
    /// `src=` fetched against the final URL). Called once per load.
    pub(crate) fn run_page_scripts(&mut self, base: &lumen_platform::Url) {
        let sources: Vec<String> = {
            let Some(page) = self.page.as_ref() else {
                return;
            };
            let document = &page.document;
            document
                .descendants(document.root())
                .filter(|node| {
                    document
                        .element(*node)
                        .is_some_and(|element| element.tag_name == "script")
                })
                .filter_map(|node| {
                    let element = document.element(node)?;
                    match element.attributes.get("src") {
                        Some(src) => {
                            let url = resolve(base, src).ok()?;
                            self.loader
                                .load(&ResourceRequest { url })
                                .ok()
                                .map(|response| response.text())
                        }
                        None => Some(document.text_content(node)),
                    }
                })
                .collect()
        };
        if sources.is_empty() {
            return;
        }
        let mut runtime = lumen_js::Runtime::new();
        let mut dirty = false;
        for source in sources {
            let Some(page) = self.page.as_mut() else {
                return;
            };
            let mut host = DomHost {
                page,
                form_values: &mut self.form_values,
                rand_state: &mut self.rand_state,
                dirty: &mut dirty,
            };
            if let Err(error) = runtime.run(&source, &mut host) {
                eprintln!("[js] script error: {error}");
            }
        }
        self.scripts = Some(runtime);
        if dirty {
            self.relayout();
        }
    }

    /// Dispatches a DOM event at `node`, bubbling to its ancestors.
    /// Returns whether any handler ran (the page may have re-rendered).
    pub fn dispatch_dom_event(&mut self, node: NodeId, event: &str) -> bool {
        let Some(mut runtime) = self.scripts.take() else {
            return false;
        };
        let mut targets: Vec<NodeId> = vec![node];
        if let Some(page) = self.page.as_ref() {
            targets.extend(page.document.ancestors(node));
        }
        let mut dirty = false;
        let mut ran = false;
        for target in targets {
            if !runtime.has_listener(target as DomNode, event) {
                continue;
            }
            let Some(page) = self.page.as_mut() else {
                break;
            };
            let mut host = DomHost {
                page,
                form_values: &mut self.form_values,
                rand_state: &mut self.rand_state,
                dirty: &mut dirty,
            };
            ran |= runtime.dispatch_event(target as DomNode, event, &mut host);
        }
        self.scripts = Some(runtime);
        if dirty {
            self.relayout();
        }
        ran
    }

    /// Runs script timers due at `now_ms`. Returns whether anything ran.
    pub fn tick_scripts(&mut self, now_ms: f64) -> bool {
        let Some(mut runtime) = self.scripts.take() else {
            return false;
        };
        let mut dirty = false;
        let ran = match self.page.as_mut() {
            Some(page) => {
                let mut host = DomHost {
                    page,
                    form_values: &mut self.form_values,
                    rand_state: &mut self.rand_state,
                    dirty: &mut dirty,
                };
                runtime.run_timers(now_ms, &mut host)
            }
            None => false,
        };
        self.scripts = Some(runtime);
        if dirty {
            self.relayout();
        }
        ran
    }

    /// Whether script timers are pending (the shell keeps ticking).
    #[must_use]
    pub fn has_script_timers(&self) -> bool {
        self.scripts
            .as_ref()
            .is_some_and(lumen_js::Runtime::has_timers)
    }
}
