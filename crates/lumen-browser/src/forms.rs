//! Form-control state and behavior on a [`Session`]: live values,
//! checkables, selects (single and multiple), number stepping, color
//! values, range fractions, and GET submission.

use crate::{LoadError, Page, ResourceLoader, Session, resolve};
use lumen_html::NodeId;

/// Minimal application/x-www-form-urlencoded percent encoding.
fn url_encode(input: &str) -> String {
    let mut output = String::with_capacity(input.len());
    for byte in input.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                output.push(byte as char);
            }
            b' ' => output.push('+'),
            other => {
                output.push('%');
                output.push_str(&format!("{other:02X}"));
            }
        }
    }
    output
}

/// A select's `<option>` elements in order, flattening `<optgroup>`s.
pub(crate) fn option_nodes(document: &lumen_html::Document, select: NodeId) -> Vec<NodeId> {
    let mut nodes = Vec::new();
    for child in document.children(select) {
        let Some(element) = document.element(*child) else {
            continue;
        };
        match element.tag_name.as_str() {
            "option" => nodes.push(*child),
            "optgroup" => {
                for grandchild in document.children(*child) {
                    if document
                        .element(*grandchild)
                        .is_some_and(|option| option.tag_name == "option")
                    {
                        nodes.push(*grandchild);
                    }
                }
            }
            _ => {}
        }
    }
    nodes
}

/// An option's submit value: the `value` attribute, else its label text.
fn option_value(document: &lumen_html::Document, option: NodeId) -> String {
    document
        .element(option)
        .and_then(|element| element.attributes.get("value"))
        .map_or_else(
            || document.text_content(option).trim().to_string(),
            str::to_string,
        )
}

impl<L: ResourceLoader> Session<L> {
    /// The current value of a form control (live edits over the parsed
    /// attribute/placeholder).
    #[must_use]
    pub fn form_value(&self, node: NodeId) -> String {
        if let Some(value) = self.form_values.get(&node) {
            return value.clone();
        }
        let Some(page) = self.page.as_ref() else {
            return String::new();
        };
        if self.is_textarea_document(&page.document, node) {
            return page.document.text_content(node);
        }
        page.document
            .element(node)
            .and_then(|element| element.attributes.get("value"))
            .unwrap_or_default()
            .to_string()
    }

    /// Whether a node is a multiline textarea.
    #[must_use]
    pub fn is_textarea(&self, node: NodeId) -> bool {
        self.page
            .as_ref()
            .and_then(|page| page.document.element(node))
            .is_some_and(|element| element.tag_name == "textarea")
    }

    /// A select's options as (value, label) pairs (optgroups flattened),
    /// plus the index of the currently selected one.
    #[must_use]
    pub fn select_options(&self, node: NodeId) -> (Vec<(String, String)>, usize) {
        let Some(page) = self.page.as_ref() else {
            return (Vec::new(), 0);
        };
        let document = &page.document;
        let nodes = option_nodes(document, node);
        let options: Vec<(String, String)> = nodes
            .iter()
            .map(|option| {
                let label = document.text_content(*option).trim().to_string();
                let value = document
                    .element(*option)
                    .and_then(|element| element.attributes.get("value"))
                    .map_or_else(|| label.clone(), str::to_string);
                (value, label)
            })
            .collect();
        let live = self.form_values.get(&node);
        let selected = options
            .iter()
            .position(|(value, _)| Some(value) == live)
            .or_else(|| {
                nodes.iter().position(|option| {
                    document
                        .element(*option)
                        .is_some_and(|element| element.attributes.contains("selected"))
                })
            })
            .unwrap_or(0);
        (options, selected)
    }

    /// Whether a select allows multiple selections (rendered inline as a
    /// list box instead of a dropdown).
    #[must_use]
    pub fn is_multiple_select(&self, node: NodeId) -> bool {
        self.page
            .as_ref()
            .and_then(|page| page.document.element(node))
            .is_some_and(|element| {
                element.tag_name == "select" && element.attributes.contains("multiple")
            })
    }

