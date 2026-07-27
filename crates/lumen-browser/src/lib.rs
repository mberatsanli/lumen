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
use lumen_platform::loader::{CorsGrant, CorsPreflight};
use lumen_platform::{LoadError, ResourceLoader, ResourceRequest, ResourceResponse, Url, resolve};

pub use editor::{EditOp, EditOverlay, EditResult, Motion, TextBuffer};
pub use forms::FormViolation;
pub use network::NetworkQueue;
pub use scripting::{DispatchOutcome, PageScripts};
use std::sync::Arc;

mod cookies;
mod editor;
mod forms;
mod network;
mod scripting;
mod storage;

/// What the session's most recent page mutation did to the painted
/// output — drives the shell's choice between a full re-raster and a
/// cheap damage-region update.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum RepaintDamage {
    /// Everything must be re-rasterized (navigation, relayout, script or
    /// animation changes, or simply no information).
    Full,
    /// A paint-only interaction restyle ran since the last full repaint:
    /// only the union of these page-space rects can differ on screen.
    /// `None` means the restyle changed nothing visible — the cached
    /// frame is still pixel-exact.
    Region(Option<lumen_engine::Rect>),
}

/// The rect two optional damage rects span together (`None` = no damage).
fn union_damage(
    a: Option<lumen_engine::Rect>,
    b: Option<lumen_engine::Rect>,
) -> Option<lumen_engine::Rect> {
    match (a, b) {
        (Some(a), Some(b)) => {
            let x0 = a.x.min(b.x);
            let y0 = a.y.min(b.y);
            let x1 = (a.x + a.width).max(b.x + b.width);
            let y1 = (a.y + a.height).max(b.y + b.height);
            Some(lumen_engine::Rect {
                x: x0,
                y: y0,
                width: x1 - x0,
                height: y1 - y0,
            })
        }
        (a, b) => a.or(b),
    }
}

/// One browsing context with linear history.
///
/// History entries are re-fetched on `back`/`forward`/`refresh`; there is
/// no document cache yet. All responses are treated as HTML.
pub struct Session<L: ResourceLoader> {
    /// Behind an `Arc` so script-initiated fetches can hand a shared,
    /// read-only handle to network workers while the session itself
    /// stays on its own thread.
    loader: Arc<L>,
    viewport: Size,
    measurer: Box<dyn TextMeasurer + Send>,
    history: Vec<Url>,
    /// `history.pushState` state (JSON) per history entry, kept
    /// parallel to `history` — `None` for plain navigations.
    history_states: Vec<Option<String>>,
    /// Index of the current entry in `history`, if any page is loaded.
    index: Option<usize>,
    pub(crate) page: Option<Page>,
    /// HTML source of the current page, kept so viewport or measurer
    /// changes can relayout locally without hitting the network.
    source: Option<String>,
    /// Author stylesheet (embedded + external), fetched once per page and
    /// shared with relayouts through the `Arc`.
    author: Arc<lumen_css::Stylesheet>,
    /// Decoded images, fetched once per page, shared the same way.
    images: Arc<ImageMap>,
    /// Image subresources discovered during the last page load, to be
    /// fetched progressively by the shell (or synchronously by CLI).
    pending_images: Vec<(NodeId, ResourceRequest)>,
    /// Increments on every navigation: progressive image results are
    /// only applied when they still belong to the current page.
    nav_serial: u64,
    /// Node currently under the pointer, for `:hover` styling.
    hovered: Option<NodeId>,
    /// Node the pointer is pressed on (`:active`).
    active: Option<NodeId>,
    /// Focused node (`:focus`) — the shell decides what focus means.
    focused: Option<NodeId>,
    /// Live form control values (overriding the parsed attributes).
    pub(crate) form_values: std::collections::HashMap<NodeId, String>,
    /// Live checkbox/radio state.
    pub(crate) form_checked: std::collections::HashMap<NodeId, bool>,
    /// The constraint violation that blocked the last submit attempt.
    pub(crate) form_violation: Option<FormViolation>,
    /// Final URLs of visited pages this session (drives `:visited`).
    visited: std::collections::HashSet<String>,
    /// Running property transitions, stepped by [`Session::tick`].
    transitions: Vec<ActiveTransition>,
    /// Declared `@keyframes` animations, driven by [`Session::tick`].
    animations: Vec<ActiveAnimation>,
    /// Whether the current stylesheet declares any `transition` at all.
    has_transitions: bool,
    /// Per-element inner scroll offsets (`overflow: scroll/auto`).
    pub(crate) scroll_offsets: std::collections::HashMap<NodeId, f32>,
    /// Editing state of the focused text control.
    pub(crate) editor: Option<editor::TextEdit>,
    /// Session cookies (Set-Cookie in, Cookie header out).
    pub(crate) cookies: cookies::CookieJar,
    /// CORS preflight grants received this session, keyed by
    /// (page origin, request URL): a cached grant skips the OPTIONS
    /// probe for the next identical non-simple cross-origin read.
    preflight_cache: std::cell::RefCell<std::collections::HashSet<(String, String)>>,
    /// Persistent per-origin localStorage maps.
    pub(crate) storage: storage::WebStorage,
    /// Set by `back`/`forward` so the next script world can fire
    /// `popstate`; consumed by [`PageScripts::new`].
    traversed: bool,
    /// Final URL of the page currently being fetched, set while its
    /// subresources load — history is only updated once the page is
    /// rendered, so `current_url` alone cannot tell whether the page
    /// being loaded is remote.
    loading_page: Option<Url>,
    /// First usable `@font-face` font of the page (TTF/OTF only —
    /// fontdue cannot parse WOFF), used as the document font.
    web_font: Option<Arc<lumen_engine::SystemFont>>,
    /// How the current stylesheet's hover rules can affect the page —
    /// picks the cheapest reaction to hover changes.
    hover_impact: lumen_engine::HoverImpact,
    /// Damage left by paint-only interaction restyles since the shell's
    /// last full raster; see [`RepaintDamage`].
    repaint_damage: RepaintDamage,
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

/// A running `@keyframes` animation: per-property keyframe tracks.
struct ActiveAnimation {
    node: NodeId,
    duration_ms: f64,
    delay_ms: f64,
    iterations: f32,
    ease: bool,
    /// (property, sorted (offset, value) frames) — only animatable
    /// properties with at least two frames.
    tracks: Vec<(&'static str, Vec<(f32, AnimatedValue)>)>,
    start_ms: Option<f64>,
}

/// One value being animated.
#[derive(Debug, Clone, Copy)]
enum AnimatedValue {
    Number(f32),
    Color(lumen_css::Color),
    Transform(lumen_engine::Transform2D),
    /// A pure rotation in degrees — matrix lerp cannot represent spins
    /// (rotate(0) and rotate(360) are the same matrix), angles can.
    Angle(f32),
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

/// How a script read (fetch/XHR) treats cookies on a cross-origin
/// request — the fetch API's `credentials` option.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ScriptCredentials {
    /// The default (`credentials: "same-origin"`): cookies ride only
    /// same-origin requests.
    SameOrigin,
    /// `credentials: "include"` (or XHR `withCredentials`): cookies
    /// ride cross-origin requests too — and the CORS grant must then
    /// name the page origin exactly (`*` is not enough, per spec).
    Include,
}

/// The same-origin-policy state of a prepared cross-origin script
/// read. Same-origin reads never produce one. It is `Send`, so it can
/// travel with the prepared request to a network worker and back.
#[derive(Debug, Clone)]
pub(crate) struct CorsCheck {
    /// The page's serialized origin ("scheme://host[:port]"), matched
    /// against the response's `Access-Control-Allow-Origin` header.
    page_origin: String,
    /// Cookies rode the request (`credentials: include`).
    credentialed: bool,
    /// Set when the request is not a CORS "simple request" and no
    /// preflight grant for it is cached yet: the probe must be answered
    /// (and pass [`Self::preflight_blocks`]) before the request goes out.
    preflight: Option<CorsPreflight>,
}

impl CorsCheck {
    /// Whether CORS forbids the page's script from reading `response`:
    /// the response needs an `Access-Control-Allow-Origin` of `*` (only
    /// for non-credentialed reads — `*` + credentials is an invalid
    /// combination per spec) or of the page's exact origin. Anything
    /// else (header missing, different origin) blocks.
    pub(crate) fn blocks(&self, response: &ResourceResponse) -> bool {
        let Some(grant) = response.access_control_allow_origin.as_deref() else {
            return true;
        };
        !self.origin_granted(grant)
    }

    /// The ACAO rule shared by the response check and the preflight
    /// check: the grant must name the page's exact origin, or be `*`
    /// for a non-credentialed read.
    fn origin_granted(&self, grant: &str) -> bool {
        let grant = grant.trim();
        grant == self.page_origin || (grant == "*" && !self.credentialed)
    }

    /// The preflight probe this check still needs answered, if any.
    pub(crate) fn preflight(&self) -> Option<&CorsPreflight> {
        self.preflight.as_ref()
    }

    /// The page origin this check grants against (preflight cache key).
    pub(crate) fn page_origin(&self) -> &str {
        &self.page_origin
    }

    /// Takes the preflight probe out of the check, when one is needed:
    /// the caller submits the probe and, once granted, resubmits the
    /// request with the probe-less check gating the actual response.
    pub(crate) fn take_preflight(&mut self) -> Option<CorsPreflight> {
        self.preflight.take()
    }

