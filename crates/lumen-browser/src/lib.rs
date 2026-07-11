//! Browser orchestration: sessions, navigation and history.
//!
//! A [`Session`] owns a resource loader and a viewport, fetches documents
//! and runs them through the engine pipeline. Navigation is deliberately
//! separate from rendering: the engine knows nothing about URLs, and this
//! crate knows nothing about painting beyond handing back a [`Page`].

use lumen_engine::{
    HeuristicMeasurer, ImageMap, Page, RasterImage, Size, TextMeasurer, collect_author_css,
    collect_image_sources,
};
use lumen_html::NodeId;
use lumen_platform::{LoadError, ResourceLoader, ResourceRequest, Url, resolve};
use std::sync::Arc;

/// One browsing context with linear history.
///
/// History entries are re-fetched on `back`/`forward`/`refresh`; there is
/// no document cache yet. All responses are treated as HTML.
pub struct Session<L: ResourceLoader> {
    loader: L,
    viewport: Size,
    measurer: Box<dyn TextMeasurer + Send>,
    history: Vec<Url>,
    /// Index of the current entry in `history`, if any page is loaded.
    index: Option<usize>,
    page: Option<Page>,
    /// HTML source of the current page, kept so viewport or measurer
    /// changes can relayout locally without hitting the network.
    source: Option<String>,
    /// Author stylesheet (embedded + external), fetched once per page and
    /// shared with relayouts through the `Arc`.
    author: Arc<lumen_css::Stylesheet>,
    /// Decoded images, fetched once per page, shared the same way.
    images: Arc<ImageMap>,
    /// Node currently under the pointer, for `:hover` styling.
    hovered: Option<NodeId>,
    /// Node the pointer is pressed on (`:active`).
    active: Option<NodeId>,
    /// Focused node (`:focus`) — the shell decides what focus means.
    focused: Option<NodeId>,
    /// Live form control values (overriding the parsed attributes).
    form_values: std::collections::HashMap<NodeId, String>,
    /// Live checkbox/radio state.
    form_checked: std::collections::HashMap<NodeId, bool>,
    /// Final URLs of visited pages this session (drives `:visited`).
    visited: std::collections::HashSet<String>,
    /// Running property transitions, stepped by [`Session::tick`].
    transitions: Vec<ActiveTransition>,
    /// Whether the current stylesheet declares any `transition` at all.
    has_transitions: bool,
    /// Per-element inner scroll offsets (`overflow: scroll/auto`).
    scroll_offsets: std::collections::HashMap<NodeId, f32>,
    /// First usable `@font-face` font of the page (TTF/OTF only —
    /// fontdue cannot parse WOFF), used as the document font.
    web_font: Option<Arc<lumen_engine::SystemFont>>,
    /// How the current stylesheet's hover rules can affect the page —
    /// picks the cheapest reaction to hover changes.
    hover_impact: lumen_engine::HoverImpact,
}

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

/// One value being animated.
#[derive(Debug, Clone, Copy)]
enum AnimatedValue {
    Number(f32),
    Color(lumen_css::Color),
    Transform(lumen_engine::Transform2D),
}

/// A property transition in flight.
#[derive(Debug, Clone)]
struct ActiveTransition {
    node: NodeId,
    property: &'static str,
    from: AnimatedValue,
    to: AnimatedValue,
    /// Set on the first tick.
    start_ms: Option<f64>,
    duration_ms: f64,
    delay_ms: f64,
    ease: bool,
}

fn lerp_color(from: lumen_css::Color, to: lumen_css::Color, t: f32) -> lumen_css::Color {
    let mix = |a: u8, b: u8| (f32::from(a) + (f32::from(b) - f32::from(a)) * t) as u8;
    lumen_css::Color {
        r: mix(from.r, to.r),
        g: mix(from.g, to.g),
        b: mix(from.b, to.b),
        a: mix(from.a, to.a),
    }
}