    /// Whether an option is currently selected (live toggles win over the
    /// parsed `selected` attribute).
    #[must_use]
    pub fn option_selected(&self, option: NodeId) -> bool {
        self.form_checked.get(&option).copied().unwrap_or_else(|| {
            self.page
                .as_ref()
                .and_then(|page| page.document.element(option))
                .is_some_and(|element| element.attributes.contains("selected"))
        })
    }

    /// Clicks an option in a multiple select: a plain click selects just
    /// that option, a toggling click (Cmd/Ctrl) flips it and keeps the
    /// rest.
    pub fn click_option(&mut self, select: NodeId, option: NodeId, toggle: bool) {
        let options = match self.page.as_ref() {
            Some(page) => option_nodes(&page.document, select),
            None => return,
        };
        if !options.contains(&option) {
            return;
        }
        if toggle {
            let current = self.option_selected(option);
            self.form_checked.insert(option, !current);
        } else {
            for peer in options {
                self.form_checked.insert(peer, peer == option);
            }
        }
        self.relayout();
    }

    /// Selects an option by index: the live value and the displayed label
    /// both update.
    pub fn set_selected_option(&mut self, node: NodeId, index: usize) {
        let (options, _) = self.select_options(node);
        let Some((value, label)) = options.get(index).cloned() else {
            return;
        };
        self.form_values.insert(node, value);
        if let Some(page) = self.page.as_mut() {
            page.document.upsert_generated_text(node, true, &label);
        }
        self.relayout();
    }

    /// Sets a range input from a 0..=1 fraction: the value attribute
    /// updates so the fraction bar restyles.
    pub fn set_range_fraction(&mut self, node: NodeId, fraction: f32) {
        let Some(page) = self.page.as_mut() else {
            return;
        };
        let Some(element) = page.document.element(node) else {
            return;
        };
        let attr = |name: &str, default: f32| -> f32 {
            element
                .attributes
                .get(name)
                .and_then(|value| value.parse().ok())
                .unwrap_or(default)
        };
        let (min, max) = (attr("min", 0.0), attr("max", 100.0));
        let value = min + (max - min) * fraction.clamp(0.0, 1.0);
        let rounded = format!("{}", value.round());
        page.document.set_attribute(node, "value", &rounded);
        self.form_values.insert(node, rounded);
        self.relayout();
    }

    /// Whether a node is an `<input type=number>` (arrow keys step it).
    #[must_use]
    pub fn is_number_input(&self, node: NodeId) -> bool {
        self.page
            .as_ref()
            .and_then(|page| page.document.element(node))
            .is_some_and(|element| {
                element.tag_name == "input" && element.attributes.get("type") == Some("number")
            })
    }

    /// Steps a number input by `direction` × its `step` attribute,
    /// clamped to min/max. Returns the new text on success.
    pub fn step_number_input(&mut self, node: NodeId, direction: f32) -> Option<String> {
        let (step, min, max) = {
            let element = self.page.as_ref()?.document.element(node)?;
            if element.tag_name != "input" || element.attributes.get("type") != Some("number") {
                return None;
            }
            let attr = |name: &str| -> Option<f32> {
                element
                    .attributes
                    .get(name)
                    .and_then(|value| value.parse().ok())
            };
            (attr("step").unwrap_or(1.0), attr("min"), attr("max"))
        };
        let current: f32 = self.form_value(node).trim().parse().unwrap_or(0.0);
        let mut value = current + step * direction;
        if let Some(min) = min {
            value = value.max(min);
        }
        if let Some(max) = max {
            value = value.min(max);
        }
        let text = if (value - value.round()).abs() < 1e-4 {
            format!("{}", value.round() as i64)
        } else {
            format!("{value}")
        };
        self.set_form_value(node, &text);
        // The live edit buffer follows the stepped value.
        if let Some(edit) = self.editor.as_mut()
            && edit.node == node
        {
            edit.buffer.text = text.clone();
            edit.buffer.move_to(usize::MAX, false);
        }
        Some(text)
    }