    /// Whether an answered preflight still forbids the read: its grant
    /// must pass the ACAO rule AND cover the request's method and
    /// custom headers.
    pub(crate) fn preflight_blocks(&self, grant: &CorsGrant) -> bool {
        let Some(probe) = &self.preflight else {
            return false;
        };
        !grant
            .allow_origin
            .as_deref()
            .is_some_and(|origin| self.origin_granted(origin))
            || !grant.covers(&probe.method, &probe.headers, self.credentialed)
    }
}

/// Scheme/host/port equality (url's opaque `Origin` would make every
/// file: page cross-origin with itself).
pub(crate) fn same_origin(a: &Url, b: &Url) -> bool {
    a.scheme() == b.scheme()
        && a.host_str() == b.host_str()
        && a.port_or_known_default() == b.port_or_known_default()
}

/// A CORS-safelisted ("simple") Content-Type value, parameters ignored.
fn simple_content_type(value: &str) -> bool {
    let mime = value.split(';').next().unwrap_or("").trim();
    mime.eq_ignore_ascii_case("text/plain")
        || mime.eq_ignore_ascii_case("application/x-www-form-urlencoded")
        || mime.eq_ignore_ascii_case("multipart/form-data")
}

/// A CORS-safelisted request header: Accept, Accept-Language,
/// Content-Language, Range, or Content-Type with a simple value.
fn safelisted_header(name: &str, value: &str) -> bool {
    matches!(
        name.to_ascii_lowercase().as_str(),
        "accept" | "accept-language" | "content-language" | "range"
    ) || (name.eq_ignore_ascii_case("content-type") && simple_content_type(value))
}

/// The CORS "simple request" test: only these may cross origins
/// without a preflight probe.
fn needs_preflight(
    method: &str,
    body: &Option<(String, Vec<u8>)>,
    headers: &[(String, String)],
) -> bool {
    !matches!(method, "GET" | "HEAD" | "POST")
        || !headers
            .iter()
            .all(|(name, value)| safelisted_header(name, value))
        || body
            .as_ref()
            .is_some_and(|(content_type, _)| !simple_content_type(content_type))
}

impl<L: ResourceLoader> Session<L> {
    #[must_use]
    pub fn new(loader: L, viewport: Size) -> Self {
        Self {
            loader: Arc::new(loader),
            viewport,
            measurer: Box::new(HeuristicMeasurer),
            history: Vec::new(),
            history_states: Vec::new(),
            index: None,
            page: None,
            source: None,
            author: Arc::new(lumen_css::Stylesheet::default()),
            images: Arc::new(ImageMap::new()),
            pending_images: Vec::new(),
            nav_serial: 0,
            hovered: None,
            active: None,
            focused: None,
            form_values: std::collections::HashMap::new(),
            form_checked: std::collections::HashMap::new(),
            form_violation: None,
            visited: std::collections::HashSet::new(),
            transitions: Vec::new(),
            animations: Vec::new(),
            has_transitions: false,
            scroll_offsets: std::collections::HashMap::new(),
            editor: None,
            cookies: cookies::CookieJar::default(),
            preflight_cache: std::cell::RefCell::new(std::collections::HashSet::new()),
            storage: storage::WebStorage::for_config_dir(),
            traversed: false,
            loading_page: None,
            web_font: None,
            hover_impact: lumen_engine::HoverImpact::Nothing,
            repaint_damage: RepaintDamage::Full,
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
        self.load_with_body(url, None)
    }

    /// [`Self::load`] with an optional POST body (form submission).
    /// History records the URL only: back/refresh re-GET, like early
    /// browsers before re-POST prompts.
    pub(crate) fn load_with_body(
        &mut self,
        url: Url,
        body: Option<(String, Vec<u8>)>,
    ) -> Result<&Page, LoadError> {
        let final_url = self.fetch_and_render_with(url, body)?;
        if let Some(index) = self.index {
            self.history.truncate(index + 1);
            self.history_states.truncate(index + 1);
        }
        self.visited.insert(final_url.to_string());
        self.history.push(final_url);
        self.history_states.push(None);
        self.index = Some(self.history.len() - 1);
        self.traversed = false;
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
        self.traversed = true;
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
        self.traversed = true;
        Ok(self.page.as_ref().expect("fetch_and_render set the page"))
    }

    /// Number of entries in this session's history (`history.length`).
    #[must_use]
    pub fn history_len(&self) -> usize {
        self.history.len()
    }

    /// The `pushState` state (JSON) of the current entry, if any.
    pub(crate) fn history_state(&self) -> Option<String> {
        self.index
            .and_then(|index| self.history_states.get(index))
            .cloned()
            .flatten()
    }

    /// `history.pushState`: appends a new entry pointing at `url`
    /// (dropping forward entries) WITHOUT reloading — the SPA contract.
    /// The same-origin check already happened script-side.
    pub(crate) fn push_state(&mut self, url: Url, state: Option<String>) {
        let Some(index) = self.index else {
            return;
        };
        self.history.truncate(index + 1);
        self.history_states.truncate(index + 1);
        self.visited.insert(url.to_string());
        self.history.push(url);
        self.history_states.push(state);
        self.index = Some(self.history.len() - 1);
    }

    /// `history.replaceState`: rewrites the current entry in place.
    pub(crate) fn replace_state(&mut self, url: Url, state: Option<String>) {
        let Some(index) = self.index else {
            return;
        };
        self.visited.insert(url.to_string());
        self.history[index] = url;
        self.history_states[index] = state;
    }

    /// Whether the last navigation was a `back`/`forward` traversal;
    /// consumed (cleared) by the reader.
    pub(crate) fn take_traversed(&mut self) -> bool {
        std::mem::take(&mut self.traversed)
    }

    /// Points localStorage persistence at `root` (tests, embedders).
    pub fn set_storage_root(&mut self, root: std::path::PathBuf) {
        self.storage.set_root(root);
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
    /// The currently focused node, if any.
    #[must_use]
    pub fn focused(&self) -> Option<NodeId> {
        self.focused
    }

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
                        "option" => live.unwrap_or_else(|| element.attributes.contains("selected")),
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
                        let damage =
                            lumen_engine::repaint_page_interactive_damaged(page, &interaction);
                        // A pending full repaint subsumes any damage;
                        // consecutive paint-only restyles union together.
                        self.repaint_damage = match self.repaint_damage {
                            RepaintDamage::Full => RepaintDamage::Full,
                            RepaintDamage::Region(so_far) => {
                                RepaintDamage::Region(union_damage(so_far, damage))
                            }
                        };
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
        if self.transitions.is_empty() && self.animations.is_empty() {
            return false;
        }
        let Some(page) = self.page.as_mut() else {
            self.transitions.clear();
            self.animations.clear();
            return false;
        };
        // Damage accumulated by the animated nodes this tick; transform
        // animations and canvas-propagating backgrounds force a full
        // repaint instead (same conservative rule as style damage).
        let mut full_repaint = false;
        let mut tick_damage: Option<lumen_engine::Rect> = None;
        let mut note_node_damage = |page: &lumen_engine::Page, node: NodeId| {
            tick_damage = union_damage(tick_damage, lumen_engine::node_paint_damage(page, node));
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
            if matches!(
                (transition.from, transition.to),
                (AnimatedValue::Transform(_), AnimatedValue::Transform(_))
            ) {
                // Paint can land anywhere: transform damage is not a rect.
                full_repaint = true;
            } else {
                note_node_damage(page, transition.node);
            }
            if progress < 1.0 {
                any_active = true;
            }
        }
        // @keyframes animations: find the surrounding frames for the
        // current cycle position and interpolate.
        for animation in &mut self.animations {
            let start = *animation.start_ms.get_or_insert(now_ms);
            let elapsed = (now_ms - start - animation.delay_ms).max(0.0);
            let cycles = elapsed / animation.duration_ms.max(0.001);
            let finished = cycles as f32 >= animation.iterations;
            let t = if finished {
                1.0
            } else {
                (cycles.fract()) as f32
            };
            if let Some(style) = page.styles.by_node.get_mut(&animation.node) {
                for (property, frames) in &animation.tracks {
                    let after = frames
                        .iter()
                        .position(|(offset, _)| *offset >= t)
                        .unwrap_or(frames.len() - 1);
                    let before = after.saturating_sub(if frames[after].0 > t { 1 } else { 0 });
                    let (from_offset, from) = frames[before];
                    let (to_offset, to) = frames[after.max(before)];
                    let span = (to_offset - from_offset).max(f32::EPSILON);
                    let local = ((t - from_offset) / span).clamp(0.0, 1.0);
                    let eased = if animation.ease {
                        local * local * (3.0 - 2.0 * local)
                    } else {
                        local
                    };
                    match (from, to) {
                        (AnimatedValue::Number(from), AnimatedValue::Number(to)) => {
                            style.opacity = from + (to - from) * eased;
                        }
                        (AnimatedValue::Color(from), AnimatedValue::Color(to)) => {
                            let value = lerp_color(from, to, eased);
                            if *property == "color" {
                                style.color = value;
                            } else {
                                style.background_color = (value.a > 0).then_some(value);
                            }
                        }
                        (AnimatedValue::Transform(from), AnimatedValue::Transform(to)) => {
                            let value = lumen_engine::Transform2D::lerp(from, to, eased);
                            style.transform = (!value.is_identity()).then_some(value);
                        }
                        (AnimatedValue::Angle(from), AnimatedValue::Angle(to)) => {
                            let degrees = from + (to - from) * eased;
                            style.transform = lumen_engine::parse_transform_value(
                                &format!("rotate({degrees}deg)"),
                                style.font_size,
                            )
                            .filter(|value| !value.is_identity());
                        }
                        _ => {}
                    }
                }
            }
            let mutates_transform = animation.tracks.iter().any(|(property, frames)| {
                *property == "transform"
                    || frames.iter().any(|(_, value)| {
                        matches!(value, AnimatedValue::Transform(_) | AnimatedValue::Angle(_))
                    })
            });
            // html/body backgrounds propagate to the whole canvas.
            let canvas_background = animation
                .tracks
                .iter()
                .any(|(property, _)| *property == "background-color")
                && page
                    .document
                    .element(animation.node)
                    .is_some_and(|element| matches!(element.tag_name.as_str(), "html" | "body"));
            if mutates_transform || canvas_background {
                full_repaint = true;
            } else {
                note_node_damage(page, animation.node);
            }
            if !finished {
                any_active = true;
            }
        }
        self.animations.retain(|animation| {
            animation.start_ms.is_none_or(|start| {
                let cycles = ((now_ms - start - animation.delay_ms).max(0.0))
                    / animation.duration_ms.max(0.001);
                (cycles as f32) < animation.iterations
            })
        });
        lumen_engine::refresh_paint(page);
        if full_repaint {
            self.note_full_repaint();
        } else {
            // A pending full repaint subsumes any damage; consecutive
            // ticks union together.
            self.repaint_damage = match self.repaint_damage {
                RepaintDamage::Full => RepaintDamage::Full,
                RepaintDamage::Region(so_far) => {
                    RepaintDamage::Region(union_damage(so_far, tick_damage))
                }
            };
        }
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
        self.note_full_repaint();
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

    /// Re-lays out the page reusing the already-computed styles, skipping
    /// selector matching and generated-content/list-marker passes. Valid
    /// only when the edit changed a node's text but not the document
    /// structure or anything a selector matches — i.e. live text-input
    /// value updates. Layout itself still runs in full, so geometry stays
    /// correct even for content-sized controls; only the expensive style
    /// cascade is skipped.
    fn relayout_reusing_styles(&mut self) {
        self.note_full_repaint();
        let Some(mut page) = self.page.take() else {
            return;
        };
        {
            let measurer = self.effective_measurer();
            page.layout = lumen_engine::layout_document(
                &page.document,
                &page.styles,
                page.viewport,
                measurer,
                &page.images,
            );
            page.display_list = lumen_engine::build_display_list(&page.layout, &page.images);
        }
        self.page = Some(page);
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
        self.note_full_repaint();
        true
    }

    /// The page's own `@font-face` font, when one loaded.
    #[must_use]
    pub fn web_font(&self) -> Option<Arc<lumen_engine::SystemFont>> {
        self.web_font.clone()
    }

    /// The measurer layout runs with: the web font when one loaded, else
    /// the shell-provided measurer.
    pub(crate) fn effective_measurer(&self) -> &dyn TextMeasurer {
        match &self.web_font {
            Some(font) => font.as_ref(),
            None => self.measurer.as_ref(),
        }
    }

    /// Loads a resource with the session's cookies attached, storing any
    /// `Set-Cookie` headers from the response. Every network access of
    /// the session funnels through here. A remote (http/https) page may
    /// never read local files: `file:` requests are rejected while one
    /// is loaded.
    pub(crate) fn fetch_resource(
        &mut self,
        url: Url,
        body: Option<(String, Vec<u8>)>,
    ) -> Result<ResourceResponse, LoadError> {
        let request = self.prepare_request(url, body)?;
        let response = self.loader.load(&request)?;
        self.store_response_cookies(&response);
        Ok(response)
    }

    /// The PREPARE half of a fetch, read-only: applies the remote-page
    /// `file:` gate and attaches the jar's Cookie header. The prepared
    /// request is `Send`, so script-initiated loads can cross to a
    /// network worker without the session leaving its thread.
    ///
    /// This is the no-cors subresource policy; a script READ (fetch/XHR)
    /// must go through [`Self::prepare_script_read`] instead, which
    /// layers the same-origin policy on top.
    pub(crate) fn prepare_request(
        &self,
        url: Url,
        body: Option<(String, Vec<u8>)>,
    ) -> Result<ResourceRequest, LoadError> {
        if url.scheme() == "file"
            && self
                .page_origin()
                .is_some_and(|scheme| matches!(scheme, "http" | "https"))
        {
            return Err(LoadError::UnsupportedScheme(
                "file (blocked from a remote page)".to_string(),
            ));
        }
        Ok(ResourceRequest {
            cookie: self.cookies.header_for_http(&url),
            url,
            body,
        })
    }

    /// The APPLY half of a fetch: stores a response's `Set-Cookie`
    /// headers in the jar. Runs on the session's own thread once a
    /// (possibly worker-executed) load lands.
    pub(crate) fn store_response_cookies(&mut self, response: &ResourceResponse) {
        for header in &response.set_cookies {
            self.cookies.store(&response.final_url, header);
        }
    }

    /// Image subresources discovered during the last page load, taken
    /// out for the shell to fetch progressively. The page is already
    /// rendered at this point — it does not wait for images.
    pub fn take_pending_images(&mut self) -> Vec<(NodeId, ResourceRequest)> {
        std::mem::take(&mut self.pending_images)
    }

    /// Whether any image subresources are still unfetched.
    #[must_use]
    pub fn has_pending_images(&self) -> bool {
        !self.pending_images.is_empty()
    }

    /// Navigation counter for progressive image loading: results are
    /// applied only when they still belong to the current page.
    #[must_use]
    pub fn nav_serial(&self) -> u64 {
        self.nav_serial
    }

    /// The shared loader handle, for shells that fetch subresources
    /// (e.g. images) on their own worker threads.
    pub fn loader_handle(&self) -> Arc<L> {
        Arc::clone(&self.loader)
    }

    /// Fetches image requests in parallel on scoped worker threads
    /// (up to 8 at a time). Runs on the caller's thread; the results
    /// are handed to [`Self::apply_image_responses`] wherever the
    /// session lives.
    #[must_use]
    pub fn fetch_images(
        loader: &L,
        tasks: Vec<(NodeId, ResourceRequest)>,
    ) -> Vec<(NodeId, ResourceResponse)> {
        const IMAGE_FETCH_WORKERS: usize = 8;
        if tasks.is_empty() {
            return Vec::new();
        }
        let workers = IMAGE_FETCH_WORKERS.min(tasks.len());
        std::thread::scope(|scope| {
            let (tx, rx) = std::sync::mpsc::channel::<(NodeId, ResourceResponse)>();
            let tasks = Arc::new(std::sync::Mutex::new(tasks.into_iter()));
            let mut handles = Vec::new();
            for _ in 0..workers {
                let tx = tx.clone();
                let tasks = Arc::clone(&tasks);
                handles.push(scope.spawn(move || {
                    while let Some((node, request)) = tasks.lock().unwrap().next() {
                        if let Ok(response) = loader.load(&request) {
                            let _ = tx.send((node, response));
                        }
                    }
                }));
            }
            drop(tx);
            let results: Vec<_> = rx.iter().collect();
            for handle in handles {
                let _ = handle.join();
            }
            results
        })
    }

    /// Decodes fetched image responses into the page and relayouts so
    /// intrinsic image sizes apply (progressive image loading).
    pub fn apply_image_responses(&mut self, results: Vec<(NodeId, ResourceResponse)>) {
        if results.is_empty() || self.page.is_none() {
            return;
        }
        let mut images = (*self.images).clone();
        for (node, response) in results {
            self.store_response_cookies(&response);
            if let Some(image) = RasterImage::decode(&response.body) {
                images.insert(node, Arc::new(image));
            }
        }
        self.images = Arc::new(images);
        self.relayout();
    }

    /// Drives the pending image fetches synchronously (CLI, tests).
    pub fn load_pending_images_blocking(&mut self) {
        let tasks = self.take_pending_images();
        let results = Self::fetch_images(self.loader.as_ref(), tasks);
        self.apply_image_responses(results);
    }

    /// [`Self::prepare_request`] for a script READ (fetch/XHR) — a
    /// request whose response body the page's own JavaScript wants to
    /// see. Unlike subresource loads (scripts, CSS, images — no-cors
    /// mode, unchanged behavior), these follow the same-origin policy:
    ///
    /// - Same-origin with the page: the jar's Cookie header rides and
    ///   the response is readable (no [`CorsCheck`] comes back).
    /// - Cross-origin: the request still goes out, but WITHOUT the
    ///   Cookie header unless `credentials` is
    ///   [`ScriptCredentials::Include`], and the returned [`CorsCheck`]
    ///   must gate the response before any byte reaches JS
    ///   ([`CorsCheck::blocks`]).
    /// - Cross-origin AND not a CORS "simple request" (a method beyond
    ///   GET/HEAD/POST, a custom header, or a non-simple Content-Type):
    ///   the check also carries a [`CorsPreflight`] probe that must be
    ///   answered first ([`CorsCheck::preflight_blocks`]) — unless a
    ///   grant for this origin+URL is already cached
    ///   ([`Self::cache_preflight`]).
    pub(crate) fn prepare_script_read(
        &self,
        url: Url,
        body: Option<(String, Vec<u8>)>,
        credentials: ScriptCredentials,
        method: &str,
        headers: &[(String, String)],
    ) -> Result<(ResourceRequest, Option<CorsCheck>), LoadError> {
        let mut request = self.prepare_request(url, body)?;
        let page = self.loading_page.as_ref().or_else(|| self.current_url());
        let Some(page) = page else {
            return Ok((request, None));
        };
        if same_origin(page, &request.url) {
            return Ok((request, None));
        }
        if credentials == ScriptCredentials::SameOrigin {
            // `credentials: same-origin` (the fetch default): the jar's
            // cookies must not leak across origins.
            request.cookie = None;
        }
        let page_origin = page.origin().unicode_serialization();
        let preflight = (needs_preflight(method, &request.body, headers)
            && !self
                .preflight_cache
                .borrow()
                .contains(&(page_origin.clone(), request.url.to_string())))
        .then(|| CorsPreflight {
            url: request.url.clone(),
            method: method.to_string(),
            headers: headers
                .iter()
                .filter(|(name, value)| !safelisted_header(name, value))
                .map(|(name, _)| name.to_ascii_lowercase())
                .collect(),
        });
        Ok((
            request,
            Some(CorsCheck {
                page_origin,
                credentialed: credentials == ScriptCredentials::Include,
                preflight,
            }),
        ))
    }

    /// Caches a preflight grant for (page origin, request URL): the
    /// next identical non-simple read skips the OPTIONS probe. The
    /// cache lives and dies with the session — no TTL.
    pub(crate) fn cache_preflight(&self, page_origin: &str, url: &Url) {
        self.preflight_cache
            .borrow_mut()
            .insert((page_origin.to_string(), url.to_string()));
    }

    /// A shared handle to the session's loader for network workers.
    pub(crate) fn shared_loader(&self) -> Arc<L> {
        self.loader.clone()
    }

    /// The scheme of the page being shown or loaded, if any.
    fn page_origin(&self) -> Option<&str> {
        self.loading_page
            .as_ref()
            .or_else(|| self.current_url())
            .map(Url::scheme)
    }

    #[must_use]
    pub fn page(&self) -> Option<&Page> {
        self.page.as_ref()
    }

    /// Damage accumulated by paint-only interaction restyles since the
    /// shell's last raster: the shell may re-rasterize only this region
    /// (or nothing at all for `Region(None)`) instead of the full frame.
    #[must_use]
    pub fn repaint_damage(&self) -> RepaintDamage {
        self.repaint_damage
    }

    /// Marks the shell's frame cache current with the page. Call after
    /// every re-raster — full, scrolled blit or damage-region — so the
    /// accumulated damage is never applied twice.
    pub fn note_rasterized(&mut self) {
        self.repaint_damage = RepaintDamage::Region(None);
    }

    /// Marks the next repaint as full (relayout, navigation, scripts,
    /// animation ticks, inner scrolling — anything but the paint-only
    /// interaction path, which tracks its own damage).
    fn note_full_repaint(&mut self) {
        self.repaint_damage = RepaintDamage::Full;
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
        self.fetch_and_render_with(url, None)
    }

    /// [`Self::fetch_and_render`] with an optional POST body.
    fn fetch_and_render_with(
        &mut self,
        url: Url,
        body: Option<(String, Vec<u8>)>,
    ) -> Result<Url, LoadError> {
        let response = self.fetch_resource(url, body)?;
        // Subresources of a remote page may not escape to file:; history
        // is only updated once this page rendered, so remember it here.
        self.loading_page = Some(response.final_url.clone());
        let source = response.text();
        let document = lumen_html::parse_document(&source);

        // External stylesheets: resolved against the final URL, fetched in
        // document order; failures skip that sheet without failing the page.
        let base = response.final_url.clone();
        let author_css = collect_author_css(&document, |href| {
            let url = resolve(&base, href).ok()?;
            self.fetch_resource(url, None)
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
        // Collect candidate sources first: fetching needs &mut self.
        let face_sources: Vec<(String, Option<String>)> = self
            .author
            .font_faces
            .iter()
            .flat_map(|face| face.sources.clone())
            .collect();
        'faces: for (source, format) in &face_sources {
            let usable = match format.as_deref() {
                Some("truetype" | "opentype" | "woff" | "woff2") => true,
                Some(_) => false,
                None => {
                    let lower = source.to_ascii_lowercase();
                    lower.ends_with(".ttf")
                        || lower.ends_with(".otf")
                        || lower.ends_with(".woff")
                        || lower.ends_with(".woff2")
                }
            };
            if !usable {
                continue;
            }
            let Ok(url) = resolve(&base, source) else {
                continue;
            };
            if let Ok(response) = self.fetch_resource(url, None)
                && let Some(font) = lumen_engine::SystemFont::from_bytes(&response.body)
            {
                self.web_font = Some(Arc::new(font));
                break 'faces;
            }
        }

        // Images: discovered here, fetched progressively — the page
        // renders immediately with placeholder boxes and the shell
        // fetches in the background (see `take_pending_images`),
        // relayouting when they arrive. CLI/tests drive the same
        // pipeline synchronously via `load_pending_images_blocking`.
        // Capped so image-heavy pages stay bounded.
        const MAX_IMAGES_PER_PAGE: usize = 32;
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
        self.nav_serial += 1;
        self.pending_images = sources
            .into_iter()
            .take(MAX_IMAGES_PER_PAGE)
            .filter_map(|(node, src)| {
                let url = resolve(&base, &src).ok()?;
                let request = self.prepare_request(url, None).ok()?;
                Some((node, request))
            })
            .collect();
        self.images = Arc::new(ImageMap::new());

        self.hovered = None; // New document, new node ids.
        self.active = None;
        self.focused = None;
        self.editor = None;
        self.animations.clear();
        self.transitions.clear();
        self.scroll_offsets.clear();
        self.form_values.clear();
        self.form_checked.clear();
        self.form_violation = None;
        self.note_full_repaint();
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
        self.spawn_animations();
        self.loading_page = None;
        Ok(response.final_url)
    }

    /// Builds animation tracks from `@keyframes` for every node whose
    /// computed style names one.
    fn spawn_animations(&mut self) {
        self.animations.clear();
        let Some(page) = self.page.as_ref() else {
            return;
        };
        for (node, style) in &page.styles.by_node {
            let Some(spec) = &style.animation else {
                continue;
            };
            let Some(block) = page.stylesheet.keyframes(&spec.name) else {
                continue;
            };
            let font_size = style.font_size;
            let mut tracks: Vec<(&'static str, Vec<(f32, AnimatedValue)>)> = Vec::new();
            for property in ["opacity", "transform", "background-color", "color"] {
                let mut frames: Vec<(f32, AnimatedValue)> = Vec::new();
                for (offset, declarations) in &block.frames {
                    let Some(declaration) = declarations
                        .iter()
                        .rev()
                        .find(|declaration| declaration.name == property)
                    else {
                        continue;
                    };
                    let value = match property {
                        // A bare `0` parses as a zero length, not a number.
                        "opacity" => match &declaration.value {
                            lumen_css::CssValue::Number(value)
                            | lumen_css::CssValue::Length(value, _) => {
                                Some(AnimatedValue::Number(*value))
                            }
                            _ => None,
                        },
                        "transform" => {
                            let raw = declaration.value.raw_text();
                            let trimmed = raw.trim();
                            let angle = trimmed
                                .strip_prefix("rotate(")
                                .and_then(|rest| rest.strip_suffix(')'))
                                .and_then(|inner| {
                                    inner.trim().strip_suffix("deg")?.trim().parse::<f32>().ok()
                                });
                            match angle {
                                Some(degrees) => Some(AnimatedValue::Angle(degrees)),
                                None => lumen_engine::parse_transform_value(trimmed, font_size)
                                    .map(AnimatedValue::Transform),
                            }
                        }
                        _ => match &declaration.value {
                            lumen_css::CssValue::Color(color) => Some(AnimatedValue::Color(*color)),
                            lumen_css::CssValue::Keyword(keyword) => {
                                lumen_css::Color::parse(keyword).map(AnimatedValue::Color)
                            }
                            _ => None,
                        },
                    };
                    if let Some(value) = value {
                        frames.push((*offset, value));
                    }
                }
                if frames.len() >= 2 {
                    let name: &'static str = match property {
                        "opacity" => "opacity",
                        "transform" => "transform",
                        "background-color" => "background-color",
                        _ => "color",
                    };
                    tracks.push((name, frames));
                }
            }
            if !tracks.is_empty() {
                self.animations.push(ActiveAnimation {
                    node: *node,
                    duration_ms: f64::from(spec.duration) * 1000.0,
                    delay_ms: f64::from(spec.delay) * 1000.0,
                    iterations: spec.iterations,
                    ease: spec.ease,
                    tracks,
                    start_ms: None,
                });
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lumen_platform::ResourceResponse;
    use std::collections::HashMap;

    struct FakeLoader {
        pages: HashMap<String, Vec<u8>>,
        loads: std::sync::Mutex<Vec<String>>,
        /// POST bodies per load (None for GETs).
        bodies: std::sync::Mutex<Vec<Option<String>>>,
        /// Cookie headers per load.
        cookies_sent: std::sync::Mutex<Vec<Option<String>>>,
        /// Set-Cookie headers served per URL.
        serve_cookies: HashMap<String, Vec<String>>,
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
                cookies_sent: std::sync::Mutex::new(Vec::new()),
                serve_cookies: HashMap::new(),
            }
        }

        fn with_set_cookie(mut self, url: &str, header: &str) -> Self {
            self.serve_cookies
                .entry(url.to_string())
                .or_default()
                .push(header.to_string());
            self
        }

        fn with_bytes(mut self, url: &str, bytes: &[u8]) -> Self {
            self.pages.insert(url.to_string(), bytes.to_vec());
            self
        }
    }

    impl ResourceLoader for FakeLoader {
        fn load(&self, request: &ResourceRequest) -> Result<ResourceResponse, LoadError> {
            self.loads.lock().unwrap().push(request.url.to_string());
            self.bodies.lock().unwrap().push(
                request
                    .body
                    .as_ref()
                    .map(|(_, bytes)| String::from_utf8_lossy(bytes).into_owned()),
            );
            self.cookies_sent
                .lock()
                .unwrap()
                .push(request.cookie.clone());
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
                access_control_allow_origin: None,
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
    fn script_read_same_origin_keeps_cookies_cross_origin_drops_them() {
        let mut session = session();
        session.load(url("https://a.test/")).unwrap();
        session.cookies.store(&url("https://a.test/"), "sid=1");
        // Same-origin read: the jar's cookie rides, no CORS check.
        let (request, check) = session
            .prepare_script_read(
                url("https://a.test/data"),
                None,
                ScriptCredentials::SameOrigin,
                "GET",
                &[],
            )
            .unwrap();
        assert_eq!(request.cookie.as_deref(), Some("sid=1"));
        assert!(check.is_none());
        // Cross-origin read (same host, another scheme+port — the jar's
        // cookie WOULD domain-match): the cookie stays home, a check
        // comes back.
        let (request, check) = session
            .prepare_script_read(
                url("http://a.test:8080/data"),
                None,
                ScriptCredentials::SameOrigin,
                "GET",
                &[],
            )
            .unwrap();
        assert_eq!(request.cookie, None);
        let check = check.expect("cross-origin read carries a CORS check");
        assert!(!check.credentialed);
        assert!(check.preflight.is_none(), "GET is a simple request");
        // credentials: include sends the cookie cross-origin too.
        let (request, check) = session
            .prepare_script_read(
                url("http://a.test:8080/data"),
                None,
                ScriptCredentials::Include,
                "GET",
                &[],
            )
            .unwrap();
        assert_eq!(request.cookie.as_deref(), Some("sid=1"));
        assert!(check.expect("cross-origin read").credentialed);
    }

    #[test]
    fn non_simple_reads_carry_a_preflight_until_cached() {
        let mut session = session();
        session.load(url("https://a.test/")).unwrap();
        let custom = [("X-Token".to_string(), "abc".to_string())];
        // A custom header makes the read non-simple: a probe comes back.
        let (_, check) = session
            .prepare_script_read(
                url("http://a.test:8080/data"),
                None,
                ScriptCredentials::SameOrigin,
                "GET",
                &custom,
            )
            .unwrap();
        let probe = check
            .expect("cross-origin read")
            .preflight
            .expect("custom header needs a preflight");
        assert_eq!(probe.method, "GET");
        assert_eq!(probe.headers, vec!["x-token"]);
        // A PUT without headers is non-simple too (the method).
        let (_, check) = session
            .prepare_script_read(
                url("http://a.test:8080/data"),
                Some(("text/plain".to_string(), b"hi".to_vec())),
                ScriptCredentials::SameOrigin,
                "PUT",
                &[],
            )
            .unwrap();
        let probe = check
            .expect("cross-origin read")
            .preflight
            .expect("PUT probe");
        assert_eq!(probe.method, "PUT");
        assert!(probe.headers.is_empty());
        // A POST with a simple Content-Type stays simple (no probe).
        let (_, check) = session
            .prepare_script_read(
                url("http://a.test:8080/data"),
                Some(("text/plain;charset=UTF-8".to_string(), b"hi".to_vec())),
                ScriptCredentials::SameOrigin,
                "POST",
                &[],
            )
            .unwrap();
        assert!(check.expect("cross-origin read").preflight.is_none());
        // Once a grant is cached for the origin+URL, the probe is gone.
        session.cache_preflight("https://a.test", &url("http://a.test:8080/data"));
        let (_, check) = session
            .prepare_script_read(
                url("http://a.test:8080/data"),
                None,
                ScriptCredentials::SameOrigin,
                "GET",
                &custom,
            )
            .unwrap();
        assert!(check.expect("cross-origin read").preflight.is_none());
    }

    #[test]
    fn preflight_grants_are_checked_against_method_headers_and_origin() {
        let probe = |headers: &[&str]| CorsPreflight {
            url: url("http://a.test:8080/data"),
            method: "PUT".to_string(),
            headers: headers.iter().map(|h| (*h).to_string()).collect(),
        };
        let check = |preflight, credentialed| CorsCheck {
            page_origin: "https://a.test".to_string(),
            credentialed,
            preflight,
        };
        let grant = |origin: Option<&str>, methods: &[&str], headers: &[&str]| CorsGrant {
            allow_origin: origin.map(str::to_string),
            allow_methods: methods.iter().map(|m| (*m).to_string()).collect(),
            allow_headers: headers.iter().map(|h| (*h).to_string()).collect(),
        };
        let put_token = check(Some(probe(&["x-token"])), false);
        // A full grant passes; each missing piece blocks on its own.
        assert!(!put_token.preflight_blocks(&grant(
            Some("https://a.test"),
            &["get", "put"],
            &["x-token"]
        )));
        assert!(put_token.preflight_blocks(&grant(None, &["put"], &["x-token"])));
        assert!(put_token.preflight_blocks(&grant(
            Some("https://evil.test"),
            &["put"],
            &["x-token"]
        )));
        assert!(put_token.preflight_blocks(&grant(Some("https://a.test"), &["get"], &["x-token"])));
        assert!(put_token.preflight_blocks(&grant(Some("https://a.test"), &["put"], &[])));
        // Wildcards cover a non-credentialed read...
        assert!(!put_token.preflight_blocks(&grant(Some("*"), &["*"], &["*"])));
        // ...but not a credentialed one (spec reads '*' literally).
        let credentialed = check(Some(probe(&["x-token"])), true);
        assert!(credentialed.preflight_blocks(&grant(Some("*"), &["*"], &["*"])));
        assert!(!credentialed.preflight_blocks(&grant(
            Some("https://a.test"),
            &["put"],
            &["x-token"]
        )));
    }

    #[test]
    fn cors_check_blocks_missing_wildcard_and_foreign_grants() {
        let response = |acao: Option<&str>| ResourceResponse {
            final_url: url("http://a.test:8080/data"),
            content_type: None,
            body: Vec::new(),
            set_cookies: Vec::new(),
            access_control_allow_origin: acao.map(str::to_string),
        };
        let check = CorsCheck {
            page_origin: "https://a.test".to_string(),
            credentialed: false,
            preflight: None,
        };
        assert!(check.blocks(&response(None)));
        assert!(!check.blocks(&response(Some("*"))));
        assert!(!check.blocks(&response(Some("https://a.test"))));
        assert!(check.blocks(&response(Some("https://evil.test"))));
        // ACAO '*' is not a valid grant for a credentialed read (spec).
        let credentialed = CorsCheck {
            credentialed: true,
            ..check.clone()
        };
        assert!(credentialed.blocks(&response(Some("*"))));
        assert!(!credentialed.blocks(&response(Some("https://a.test"))));
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
            *session.loader.loads.lock().unwrap(),
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
        assert_eq!(session.loader.loads.lock().unwrap().len(), 1);
    }

    fn find_tag(page: &Page, tag: &str) -> NodeId {
        page.document
            .descendants(page.document.root())
            .find(|id| {
                page.document
                    .element(*id)
                    .is_some_and(|element| element.tag_name == tag)
            })
            .unwrap_or_else(|| panic!("no <{tag}>"))
    }

    #[test]
    fn paint_only_hover_reports_damage_region() {
        let mut session = Session::new(
            FakeLoader::new(&[(
                "https://a.test/",
                "<style>p { width: 100px; height: 20px; }\
                        p:hover { color: #ff0000; }</style><p>hi</p>",
            )]),
            VIEWPORT,
        );
        session.load(url("https://a.test/")).unwrap();
        // Navigation requires a full raster; once the shell has
        // rasterized it, the cache is current and damage is empty.
        assert_eq!(session.repaint_damage(), RepaintDamage::Full);
        session.note_rasterized();
        assert_eq!(session.repaint_damage(), RepaintDamage::Region(None));

        let p = find_tag(session.page().unwrap(), "p");
        let p_box = session
            .page()
            .unwrap()
            .layout
            .find_by_node(p)
            .unwrap()
            .border_box();
        assert!(session.set_hovered(Some(p)));
        assert_eq!(session.repaint_damage(), RepaintDamage::Region(Some(p_box)));

        // Hovering off unions with the pending damage (same rect here).
        assert!(session.set_hovered(None));
        assert_eq!(session.repaint_damage(), RepaintDamage::Region(Some(p_box)));
    }

    #[test]
    fn paint_only_restyle_without_visible_change_reports_empty_damage() {
        // `cursor` is paint-level for hover_impact but never reaches the
        // raster: the restyle runs, yet nothing visible changes.
        let mut session = Session::new(
            FakeLoader::new(&[(
                "https://a.test/",
                "<style>p:hover { cursor: pointer; }</style><p>hi</p>",
            )]),
            VIEWPORT,
        );
        session.load(url("https://a.test/")).unwrap();
        session.note_rasterized();
        let p = find_tag(session.page().unwrap(), "p");
        assert!(session.set_hovered(Some(p)));
        assert_eq!(session.repaint_damage(), RepaintDamage::Region(None));
    }

    #[test]
    fn relayout_after_hover_resets_damage_to_full() {
        let mut session = Session::new(
            FakeLoader::new(&[(
                "https://a.test/",
                "<style>p:hover { color: #ff0000; }</style><p>hi</p>",
            )]),
            VIEWPORT,
        );
        session.load(url("https://a.test/")).unwrap();
        session.note_rasterized();
        let p = find_tag(session.page().unwrap(), "p");
        assert!(session.set_hovered(Some(p)));
        assert!(matches!(
            session.repaint_damage(),
            RepaintDamage::Region(Some(_))
        ));
        session.set_viewport(Size {
            width: 400.0,
            height: 600.0,
        });
        assert_eq!(session.repaint_damage(), RepaintDamage::Full);
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
        assert_eq!(session.loader.loads.lock().unwrap().len(), 3);
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
        assert_eq!(session.loader.loads.lock().unwrap().len(), 2);
    }

    #[test]
    fn layout_hover_rules_skip_relayout_for_unrelated_targets() {
        let mut session = Session::new(
            FakeLoader::new(&[(
                "https://a.test/",
                // `display: block` gives the link a real layout box:
                // html5ever nests it in <body> where a plain inline
                // element flows in line fragments and has no box to
                // inspect for the hover padding.
                "<style>a { display: block; } a:hover { padding: 8px; }</style>\
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

    /// Finds the first control whose attribute `name` matches.
    fn control_with(session: &Session<FakeLoader>, name: &str, value: &str) -> NodeId {
        let document = &session.page().unwrap().document;
        document
            .descendants(document.root())
            .find(|id| {
                document
                    .element(*id)
                    .is_some_and(|element| element.attributes.get(name) == Some(value))
            })
            .unwrap()
    }

    #[test]
    fn range_overflow_blocks_submission_with_a_violation() {
        let mut session = Session::new(
            FakeLoader::new(&[
                (
                    "https://a.test/",
                    "<form action='/go'>\
                     <input type='number' name='n' max='99' value='123'>\
                     <input type='submit' value='Go'></form>",
                ),
                ("https://a.test/go?n=123", "<p>should not load</p>"),
            ]),
            VIEWPORT,
        );
        session.load(url("https://a.test/")).unwrap();
        let field = control_with(&session, "name", "n");
        session.submit_form(field).unwrap();
        // The navigation never happened; the violation is on the session.
        assert_eq!(session.current_url().unwrap().as_str(), "https://a.test/");
        let violation = session.form_violation().unwrap();
        assert_eq!(violation.node, field);
        assert_eq!(violation.message, "Value must be less than or equal to 99.");
    }

    #[test]
    fn range_underflow_and_document_order_pick_the_first_violation() {
        let mut session = Session::new(
            FakeLoader::new(&[(
                "https://a.test/",
                "<form action='/go'>\
                 <input type='number' name='low' min='10' value='2'>\
                 <input type='number' name='high' max='99' value='123'>\
                 </form>",
            )]),
            VIEWPORT,
        );
        session.load(url("https://a.test/")).unwrap();
        let high = control_with(&session, "name", "high");
        // Both fields are invalid; the earlier one in the form wins.
        session.submit_form(high).unwrap();
        let violation = session.form_violation().unwrap();
        assert_eq!(violation.node, control_with(&session, "name", "low"));
        assert_eq!(
            violation.message,
            "Value must be greater than or equal to 10."
        );
        assert_eq!(session.current_url().unwrap().as_str(), "https://a.test/");
    }

    #[test]
    fn required_empty_input_blocks_submission() {
        let mut session = Session::new(
            FakeLoader::new(&[
                (
                    "https://a.test/",
                    "<form action='/go'>\
                     <input type='text' name='q' required>\
                     <textarea name='not' required></textarea></form>",
                ),
                ("https://a.test/go?q=x&not=y", "<p>ok</p>"),
            ]),
            VIEWPORT,
        );
        session.load(url("https://a.test/")).unwrap();
        let field = control_with(&session, "name", "q");
        session.submit_form(field).unwrap();
        assert_eq!(session.current_url().unwrap().as_str(), "https://a.test/");
        let violation = session.form_violation().unwrap();
        assert_eq!(violation.node, field);
        assert_eq!(violation.message, "Please fill out this field.");
        // Filling both required fields lets the submit through and
        // clears the violation.
        session.set_form_value(field, "x");
        session.set_form_value(control_with(&session, "name", "not"), "y");
        session.submit_form(field).unwrap();
        assert_eq!(session.form_violation(), None);
        assert_eq!(
            session.current_url().unwrap().as_str(),
            "https://a.test/go?q=x&not=y"
        );
    }

    #[test]
    fn a_valid_value_submits_and_clears_the_violation() {
        let mut session = Session::new(
            FakeLoader::new(&[
                (
                    "https://a.test/",
                    "<form action='/go'>\
                     <input type='number' name='n' min='1' max='99' value='123'>\
                     </form>",
                ),
                ("https://a.test/go?n=50", "<p>ok</p>"),
            ]),
            VIEWPORT,
        );
        session.load(url("https://a.test/")).unwrap();
        let field = control_with(&session, "name", "n");
        session.submit_form(field).unwrap();
        assert!(session.form_violation().is_some());
        // Correcting the value lets the same submit navigate.
        session.set_form_value(field, "50");
        session.submit_form(field).unwrap();
        assert_eq!(session.form_violation(), None);
        assert_eq!(
            session.current_url().unwrap().as_str(),
            "https://a.test/go?n=50"
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
    fn page_scripts_run_listen_and_time() {
        let mut session = Session::new(
            FakeLoader::new(&[(
                "https://a.test/",
                "<button id='b'>Art\u{131}r</button><p id='out'>0</p>\
                 <script>\
                 let n = 0;\
                 const out = document.getElementById('out');\
                 out.textContent = 'haz\u{131}r';\
                 document.getElementById('b').addEventListener('click', () => {\
                   n++; out.textContent = 'n=' + n;\
                 });\
                 setTimeout(() => { out.textContent = out.textContent + '!'; }, 100);\
                 </script>",
            )]),
            VIEWPORT,
        );
        session.load(url("https://a.test/")).unwrap();
        // The shell builds the script world after navigation.
        let mut scripts = PageScripts::new(&mut session).expect("page has scripts");
        let by_id = |session: &Session<FakeLoader>, id: &str| {
            let document = &session.page().unwrap().document;
            document
                .descendants(document.root())
                .find(|node| {
                    document
                        .element(*node)
                        .is_some_and(|element| element.attributes.get("id") == Some(id))
                })
                .unwrap()
        };
        let text = |session: &Session<FakeLoader>| {
            let out = by_id(session, "out");
            session.page().unwrap().document.text_content(out)
        };
        // The load-time script already ran.
        assert_eq!(text(&session), "haz\u{131}r");
        // Click dispatch reaches the listener and the page re-renders.
        let button = by_id(&session, "b");
        assert!(scripts.has_listener(button, "click"));
        assert!(scripts.dispatch(&mut session, button, "click").handled);
        assert!(scripts.dispatch(&mut session, button, "click").handled);
        assert_eq!(text(&session), "n=2");
        // Timers fire on tick.
        assert!(scripts.has_timers());
        assert!(!scripts.tick(&mut session, 50.0));
        assert!(scripts.tick(&mut session, 150.0));
        assert_eq!(text(&session), "n=2!");
        assert!(!scripts.has_timers());
    }

    #[test]
    fn keyframes_animations_drive_styles_and_finish() {
        let mut session = Session::new(
            FakeLoader::new(&[(
                "https://a.test/",
                "<style>@keyframes belir { from { opacity: 0; } to { opacity: 1; } }\
                 .kut { animation: belir 1s linear 1; }</style>\
                 <div class='kut' id='k'>x</div>",
            )]),
            VIEWPORT,
        );
        session.load(url("https://a.test/")).unwrap();
        let document = &session.page().unwrap().document;
        let node = document.get_element_by_id("k").unwrap();
        let opacity =
            |session: &Session<FakeLoader>| session.page().unwrap().styles.by_node[&node].opacity;
        assert!(session.tick(0.0)); // starts the clock
        assert!(opacity(&session) < 0.05, "{}", opacity(&session));
        session.tick(500.0);
        assert!(
            (opacity(&session) - 0.5).abs() < 0.05,
            "{}",
            opacity(&session)
        );
        session.tick(2000.0);
        assert!((opacity(&session) - 1.0).abs() < 0.01);
        // One iteration only: the animation retires.
        assert!(!session.tick(3000.0));
    }

    #[test]
    fn rotation_animations_spin_through_midpoints() {
        let mut session = Session::new(
            FakeLoader::new(&[(
                "https://a.test/",
                "<style>@keyframes don { from { transform: rotate(0deg); }\
                 to { transform: rotate(360deg); } }\
                 .d { animation: don 1s linear infinite; }</style>\
                 <div class='d' id='k'>x</div>",
            )]),
            VIEWPORT,
        );
        session.load(url("https://a.test/")).unwrap();
        let document = &session.page().unwrap().document;
        let node = document.get_element_by_id("k").unwrap();
        session.tick(0.0);
        session.tick(250.0); // quarter turn: rotate(90deg), b ≈ 1
        let transform = session.page().unwrap().styles.by_node[&node]
            .transform
            .expect("mid-spin transform");
        assert!((transform.b - 1.0).abs() < 0.01, "{transform:?}");
        // Infinite animations never retire.
        assert!(session.tick(10_000.0));
    }

    #[test]
    fn color_animation_ticks_damage_only_the_animated_box() {
        let mut session = Session::new(
            FakeLoader::new(&[(
                "https://a.test/",
                "<style>@keyframes c { from { background-color: #000000; } \
                 to { background-color: #ffffff; } }\
                 .kut { animation: c 1s linear infinite; width: 50px; height: 50px; }</style>\
                 <div class='kut' id='k'>x</div>",
            )]),
            VIEWPORT,
        );
        session.load(url("https://a.test/")).unwrap();
        // The shell's first raster consumes the load-time full repaint.
        session.note_rasterized();
        session.tick(0.0);
        let RepaintDamage::Region(Some(rect)) = session.repaint_damage() else {
            panic!("expected regional damage, got {:?}", session.repaint_damage());
        };
        let page = session.page().unwrap();
        let node = page.document.get_element_by_id("k").unwrap();
        let border = page.layout.find_by_node(node).unwrap().border_box();
        // The damage covers the animated box (plus paint outset) but is
        // far smaller than the page.
        assert!(rect.x <= border.x && rect.y <= border.y);
        assert!(rect.x + rect.width >= border.x + border.width);
        assert!(rect.y + rect.height >= border.y + border.height);
        assert!(rect.width < page.viewport.width);
        // The shell consumes the marker on raster; the next tick re-arms.
        session.note_rasterized();
        session.tick(16.0);
        assert!(matches!(
            session.repaint_damage(),
            RepaintDamage::Region(Some(_))
        ));
    }

    #[test]
    fn transform_animation_ticks_force_a_full_repaint() {
        let mut session = Session::new(
            FakeLoader::new(&[(
                "https://a.test/",
                "<style>@keyframes r { from { transform: rotate(0deg); } \
                 to { transform: rotate(360deg); } }\
                 .d { animation: r 1s linear infinite; }</style>\
                 <div class='d'>x</div>",
            )]),
            VIEWPORT,
        );
        session.load(url("https://a.test/")).unwrap();
        session.tick(0.0);
        // Transformed paint can land anywhere: no damage rect.
        assert_eq!(session.repaint_damage(), RepaintDamage::Full);
    }

    #[test]
    fn post_forms_send_urlencoded_bodies() {
        let mut session = Session::new(
            FakeLoader::new(&[
                (
                    "https://a.test/",
                    "<form method='post' action='/giris'>\
                     <input type='text' name='ad' value='lumen'>\
                     <input type='hidden' name='k' value='1'></form>",
                ),
                ("https://a.test/giris", "<p>girildi</p>"),
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
        session.submit_form(field).unwrap();
        // The action URL carries no query; the body carries the pairs.
        assert_eq!(
            session.current_url().unwrap().as_str(),
            "https://a.test/giris"
        );
        let bodies = session.loader.bodies.lock().unwrap();
        assert_eq!(bodies.last().unwrap().as_deref(), Some("ad=lumen&k=1"));
    }

    #[test]
    fn cookies_persist_across_navigations() {
        let mut session = Session::new(
            FakeLoader::new(&[
                ("https://a.test/", "<a href='/ic'>gir</a>"),
                ("https://a.test/ic", "<p>ic</p>"),
            ])
            .with_set_cookie("https://a.test/", "sid=gizli; Path=/"),
            VIEWPORT,
        );
        session.load(url("https://a.test/")).unwrap();
        session.follow("/ic").unwrap();
        let cookies = session.loader.cookies_sent.lock().unwrap();
        // First request: no cookie yet; second carries the session id.
        assert_eq!(cookies[0], None);
        assert_eq!(cookies[1].as_deref(), Some("sid=gizli"));
    }

    #[test]
    fn document_cookie_reads_and_writes_the_jar() {
        let mut session = Session::new(
            FakeLoader::new(&[(
                "https://a.test/",
                "<p id='out'>-</p>\
                 <script>\
                 document.cookie = 'tema=koyu; Path=/';\
                 document.getElementById('out').textContent = document.cookie;\
                 </script>",
            )])
            .with_set_cookie("https://a.test/", "sid=abc; Path=/"),
            VIEWPORT,
        );
        session.load(url("https://a.test/")).unwrap();
        let _scripts = PageScripts::new(&mut session).expect("page has scripts");
        let document = &session.page().unwrap().document;
        let out = document.get_element_by_id("out").unwrap();
        // The script saw the network cookie plus its own write.
        assert_eq!(document.text_content(out), "sid=abc; tema=koyu");
        // And the write landed in the jar for future requests.
        assert_eq!(
            session
                .cookies
                .header_for_http(&url("https://a.test/x"))
                .unwrap(),
            "sid=abc; tema=koyu"
        );
    }

    #[test]
    fn http_only_cookies_are_hidden_from_document_cookie() {
        let mut session = Session::new(
            FakeLoader::new(&[(
                "https://a.test/",
                "<p id='out'>-</p>\
                 <script>\
                 document.getElementById('out').textContent = document.cookie;\
                 </script>",
            )])
            .with_set_cookie("https://a.test/", "sid=abc; HttpOnly; Path=/")
            .with_set_cookie("https://a.test/", "tema=koyu; Path=/"),
            VIEWPORT,
        );
        session.load(url("https://a.test/")).unwrap();
        let _scripts = PageScripts::new(&mut session).expect("page has scripts");
        let document = &session.page().unwrap().document;
        let out = document.get_element_by_id("out").unwrap();
        // The script only saw the script-visible cookie…
        assert_eq!(document.text_content(out), "tema=koyu");
        // …but the HttpOnly one still rides HTTP requests.
        assert_eq!(
            session
                .cookies
                .header_for_http(&url("https://a.test/x"))
                .unwrap(),
            "sid=abc; tema=koyu"
        );
    }

    #[test]
    fn secure_set_cookie_over_http_is_ignored() {
        let mut session = Session::new(
            FakeLoader::new(&[
                ("http://a.test/", "<a href='/iki'>x</a>"),
                ("http://a.test/iki", "<p>iki</p>"),
            ])
            .with_set_cookie("http://a.test/", "sid=abc; Secure; Path=/"),
            VIEWPORT,
        );
        session.load(url("http://a.test/")).unwrap();
        session.follow("/iki").unwrap();
        let cookies = session.loader.cookies_sent.lock().unwrap();
        // The insecure origin's Secure cookie was never stored.
        assert_eq!(cookies[1], None);
    }

    #[test]
    fn document_cookie_cannot_write_secure_over_http() {
        let mut session = Session::new(
            FakeLoader::new(&[(
                "http://a.test/",
                "<script>document.cookie = 's=1; Secure; Path=/';</script>",
            )]),
            VIEWPORT,
        );
        session.load(url("http://a.test/")).unwrap();
        let _scripts = PageScripts::new(&mut session).expect("page has scripts");
        assert!(
            session
                .cookies
                .header_for_http(&url("http://a.test/"))
                .is_none()
        );
    }

    #[test]
    fn remote_pages_cannot_fetch_file_urls() {
        let mut session = Session::new(
            FakeLoader::new(&[(
                "https://a.test/",
                "<link rel='stylesheet' href='file:///etc/passwd'><p>hi</p>",
            )]),
            VIEWPORT,
        );
        session.load(url("https://a.test/")).unwrap();
        // The page still renders; the file: stylesheet never reached the loader.
        assert!(session.page().is_some());
        assert_eq!(
            *session.loader.loads.lock().unwrap(),
            vec!["https://a.test/"]
        );
    }

    #[test]
    fn scripts_request_focus_changes() {
        let mut session = Session::new(
            FakeLoader::new(&[(
                "https://a.test/",
                "<input id='alan'>\
                 <script>document.getElementById('alan').focus();</script>",
            )]),
            VIEWPORT,
        );
        session.load(url("https://a.test/")).unwrap();
        let mut scripts = PageScripts::new(&mut session).expect("page has scripts");
        let document = &session.page().unwrap().document;
        let field = document.get_element_by_id("alan").unwrap();
        assert_eq!(scripts.take_focus_request(), Some(Some(field)));
        assert_eq!(scripts.take_focus_request(), None);
    }

    #[test]
    fn prevent_default_reaches_the_dispatcher() {
        let mut session = Session::new(
            FakeLoader::new(&[(
                "https://a.test/",
                "<a id='l' href='/x'>git</a><a id='m' href='/y'>serbest</a>\
                 <script>\
                 document.getElementById('l').addEventListener('click', (e) => {\
                   e.preventDefault();\
                 });\
                 document.getElementById('m').addEventListener('click', () => {});\
                 </script>",
            )]),
            VIEWPORT,
        );
        session.load(url("https://a.test/")).unwrap();
        let mut scripts = PageScripts::new(&mut session).expect("page has scripts");
        let document = &session.page().unwrap().document;
        let blocked = document.get_element_by_id("l").unwrap();
        let free = document.get_element_by_id("m").unwrap();
        let outcome = scripts.dispatch(&mut session, blocked, "click");
        assert!(outcome.handled && outcome.prevented);
        let outcome = scripts.dispatch(&mut session, free, "click");
        assert!(outcome.handled && !outcome.prevented);
    }

    #[test]
    fn fetch_resolves_with_page_relative_resources() {
        let mut session = Session::new(
            FakeLoader::new(&[
                (
                    "https://a.test/",
                    "<p id='out'>bekliyor</p>\
                     <script>\
                     fetch('/veri.json')\
                       .then((response) => response.json())\
                       .then((data) => {\
                         document.getElementById('out').textContent =\
                           data.ad + ' ' + data.sayilar.length;\
                       });\
                     </script>",
                ),
                (
                    "https://a.test/veri.json",
                    "{\"ad\": \"lumen\", \"sayilar\": [1, 2, 3]}",
                ),
            ]),
            VIEWPORT,
        );
        session.load(url("https://a.test/")).unwrap();
        let _scripts = PageScripts::new(&mut session).expect("page has scripts");
        let document = &session.page().unwrap().document;
        let out = document.get_element_by_id("out").unwrap();
        assert_eq!(document.text_content(out), "lumen 3");
    }

    #[test]
    fn scripts_request_navigation_via_location() {
        let mut session = Session::new(
            FakeLoader::new(&[(
                "https://a.test/",
                "<button id='git'>git</button>\
                 <script>\
                 console.log(location.href);\
                 document.getElementById('git').addEventListener('click', () => {\
                   location.href = '/sonraki';\
                 });\
                 </script>",
            )]),
            VIEWPORT,
        );
        session.load(url("https://a.test/")).unwrap();
        let mut scripts = PageScripts::new(&mut session).expect("page has scripts");
        assert!(scripts.take_navigation().is_none());
        let document = &session.page().unwrap().document;
        let button = document.get_element_by_id("git").unwrap();
        scripts.dispatch(&mut session, button, "click");
        assert_eq!(scripts.take_navigation().as_deref(), Some("/sonraki"));
    }

    #[test]
    fn scripts_create_append_and_remove_elements() {
        let mut session = Session::new(
            FakeLoader::new(&[(
                "https://a.test/",
                "<ul id='list'><li id='eski'>eski</li></ul>\
                 <script>\
                 const list = document.getElementById('list');\
                 const li = document.createElement('li');\
                 li.textContent = 'yeni';\
                 list.appendChild(li);\
                 document.getElementById('eski').remove();\
                 </script>",
            )]),
            VIEWPORT,
        );
        session.load(url("https://a.test/")).unwrap();
        let _scripts = PageScripts::new(&mut session).expect("page has scripts");
        let document = &session.page().unwrap().document;
        let list = document.get_element_by_id("list").unwrap();
        assert_eq!(document.text_content(list).trim(), "\u{2022} yeni");
        assert_eq!(document.children(list).len(), 1);
        // The removed node left the tree entirely.
        assert!(document.get_element_by_id("eski").is_none());
    }

    #[test]
    fn session_owned_editing_types_selects_and_overlays() {
        let mut session = Session::new(
            FakeLoader::new(&[(
                "https://a.test/",
                "<form><input type='text' name='q' value='abc'></form>",
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
        assert!(session.begin_edit(field, None));
        assert_eq!(session.editing(), Some(field));
        // Everything starts selected: typing replaces the value.
        session.edit(EditOp::Insert("merhaba".to_string()));
        assert_eq!(session.form_value(field), "merhaba");
        // Select-all then word-left selection math still works.
        session.edit(EditOp::SelectAll);
        assert_eq!(
            session.edit_buffer().unwrap().selected_text(),
            "merhaba".to_string()
        );
        let overlay = session.edit_overlay().expect("overlay while editing");
        assert!(overlay.caret.is_some());
        assert!(overlay.selection.is_some());
        // Ending the edit clears the state.
        session.end_edit();
        assert_eq!(session.editing(), None);
        assert!(session.edit_overlay().is_none());
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
    fn typing_reuses_styles_but_matches_full_layout() {
        // Live typing takes the style-reuse fast path; its geometry must
        // match a full build of the same final value, including where a
        // following sibling lands.
        let typed = {
            let mut session = Session::new(
                FakeLoader::new(&[(
                    "https://a.test/",
                    "<form><input id='q' type='text' value=''><p id='after'>x</p></form>",
                )]),
                VIEWPORT,
            );
            session.load(url("https://a.test/")).unwrap();
            let field = session
                .page()
                .unwrap()
                .document
                .get_element_by_id("q")
                .unwrap();
            assert!(session.begin_edit(field, None));
            session.edit(EditOp::Insert("hello world".to_string()));
            session
        };
        let baked = {
            let mut session = Session::new(
                FakeLoader::new(&[(
                    "https://a.test/",
                    "<form><input id='q' type='text' value='hello world'><p id='after'>x</p></form>",
                )]),
                VIEWPORT,
            );
            session.load(url("https://a.test/")).unwrap();
            session
        };
        let box_of = |session: &Session<FakeLoader>, id: &str| {
            let page = session.page().unwrap();
            let node = page.document.get_element_by_id(id).unwrap();
            page.layout.find_by_node(node).unwrap().border_box()
        };
        for id in ["q", "after"] {
            let (fast, full) = (box_of(&typed, id), box_of(&baked, id));
            assert!(
                (fast.x - full.x).abs() < 0.5
                    && (fast.y - full.y).abs() < 0.5
                    && (fast.width - full.width).abs() < 0.5
                    && (fast.height - full.height).abs() < 0.5,
                "fast-path box for #{id} {fast:?} != full {full:?}"
            );
        }
        assert_eq!(
            typed.form_value(
                typed
                    .page()
                    .unwrap()
                    .document
                    .get_element_by_id("q")
                    .unwrap()
            ),
            "hello world"
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
        let rendered = session.page().unwrap().display_list.iter().any(|command| {
            matches!(command,
                lumen_engine::DisplayCommand::DrawText { text, .. } if text == "a   b")
        });
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
    fn number_input_clamps_typed_values_to_min_max() {
        let mut session = Session::new(
            FakeLoader::new(&[(
                "https://a.test/",
                "<form><input type='number' name='n' value='' min='0' max='99'></form>",
            )]),
            VIEWPORT,
        );
        session.load(url("https://a.test/")).unwrap();
        let field = {
            let document = &session.page().unwrap().document;
            document
                .descendants(document.root())
                .find(|id| {
                    document
                        .element(*id)
                        .is_some_and(|element| element.tag_name == "input")
                })
                .unwrap()
        };
        assert!(session.begin_edit(field, None));
        // Typing past the maximum clamps on the spot: 1, 12, 123 -> 99.
        session.edit(EditOp::Insert("1".to_string()));
        session.edit(EditOp::Insert("2".to_string()));
        session.edit(EditOp::Insert("3".to_string()));
        assert_eq!(session.form_value(field), "99");
        // The lower bound is NOT enforced mid-edit: "-123" must stay
        // exactly as typed instead of degenerating to "023".
        session.edit(EditOp::SelectAll);
        for digit in ["-", "1", "2", "3"] {
            session.edit(EditOp::Insert(digit.to_string()));
        }
        assert_eq!(session.form_value(field), "-123");
        // ...it clamps up when editing ends.
        session.end_edit();
        assert_eq!(session.form_value(field), "0");
        // A partial state like "-" is left alone mid-edit.
        assert!(session.begin_edit(field, None));
        session.edit(EditOp::SelectAll);
        session.edit(EditOp::Insert("-".to_string()));
        assert_eq!(session.form_value(field), "-");
    }

    #[test]
    fn number_input_below_min_is_not_mangled_while_typing() {
        let mut session = Session::new(
            FakeLoader::new(&[(
                "https://a.test/",
                "<form><input type='number' name='n' value='' min='5' max='99'></form>",
            )]),
            VIEWPORT,
        );
        session.load(url("https://a.test/")).unwrap();
        let field = {
            let document = &session.page().unwrap().document;
            document
                .descendants(document.root())
                .find(|id| {
                    document
                        .element(*id)
                        .is_some_and(|element| element.tag_name == "input")
                })
                .unwrap()
        };
        // With min=5, typing "35" must not turn the leading "3" into "5".
        assert!(session.begin_edit(field, None));
        session.edit(EditOp::Insert("3".to_string()));
        assert_eq!(session.form_value(field), "3");
        session.edit(EditOp::Insert("5".to_string()));
        assert_eq!(session.form_value(field), "35");
        // Left below the minimum, the value clamps up when editing ends.
        session.edit(EditOp::SelectAll);
        session.edit(EditOp::Insert("2".to_string()));
        session.end_edit();
        assert_eq!(session.form_value(field), "5");
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
        assert_eq!(
            background(&session),
            lumen_css::Color::rgb(0x22, 0x66, 0xaa)
        );
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
        // Progressive loading renders first; drive it synchronously.
        session.load_pending_images_blocking();
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
        assert_eq!(session.loader.loads.lock().unwrap().len(), 1);
    }
}