/// A select's `<option>` elements in order, flattening `<optgroup>`s.
fn option_nodes(document: &lumen_html::Document, select: NodeId) -> Vec<NodeId> {
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
    #[must_use]
    pub fn new(loader: L, viewport: Size) -> Self {
        Self {
            loader,
            viewport,
            measurer: Box::new(HeuristicMeasurer),
            history: Vec::new(),
            index: None,
            page: None,
            source: None,
            author: Arc::new(lumen_css::Stylesheet::default()),
            images: Arc::new(ImageMap::new()),
            hovered: None,
            active: None,
            focused: None,
            form_values: std::collections::HashMap::new(),
            form_checked: std::collections::HashMap::new(),
            visited: std::collections::HashSet::new(),
            transitions: Vec::new(),
            has_transitions: false,
            scroll_offsets: std::collections::HashMap::new(),
            web_font: None,
            hover_impact: lumen_engine::HoverImpact::Nothing,
        }
    }

    /// Replaces the text measurer (e.g. with real font metrics) and
    /// relayouts the current page if one is loaded. No network access.
    pub fn set_measurer(&mut self, measurer: Box<dyn TextMeasurer + Send>) {
        self.measurer = measurer;
        self.relayout();
    }

    /// Navigates to `url`: fetches, renders, pushes a history entry and
    /// drops any forward entries.
    pub fn load(&mut self, url: Url) -> Result<&Page, LoadError> {
        let final_url = self.fetch_and_render(url)?;
        if let Some(index) = self.index {
            self.history.truncate(index + 1);
        }
        self.visited.insert(final_url.to_string());
        self.history.push(final_url);
        self.index = Some(self.history.len() - 1);
        Ok(self.page.as_ref().expect("fetch_and_render set the page"))
    }

    /// Re-fetches the current entry.
    pub fn refresh(&mut self) -> Result<&Page, LoadError> {
        let url = self.require_current()?;
        self.fetch_and_render(url)?;
        Ok(self.page.as_ref().expect("fetch_and_render set the page"))
    }

    /// Goes one entry back, if possible.
    pub fn back(&mut self) -> Result<&Page, LoadError> {
        let index = self
            .index
            .filter(|index| *index > 0)
            .ok_or_else(|| LoadError::InvalidUrl("no earlier history entry".to_string()))?;
        self.fetch_and_render(self.history[index - 1].clone())?;
        self.index = Some(index - 1);
        Ok(self.page.as_ref().expect("fetch_and_render set the page"))
    }

    /// Goes one entry forward, if possible.
    pub fn forward(&mut self) -> Result<&Page, LoadError> {
        let index = self
            .index
            .filter(|index| index + 1 < self.history.len())
            .ok_or_else(|| LoadError::InvalidUrl("no later history entry".to_string()))?;
        self.fetch_and_render(self.history[index + 1].clone())?;
        self.index = Some(index + 1);
        Ok(self.page.as_ref().expect("fetch_and_render set the page"))
    }

    /// Resolves a (possibly relative) `href` against the current page and
    /// navigates to it.
    pub fn follow(&mut self, href: &str) -> Result<&Page, LoadError> {
        let base = self.require_current()?;
        let url = resolve(&base, href)?;
        self.load(url)
    }

    /// Changes the viewport and lays the current page out again from the
    /// cached source. No network access, so it is safe to call on every
    /// window resize event.
    pub fn set_viewport(&mut self, viewport: Size) {
        self.viewport = viewport;
        self.relayout();
    }

    /// Updates the hovered node for `:hover` styling. Returns whether the
    /// page was restyled (only when the hovered node actually changed).
    /// No network access.
    pub fn set_hovered(&mut self, node: Option<NodeId>) -> bool {
        if self.hovered == node {
            return false;
        }
        let previous = self.hovered;
        self.hovered = node;
        self.react_to_interaction_change(|document, author| {
            lumen_engine::interaction_styles_may_change(
                document,
                author,
                &lumen_engine::InteractionState::new(document, previous, None, None),
            )
        })
    }

    /// Updates the `:active` (pressed) node. Same reaction ladder as
    /// hover.
    pub fn set_active(&mut self, node: Option<NodeId>) -> bool {
        if self.active == node {
            return false;
        }
        let previous = self.active;
        self.active = node;
        self.react_to_interaction_change(|document, author| {
            lumen_engine::interaction_styles_may_change(
                document,
                author,
                &lumen_engine::InteractionState::new(document, None, previous, None),
            )
        })
    }

    /// Updates the `:focus` node. Same reaction ladder as hover.
    pub fn set_focused(&mut self, node: Option<NodeId>) -> bool {
        if self.focused == node {
            return false;
        }
        let previous = self.focused;
        self.focused = node;
        self.react_to_interaction_change(|document, author| {
            lumen_engine::interaction_styles_may_change(
                document,
                author,
                &lumen_engine::InteractionState::new(document, None, None, previous),
            )
        })
    }

    /// The interaction state for the current session fields.
    fn interaction(&self) -> Option<lumen_engine::InteractionState> {
        let page = self.page.as_ref()?;
        Some(
            lumen_engine::InteractionState::new(
                &page.document,
                self.hovered,
                self.active,
                self.focused,
            )
            .with_visited(self.visited_link_nodes(&page.document))
            .with_checked(self.checked_nodes(&page.document)),
        )
    }

    /// All checked checkables (live toggles over checked attributes).
    fn checked_nodes(&self, document: &lumen_html::Document) -> std::collections::HashSet<NodeId> {
        document
            .descendants(document.root())
            .filter(|node| {
                document.element(*node).is_some_and(|element| {
                    let live = self.form_checked.get(node).copied();
                    match element.tag_name.as_str() {
                        "input" => {
                            matches!(element.attributes.get("type"), Some("checkbox" | "radio"))
                                && live.unwrap_or_else(|| element.attributes.contains("checked"))
                        }
                        // Selected options match :checked so multiple
                        // selects can style their list rows.
                        "option" => {
                            live.unwrap_or_else(|| element.attributes.contains("selected"))
                        }
                        _ => false,
                    }
                })
            })
            .collect()
    }

    /// Link elements whose resolved href is in this session's history.
    fn visited_link_nodes(
        &self,
        document: &lumen_html::Document,
    ) -> std::collections::HashSet<NodeId> {
        let Some(base) = self
            .index
            .and_then(|index| self.history.get(index))
            .cloned()
        else {
            return std::collections::HashSet::new();
        };
        self.visited_link_nodes_against(document, &base)
    }

    /// Same, resolving hrefs against an explicit base URL.
    fn visited_link_nodes_against(
        &self,
        document: &lumen_html::Document,
        base: &Url,
    ) -> std::collections::HashSet<NodeId> {
        document
            .descendants(document.root())
            .filter(|node| {
                document.element(*node).is_some_and(|element| {
                    element.attributes.get("href").is_some_and(|href| {
                        resolve(base, href)
                            .map(|url| self.visited.contains(url.as_str()))
                            .unwrap_or(false)
                    })
                })
            })
            .collect()
    }

    /// Shared reaction to an interaction change: skip when the sheet has
    /// no interactive rules, or when neither the old nor the new state
    /// triggers one; repaint without relayout for paint-only rules.
    fn react_to_interaction_change(
        &mut self,
        old_state_matters: impl Fn(&lumen_html::Document, &lumen_css::Stylesheet) -> bool,
    ) -> bool {
        if self.hover_impact == lumen_engine::HoverImpact::Nothing {
            return false;
        }
        let affects = self.page.as_ref().is_some_and(|page| {
            let new_state = lumen_engine::InteractionState::new(
                &page.document,
                self.hovered,
                self.active,
                self.focused,
            );
            lumen_engine::interaction_styles_may_change(&page.document, &self.author, &new_state)
                || old_state_matters(&page.document, &self.author)
        });
        if !affects {
            return false;
        }
        let snapshot = if self.has_transitions {
            self.page.as_ref().map(|page| page.styles.by_node.clone())
        } else {
            None
        };
        let changed = match self.hover_impact {
            lumen_engine::HoverImpact::Nothing => false,
            lumen_engine::HoverImpact::PaintOnly => {
                let Some(interaction) = self.interaction() else {
                    return false;
                };
                match &mut self.page {
                    Some(page) => {
                        lumen_engine::repaint_page_interactive(page, &interaction);
                        true
                    }
                    None => false,
                }
            }
            lumen_engine::HoverImpact::Layout => {
                self.relayout();
                true
            }
        };
        if changed && let Some(old_styles) = snapshot {
            self.spawn_transitions(&old_styles);
        }
        changed
    }

    /// Compares pre/post-restyle styles and spawns transitions for the
    /// animatable paint-only properties (opacity, colors, transform)
    /// covered by each node's `transition` declarations.
    fn spawn_transitions(
        &mut self,
        old_styles: &std::collections::HashMap<NodeId, lumen_engine::ComputedStyle>,
    ) {
        let Some(page) = self.page.as_ref() else {
            return;
        };
        let mut spawned: Vec<ActiveTransition> = Vec::new();
        for (node, new_style) in &page.styles.by_node {
            let Some(old_style) = old_styles.get(node) else {
                continue;
            };
            for spec in &new_style.transitions {
                let wants = |name: &str| spec.property == "all" || spec.property == name;
                let mut push = |property: &'static str, from: AnimatedValue, to: AnimatedValue| {
                    spawned.push(ActiveTransition {
                        node: *node,
                        property,
                        from,
                        to,
                        start_ms: None,
                        duration_ms: f64::from(spec.duration) * 1000.0,
                        delay_ms: f64::from(spec.delay) * 1000.0,
                        ease: spec.ease,
                    });
                };
                if wants("opacity") && (old_style.opacity - new_style.opacity).abs() > f32::EPSILON
                {
                    push(
                        "opacity",
                        AnimatedValue::Number(old_style.opacity),
                        AnimatedValue::Number(new_style.opacity),
                    );
                }
                if wants("color") && old_style.color != new_style.color {
                    push(
                        "color",
                        AnimatedValue::Color(old_style.color),
                        AnimatedValue::Color(new_style.color),
                    );
                }
                if wants("background-color") {
                    let transparent = lumen_css::Color::rgba(0, 0, 0, 0);
                    let from = old_style.background_color.unwrap_or(transparent);
                    let to = new_style.background_color.unwrap_or(transparent);
                    if from != to {
                        push(
                            "background-color",
                            AnimatedValue::Color(from),
                            AnimatedValue::Color(to),
                        );
                    }
                }
                if wants("transform") {
                    let from = old_style
                        .transform
                        .unwrap_or(lumen_engine::Transform2D::IDENTITY);
                    let to = new_style
                        .transform
                        .unwrap_or(lumen_engine::Transform2D::IDENTITY);
                    if from != to {
                        push(
                            "transform",
                            AnimatedValue::Transform(from),
                            AnimatedValue::Transform(to),
                        );
                    }
                }
            }
        }
        // A new transition on the same node+property replaces the old one.
        for transition in spawned {
            self.transitions.retain(|existing| {
                !(existing.node == transition.node && existing.property == transition.property)
            });
            self.transitions.push(transition);
        }
    }

    /// Steps running transitions to `now_ms` (any monotonic clock),
    /// patching styles and repainting. Returns whether animation frames
    /// are still needed.
    pub fn tick(&mut self, now_ms: f64) -> bool {
        if self.transitions.is_empty() {
            return false;
        }
        let Some(page) = self.page.as_mut() else {
            self.transitions.clear();
            return false;
        };
        let mut any_active = false;
        for transition in &mut self.transitions {
            let start = *transition.start_ms.get_or_insert(now_ms);
            let progress = if transition.duration_ms <= 0.0 {
                1.0
            } else {
                (((now_ms - start - transition.delay_ms) / transition.duration_ms).clamp(0.0, 1.0))
                    as f32
            };
            let eased = if transition.ease {
                progress * progress * (3.0 - 2.0 * progress) // smoothstep ≈ ease
            } else {
                progress
            };
            if let Some(style) = page.styles.by_node.get_mut(&transition.node) {
                match (transition.from, transition.to) {
                    (AnimatedValue::Number(from), AnimatedValue::Number(to)) => {
                        style.opacity = from + (to - from) * eased;
                    }
                    (AnimatedValue::Color(from), AnimatedValue::Color(to)) => {
                        let value = lerp_color(from, to, eased);
                        match transition.property {
                            "color" => style.color = value,
                            _ => style.background_color = (value.a > 0).then_some(value),
                        }
                    }
                    (AnimatedValue::Transform(from), AnimatedValue::Transform(to)) => {
                        let value = lumen_engine::Transform2D::lerp(from, to, eased);
                        style.transform = (!value.is_identity()).then_some(value);
                    }
                    _ => {}
                }
            }
            if progress < 1.0 {
                any_active = true;
            }
        }
        lumen_engine::refresh_paint(page);
        self.transitions.retain(|transition| {
            transition.start_ms.is_none_or(|start| {
                ((now_ms - start - transition.delay_ms) / transition.duration_ms.max(0.001)) < 1.0
            })
        });
        any_active
    }

    /// The nearest `<a href>` at or above `node`, for link hit testing.
    #[must_use]
    pub fn link_target(&self, node: NodeId) -> Option<String> {
        let document = &self.page.as_ref()?.document;
        std::iter::once(node)
            .chain(document.ancestors(node))
            .find_map(|candidate| {
                let element = document.element(candidate)?;
                if element.tag_name == "a" {
                    element.attributes.get("href").map(str::to_string)
                } else {
                    None
                }
            })
    }

    fn relayout(&mut self) {
        // Reuse the page's document: it already carries materialized
        // ::before/::after nodes, so hover ids (which may point at
        // generated content) stay valid. Fall back to reparsing the
        // cached source when there is no page yet.
        let document = match self.page.take() {
            Some(page) => page.document,
            None => match &self.source {
                Some(source) => lumen_html::parse_document(source),
                None => return,
            },
        };
        let interaction =
            lumen_engine::InteractionState::new(&document, self.hovered, self.active, self.focused)
                .with_visited(self.visited_link_nodes(&document))
                .with_checked(self.checked_nodes(&document));
        self.page = Some(lumen_engine::page_from_document_interactive(
            document,
            self.author.clone(),
            self.images.clone(),
            self.viewport,
            self.effective_measurer(),
            &interaction,
        ));
    }

    /// Current per-element scroll offsets.
    #[must_use]
    pub fn scroll_offsets(&self) -> &std::collections::HashMap<NodeId, f32> {
        &self.scroll_offsets
    }

    /// Scrolls an `overflow: scroll/auto` element by `delta`, clamped to
    /// its content. Returns whether the offset changed (the display list
    /// is rebuilt in place — no relayout).
    pub fn scroll_inner(&mut self, node: NodeId, delta: f32) -> bool {
        let Some(page) = self.page.as_mut() else {
            return false;
        };
        let Some(target) = page.layout.find_by_node(node) else {
            return false;
        };
        let max = target.max_inner_scroll();
        let current = self.scroll_offsets.get(&node).copied().unwrap_or(0.0);
        let next = (current + delta).clamp(0.0, max);
        if (next - current).abs() < 0.5 {
            return false;
        }
        if next == 0.0 {
            self.scroll_offsets.remove(&node);
        } else {
            self.scroll_offsets.insert(node, next);
        }
        page.display_list = lumen_engine::build_display_list_scrolled(
            &page.layout,
            &page.images,
            &self.scroll_offsets,
        );
        true
    }

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
        let (action, pairs) = {
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
            (action, pairs)
        };
        let mut url = resolve(&base, &action)?;
        let query: String = pairs
            .iter()
            .map(|(name, value)| format!("{}={}", url_encode(name), url_encode(value)))
            .collect::<Vec<_>>()
            .join("&");
        url.set_query(if query.is_empty() { None } else { Some(&query) });
        self.load(url)
    }

    /// The page's own `@font-face` font, when one loaded.
    #[must_use]
    pub fn web_font(&self) -> Option<Arc<lumen_engine::SystemFont>> {
        self.web_font.clone()
    }

    /// The measurer layout runs with: the web font when one loaded, else
    /// the shell-provided measurer.
    fn effective_measurer(&self) -> &dyn TextMeasurer {
        match &self.web_font {
            Some(font) => font.as_ref(),
            None => self.measurer.as_ref(),
        }
    }

    #[must_use]
    pub fn page(&self) -> Option<&Page> {
        self.page.as_ref()
    }

    /// The text of the page's `<title>` element, when present.
    #[must_use]
    pub fn title(&self) -> Option<String> {
        let page = self.page()?;
        let document = &page.document;
        let title = document.descendants(document.root()).find(|id| {
            document
                .element(*id)
                .is_some_and(|element| element.tag_name == "title")
        })?;
        let text: String = document
            .children(title)
            .iter()
            .filter_map(|child| match &document.node(*child).kind {
                lumen_html::NodeKind::Text(text) => Some(text.as_str()),
                _ => None,
            })
            .collect();
        let text = text.trim().to_string();
        (!text.is_empty()).then_some(text)
    }

    pub fn current_url(&self) -> Option<&Url> {
        self.index.map(|index| &self.history[index])
    }

    #[must_use]
    pub fn can_go_back(&self) -> bool {
        self.index.is_some_and(|index| index > 0)
    }

    #[must_use]
    pub fn can_go_forward(&self) -> bool {
        self.index
            .is_some_and(|index| index + 1 < self.history.len())
    }

    fn require_current(&self) -> Result<Url, LoadError> {
        self.current_url()
            .cloned()
            .ok_or_else(|| LoadError::InvalidUrl("no page loaded".to_string()))
    }

    /// Fetches `url` and rebuilds the page. Returns the final URL after
    /// redirects (which is what history should record).
    fn fetch_and_render(&mut self, url: Url) -> Result<Url, LoadError> {
        let response = self.loader.load(&ResourceRequest { url })?;
        let source = response.text();
        let document = lumen_html::parse_document(&source);

        // External stylesheets: resolved against the final URL, fetched in
        // document order; failures skip that sheet without failing the page.
        let base = response.final_url.clone();
        let author_css = collect_author_css(&document, |href| {
            let url = resolve(&base, href).ok()?;
            self.loader
                .load(&ResourceRequest { url })
                .ok()
                .map(|response| response.text())
        });
        self.author = Arc::new(lumen_css::parse_stylesheet(&author_css));
        self.hover_impact = lumen_engine::hover_impact(&self.author);
        self.transitions.clear();
        self.has_transitions = self.author.rules.iter().any(|rule| {
            rule.declarations
                .iter()
                .any(|declaration| declaration.name == "transition")
        });

        // @font-face: fetch the first source fontdue can parse (ttf/otf;
        // woff/woff2 are skipped) and use it as the document font.
        self.web_font = None;
        'faces: for face in &self.author.font_faces {
            for (source, format) in &face.sources {
                let usable = match format.as_deref() {
                    Some("truetype" | "opentype") => true,
                    Some(_) => false,
                    None => {
                        let lower = source.to_ascii_lowercase();
                        lower.ends_with(".ttf") || lower.ends_with(".otf")
                    }
                };
                if !usable {
                    continue;
                }
                let Ok(url) = resolve(&base, source) else {
                    continue;
                };
                if let Ok(response) = self.loader.load(&ResourceRequest { url })
                    && let Some(font) = lumen_engine::SystemFont::from_bytes(&response.body)
                {
                    self.web_font = Some(Arc::new(font));
                    break 'faces;
                }
            }
        }

        // Images: fetched once per page; failures leave a placeholder box.
        // Capped so image-heavy pages cannot stall navigation for minutes.
        const MAX_IMAGES_PER_PAGE: usize = 32;
        let mut images = ImageMap::new();
        // CSS background images need computed styles to discover; this
        // extra style pass runs at load only.
        let mut sources = collect_image_sources(&document);
        let styles = lumen_engine::compute_styles(&document, &self.author);
        let mut backgrounds: Vec<(NodeId, String)> = styles
            .by_node
            .iter()
            .filter_map(|(node, style)| {
                style
                    .background_layers
                    .iter()
                    .find_map(|layer| match &layer.image {
                        lumen_engine::BackgroundImage::Url(src) => Some((*node, src.clone())),
                        _ => None,
                    })
            })
            .collect();
        backgrounds.sort_unstable();
        sources.extend(backgrounds);
        for (node, src) in sources.into_iter().take(MAX_IMAGES_PER_PAGE) {
            let Ok(url) = resolve(&base, &src) else {
                continue;
            };
            if let Ok(response) = self.loader.load(&ResourceRequest { url })
                && let Some(image) = RasterImage::decode(&response.body)
            {
                images.insert(node, Arc::new(image));
            }
        }
        self.images = Arc::new(images);

        self.hovered = None; // New document, new node ids.
        self.active = None;
        self.focused = None;
        self.transitions.clear();
        self.scroll_offsets.clear();
        self.form_values.clear();
        self.form_checked.clear();
        // Mark this page itself visited before building, so its own links
        // back to already-seen pages style immediately.
        self.visited.insert(response.final_url.to_string());
        let interaction = lumen_engine::InteractionState::default()
            .with_visited(self.visited_link_nodes_against(&document, &response.final_url))
            .with_checked(self.checked_nodes(&document));
        self.page = Some(lumen_engine::page_from_document_interactive(
            document,
            self.author.clone(),
            self.images.clone(),
            self.viewport,
            self.effective_measurer(),
            &interaction,
        ));
        self.source = Some(source);
        Ok(response.final_url)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lumen_platform::ResourceResponse;
    use std::cell::RefCell;
    use std::collections::HashMap;

    struct FakeLoader {
        pages: HashMap<String, Vec<u8>>,
        loads: RefCell<Vec<String>>,
    }

    impl FakeLoader {
        fn new(pages: &[(&str, &str)]) -> Self {
            Self {
                pages: pages
                    .iter()
                    .map(|(url, html)| ((*url).to_string(), html.as_bytes().to_vec()))
                    .collect(),
                loads: RefCell::new(Vec::new()),
            }
        }

        fn with_bytes(mut self, url: &str, bytes: &[u8]) -> Self {
            self.pages.insert(url.to_string(), bytes.to_vec());
            self
        }
    }

    impl ResourceLoader for FakeLoader {
        fn load(&self, request: &ResourceRequest) -> Result<ResourceResponse, LoadError> {
            self.loads.borrow_mut().push(request.url.to_string());
            let body = self
                .pages
                .get(request.url.as_str())
                .ok_or_else(|| LoadError::Http(format!("404: {}", request.url)))?;
            Ok(ResourceResponse {
                final_url: request.url.clone(),
                content_type: None,
                body: body.clone(),
            })
        }
    }

    /// Encodes a 1×1 red PNG in memory.
    fn red_pixel_png() -> Vec<u8> {
        let mut bytes = std::io::Cursor::new(Vec::new());
        image::RgbaImage::from_pixel(1, 1, image::Rgba([255, 0, 0, 255]))
            .write_to(&mut bytes, image::ImageFormat::Png)
            .expect("in-memory png encode");
        bytes.into_inner()
    }

    const VIEWPORT: Size = Size {
        width: 800.0,
        height: 600.0,
    };

    fn url(value: &str) -> Url {
        Url::parse(value).unwrap()
    }

    fn session() -> Session<FakeLoader> {
        Session::new(
            FakeLoader::new(&[
                ("https://a.test/", "<h1>A</h1>"),
                ("https://a.test/two", "<h1>B</h1>"),
                ("https://a.test/three", "<h1>C</h1>"),
            ]),
            VIEWPORT,
        )
    }

    fn heading(page: &Page) -> String {
        page.document.text_content(page.document.root())
    }

    #[test]
    fn load_renders_and_records_history() {
        let mut session = session();
        let page = session.load(url("https://a.test/")).unwrap();
        assert_eq!(heading(page), "A");
        assert_eq!(session.current_url().unwrap().as_str(), "https://a.test/");
        assert!(!session.can_go_back());
    }

    #[test]
    fn back_and_forward_walk_history() {
        let mut session = session();
        session.load(url("https://a.test/")).unwrap();
        session.load(url("https://a.test/two")).unwrap();
        assert!(session.can_go_back());

        let page = session.back().unwrap();
        assert_eq!(heading(page), "A");
        assert!(session.can_go_forward());

        let page = session.forward().unwrap();
        assert_eq!(heading(page), "B");
        assert!(!session.can_go_forward());
    }

    #[test]
    fn navigating_after_back_drops_forward_entries() {
        let mut session = session();
        session.load(url("https://a.test/")).unwrap();
        session.load(url("https://a.test/two")).unwrap();
        session.back().unwrap();
        session.load(url("https://a.test/three")).unwrap();
        assert!(!session.can_go_forward());
        assert!(session.back().is_ok());
        assert_eq!(session.current_url().unwrap().as_str(), "https://a.test/");
    }

    #[test]
    fn refresh_refetches_current_url() {
        let mut session = session();
        session.load(url("https://a.test/")).unwrap();
        session.refresh().unwrap();
        assert_eq!(
            *session.loader.loads.borrow(),
            vec!["https://a.test/", "https://a.test/"]
        );
    }

    #[test]
    fn follow_resolves_relative_links() {
        let mut session = session();
        session.load(url("https://a.test/")).unwrap();
        let page = session.follow("two").unwrap();
        assert_eq!(heading(page), "B");
        assert_eq!(
            session.current_url().unwrap().as_str(),
            "https://a.test/two"
        );
    }

    #[test]
    fn errors_do_not_corrupt_history() {
        let mut session = session();
        session.load(url("https://a.test/")).unwrap();
        assert!(session.load(url("https://a.test/missing")).is_err());
        // The failed load did not become the current entry.
        assert_eq!(session.current_url().unwrap().as_str(), "https://a.test/");
        assert!(session.back().is_err());
    }

    #[test]
    fn hover_restyles_locally_and_reports_changes() {
        let mut session = Session::new(
            FakeLoader::new(&[(
                "https://a.test/",
                "<style>p:hover { color: #ff0000; }</style><p>hi</p>",
            )]),
            VIEWPORT,
        );
        session.load(url("https://a.test/")).unwrap();
        let page = session.page().unwrap();
        let p = page
            .document
            .descendants(page.document.root())
            .find(|id| {
                page.document
                    .element(*id)
                    .is_some_and(|element| element.tag_name == "p")
            })
            .unwrap();

        assert!(session.set_hovered(Some(p)));
        assert!(!session.set_hovered(Some(p))); // unchanged
        let hovered_style = &session.page().unwrap().styles.by_node[&p];
        assert_eq!(hovered_style.color, lumen_css::Color::rgb(255, 0, 0));
        assert!(session.set_hovered(None));
        // Initial load only; hover never touched the network.
        assert_eq!(session.loader.loads.borrow().len(), 1);
    }

    #[test]
    fn link_target_finds_nearest_anchor() {
        let mut session = Session::new(
            FakeLoader::new(&[(
                "https://a.test/",
                "<div><a href='two'><span>go</span></a></div>",
            )]),
            VIEWPORT,
        );
        session.load(url("https://a.test/")).unwrap();
        let page = session.page().unwrap();
        let span = page
            .document
            .descendants(page.document.root())
            .find(|id| {
                page.document
                    .element(*id)
                    .is_some_and(|element| element.tag_name == "span")
            })
            .unwrap();
        assert_eq!(session.link_target(span).as_deref(), Some("two"));
        let div = page
            .document
            .descendants(page.document.root())
            .find(|id| {
                page.document
                    .element(*id)
                    .is_some_and(|element| element.tag_name == "div")
            })
            .unwrap();
        assert_eq!(session.link_target(div), None);
    }

    #[test]
    fn external_stylesheets_load_resolve_and_apply() {
        let mut session = Session::new(
            FakeLoader::new(&[
                (
                    "https://a.test/docs/page",
                    "<link rel='stylesheet' href='theme.css'>\
                     <link rel=\"STYLESHEET\" href=\"/root.css\">\
                     <link rel='icon' href='favicon.ico'>\
                     <p>hi</p>",
                ),
                ("https://a.test/docs/theme.css", "p { color: #ff0000; }"),
                ("https://a.test/root.css", "p { font-size: 20px; }"),
            ]),
            VIEWPORT,
        );
        session.load(url("https://a.test/docs/page")).unwrap();
        let page = session.page().unwrap();
        let p = page
            .document
            .descendants(page.document.root())
            .find(|id| {
                page.document
                    .element(*id)
                    .is_some_and(|element| element.tag_name == "p")
            })
            .unwrap();
        let style = &page.styles.by_node[&p];
        assert_eq!(style.color, lumen_css::Color::rgb(255, 0, 0));
        assert_eq!(style.font_size, 20.0);
        // Page + 2 stylesheets; the icon link was not fetched.
        assert_eq!(session.loader.loads.borrow().len(), 3);
    }

    #[test]
    fn missing_external_stylesheet_does_not_break_the_page() {
        let mut session = Session::new(
            FakeLoader::new(&[(
                "https://a.test/",
                "<link rel='stylesheet' href='gone.css'>\
                 <style>p { color: #00ff00; }</style><p>hi</p>",
            )]),
            VIEWPORT,
        );
        session.load(url("https://a.test/")).unwrap();
        let page = session.page().unwrap();
        assert!(
            page.document
                .text_content(page.document.root())
                .contains("hi")
        );
        let p = page
            .document
            .descendants(page.document.root())
            .find(|id| {
                page.document
                    .element(*id)
                    .is_some_and(|element| element.tag_name == "p")
            })
            .unwrap();
        assert_eq!(
            page.styles.by_node[&p].color,
            lumen_css::Color::rgb(0, 255, 0)
        );
    }

    #[test]
    fn hover_and_resize_do_not_refetch_external_css() {
        let mut session = Session::new(
            FakeLoader::new(&[
                (
                    "https://a.test/",
                    "<link rel='stylesheet' href='a.css'><p>hi</p>",
                ),
                ("https://a.test/a.css", "p { color: red; }"),
            ]),
            VIEWPORT,
        );
        session.load(url("https://a.test/")).unwrap();
        session.set_viewport(Size {
            width: 500.0,
            height: 400.0,
        });
        session.set_hovered(Some(1));
        assert_eq!(session.loader.loads.borrow().len(), 2);
    }

    #[test]
    fn layout_hover_rules_skip_relayout_for_unrelated_targets() {
        let mut session = Session::new(
            FakeLoader::new(&[(
                "https://a.test/",
                "<style>a:hover { padding: 8px; }</style>\
                 <p>unrelated paragraph</p><a href='/x'>link</a>",
            )]),
            VIEWPORT,
        );
        session.load(url("https://a.test/")).unwrap();
        let document = &session.page().unwrap().document;
        let find_tag = |tag: &str| {
            document
                .descendants(document.root())
                .find(|id| {
                    document
                        .element(*id)
                        .is_some_and(|element| element.tag_name == tag)
                })
                .unwrap()
        };
        let (paragraph, anchor) = (find_tag("p"), find_tag("a"));
        // Hovering something no hover rule involves: no restyle at all.
        assert!(!session.set_hovered(Some(paragraph)));
        // Hovering the link: the padding rule fires, full relayout.
        assert!(session.set_hovered(Some(anchor)));
        let hovered_width = session
            .page()
            .unwrap()
            .layout
            .find_by_node(anchor)
            .map(|laid| laid.dimensions.padding.top);
        assert_eq!(hovered_width, Some(8.0));
    }

    #[test]
    fn form_values_edit_and_submit_as_get_query() {
        let mut session = Session::new(
            FakeLoader::new(&[
                (
                    "https://a.test/",
                    "<form action='/search'>\
                     <input type='text' name='q' value=''>\
                     <input type='hidden' name='hl' value='tr'>\
                     <input type='checkbox' name='safe' value='1' checked>\
                     <input type='submit' value='Go'></form>",
                ),
                (
                    "https://a.test/search?q=hello+w%26rld&hl=tr&safe=1",
                    "<p>results</p>",
                ),
            ]),
            VIEWPORT,
        );
        session.load(url("https://a.test/")).unwrap();
        let document = &session.page().unwrap().document;
        let field = document
            .descendants(document.root())
            .find(|id| {
                document
                    .element(*id)
                    .is_some_and(|element| element.attributes.get("name") == Some("q"))
            })
            .unwrap();
        session.set_form_value(field, "hello w&rld");
        assert_eq!(session.form_value(field), "hello w&rld");
        session.submit_form(field).unwrap();
        assert_eq!(
            session.current_url().unwrap().as_str(),
            "https://a.test/search?q=hello+w%26rld&hl=tr&safe=1"
        );
    }

    #[test]
    fn selects_textareas_and_ranges_submit_their_values() {
        let mut session = Session::new(
            FakeLoader::new(&[
                (
                    "https://a.test/",
                    "<form action='/go'>\
                     <select name='renk'><option value='r'>Kirmizi</option>\
                     <option value='b' selected>Mavi</option></select>\
                     <textarea name='not'>merhaba</textarea>\
                     <input type='range' name='ses' min='0' max='10' value='5'>\
                     </form>",
                ),
                ("https://a.test/go?renk=r&not=yeni+not&ses=8", "<p>ok</p>"),
            ]),
            VIEWPORT,
        );
        session.load(url("https://a.test/")).unwrap();
        let document = &session.page().unwrap().document;
        let find_tag = |tag: &str| {
            document
                .descendants(document.root())
                .find(|id| {
                    document
                        .element(*id)
                        .is_some_and(|element| element.tag_name == tag)
                })
                .unwrap()
        };
        let (select, textarea) = (find_tag("select"), find_tag("textarea"));
        let range = document
            .descendants(document.root())
            .find(|id| {
                document
                    .element(*id)
                    .is_some_and(|element| element.attributes.get("type") == Some("range"))
            })
            .unwrap();
        // Initially the selected attr wins; the label shows.
        let (options, selected) = session.select_options(select);
        assert_eq!(selected, 1);
        assert_eq!(options[0].1, "Kirmizi");
        session.set_selected_option(select, 0);
        assert_eq!(session.select_options(select).1, 0);
        session.set_form_value(textarea, "yeni not");
        assert_eq!(session.form_value(textarea), "yeni not");
        session.set_range_fraction(range, 0.8);
        session.submit_form(select).unwrap();
        assert_eq!(
            session.current_url().unwrap().as_str(),
            "https://a.test/go?renk=r&not=yeni+not&ses=8"
        );
    }

    #[test]
    fn interaction_state_resets_across_navigation() {
        let mut session = Session::new(
            FakeLoader::new(&[
                (
                    "https://a.test/",
                    "<form action='/next'><input type='text' name='q'>\
                     <p>filler filler filler filler filler filler filler</p>\
                     <p>more filler to raise node counts</p></form>",
                ),
                ("https://a.test/next?q=", "<p>tiny</p>"),
            ]),
            VIEWPORT,
        );
        session.load(url("https://a.test/")).unwrap();
        let document = &session.page().unwrap().document;
        let field = document
            .descendants(document.root())
            .find(|id| {
                document
                    .element(*id)
                    .is_some_and(|element| element.tag_name == "input")
            })
            .unwrap();
        // Focus + press a high-id node, then navigate to a smaller page:
        // stale ids must not be walked on the new document.
        session.set_focused(Some(field));
        session.set_active(Some(field));
        session.submit_form(field).unwrap();
        session.set_viewport(Size {
            width: 500.0,
            height: 400.0,
        });
        assert!(session.page().is_some());
    }

    #[test]
    fn typed_values_render_in_the_display_list() {
        let mut session = Session::new(
            FakeLoader::new(&[(
                "https://a.test/",
                "<form><input type='text' name='q' placeholder='ara'></form>",
            )]),
            VIEWPORT,
        );
        session.load(url("https://a.test/")).unwrap();
        let document = &session.page().unwrap().document;
        let field = document
            .descendants(document.root())
            .find(|id| {
                document
                    .element(*id)
                    .is_some_and(|element| element.tag_name == "input")
            })
            .unwrap();
        let texts = |session: &Session<FakeLoader>| -> Vec<String> {
            session
                .page()
                .unwrap()
                .display_list
                .iter()
                .filter_map(|command| match command {
                    lumen_engine::DisplayCommand::DrawText { text, .. } => Some(text.clone()),
                    _ => None,
                })
                .collect()
        };
        assert!(texts(&session).iter().any(|text| text.contains("ara")));
        session.set_form_value(field, "merhaba");
        let after = texts(&session);
        assert!(
            after.iter().any(|text| text.contains("merhaba")),
            "typed value missing: {after:?}"
        );
        assert!(
            !after
                .iter()
                .any(|text| text.contains("ara") && !text.contains("merhaba"))
        );
    }

    #[test]
    fn emptied_input_without_placeholder_keeps_its_height() {
        let mut session = Session::new(
            FakeLoader::new(&[(
                "https://a.test/",
                "<form><input type='email' name='e' value='a@b.c'></form>",
            )]),
            VIEWPORT,
        );
        session.load(url("https://a.test/")).unwrap();
        let document = &session.page().unwrap().document;
        let field = document
            .descendants(document.root())
            .find(|id| {
                document
                    .element(*id)
                    .is_some_and(|element| element.tag_name == "input")
            })
            .unwrap();
        let height = |session: &Session<FakeLoader>| {
            session
                .page()
                .unwrap()
                .layout
                .find_by_node(field)
                .unwrap()
                .content_box()
                .height
        };
        let before = height(&session);
        session.set_form_value(field, "");
        assert!(
            (height(&session) - before).abs() < 0.5,
            "emptied input shrank: {} -> {}",
            before,
            height(&session)
        );
    }

    #[test]
    fn input_values_keep_consecutive_spaces() {
        // `white-space: pre` on inputs: runs of spaces must render as
        // typed, or the shell's caret math drifts right of the text.
        let mut session = Session::new(
            FakeLoader::new(&[(
                "https://a.test/",
                "<form><input type='text' name='q'></form>",
            )]),
            VIEWPORT,
        );
        session.load(url("https://a.test/")).unwrap();
        let document = &session.page().unwrap().document;
        let field = document
            .descendants(document.root())
            .find(|id| {
                document
                    .element(*id)
                    .is_some_and(|element| element.tag_name == "input")
            })
            .unwrap();
        session.set_form_value(field, "a   b");
        let rendered = session
            .page()
            .unwrap()
            .display_list
            .iter()
            .any(|command| matches!(command,
                lumen_engine::DisplayCommand::DrawText { text, .. } if text == "a   b"));
        assert!(rendered, "spaces collapsed in the rendered input value");
    }

    #[test]
    fn multiple_select_toggles_and_submits_every_selection() {
        let mut session = Session::new(
            FakeLoader::new(&[
                (
                    "https://a.test/",
                    "<form action='/go'><select name='fruit' multiple>\
                     <optgroup label='Soft'><option value='banana' selected>Banana</option>\
                     <option value='berry'>Berry</option></optgroup>\
                     <option value='apple'>Apple</option></select></form>",
                ),
                ("https://a.test/go?fruit=banana&fruit=apple", "<p>ok</p>"),
            ]),
            VIEWPORT,
        );
        session.load(url("https://a.test/")).unwrap();
        let document = &session.page().unwrap().document;
        let by_tag = |tag: &str| {
            document
                .descendants(document.root())
                .find(|id| {
                    document
                        .element(*id)
                        .is_some_and(|element| element.tag_name == tag)
                })
                .unwrap()
        };
        let select = by_tag("select");
        let apple = document
            .descendants(select)
            .find(|id| {
                document
                    .element(*id)
                    .is_some_and(|element| element.attributes.get("value") == Some("apple"))
            })
            .unwrap();
        assert!(session.is_multiple_select(select));
        // The optgroup option starts selected; a toggling click adds apple.
        session.click_option(select, apple, true);
        assert!(session.option_selected(apple));
        session.submit_form(select).unwrap();
        assert_eq!(
            session.current_url().unwrap().as_str(),
            "https://a.test/go?fruit=banana&fruit=apple"
        );
    }

    #[test]
    fn number_input_steps_within_bounds() {
        let mut session = Session::new(
            FakeLoader::new(&[(
                "https://a.test/",
                "<form><input type='number' name='n' value='2' min='0' max='3' step='2'></form>",
            )]),
            VIEWPORT,
        );
        session.load(url("https://a.test/")).unwrap();
        let document = &session.page().unwrap().document;
        let field = document
            .descendants(document.root())
            .find(|id| {
                document
                    .element(*id)
                    .is_some_and(|element| element.tag_name == "input")
            })
            .unwrap();
        assert!(session.is_number_input(field));
        assert_eq!(session.step_number_input(field, 1.0).as_deref(), Some("3"));
        assert_eq!(session.step_number_input(field, -1.0).as_deref(), Some("1"));
        assert_eq!(session.step_number_input(field, -1.0).as_deref(), Some("0"));
        assert_eq!(session.form_value(field), "0");
    }

    #[test]
    fn color_value_drives_the_swatch_background() {
        let mut session = Session::new(
            FakeLoader::new(&[(
                "https://a.test/",
                "<form><input type='color' name='c' value='#ff0000'></form>",
            )]),
            VIEWPORT,
        );
        session.load(url("https://a.test/")).unwrap();
        let document = &session.page().unwrap().document;
        let field = document
            .descendants(document.root())
            .find(|id| {
                document
                    .element(*id)
                    .is_some_and(|element| element.tag_name == "input")
            })
            .unwrap();
        let background = |session: &Session<FakeLoader>| {
            session
                .page()
                .unwrap()
                .styles
                .by_node
                .get(&field)
                .and_then(|style| style.background_color)
                .unwrap()
        };
        assert_eq!(background(&session), lumen_css::Color::rgb(0xff, 0, 0));
        session.set_color_value(field, "#2266aa");
        assert_eq!(background(&session), lumen_css::Color::rgb(0x22, 0x66, 0xaa));
    }

    #[test]
    fn checkables_toggle_and_radios_group() {
        let mut session = Session::new(
            FakeLoader::new(&[(
                "https://a.test/",
                "<form><input type='checkbox' name='c'>\
                 <input type='radio' name='r' value='1'>\
                 <input type='radio' name='r' value='2' checked></form>",
            )]),
            VIEWPORT,
        );
        session.load(url("https://a.test/")).unwrap();
        let document = &session.page().unwrap().document;
        let inputs: Vec<_> = document
            .descendants(document.root())
            .filter(|id| {
                document
                    .element(*id)
                    .is_some_and(|element| element.tag_name == "input")
            })
            .collect();
        let (check, radio1, radio2) = (inputs[0], inputs[1], inputs[2]);
        assert!(!session.is_checked(check));
        assert!(session.toggle_checkable(check));
        assert!(session.is_checked(check));
        assert!(session.is_checked(radio2));
        assert!(session.toggle_checkable(radio1));
        assert!(session.is_checked(radio1));
        assert!(!session.is_checked(radio2));
    }

    #[test]
    fn visited_links_match_after_navigation() {
        let mut session = Session::new(
            FakeLoader::new(&[
                (
                    "https://a.test/",
                    "<style>a:visited { color: #800080; }</style>\
                     <a href='/there'>go</a>",
                ),
                ("https://a.test/there", "<a href='/'>back</a>"),
            ]),
            VIEWPORT,
        );
        session.load(url("https://a.test/")).unwrap();
        session.follow("/there").unwrap();
        session.follow("/").unwrap();
        // The link to /there is now visited: purple.
        let color = session
            .page()
            .unwrap()
            .display_list
            .iter()
            .find_map(|command| match command {
                lumen_engine::DisplayCommand::DrawText { color, .. } => Some(color.to_string()),
                _ => None,
            })
            .unwrap();
        assert_eq!(color, "#800080");
    }

    #[test]
    fn transitions_interpolate_hover_colors_over_time() {
        let mut session = Session::new(
            FakeLoader::new(&[(
                "https://a.test/",
                "<style>a { color: #000000; transition: color 1s linear; }\
                 a:hover { color: #ffffff; }</style><a href='/x'>fade</a>",
            )]),
            VIEWPORT,
        );
        session.load(url("https://a.test/")).unwrap();
        let document = &session.page().unwrap().document;
        let anchor = document
            .descendants(document.root())
            .find(|id| {
                document
                    .element(*id)
                    .is_some_and(|element| element.tag_name == "a")
            })
            .unwrap();
        let text_red = |session: &Session<FakeLoader>| {
            session
                .page()
                .unwrap()
                .display_list
                .iter()
                .find_map(|command| match command {
                    lumen_engine::DisplayCommand::DrawText { color, .. } => Some(color.r),
                    _ => None,
                })
                .unwrap()
        };
        assert!(session.set_hovered(Some(anchor)));
        // First tick anchors the clock at the old value...
        assert!(session.tick(0.0));
        assert_eq!(text_red(&session), 0);
        // ...halfway through it is mid-gray...
        assert!(session.tick(500.0));
        let mid = text_red(&session);
        assert!((100..=155).contains(&mid), "midpoint: {mid}");
        // ...and it finishes at white with no frames left.
        assert!(!session.tick(1100.0));
        assert_eq!(text_red(&session), 255);
    }

    #[test]
    fn inner_scroll_shifts_clipped_content() {
        let mut session = Session::new(
            FakeLoader::new(&[(
                "https://a.test/",
                "<style>.s { overflow: scroll; height: 50px; }\
                 .tall { height: 200px; background-color: #ff0000; }</style>\
                 <div class='s'><div class='tall'></div></div>",
            )]),
            VIEWPORT,
        );
        session.load(url("https://a.test/")).unwrap();
        let document = &session.page().unwrap().document;
        let scroller = document
            .descendants(document.root())
            .find(|id| {
                document
                    .element(*id)
                    .is_some_and(|element| element.has_class("s"))
            })
            .unwrap();
        let red_y = |session: &Session<FakeLoader>| {
            session
                .page()
                .unwrap()
                .display_list
                .iter()
                .find_map(|command| match command {
                    lumen_engine::DisplayCommand::FillRect { rect, color, .. }
                        if color.r == 255 =>
                    {
                        Some(rect.y)
                    }
                    _ => None,
                })
                .unwrap()
        };
        let before = red_y(&session);
        assert!(session.scroll_inner(scroller, 30.0));
        assert_eq!(red_y(&session), before - 30.0);
        // Clamped at the content extent (200 - 50 = 150).
        assert!(session.scroll_inner(scroller, 1000.0));
        assert_eq!(red_y(&session), before - 150.0);
        assert!(!session.scroll_inner(scroller, 10.0));
    }

    #[test]
    fn active_and_focus_restyle_their_targets() {
        let mut session = Session::new(
            FakeLoader::new(&[(
                "https://a.test/",
                "<style>a:active { color: #ff0000; } a:focus { color: #00ff00; }</style>\
                 <a href='/x'>press me</a>",
            )]),
            VIEWPORT,
        );
        session.load(url("https://a.test/")).unwrap();
        let document = &session.page().unwrap().document;
        let anchor = document
            .descendants(document.root())
            .find(|id| {
                document
                    .element(*id)
                    .is_some_and(|element| element.tag_name == "a")
            })
            .unwrap();
        let text_color = |session: &Session<FakeLoader>| {
            session
                .page()
                .unwrap()
                .display_list
                .iter()
                .find_map(|command| match command {
                    lumen_engine::DisplayCommand::DrawText { color, .. } => Some(color.to_string()),
                    _ => None,
                })
                .unwrap()
        };
        assert!(session.set_active(Some(anchor)));
        assert_eq!(text_color(&session), "#ff0000");
        assert!(session.set_active(None));
        assert!(session.set_focused(Some(anchor)));
        assert_eq!(text_color(&session), "#00ff00");
    }

    #[test]
    fn paint_only_hover_repaints_without_relayout() {
        let mut session = Session::new(
            FakeLoader::new(&[(
                "https://a.test/",
                "<style>p { color: #111111; } p:hover { color: #ff0000; }</style>\
                 <p>hover target</p>",
            )]),
            VIEWPORT,
        );
        session.load(url("https://a.test/")).unwrap();
        let paragraph = session
            .page()
            .unwrap()
            .document
            .descendants(session.page().unwrap().document.root())
            .find(|id| {
                session
                    .page()
                    .unwrap()
                    .document
                    .element(*id)
                    .is_some_and(|element| element.tag_name == "p")
            })
            .unwrap();
        let layout_before = session.page().unwrap().layout.clone();
        assert!(session.set_hovered(Some(paragraph)));
        let page = session.page().unwrap();
        // Geometry identical (styles inside differ, boxes do not move).
        assert_eq!(
            page.layout.children[0].border_box(),
            layout_before.children[0].border_box()
        );
        // The repaint shows the hover color.
        let hovered_text = page.display_list.iter().find_map(|command| match command {
            lumen_engine::DisplayCommand::DrawText { color, .. } => Some(color.to_string()),
            _ => None,
        });
        assert_eq!(hovered_text.as_deref(), Some("#ff0000"));
    }

    #[test]
    fn paint_only_hover_on_unrelated_element_is_free() {
        let mut session = Session::new(
            FakeLoader::new(&[(
                "https://a.test/",
                "<style>a:hover { color: #ff0000; }</style>\
                 <p>plain text</p><a href='/x'>link</a>",
            )]),
            VIEWPORT,
        );
        session.load(url("https://a.test/")).unwrap();
        let document = &session.page().unwrap().document;
        let paragraph = document
            .descendants(document.root())
            .find(|id| {
                document
                    .element(*id)
                    .is_some_and(|element| element.tag_name == "p")
            })
            .unwrap();
        // The paragraph triggers no hover rule: no restyle, no redraw.
        assert!(!session.set_hovered(Some(paragraph)));
    }

    #[test]
    fn hover_without_hover_rules_is_free() {
        let mut session = Session::new(
            FakeLoader::new(&[("https://a.test/", "<p>plain</p>")]),
            VIEWPORT,
        );
        session.load(url("https://a.test/")).unwrap();
        let node = session.page().unwrap().layout.children[0].node_id;
        // Reports "nothing changed": no restyle, no repaint needed.
        assert!(!session.set_hovered(Some(node)));
    }

    #[test]
    fn hovering_generated_content_does_not_panic() {
        let mut session = Session::new(
            FakeLoader::new(&[(
                "https://a.test/",
                "<style>p::before { content: \"* \"; } p:hover { color: #ff0000; }</style>\
                 <p>hover me</p>",
            )]),
            VIEWPORT,
        );
        session.load(url("https://a.test/")).unwrap();
        // The generated node has the highest id — exactly what a hit test
        // over the ::before text would return.
        let generated = session.page().unwrap().document.nodes().len() - 1;
        assert!(session.set_hovered(Some(generated)));
        assert!(session.page().is_some());
        // And a viewport change (full relayout) with the hover still set.
        session.set_viewport(Size {
            width: 640.0,
            height: 480.0,
        });
        assert!(session.page().is_some());
    }

    #[test]
    fn media_queries_restyle_on_viewport_change() {
        let mut session = Session::new(
            FakeLoader::new(&[(
                "https://a.test/",
                "<style>div { height: 10px; background-color: #111111; }\
                 @media (max-width: 600px) { div { background-color: #222222; } }</style>\
                 <div></div>",
            )]),
            VIEWPORT,
        );
        session.load(url("https://a.test/")).unwrap();
        let fill_colors = |session: &Session<FakeLoader>| -> Vec<String> {
            session
                .page()
                .unwrap()
                .display_list
                .iter()
                .filter_map(|command| match command {
                    lumen_engine::DisplayCommand::FillRect { color, .. } => Some(color.to_string()),
                    _ => None,
                })
                .collect()
        };
        assert!(fill_colors(&session).contains(&"#111111".to_string()));
        session.set_viewport(Size {
            width: 500.0,
            height: 600.0,
        });
        assert!(fill_colors(&session).contains(&"#222222".to_string()));
    }

    #[test]
    fn title_comes_from_the_title_element() {
        let mut session = Session::new(
            FakeLoader::new(&[(
                "https://a.test/",
                "<html><head><title>  My Page </title></head><body>x</body></html>",
            )]),
            VIEWPORT,
        );
        session.load(url("https://a.test/")).unwrap();
        assert_eq!(session.title().as_deref(), Some("My Page"));
    }

    #[test]
    fn images_load_decode_and_lay_out() {
        let mut session = Session::new(
            FakeLoader::new(&[("https://a.test/", "<img src='red.png' width='40'>")])
                .with_bytes("https://a.test/red.png", &red_pixel_png()),
            VIEWPORT,
        );
        session.load(url("https://a.test/")).unwrap();
        let page = session.page().unwrap();
        assert_eq!(page.images.len(), 1);
        let image_command = page.display_list.iter().find_map(|command| match command {
            lumen_engine::DisplayCommand::DrawImage { rect, image, .. } => {
                Some((rect.width, rect.height, image.width))
            }
            _ => None,
        });
        // width attr 40, square intrinsic ratio → 40x40.
        assert_eq!(image_command, Some((40.0, 40.0, 1)));
        // The SVG backend embeds the original bytes as a data URI.
        let svg = lumen_engine::render_svg(page);
        assert!(svg.contains("data:image/png;base64,"));
    }

    #[test]
    fn broken_image_leaves_page_intact() {
        let mut session = Session::new(
            FakeLoader::new(&[("https://a.test/", "<img src='gone.png'><p>still here</p>")]),
            VIEWPORT,
        );
        session.load(url("https://a.test/")).unwrap();
        let page = session.page().unwrap();
        assert!(page.images.is_empty());
        assert!(
            page.document
                .text_content(page.document.root())
                .contains("still here")
        );
    }

    #[test]
    fn viewport_change_relayouts_without_refetching() {
        let mut session = session();
        session.load(url("https://a.test/")).unwrap();
        session.set_viewport(Size {
            width: 400.0,
            height: 300.0,
        });
        assert_eq!(session.page().unwrap().viewport.width, 400.0);
        // Only the initial load hit the loader.
        assert_eq!(session.loader.loads.borrow().len(), 1);
    }
}