    /// Sets a color input's value: the attribute drives the swatch's
    /// computed background color.
    pub fn set_color_value(&mut self, node: NodeId, value: &str) {
        let Some(page) = self.page.as_mut() else {
            return;
        };
        page.document.set_attribute(node, "value", value);
        self.form_values.insert(node, value.to_string());
        self.relayout();
    }

    /// Whether a node is an editable text-ish input.
    #[must_use]
    pub fn is_text_input(&self, node: NodeId) -> bool {
        self.page
            .as_ref()
            .and_then(|page| page.document.element(node))
            .is_some_and(|element| {
                element.tag_name == "input"
                    && matches!(
                        element.attributes.get("type").unwrap_or("text"),
                        "text" | "search" | "email" | "url" | "password" | "tel" | "number"
                    )
            })
    }

    /// Sets a text control's live value: the generated value text updates
    /// in place and the page relayouts (nowrap + clipping keep it tidy).
    pub fn set_form_value(&mut self, node: NodeId, value: &str) {
        self.form_values.insert(node, value.to_string());
        let display = {
            let element = self
                .page
                .as_ref()
                .and_then(|page| page.document.element(node));
            let is_password =
                element.and_then(|element| element.attributes.get("type")) == Some("password");
            if value.is_empty() {
                // An emptied field shows its placeholder again, like real
                // browsers do. Without one, a lone space keeps the line
                // box (and the control's height) alive.
                let placeholder = element
                    .and_then(|element| element.attributes.get("placeholder"))
                    .unwrap_or_default()
                    .to_string();
                if placeholder.is_empty() {
                    " ".to_string()
                } else {
                    placeholder
                }
            } else if is_password {
                "\u{2022}".repeat(value.chars().count())
            } else {
                value.to_string()
            }
        };
        if let Some(page) = self.page.as_mut() {
            let is_textarea = page
                .document
                .element(node)
                .is_some_and(|element| element.tag_name == "textarea");
            if is_textarea {
                page.document.set_text_content(node, value);
            } else {
                page.document.upsert_generated_text(node, true, &display);
            }
        }
        self.relayout();
    }

    /// [`set_form_value`] with an explicit display text: the shell's
    /// horizontal window into a long single-line value (the caret must
    /// stay visible, so the rendered text is a tail slice).
    pub fn set_form_value_display(&mut self, node: NodeId, value: &str, display: &str) {
        self.form_values.insert(node, value.to_string());
        if let Some(page) = self.page.as_mut() {
            page.document.upsert_generated_text(node, true, display);
        }
        self.relayout();
    }

    fn is_textarea_document(&self, document: &lumen_html::Document, node: NodeId) -> bool {
        document
            .element(node)
            .is_some_and(|element| element.tag_name == "textarea")
    }

    /// Toggles a checkbox (or selects a radio, clearing its name group).
    /// Returns whether anything changed.
    pub fn toggle_checkable(&mut self, node: NodeId) -> bool {
        let Some(page) = self.page.as_ref() else {
            return false;
        };
        let Some(element) = page.document.element(node) else {
            return false;
        };
        if element.tag_name != "input" {
            return false;
        }
        let kind = element.attributes.get("type").unwrap_or("text");
        match kind {
            "checkbox" => {
                let current = self.is_checked(node);
                self.form_checked.insert(node, !current);
            }
            "radio" => {
                let group = element.attributes.get("name").map(str::to_string);
                let peers: Vec<NodeId> = page
                    .document
                    .descendants(page.document.root())
                    .filter(|candidate| {
                        page.document.element(*candidate).is_some_and(|peer| {
                            peer.tag_name == "input"
                                && peer.attributes.get("type") == Some("radio")
                                && peer.attributes.get("name").map(str::to_string) == group
                        })
                    })
                    .collect();
                for peer in peers {
                    self.form_checked.insert(peer, peer == node);
                }
            }
            _ => return false,
        }
        self.relayout();
        true
    }

    /// Whether a checkbox/radio is currently checked.
    #[must_use]
    pub fn is_checked(&self, node: NodeId) -> bool {
        self.form_checked.get(&node).copied().unwrap_or_else(|| {
            self.page
                .as_ref()
                .and_then(|page| page.document.element(node))
                .is_some_and(|element| element.attributes.contains("checked"))
        })
    }

    /// Submits the form containing `node` with method GET: name=value
    /// pairs of its controls become the action URL's query.
    pub fn submit_form(&mut self, node: NodeId) -> Result<&Page, LoadError> {
        let base = self.require_current()?;
        let (action, method, pairs) = {
            let page = self
                .page
                .as_ref()
                .ok_or_else(|| LoadError::InvalidUrl("no page".to_string()))?;
            let document = &page.document;
            let form = std::iter::once(node)
                .chain(document.ancestors(node))
                .find(|candidate| {
                    document
                        .element(*candidate)
                        .is_some_and(|element| element.tag_name == "form")
                })
                .ok_or_else(|| LoadError::InvalidUrl("no enclosing form".to_string()))?;
            let action = document
                .element(form)
                .and_then(|element| element.attributes.get("action"))
                .unwrap_or("")
                .to_string();
            let method = document
                .element(form)
                .and_then(|element| element.attributes.get("method"))
                .unwrap_or("get")
                .to_ascii_lowercase();
            let mut pairs: Vec<(String, String)> = Vec::new();
            for control in document.descendants(form) {
                let Some(element) = document.element(control) else {
                    continue;
                };
                let tag = element.tag_name.clone();
                if !matches!(tag.as_str(), "input" | "select" | "textarea") {
                    continue;
                }
                let Some(name) = element.attributes.get("name") else {
                    continue;
                };
                if tag == "select" {
                    if element.attributes.contains("multiple") {
                        // Every selected option submits its own pair.
                        for option in option_nodes(document, control) {
                            if self.option_selected(option) {
                                pairs.push((name.to_string(), option_value(document, option)));
                            }
                        }
                    } else {
                        let (options, selected) = self.select_options(control);
                        if let Some((value, _)) = options.get(selected) {
                            pairs.push((name.to_string(), value.clone()));
                        }
                    }
                    continue;
                }
                if tag == "textarea" {
                    pairs.push((name.to_string(), self.form_value(control)));
                    continue;
                }
                let kind = element.attributes.get("type").unwrap_or("text");
                match kind {
                    "checkbox" | "radio" => {
                        if self.is_checked(control) {
                            let value = element.attributes.get("value").unwrap_or("on");
                            pairs.push((name.to_string(), value.to_string()));
                        }
                    }
                    "submit" | "button" | "reset" | "hidden" if kind == "hidden" => {
                        pairs.push((
                            name.to_string(),
                            element.attributes.get("value").unwrap_or("").to_string(),
                        ));
                    }
                    "submit" | "button" | "reset" => {}
                    _ => pairs.push((name.to_string(), self.form_value(control))),
                }
            }
            (action, method, pairs)
        };
        let mut url = resolve(&base, &action)?;
        let encoded: String = pairs
            .iter()
            .map(|(name, value)| format!("{}={}", url_encode(name), url_encode(value)))
            .collect::<Vec<_>>()
            .join("&");
        if method == "post" {
            // POST: the pairs travel as an urlencoded body, not the URL.
            return self.load_with_body(
                url,
                Some((
                    "application/x-www-form-urlencoded".to_string(),
                    encoded.into_bytes(),
                )),
            );
        }
        url.set_query(if encoded.is_empty() { None } else { Some(&encoded) });
        self.load(url)
    }
}
