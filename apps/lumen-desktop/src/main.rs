//! Lumen desktop shell: a native window that shows pages rendered by the
//! custom pipeline (no system WebView — the window is just a pixel
//! surface for the display-list rasterizer).
//!
//! Usage:
//!
//! ```bash
//! cargo run -p lumen-desktop -- examples/card.html
//! cargo run -p lumen-desktop -- https://example.com
//! ```
//!
//! Keys: arrows / PageUp / PageDown / Home scroll, `r` refreshes,
//! `[` / `]` go back / forward, `l` (or clicking the bar) edits the URL,
//! Enter navigates, Escape cancels editing. Drag over text to select it;
//! Cmd/Ctrl+C copies and Cmd/Ctrl+A selects the whole page. Cmd/Ctrl+F
//! opens the find bar (type to search, Enter cycles matches, Escape
//! closes). F12 toggles a debug HUD with FPS, frame
//! times, memory and page statistics. The address and find inputs support full editing: caret
//! movement, Shift+arrows selection, Home/End, Cmd/Ctrl+A/C/X/V.

mod popups;
mod text_input;

use lumen_browser::{EditOp, Motion, Session};
use lumen_engine::{
    Caret, DisplayCommand, HeuristicMeasurer, Rect, Selection, Size, SystemFont, TextRun,
    caret_at_point, collect_text_runs, highlight_rects, rasterize_over, rasterize_region,
    rasterize_with, selected_text,
};
use lumen_engine::{FontWeight, TextMeasurer, TextMetrics, TextStyle};
use lumen_platform::{DefaultLoader, Url, url_from_user_input};
use popups::{
    COLOR_SWATCHES, ColorPopup, SELECT_ROW_HEIGHT, SWATCH_COLUMNS, SWATCH_GAP, SWATCH_SIZE,
    SelectPopup, ViolationPopup, rect_contains, swatch_rect, violation_rect,
};
use std::cell::RefCell;
use std::collections::HashMap;
use std::collections::VecDeque;
use std::num::NonZeroU32;
use std::rc::Rc;
use std::sync::Arc;
use std::time::{Duration, Instant};
use text_input::{EditOutcome, TextInput, apply_edit};
use winit::application::ApplicationHandler;
use winit::event::Modifiers;
use winit::event::{ElementState, KeyEvent, MouseButton, MouseScrollDelta, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop};
use winit::keyboard::{Key, NamedKey};
use winit::window::CursorIcon;
use winit::window::{Window, WindowId};

const SCROLL_STEP: f32 = 48.0;
/// Address-bar height in CSS pixels.
const BAR_HEIGHT: f32 = 36.0;
/// The tab strip sits directly below the address bar.
const TAB_HEIGHT: f32 = 30.0;
/// Total chrome height above the page: address bar + tab strip. The page
/// content and every page-space overlay are offset by this.
const CHROME_HEIGHT: f32 = BAR_HEIGHT + TAB_HEIGHT;

/// A navigation action executed on a background thread, so slow servers
/// never freeze the UI.
enum Nav {
    /// GET-submit the form containing this node.
    Submit(usize),
    Load(Url),
    Follow(String),
    Back,
    Forward,
    Refresh,
}

impl Nav {
    fn label(&self) -> String {
        match self {
            Self::Submit(_) => "form".to_string(),
            Self::Load(url) => url.to_string(),
            Self::Follow(href) => href.clone(),
            Self::Back => "back".to_string(),
            Self::Forward => "forward".to_string(),
            Self::Refresh => "refresh".to_string(),
        }
    }
}

/// Sent back from the loader thread when a navigation finishes.
struct NavDone {
    /// Id of the tab that started the navigation; the user may have
    /// switched away (or closed it) while the loader was working.
    tab: u64,
    session: Box<Session<DefaultLoader>>,
    error: Option<String>,
}

/// The session is either usable or away on a loader thread.
enum SessionState {
    Ready(Box<Session<DefaultLoader>>),
    Loading { target: String },
}

/// One browser tab's swappable state. The active tab's copy lives in the
/// `App` fields directly (`state`, `scroll_y`, `input`, `page_scripts`);
/// this holds the parked state of every other tab. Loader results carry
/// the tab id they started from, so a result lands in its own tab even
/// when the user switched away mid-load.
struct Tab {
    /// Stable identity: survives switching and positional shifts.
    id: u64,
    state: SessionState,
    scroll_y: f32,
    input: String,
    page_scripts: Option<lumen_browser::PageScripts>,
}

/// A saved page.
#[derive(Clone, serde::Serialize, serde::Deserialize)]
struct Bookmark {
    title: String,
    url: String,
}

/// The bookmark list, persisted as JSON in the platform config directory
/// (`<config>/lumen/bookmarks.json`), the way a browser keeps them.
#[derive(Default, serde::Serialize, serde::Deserialize)]
struct Bookmarks {
    items: Vec<Bookmark>,
}

impl Bookmarks {
    /// `<config>/lumen/bookmarks.json`, if a config directory exists.
    fn path() -> Option<std::path::PathBuf> {
        Some(dirs::config_dir()?.join("lumen").join("bookmarks.json"))
    }

    /// Loads the saved bookmarks, or an empty list when none exist.
    fn load() -> Self {
        let Some(path) = Self::path() else {
            return Self::default();
        };
        std::fs::read(&path)
            .ok()
            .and_then(|bytes| serde_json::from_slice(&bytes).ok())
            .unwrap_or_default()
    }

    /// Writes the list back to disk, creating the directory as needed.
    fn save(&self) {
        let Some(path) = Self::path() else { return };
        if let Some(parent) = path.parent()
            && let Err(error) = std::fs::create_dir_all(parent)
        {
            eprintln!("bookmarks: {error}");
            return;
        }
        match serde_json::to_vec_pretty(self) {
            Ok(bytes) => {
                if let Err(error) = std::fs::write(&path, bytes) {
                    eprintln!("bookmarks: {error}");
                }
            }
            Err(error) => eprintln!("bookmarks: {error}"),
        }
    }

    fn contains(&self, url: &str) -> bool {
        self.items.iter().any(|item| item.url == url)
    }

    /// Adds or removes `url`; returns whether it is now bookmarked.
    fn toggle(&mut self, url: &str, title: &str) -> bool {
        if let Some(index) = self.items.iter().position(|item| item.url == url) {
            self.items.remove(index);
            self.save();
            false
        } else {
            self.items.push(Bookmark {
                title: title.to_string(),
                url: url.to_string(),
            });
            self.save();
            true
        }
    }
}

fn main() {
    // Split flags from the positional file/URL: `--debug`/`-d` opens the
    // HUD on launch.
    let mut debug = false;
    let mut input = None;
    for arg in std::env::args().skip(1) {
        match arg.as_str() {
            "--debug" | "-d" => debug = true,
            _ if input.is_none() => input = Some(arg),
            _ => {}
        }
    }
    let Some(input) = input else {
        eprintln!("usage: lumen-desktop [--debug] <file-or-url>");
        std::process::exit(2);
    };

    let event_loop = match EventLoop::<NavDone>::with_user_event().build() {
        Ok(event_loop) => event_loop,
        Err(error) => {
            eprintln!("error: cannot start event loop: {error}");
            std::process::exit(1);
        }
    };
    let proxy = event_loop.create_proxy();

    let mut app = App::new(input, proxy);
    app.debug_hud = debug;
    if let Err(error) = event_loop.run_app(&mut app) {
        eprintln!("error: {error}");
        std::process::exit(1);
    }
}

/// Shares one loaded font between the session's measurer (which may move
/// to a loader thread) and the rasterizer.
#[derive(Clone)]
struct SharedFont(Arc<SystemFont>);

impl TextMeasurer for SharedFont {
    fn measure(&self, text: &str, style: &TextStyle) -> TextMetrics {
        self.0.measure(text, style)
    }
}

/// What a page-input key resolved to: an edit op for the session, or a
/// shell-level action.
enum PageEdit {
    Op(EditOp),
    Submit,
    Cancel,
    Copy,
    Cut,
    Paste,
}

/// Translates a key event into a page edit action (clipboard stays with
/// the shell; the session owns the buffer).
fn page_edit_action(key: &Key, command: bool, shift: bool, alt: bool) -> Option<PageEdit> {
    Some(match key {
        Key::Named(NamedKey::Enter) => PageEdit::Submit,
        Key::Named(NamedKey::Escape) => PageEdit::Cancel,
        Key::Named(NamedKey::Backspace) => PageEdit::Op(EditOp::Backspace { word: alt }),
        Key::Named(NamedKey::Delete) => PageEdit::Op(EditOp::DeleteForward),
        Key::Named(NamedKey::ArrowLeft) => PageEdit::Op(EditOp::Move {
            motion: if command {
                Motion::LineStart
            } else if alt {
                Motion::WordLeft
            } else {
                Motion::Left
            },
            select: shift,
        }),
        Key::Named(NamedKey::ArrowRight) => PageEdit::Op(EditOp::Move {
            motion: if command {
                Motion::LineEnd
            } else if alt {
                Motion::WordRight
            } else {
                Motion::Right
            },
            select: shift,
        }),
        Key::Named(NamedKey::Home) => PageEdit::Op(EditOp::Move {
            motion: Motion::LineStart,
            select: shift,
        }),
        Key::Named(NamedKey::End) => PageEdit::Op(EditOp::Move {
            motion: Motion::LineEnd,
            select: shift,
        }),
        Key::Named(NamedKey::Space) => PageEdit::Op(EditOp::Insert(" ".to_string())),
        Key::Character(text) if command => match text.as_str() {
            "a" => PageEdit::Op(EditOp::SelectAll),
            "c" => PageEdit::Copy,
            "x" => PageEdit::Cut,
            "v" => PageEdit::Paste,
            _ => return None,
        },
        Key::Character(text) => PageEdit::Op(EditOp::Insert(text.to_string())),
        _ => return None,
    })
}

/// The slot of tab `id`, if it still exists — a navigation result whose
/// tab closed mid-load has nowhere to land and is dropped.
fn tab_position(tabs: &[Tab], id: u64) -> Option<usize> {
    tabs.iter().position(|tab| tab.id == id)
}

/// The session's inner scroll offsets (empty map while loading).
fn session_offsets(state: &SessionState) -> &std::collections::HashMap<usize, f32> {
    static EMPTY: std::sync::OnceLock<std::collections::HashMap<usize, f32>> =
        std::sync::OnceLock::new();
    match state {
        SessionState::Ready(session) => session.scroll_offsets(),
        SessionState::Loading { .. } => EMPTY.get_or_init(std::collections::HashMap::new),
    }
}

/// Draws proportional thumbs on `overflow: scroll/auto` boxes.
fn draw_inner_scrollbars(
    framebuffer: &mut lumen_engine::Framebuffer,
    layout: &lumen_engine::LayoutBox,
    offsets: &std::collections::HashMap<usize, f32>,
    page_scroll: f32,
    scale: f32,
) {
    let max = layout.max_inner_scroll();
    if max > 0.0 && layout.style.overflow == lumen_engine::Overflow::Scroll {
        let content = layout.content_box();
        let offset = offsets.get(&layout.node_id).copied().unwrap_or(0.0);
        let track = content.height;
        let thumb = (track * track / (track + max)).max(12.0);
        let y = content.y + (track - thumb) * (offset / max).clamp(0.0, 1.0);
        framebuffer.blend_fill(
            lumen_engine::Rect {
                x: (content.x + content.width - 5.0) * scale,
                y: (y - page_scroll + CHROME_HEIGHT) * scale,
                width: 3.0 * scale,
                height: thumb * scale,
            },
            lumen_css::Color::rgb(0x55, 0x52, 0x5c),
            130,
        );
    }
    for child in &layout.children {
        draw_inner_scrollbars(framebuffer, child, offsets, page_scroll, scale);
    }
}

fn count_boxes(layout: &lumen_engine::LayoutBox) -> usize {
    1 + layout.children.iter().map(count_boxes).sum::<usize>()
}

/// Clipboard text bound for a form control: single-line inputs flatten
/// newlines into spaces, multiline textareas keep them.
fn paste_text(pasted: String, multiline: bool) -> String {
    if multiline {
        pasted
    } else {
        pasted.replace(['\n', '\r'], " ")
    }
}

/// Gets or rebuilds a generation-keyed cache slot: a hit returns the
/// stored value, a bumped generation rebuilds exactly once.
fn cached<T>(slot: &mut Option<(u64, T)>, generation: u64, build: impl FnOnce() -> T) -> &T {
    if !matches!(slot, Some((key, _)) if *key == generation) {
        *slot = Some((generation, build()));
    }
    match slot {
        Some((_, value)) => value,
        None => unreachable!("just stored"),
    }
}

/// One text run prepared for find-in-page: the ASCII-lowercased text plus
/// the byte offset of each char boundary, so match byte offsets convert to
/// caret (char) offsets without rescanning the string.
struct FindRun {
    lower: String,
    char_starts: Vec<usize>,
}

impl FindRun {
    fn new(text: &str) -> Self {
        // ASCII-only lowercasing never changes byte or char lengths, so
        // these boundaries also index the original text.
        let lower = text.to_ascii_lowercase();
        let char_starts = lower.char_indices().map(|(index, _)| index).collect();
        Self { lower, char_starts }
    }

    /// The char offset of a byte offset that sits on a char boundary.
    fn char_offset(&self, byte: usize) -> usize {
        self.char_starts.partition_point(|start| *start < byte)
    }
}

/// All (case-insensitive) matches of `needle` across the runs, as
/// selections in caret offsets. Matches never span runs.
fn find_matches_in_runs(runs: &[FindRun], needle: &str) -> Vec<Selection> {
    let needle_chars = needle.chars().count();
    let mut matches = Vec::new();
    for (index, run) in runs.iter().enumerate() {
        // match_indices yields byte offsets, always on char boundaries.
        for (byte_start, _) in run.lower.match_indices(needle) {
            let start = run.char_offset(byte_start);
            matches.push(Selection {
                anchor: Caret {
                    run: index,
                    offset: start,
                },
                focus: Caret {
                    run: index,
                    offset: start + needle_chars,
                },
            });
        }
    }
    matches
}

/// Ellipsizes `text` to fit `max_width`: keeps the longest char prefix
/// whose width with a trailing "…" still fits, then appends "…". The
/// caller handles the fits-unclipped case; `measure` must be monotone
/// over prefixes (binary search replaces the old O(n²) char-at-a-time
/// re-measurement).
fn clip_with_ellipsis(text: &str, max_width: f32, measure: impl Fn(&str) -> f32) -> String {
    // Byte offsets of char boundaries; boundaries[k] ends the k-char prefix.
    let mut boundaries: Vec<usize> = text.char_indices().map(|(index, _)| index).collect();
    boundaries.push(text.len());
    let fits = |chars: usize| {
        let mut candidate = String::with_capacity(boundaries[chars] + 3);
        candidate.push_str(&text[..boundaries[chars]]);
        candidate.push('…');
        measure(&candidate) <= max_width
    };
    // The largest fitting prefix.
    let (mut low, mut high) = (0usize, boundaries.len() - 1);
    while low < high {
        let mid = low + (high - low).div_ceil(2);
        if fits(mid) {
            low = mid;
        } else {
            high = mid - 1;
        }
    }
    let mut clipped = String::with_capacity(boundaries[low] + 3);
    clipped.push_str(&text[..boundaries[low]]);
    clipped.push('…');
    clipped
}

fn clipboard_set(text: &str) {
    if text.is_empty() {
        return;
    }
    match arboard::Clipboard::new() {
        Ok(mut clipboard) => {
            if let Err(error) = clipboard.set_text(text.to_string()) {
                eprintln!("clipboard: {error}");
            }
        }
        Err(error) => eprintln!("clipboard: {error}"),
    }
}

fn clipboard_get() -> Option<String> {
    arboard::Clipboard::new().ok()?.get_text().ok()
}

/// Which chrome text bar a key event is being routed to.
#[derive(Clone, Copy, PartialEq, Eq)]
enum EditBar {
    Find,
    Url,
}

struct App {
    input: String,
    state: SessionState,
    proxy: winit::event_loop::EventLoopProxy<NavDone>,
    font: Option<Arc<SystemFont>>,
    window: Option<Rc<Window>>,
    surface: Option<softbuffer::Surface<Rc<Window>, Rc<Window>>>,
    scroll_y: f32,
    /// Last cursor position in CSS pixels (window coordinates, bar included).
    cursor: Option<(f32, f32)>,
    /// The URL text being edited, when the address bar has focus.
    url_input: Option<TextInput>,
    /// Where the left button went down (CSS window coords), while held.
    press: Option<(f32, f32)>,
    /// Selection anchor caret while dragging.
    select_anchor: Option<Caret>,
    selection: Option<Selection>,
    modifiers: Modifiers,
    /// The find-bar query, when Ctrl/Cmd+F is active.
    find_input: Option<TextInput>,
    /// Whether the current mouse press is drag-selecting inside the
    /// edited text control (suppresses page text selection).
    input_drag: bool,
    /// Open select dropdown: (select node, options, page-coords rect of
    /// the list, index under the pointer).
    select_popup: Option<SelectPopup>,
    /// Open color-input palette.
    color_popup: Option<ColorPopup>,
    /// Open form-validation bubble (a blocked submit's message).
    violation_popup: Option<ViolationPopup>,
    /// The current page's JavaScript world (main-thread only: Boa's GC
    /// handles cannot cross the loader thread).
    page_scripts: Option<lumen_browser::PageScripts>,
    /// Range input being dragged, while the button is held.
    range_drag: Option<usize>,
    find_matches: Vec<Selection>,
    find_index: usize,
    /// Damage tracking: bumped whenever the page raster could change.
    page_generation: u64,
    /// Monotonic clock origin for animation ticks.
    started: Instant,
    /// Debug HUD (F12): FPS, memory, frame + page stats.
    debug_hud: bool,
    /// Recent frames: (when it finished, how long it took).
    frame_times: VecDeque<(Instant, Duration)>,
    /// How the last frame produced its page pixels.
    last_frame_kind: &'static str,
    /// Resident memory, refreshed at most once a second via `ps`.
    rss_megabytes: Option<f64>,
    rss_checked: Option<Instant>,
    /// Cached page raster keyed by (generation, scroll, size).
    page_frame: Option<((u64, u32, u32, u32), lumen_engine::Framebuffer)>,
    /// Reused per-redraw composition buffer; overlays and chrome draw here
    /// so the cached page raster stays pristine without a fresh allocation
    /// on every frame.
    compose_frame: Option<lumen_engine::Framebuffer>,
    /// The page's text runs, collected at most once per page generation
    /// and shared by hover hit-testing, the caret, selection and find
    /// (each used to re-walk the whole layout on its own). The inner
    /// `Option` is `None` while no page is loaded.
    text_runs: Option<(u64, Option<Rc<Vec<TextRun>>>)>,
    /// Find-in-page's per-run lowercase + boundary tables, rebuilt only
    /// when the page generation changes — not on every keystroke.
    find_runs: Option<(u64, Rc<Vec<FindRun>>)>,
    /// Ellipsized chrome strings keyed by (text, font size bits, max width
    /// bits, web-font identity); tabs and bookmark rows re-clip the same
    /// strings every frame.
    clip_cache: RefCell<HashMap<(String, u32, u32, usize), String>>,
    /// Parked tabs (all except the active one, whose live state is in the
    /// fields above). Indexed positionally; `active` selects the live one.
    tabs: Vec<Tab>,
    /// Index of the active tab within the strip. The parked entry at this
    /// index is a placeholder — the live state is in the `App` fields.
    active: usize,
    /// Next never-reused tab id.
    next_tab_id: u64,
    /// Saved pages, persisted to disk.
    bookmarks: Bookmarks,
    /// Whether the bookmarks dropdown is open.
    bookmarks_open: bool,
}

impl App {
    fn new(input: String, proxy: winit::event_loop::EventLoopProxy<NavDone>) -> Self {
        let font = SystemFont::load_default().map(Arc::new);
        if font.is_none() {
            eprintln!("note: no system font found, using the built-in bitmap font");
        }
        let mut session = Session::new(
            DefaultLoader,
            Size {
                width: 1024.0,
                height: 768.0,
            },
        );
        if let Some(font) = &font {
            session.set_measurer(Box::new(SharedFont(font.clone())));
        }
        Self {
            input,
            state: SessionState::Ready(Box::new(session)),
            proxy,
            font,
            window: None,
            surface: None,
            scroll_y: 0.0,
            cursor: None,
            url_input: None,
            press: None,
            select_anchor: None,
            selection: None,
            modifiers: Modifiers::default(),
            find_input: None,
            input_drag: false,
            select_popup: None,
            color_popup: None,
            violation_popup: None,
            page_scripts: None,
            range_drag: None,
            find_matches: Vec::new(),
            find_index: 0,
            page_generation: 0,
            started: Instant::now(),
            debug_hud: false,
            frame_times: VecDeque::new(),
            last_frame_kind: "full",
            rss_megabytes: None,
            rss_checked: None,
            page_frame: None,
            compose_frame: None,
            text_runs: None,
            find_runs: None,
            clip_cache: RefCell::new(HashMap::new()),
            // A single placeholder tab; its parked fields are overwritten by
            // `park_active` before they are ever read.
            tabs: vec![Tab {
                id: 0,
                state: SessionState::Loading {
                    target: String::new(),
                },
                scroll_y: 0.0,
                input: String::new(),
                page_scripts: None,
            }],
            active: 0,
            next_tab_id: 1,
            bookmarks: Bookmarks::load(),
            bookmarks_open: false,
        }
    }

    /// A fresh, measurer-equipped session for a new tab.
    fn blank_session(&self) -> Session<DefaultLoader> {
        let size = self.viewport();
        let mut session = Session::new(DefaultLoader, size);
        if let Some(font) = &self.font {
            session.set_measurer(Box::new(SharedFont(font.clone())));
        }
        session
    }

    /// Stashes the live active-tab state into its parked slot.
    fn park_active(&mut self) {
        let placeholder = SessionState::Loading {
            target: String::new(),
        };
        let tab = &mut self.tabs[self.active];
        tab.state = std::mem::replace(&mut self.state, placeholder);
        tab.scroll_y = self.scroll_y;
        tab.input = std::mem::take(&mut self.input);
        tab.page_scripts = self.page_scripts.take();
    }

    /// Pulls the parked state at `self.active` into the live fields.
    fn unpark_active(&mut self) {
        let placeholder = SessionState::Loading {
            target: String::new(),
        };
        let tab = &mut self.tabs[self.active];
        self.state = std::mem::replace(&mut tab.state, placeholder);
        self.scroll_y = tab.scroll_y;
        self.input = std::mem::take(&mut tab.input);
        self.page_scripts = tab.page_scripts.take();
    }

    /// Clears per-page transient UI when switching tabs (selection, find,
    /// popups, cached raster).
    fn reset_transient(&mut self) {
        self.url_input = None;
        self.find_input = None;
        self.find_matches.clear();
        self.find_index = 0;
        self.selection = None;
        self.select_anchor = None;
        self.select_popup = None;
        self.color_popup = None;
        self.violation_popup = None;
        self.range_drag = None;
        self.page_frame = None;
        self.invalidate_page();
        self.request_redraw();
    }

    /// Switches to the tab at `index`, parking the current one.
    fn switch_tab(&mut self, index: usize) {
        if index == self.active || index >= self.tabs.len() {
            return;
        }
        self.park_active();
        self.active = index;
        self.unpark_active();
        self.reset_transient();
    }

    /// Opens a blank tab, switches to it, and focuses the address bar.
    fn new_tab(&mut self) {
        let session = self.blank_session();
        let id = self.next_tab_id;
        self.next_tab_id += 1;
        self.park_active();
        self.tabs.push(Tab {
            id,
            state: SessionState::Ready(Box::new(session)),
            scroll_y: 0.0,
            input: String::new(),
            page_scripts: None,
        });
        self.active = self.tabs.len() - 1;
        self.unpark_active();
        self.reset_transient();
        self.focus_url_bar();
    }

    /// Closes the tab at `index`; the last tab is never closed.
    fn close_tab(&mut self, index: usize) {
        if self.tabs.len() <= 1 || index >= self.tabs.len() {
            return;
        }
        if index == self.active {
            // Park so the vec slot is real, then drop it and adopt a
            // neighbour.
            self.park_active();
            self.tabs.remove(index);
            self.active = index.min(self.tabs.len() - 1);
            self.unpark_active();
            self.reset_transient();
        } else {
            self.tabs.remove(index);
            if index < self.active {
                self.active -= 1;
            }
            self.request_redraw();
        }
    }

    /// A short label for the tab at `index` (host, or "New Tab").
    fn tab_title(&self, index: usize) -> String {
        let from_state = |state: &SessionState, input: &str| -> String {
            match state {
                SessionState::Ready(session) => session
                    .current_url()
                    .and_then(|url| url.host_str().map(str::to_string))
                    .or_else(|| (!input.is_empty()).then(|| input.to_string()))
                    .unwrap_or_else(|| "New Tab".to_string()),
                SessionState::Loading { target } => {
                    if target.is_empty() {
                        "New Tab".to_string()
                    } else {
                        target.clone()
                    }
                }
            }
        };
        if index == self.active {
            from_state(&self.state, &self.input)
        } else {
            let tab = &self.tabs[index];
            from_state(&tab.state, &tab.input)
        }
    }

    /// Marks the rasterized page stale (navigation, hover, resize...).
    fn invalidate_page(&mut self) {
        self.page_generation = self.page_generation.wrapping_add(1);
    }

    /// The measurer that produced the current layout — selection geometry
    /// must use the same one.
    fn measurer(&self) -> Box<dyn TextMeasurer + '_> {
        match self.effective_font() {
            Some(font) => Box::new(SharedFont(font)),
            None => Box::new(HeuristicMeasurer),
        }
    }

    /// The font frames render with: the page's @font-face font when one
    /// loaded, else the system font.
    fn effective_font(&self) -> Option<Arc<SystemFont>> {
        self.session()
            .and_then(Session::web_font)
            .or_else(|| self.font.clone())
    }

    fn caret_at_cursor(&mut self) -> Option<Caret> {
        let (x, y) = self.page_cursor()?;
        let runs = self.text_runs()?;
        caret_at_point(&runs, x, y, self.measurer().as_ref())
    }

    /// The page's text runs, collected at most once per `page_generation`
    /// (`invalidate_page` marks every raster-affecting change, which is
    /// exactly when a re-collection is needed). The `Rc` clone callers get
    /// is free compared to re-walking the layout.
    fn text_runs(&mut self) -> Option<Rc<Vec<TextRun>>> {
        let generation = self.page_generation;
        let state = &self.state;
        cached(&mut self.text_runs, generation, || match state {
            SessionState::Ready(session) => session
                .page()
                .map(|page| Rc::new(collect_text_runs(&page.layout))),
            SessionState::Loading { .. } => None,
        })
        .clone()
    }

    /// The find-in-page run tables (lowercase text + char boundaries),
    /// built once per page generation instead of on every keystroke.
    fn find_runs(&mut self) -> Option<Rc<Vec<FindRun>>> {
        let generation = self.page_generation;
        let runs = self.text_runs()?;
        let find = cached(&mut self.find_runs, generation, || {
            Rc::new(
                runs.iter()
                    .map(|run| FindRun::new(&run.text))
                    .collect::<Vec<_>>(),
            )
        });
        Some(find.clone())
    }

    fn clear_selection(&mut self) {
        if self.selection.take().is_some() {
            self.request_redraw();
        }
        self.select_anchor = None;
    }

    fn copy_selection(&mut self) {
        let Some(selection) = self.selection else {
            return;
        };
        let Some(runs) = self.text_runs() else {
            return;
        };
        let text = selected_text(&runs, &selection);
        if text.is_empty() {
            return;
        }
        clipboard_set(&text);
    }

    /// The address to display for a ready session: the current URL, or
    /// the original input before the first successful load.
    fn display_url(&self, session: &Session<DefaultLoader>) -> String {
        session
            .current_url()
            .map_or_else(|| self.input.clone(), ToString::to_string)
    }

    fn session(&self) -> Option<&Session<DefaultLoader>> {
        match &self.state {
            SessionState::Ready(session) => Some(session),
            SessionState::Loading { .. } => None,
        }
    }

    /// Runs a navigation on a background thread; the session comes back
    /// through a user event. Ignored while another navigation is running.
    fn start_nav(&mut self, nav: Nav) {
        if matches!(self.state, SessionState::Loading { .. }) {
            return;
        }
        let target = nav.label();
        self.end_page_edit();
        self.selection = None;
        self.select_anchor = None;
        self.select_popup = None;
        self.color_popup = None;
        self.violation_popup = None;
        self.range_drag = None;
        let SessionState::Ready(mut session) =
            std::mem::replace(&mut self.state, SessionState::Loading { target })
        else {
            return;
        };
        let viewport = self.viewport();
        let proxy = self.proxy.clone();
        let tab = self.tabs[self.active].id;
        std::thread::spawn(move || {
            session.set_viewport(viewport);
            let result = match nav {
                Nav::Submit(node) => session.submit_form(node).map(|_| ()),
                Nav::Load(url) => session.load(url).map(|_| ()),
                Nav::Follow(href) => session.follow(&href).map(|_| ()),
                Nav::Back => session.back().map(|_| ()),
                Nav::Forward => session.forward().map(|_| ()),
                Nav::Refresh => session.refresh().map(|_| ()),
            };
            // Failure means the event loop is gone (window closed while
            // loading); the navigation result has nowhere to go.
            if let Err(error) = proxy.send_event(NavDone {
                tab,
                session,
                error: result.err().map(|error| error.to_string()),
            }) {
                eprintln!("note: navigation finished after shutdown: {error}");
            }
        });
        self.invalidate_page();
        self.update_title();
        self.request_redraw();
    }

    fn scale(&self) -> f32 {
        self.window
            .as_ref()
            .map_or(1.0, |window| window.scale_factor() as f32)
    }

    /// Page viewport in CSS pixels: physical size divided by the HiDPI
    /// scale, minus the address bar.
    fn viewport(&self) -> Size {
        self.window.as_ref().map_or(
            Size {
                width: 1024.0,
                height: 768.0,
            },
            |window| {
                let size = window.inner_size();
                let scale = window.scale_factor() as f32;
                Size {
                    width: size.width.max(1) as f32 / scale,
                    height: (size.height.max(1) as f32 / scale - CHROME_HEIGHT).max(1.0),
                }
            },
        )
    }

    /// Cursor position translated into page coordinates, when it is over
    /// the page area (below the address bar).
    fn page_cursor(&self) -> Option<(f32, f32)> {
        let (x, y) = self.cursor?;
        (y >= CHROME_HEIGHT).then_some((x, y - CHROME_HEIGHT + self.scroll_y))
    }

    /// Paint commands for the browser chrome (address bar, nav buttons).
    /// Geometry of the tab strip: one `Rect` per tab (in order) plus the
    /// trailing "+" new-tab button, all in CSS window coordinates.
    fn tab_layout(&self) -> (Vec<Rect>, Rect) {
        let width = self.viewport().width;
        let plus = 30.0;
        let gap = 2.0;
        let available = (width - plus - 8.0).max(0.0);
        let count = self.tabs.len().max(1) as f32;
        let tab_width = ((available - gap * (count - 1.0)) / count).clamp(40.0, 200.0);
        let mut rects = Vec::with_capacity(self.tabs.len());
        let mut x = 4.0;
        for _ in 0..self.tabs.len() {
            rects.push(Rect {
                x,
                y: BAR_HEIGHT + 3.0,
                width: tab_width,
                height: TAB_HEIGHT - 4.0,
            });
            x += tab_width + gap;
        }
        let plus_rect = Rect {
            x: x + 2.0,
            y: BAR_HEIGHT + 3.0,
            width: plus - 6.0,
            height: TAB_HEIGHT - 4.0,
        };
        (rects, plus_rect)
    }

    /// Appends the tab strip (backdrop, tabs, close buttons, "+") to the
    /// chrome display list.
    fn draw_tab_strip(&self, commands: &mut Vec<DisplayCommand>) {
        use lumen_css::Color;
        let width = self.viewport().width;
        commands.push(DisplayCommand::FillRect {
            rect: Rect {
                x: 0.0,
                y: BAR_HEIGHT,
                width,
                height: TAB_HEIGHT,
            },
            color: Color::rgb(0xe4, 0xe1, 0xe8),
            radius: lumen_engine::Corners::uniform(0.0),
        });
        let (rects, plus) = self.tab_layout();
        for (index, rect) in rects.iter().enumerate() {
            let active = index == self.active;
            commands.push(DisplayCommand::FillRect {
                rect: *rect,
                color: if active {
                    Color::rgb(0xf8, 0xf7, 0xfa)
                } else {
                    Color::rgb(0xd3, 0xcf, 0xd9)
                },
                radius: lumen_engine::Corners {
                    top_left: 6.0,
                    top_right: 6.0,
                    bottom_right: 0.0,
                    bottom_left: 0.0,
                },
            });
            // Title, clipped by a shorter width; the close box sits at the
            // right edge.
            let max_text = (rect.width - 34.0).max(0.0);
            let title = self.clip_chrome_text(&self.tab_title(index), 12.0, max_text);
            commands.push(DisplayCommand::DrawText {
                x: rect.x + 10.0,
                y: rect.y + 17.0,
                text: title,
                color: Color::rgb(0x2a, 0x27, 0x30),
                font_size: 12.0,
                font_weight: if active { 600 } else { 400 },
                underline: false,
                italic: false,
                monospace: false,
                line_through: false,
                letter_spacing: 0.0,
                decoration_color: Color::rgb(0, 0, 0),
                decoration_style: lumen_engine::BorderStyle::Solid,
            });
            if self.tabs.len() > 1 {
                commands.push(DisplayCommand::DrawText {
                    x: rect.x + rect.width - 18.0,
                    y: rect.y + 17.0,
                    text: "×".to_string(),
                    color: Color::rgb(0x6a, 0x66, 0x72),
                    font_size: 15.0,
                    font_weight: 500,
                    underline: false,
                    italic: false,
                    monospace: false,
                    line_through: false,
                    letter_spacing: 0.0,
                    decoration_color: Color::rgb(0, 0, 0),
                    decoration_style: lumen_engine::BorderStyle::Solid,
                });
            }
        }
        commands.push(DisplayCommand::DrawText {
            x: plus.x + 5.0,
            y: plus.y + 18.0,
            text: "+".to_string(),
            color: Color::rgb(0x30, 0x30, 0x30),
            font_size: 18.0,
            font_weight: 500,
            underline: false,
            italic: false,
            monospace: false,
            line_through: false,
            letter_spacing: 0.0,
            decoration_color: Color::rgb(0, 0, 0),
            decoration_style: lumen_engine::BorderStyle::Solid,
        });
    }

    /// Truncates chrome text with an ellipsis so it fits `max_width`.
    /// Results are cached by (text, size, width, font): tab titles and
    /// bookmark rows re-clip the same strings on every frame.
    fn clip_chrome_text(&self, text: &str, font_size: f32, max_width: f32) -> String {
        // A loaded web font changes widths, so its identity is in the key.
        let font_key = self
            .session()
            .and_then(Session::web_font)
            .map_or(0, |font| Arc::as_ptr(&font) as usize);
        let key = (
            text.to_string(),
            font_size.to_bits(),
            max_width.to_bits(),
            font_key,
        );
        if let Some(clipped) = self.clip_cache.borrow().get(&key) {
            return clipped.clone();
        }
        let clipped = if self.chrome_text_width(text, font_size) <= max_width {
            text.to_string()
        } else {
            clip_with_ellipsis(text, max_width, |prefix| {
                self.chrome_text_width(prefix, font_size)
            })
        };
        let mut cache = self.clip_cache.borrow_mut();
        // Bound the cache: widths change with every window resize.
        if cache.len() >= 512 {
            cache.clear();
        }
        cache.insert(key, clipped.clone());
        clipped
    }

    /// Routes a click within the tab strip (switch, close, or new tab).
    fn tab_strip_click(&mut self, x: f32, y: f32) {
        let (rects, plus) = self.tab_layout();
        if rect_contains(plus, x, y) {
            self.new_tab();
            return;
        }
        for (index, rect) in rects.iter().enumerate() {
            if rect_contains(*rect, x, y) {
                // The close box is the right ~22px of a tab.
                if self.tabs.len() > 1 && x >= rect.x + rect.width - 22.0 {
                    self.close_tab(index);
                } else {
                    self.switch_tab(index);
                }
                return;
            }
        }
    }

    fn chrome_commands(&self) -> Vec<DisplayCommand> {
        use lumen_css::Color;
        let width = self.viewport().width;
        let bar = |x: f32, y: f32, w: f32, h: f32| Rect {
            x,
            y,
            width: w,
            height: h,
        };
        let enabled = Color::rgb(0x30, 0x30, 0x30);
        let disabled = Color::rgb(0xb4, 0xb4, 0xb4);
        let mut commands = vec![
            DisplayCommand::FillRect {
                rect: bar(0.0, 0.0, width, BAR_HEIGHT),
                color: Color::rgb(0xf1, 0xef, 0xf3),
                radius: lumen_engine::Corners::uniform(0.0),
            },
            DisplayCommand::FillRect {
                rect: bar(0.0, BAR_HEIGHT - 1.0, width, 1.0),
                color: Color::rgb(0xd2, 0xce, 0xd8),
                radius: lumen_engine::Corners::uniform(0.0),
            },
            DisplayCommand::DrawText {
                x: 12.0,
                y: 25.0,
                text: "<".to_string(),
                color: if self.session().is_some_and(Session::can_go_back) {
                    enabled
                } else {
                    disabled
                },
                font_size: 18.0,
                font_weight: 700,
                underline: false,
                italic: false,
                monospace: false,
                line_through: false,
                letter_spacing: 0.0,
                decoration_color: Color::rgb(0, 0, 0),
                decoration_style: lumen_engine::BorderStyle::Solid,
            },
            DisplayCommand::DrawText {
                x: 36.0,
                y: 25.0,
                text: ">".to_string(),
                color: if self.session().is_some_and(Session::can_go_forward) {
                    enabled
                } else {
                    disabled
                },
                font_size: 18.0,
                font_weight: 700,
                underline: false,
                italic: false,
                monospace: false,
                line_through: false,
                letter_spacing: 0.0,
                decoration_color: Color::rgb(0, 0, 0),
                decoration_style: lumen_engine::BorderStyle::Solid,
            },
            DisplayCommand::FillRect {
                rect: bar(60.0, 6.0, (width - 68.0 - 56.0).max(40.0), BAR_HEIGHT - 12.0),
                color: Color::rgb(0xff, 0xff, 0xff),
                radius: lumen_engine::Corners::uniform(6.0),
            },
        ];
        // Bookmark star (filled when the current page is saved) and the
        // dropdown toggle, at the right end of the address bar.
        let starred = self
            .session()
            .and_then(Session::current_url)
            .is_some_and(|url| self.bookmarks.contains(url.as_str()));
        commands.push(DisplayCommand::DrawText {
            x: width - 50.0,
            y: 25.0,
            text: if starred { "★" } else { "☆" }.to_string(),
            color: if starred {
                Color::rgb(0xf0, 0xa5, 0x00)
            } else {
                enabled
            },
            font_size: 18.0,
            font_weight: 400,
            underline: false,
            italic: false,
            monospace: false,
            line_through: false,
            letter_spacing: 0.0,
            decoration_color: Color::rgb(0, 0, 0),
            decoration_style: lumen_engine::BorderStyle::Solid,
        });
        commands.push(DisplayCommand::DrawText {
            x: width - 26.0,
            y: 25.0,
            text: "▾".to_string(),
            color: if self.bookmarks.items.is_empty() {
                disabled
            } else {
                enabled
            },
            font_size: 15.0,
            font_weight: 400,
            underline: false,
            italic: false,
            monospace: false,
            line_through: false,
            letter_spacing: 0.0,
            decoration_color: Color::rgb(0, 0, 0),
            decoration_style: lumen_engine::BorderStyle::Solid,
        });
        match (&self.url_input, &self.state) {
            (Some(input), _) => {
                self.draw_input(&mut commands, input, 68.0, 24.0, 14.0, enabled);
            }
            (None, SessionState::Loading { target }) => commands.push(DisplayCommand::DrawText {
                x: 68.0,
                y: 24.0,
                text: format!("Loading {target}…"),
                color: Color::rgb(0x6a, 0x66, 0x72),
                font_size: 14.0,
                font_weight: 400,
                underline: false,
                italic: false,
                monospace: false,
                line_through: false,
                letter_spacing: 0.0,
                decoration_color: Color::rgb(0, 0, 0),
                decoration_style: lumen_engine::BorderStyle::Solid,
            }),
            (None, SessionState::Ready(session)) => commands.push(DisplayCommand::DrawText {
                x: 68.0,
                y: 24.0,
                text: self.display_url(session),
                color: Color::rgb(0x6a, 0x66, 0x72),
                font_size: 14.0,
                font_weight: 400,
                underline: false,
                italic: false,
                monospace: false,
                line_through: false,
                letter_spacing: 0.0,
                decoration_color: Color::rgb(0, 0, 0),
                decoration_style: lumen_engine::BorderStyle::Solid,
            }),
        }
        if let Some(query) = &self.find_input {
            let bar_width = 280.0_f32.min(width - 16.0);
            let x = width - bar_width - 8.0;
            commands.push(DisplayCommand::FillRect {
                rect: bar(x, CHROME_HEIGHT + 4.0, bar_width, 26.0),
                color: Color::rgb(0xfd, 0xf6, 0xd8),
                radius: lumen_engine::Corners::uniform(5.0),
            });
            commands.push(DisplayCommand::DrawText {
                x: x + 8.0,
                y: CHROME_HEIGHT + 22.0,
                text: "Find:".to_string(),
                color: enabled,
                font_size: 13.0,
                font_weight: 400,
                underline: false,
                italic: false,
                monospace: false,
                line_through: false,
                letter_spacing: 0.0,
                decoration_color: Color::rgb(0, 0, 0),
                decoration_style: lumen_engine::BorderStyle::Solid,
            });
            let label_width = self.chrome_text_width("Find: ", 13.0);
            let query_width = self.draw_input(
                &mut commands,
                query,
                x + 8.0 + label_width,
                CHROME_HEIGHT + 22.0,
                13.0,
                enabled,
            );
            let status = if query.text.is_empty() {
                String::new()
            } else if self.find_matches.is_empty() {
                "0/0".to_string()
            } else {
                format!("{}/{}", self.find_index + 1, self.find_matches.len())
            };
            commands.push(DisplayCommand::DrawText {
                x: x + 8.0 + label_width + query_width + 10.0,
                y: CHROME_HEIGHT + 22.0,
                text: status,
                color: Color::rgb(0x6a, 0x66, 0x72),
                font_size: 13.0,
                font_weight: 400,
                underline: false,
                italic: false,
                monospace: false,
                line_through: false,
                letter_spacing: 0.0,
                decoration_color: Color::rgb(0, 0, 0),
                decoration_style: lumen_engine::BorderStyle::Solid,
            });
        }
        self.draw_tab_strip(&mut commands);
        if self.bookmarks_open && !self.bookmarks.items.is_empty() {
            let rows = self.bookmark_menu_layout();
            if let Some(first) = rows.first() {
                let panel = Rect {
                    x: first.x - 6.0,
                    y: first.y - 6.0,
                    width: first.width + 12.0,
                    height: rows.len() as f32 * first.height + 12.0,
                };
                commands.push(DisplayCommand::FillRect {
                    rect: panel,
                    color: lumen_css::Color::rgb(0xff, 0xff, 0xff),
                    radius: lumen_engine::Corners::uniform(8.0),
                });
                commands.push(DisplayCommand::StrokeRect {
                    rect: panel,
                    widths: lumen_engine::EdgeSizes::uniform(1.0),
                    colors: lumen_engine::EdgeSizes::uniform(lumen_css::Color::rgb(
                        0xd6, 0xd1, 0xc6,
                    )),
                    styles: lumen_engine::EdgeSizes::uniform(lumen_engine::BorderStyle::Solid),
                    radius: lumen_engine::Corners::uniform(8.0),
                });
                let hovered = self.cursor.and_then(|(x, y)| {
                    rows.iter().position(|rect| rect_contains(*rect, x, y))
                });
                for (index, rect) in rows.iter().enumerate() {
                    if hovered == Some(index) {
                        commands.push(DisplayCommand::FillRect {
                            rect: *rect,
                            color: lumen_css::Color::rgb(0xea, 0xf2, 0xff),
                            radius: lumen_engine::Corners::uniform(4.0),
                        });
                    }
                    let title = &self.bookmarks.items[index].title;
                    let text = self.clip_chrome_text(title, 13.0, rect.width - 12.0);
                    commands.push(DisplayCommand::DrawText {
                        x: rect.x + 6.0,
                        y: rect.y + 17.0,
                        text,
                        color: lumen_css::Color::rgb(0x2a, 0x27, 0x30),
                        font_size: 13.0,
                        font_weight: 400,
                        underline: false,
                        italic: false,
                        monospace: false,
                        line_through: false,
                        letter_spacing: 0.0,
                        decoration_color: lumen_css::Color::rgb(0, 0, 0),
                        decoration_style: lumen_engine::BorderStyle::Solid,
                    });
                }
            }
        }
        commands
    }

    /// Rows of the open bookmarks dropdown (one `Rect` per bookmark).
    fn bookmark_menu_layout(&self) -> Vec<Rect> {
        let width = self.viewport().width;
        let panel_width = 320.0_f32.min(width - 16.0);
        let x = (width - panel_width - 8.0).max(8.0);
        let row_height = 26.0;
        (0..self.bookmarks.items.len())
            .map(|index| Rect {
                x,
                y: BAR_HEIGHT + 2.0 + index as f32 * row_height,
                width: panel_width,
                height: row_height,
            })
            .collect()
    }

    /// Toggles the current page's bookmark, using the page title or host.
    fn toggle_current_bookmark(&mut self) {
        let Some(url) = self
            .session()
            .and_then(Session::current_url)
            .map(|url| url.to_string())
        else {
            return;
        };
        let title = self
            .session()
            .and_then(Session::current_url)
            .and_then(|url| url.host_str().map(str::to_string))
            .unwrap_or_else(|| url.clone());
        self.bookmarks.toggle(&url, &title);
        self.request_redraw();
    }

    /// Width of chrome text at `font_size` with the shell's measurer.
    fn chrome_text_width(&self, text: &str, font_size: f32) -> f32 {
        self.measurer()
            .measure(
                text,
                &TextStyle {
                    font_size,
                    font_weight: FontWeight(400),
                    monospace: false,
                    letter_spacing: 0.0,
                },
            )
            .width
    }

    /// Draws a text input at `x`/`baseline`: selection highlight behind
    /// the text, then a caret line. Returns the text width.
    fn draw_input(
        &self,
        commands: &mut Vec<DisplayCommand>,
        input: &TextInput,
        x: f32,
        baseline: f32,
        font_size: f32,
        color: lumen_css::Color,
    ) -> f32 {
        use lumen_css::Color;
        let top = baseline - font_size;
        let height = font_size * 1.3;
        let (start, end) = input.selection();
        if start != end {
            let selection_x = x + self.chrome_text_width(&input.slice(0, start), font_size);
            let selection_width = self.chrome_text_width(&input.slice(start, end), font_size);
            commands.push(DisplayCommand::FillRect {
                rect: Rect {
                    x: selection_x,
                    y: top,
                    width: selection_width,
                    height,
                },
                color: Color::rgb(0xb3, 0xd4, 0xfc),
                radius: lumen_engine::Corners::uniform(0.0),
            });
        }
        commands.push(DisplayCommand::DrawText {
            x,
            y: baseline,
            text: input.text.clone(),
            color,
            font_size,
            font_weight: 400,
            underline: false,
            italic: false,
            monospace: false,
            line_through: false,
            letter_spacing: 0.0,
            decoration_color: Color::rgb(0, 0, 0),
            decoration_style: lumen_engine::BorderStyle::Solid,
        });
        let caret_x = x + self.chrome_text_width(&input.slice(0, input.caret), font_size);
        commands.push(DisplayCommand::FillRect {
            rect: Rect {
                x: caret_x,
                y: top,
                width: 1.5,
                height,
            },
            color: Color::rgb(0x30, 0x30, 0x30),
            radius: lumen_engine::Corners::uniform(0.0),
        });
        self.chrome_text_width(&input.text, font_size)
    }

    fn max_scroll(&self) -> f32 {
        let content = self
            .session()
            .and_then(Session::page)
            .map_or(0.0, |page| page.layout.content_box().height);
        (content - self.viewport().height).max(0.0)
    }

    /// Clamps and quantizes the scroll offset to whole device pixels, so
    /// scroll deltas map to exact row shifts of the cached frame.
    fn set_scroll(&mut self, y: f32) {
        let scale = self.scale();
        self.scroll_y = ((y.clamp(0.0, self.max_scroll()) * scale).round()) / scale;
    }

    fn scroll_by(&mut self, delta: f32) {
        let before = self.scroll_y;
        self.set_scroll(self.scroll_y + delta);
        // Deliberately skip hover recomputation here: a hover change bumps
        // the page generation and forces a full re-raster, which would kill
        // the scroll blit path and make scrolling stutter. Hover refreshes
        // on the next cursor move.
        if self.scroll_y != before {
            self.request_redraw();
        }
    }

    /// Scroll-only cache reuse: shifts the cached page raster by the scroll
    /// delta and rasterizes just the strip that scrolling exposed.
    ///
    /// The old cached buffer is consumed and shifted in place (`copy_within`)
    /// — no ~14 MB clone per scroll step. The shift is confined to the page
    /// area (below the chrome). Rows under the chrome are never moved and
    /// never repainted — the chrome is opaque and covers them every frame —
    /// so scrolling costs one memmove plus the exposed strip, with no fixed
    /// per-frame chrome-height repaint.
    fn blit_scrolled(
        &self,
        mut shifted: lumen_engine::Framebuffer,
        old_scroll: f32,
        width: u32,
        height: u32,
        scale: f32,
    ) -> Option<lumen_engine::Framebuffer> {
        let delta = ((self.scroll_y - old_scroll) * scale).round() as i64;
        let top = i64::from(((CHROME_HEIGHT * scale).ceil() as u32).min(height));
        let page_rows = i64::from(height) - top;
        if delta == 0 || page_rows <= 0 || delta.abs() >= page_rows {
            return None;
        }
        let page = self.session().and_then(Session::page)?;
        let row = width as usize;
        let kept = (page_rows - delta.abs()) as usize;
        let exposed = if delta > 0 {
            // Scrolled down: page rows move up; the bottom strip is new.
            let source = (top + delta) as usize * row;
            let destination = top as usize * row;
            shifted
                .pixels
                .copy_within(source..source + kept * row, destination);
            (height - delta as u32, height)
        } else {
            // Scrolled up: page rows move down; the strip under the chrome
            // is new.
            let up = (-delta) as usize;
            let source = top as usize * row;
            let destination = (top as usize + up) * row;
            shifted
                .pixels
                .copy_within(source..source + kept * row, destination);
            (top as u32, top as u32 + up as u32)
        };
        let region = (0, exposed.0, width, exposed.1);
        if region.3 > region.1 {
            rasterize_region(
                &mut shifted,
                &page.display_list,
                self.scroll_y - CHROME_HEIGHT,
                scale,
                self.effective_font().as_deref(),
                region,
            );
        }
        Some(shifted)
    }

    /// Hit-tests the current cursor position, updates `:hover` styling and
    /// the pointer shape, and redraws when the hovered node changed.
    fn update_hover(&mut self) {
        let hit = self.page_cursor().and_then(|(x, y)| {
            self.session().and_then(Session::page).and_then(|page| {
                page.layout
                    .hit_test_scrolled(x, y, session_offsets(&self.state))
            })
        });
        let over_link = hit.is_some_and(|node| {
            self.session()
                .is_some_and(|session| session.link_target(node).is_some())
        });
        let over_text = !over_link
            && self.caret_at_cursor().is_some()
            && self.page_cursor().is_some_and(|(x, y)| {
                // Only show the I-beam when actually over a text run's
                // rect; both checks share the generation-cached runs.
                self.text_runs().is_some_and(|runs| {
                    runs.iter().any(|run| {
                        x >= run.rect.x
                            && x < run.rect.x + run.rect.width
                            && y >= run.rect.y
                            && y < run.rect.y + run.rect.height
                    })
                })
            });
        // Form controls pick their cursor: clickables get the pointer,
        // editable text gets the I-beam (labels resolve to their target).
        let over_control = hit
            .and_then(|node| self.form_control_at(node))
            .and_then(|control| {
                let page = self.session().and_then(Session::page)?;
                let element = page.document.element(control)?;
                Some(match element.tag_name.as_str() {
                    "select" | "button" => CursorIcon::Pointer,
                    "textarea" => CursorIcon::Text,
                    "input" => match element.attributes.get("type").unwrap_or("text") {
                        "checkbox" | "radio" | "submit" | "button" | "reset" | "range"
                        | "color" => CursorIcon::Pointer,
                        "hidden" => CursorIcon::Default,
                        _ => CursorIcon::Text,
                    },
                    _ => CursorIcon::Default,
                })
            });
        if let Some(window) = &self.window {
            window.set_cursor(if over_link {
                CursorIcon::Pointer
            } else if let Some(cursor) = over_control {
                cursor
            } else if over_text {
                CursorIcon::Text
            } else {
                CursorIcon::Default
            });
        }
        if let SessionState::Ready(session) = &mut self.state
            && session.set_hovered(hit)
        {
            self.invalidate_page();
            self.request_redraw();
        }
    }

    fn click(&mut self) {
        // An open validation bubble dismisses on any click; the click
        // itself still goes through (Chrome behaves the same).
        if self.violation_popup.take().is_some() {
            self.request_redraw();
        }
        // An open bookmarks dropdown captures the click: a row navigates,
        // anywhere else just closes it.
        if self.bookmarks_open {
            self.bookmarks_open = false;
            if let Some((x, y)) = self.cursor {
                let rows = self.bookmark_menu_layout();
                if let Some(index) = rows.iter().position(|rect| rect_contains(*rect, x, y)) {
                    if let Ok(url) = self.bookmarks.items[index].url.parse() {
                        self.start_nav(Nav::Load(url));
                    }
                    return;
                }
            }
            self.request_redraw();
            return;
        }
        if let Some((x, y)) = self.cursor
            && y < BAR_HEIGHT
        {
            self.chrome_click(x);
            return;
        }
        if let Some((x, y)) = self.cursor
            && y < CHROME_HEIGHT
        {
            self.tab_strip_click(x, y);
            return;
        }
        // An open color palette captures the click: a swatch picks it,
        // anywhere else just closes the card.
        if let Some(popup) = self.color_popup.take() {
            if let Some((x, y)) = self.page_cursor()
                && let Some(index) =
                    (0..COLOR_SWATCHES.len()).find(|index| rect_contains(swatch_rect(popup.rect, *index), x, y))
                && let SessionState::Ready(session) = &mut self.state
            {
                session.set_color_value(popup.node, COLOR_SWATCHES[index]);
            }
            self.invalidate_page();
            self.request_redraw();
            return;
        }
        // An open select dropdown captures the click.
        if let Some(popup) = self.select_popup.take() {
            if let Some((x, y)) = self.page_cursor()
                && rect_contains(popup.rect, x, y)
            {
                let index = (((y - popup.rect.y - 6.0).max(0.0) / SELECT_ROW_HEIGHT) as usize)
                    .min(popup.options.len().saturating_sub(1));
                if let SessionState::Ready(session) = &mut self.state {
                    session.set_selected_option(popup.node, index);
                }
            }
            self.invalidate_page();
            self.request_redraw();
            return;
        }
        // A click on the page drops address-bar focus.
        if self.url_input.take().is_some() {
            self.request_redraw();
        }
        let node = self.page_cursor().and_then(|(x, y)| {
            self.session().and_then(Session::page).and_then(|page| {
                page.layout
                    .hit_test_scrolled(x, y, session_offsets(&self.state))
            })
        });
        // Script listeners see the click first, bubbling to ancestors;
        // preventDefault() cancels the default action entirely.
        if let Some(node) = node
            && let (Some(scripts), SessionState::Ready(session)) =
                (&mut self.page_scripts, &mut self.state)
        {
            let outcome = scripts.dispatch(session, node, "click");
            if outcome.handled {
                self.invalidate_page();
                self.request_redraw();
            }
            self.follow_script_navigation();
            if outcome.prevented || matches!(self.state, SessionState::Loading { .. }) {
                return;
            }
        }
        // Clicking moves :focus (cleared when clicking empty space).
        // The focus target is the nearest form control or the hit node.
        let control = node.and_then(|node| self.form_control_at(node));
        if let SessionState::Ready(session) = &mut self.state
            && session.set_focused(control.or(node))
        {
            self.invalidate_page();
            self.request_redraw();
        }
        // Form controls: text inputs begin editing, checkables toggle,
        // submit buttons submit.
        if let Some(control) = control {
            let (tag, kind) = self
                .session()
                .and_then(Session::page)
                .and_then(|page| page.document.element(control))
                .map(|element| {
                    (
                        element.tag_name.clone(),
                        element
                            .attributes
                            .get("type")
                            .unwrap_or(if element.tag_name == "button" {
                                "submit"
                            } else {
                                "text"
                            })
                            .to_string(),
                    )
                })
                .unwrap_or_default();
            match tag.as_str() {
                "select" => {
                    let multiple = self
                        .session()
                        .is_some_and(|session| session.is_multiple_select(control));
                    if multiple {
                        // List box: a plain click selects the row under the
                        // cursor, Cmd/Ctrl+click toggles it.
                        if let Some(node) = node
                            && let Some(option) = self.option_at(control, node)
                        {
                            let toggle = self.modifiers.state().super_key()
                                || self.modifiers.state().control_key();
                            if let SessionState::Ready(session) = &mut self.state {
                                session.click_option(control, option, toggle);
                            }
                            self.invalidate_page();
                            self.request_redraw();
                        }
                    } else {
                        self.open_select_popup(control);
                    }
                    return;
                }
                "textarea" => {
                    self.input_drag = false;
                    if let SessionState::Ready(session) = &mut self.state {
                        session.begin_edit(control, None);
                    }
                    self.invalidate_page();
                    self.request_redraw();
                    return;
                }
                "button" => {
                    if kind == "submit" && self.submit_allowed(control) {
                        self.end_page_edit();
                        self.start_nav(Nav::Submit(control));
                    }
                    return;
                }
                _ => {}
            }
            if kind == "range" {
                if let Some((x, _)) = self.page_cursor()
                    && let Some(page) = self.session().and_then(Session::page)
                    && let Some(laid) = page.layout.find_by_node(control)
                {
                    let content = laid.content_box();
                    let fraction = ((x - content.x) / content.width.max(1.0)).clamp(0.0, 1.0);
                    if let SessionState::Ready(session) = &mut self.state {
                        session.set_range_fraction(control, fraction);
                    }
                    self.invalidate_page();
                    self.request_redraw();
                }
                return;
            }
            match kind.as_str() {
                "color" => {
                    self.open_color_popup(control);
                    return;
                }
                "checkbox" | "radio" => {
                    if let SessionState::Ready(session) = &mut self.state
                        && session.toggle_checkable(control)
                    {
                        self.invalidate_page();
                        self.request_redraw();
                    }
                    return;
                }
                "submit" => {
                    if self.submit_allowed(control) {
                        self.end_page_edit();
                        self.start_nav(Nav::Submit(control));
                    }
                    return;
                }
                _ => {
                    // The session positions the caret at the click x.
                    let x = self.page_cursor().map(|(x, _)| x);
                    self.input_drag = false;
                    let began = match &mut self.state {
                        SessionState::Ready(session) => session.begin_edit(control, x),
                        SessionState::Loading { .. } => false,
                    };
                    if began {
                        self.invalidate_page();
                        self.request_redraw();
                        return;
                    }
                }
            }
        }
        self.end_page_edit();
        let Some(node) = node else { return };
        if let Some(href) = self.session().and_then(|session| session.link_target(node)) {
            self.start_nav(Nav::Follow(href));
        }
    }

    /// Opens the dropdown list for a select element.
    fn open_select_popup(&mut self, node: usize) {
        let Some(session) = self.session() else {
            return;
        };
        let (options, selected) = session.select_options(node);
        if options.is_empty() {
            return;
        }
        let Some(rect) = session
            .page()
            .and_then(|page| page.layout.find_by_node(node))
            .map(|laid| laid.border_box())
        else {
            return;
        };
        self.select_popup = Some(SelectPopup {
            node,
            rect: Rect {
                x: rect.x,
                y: rect.y + rect.height + 4.0,
                width: rect.width.max(160.0),
                height: SELECT_ROW_HEIGHT * options.len() as f32 + 12.0,
            },
            hovered: selected,
            selected,
            options,
        });
        self.request_redraw();
    }

    /// Opens the validation bubble for a blocked submit: a dark
    /// Chrome-style card below the violating control.
    fn open_violation_popup(&mut self) {
        let Some(session) = self.session() else {
            return;
        };
        let Some(violation) = session.form_violation() else {
            return;
        };
        let Some(control) = session
            .page()
            .and_then(|page| page.layout.find_by_node(violation.node))
            .map(|laid| laid.border_box())
        else {
            return;
        };
        let (node, message) = (violation.node, violation.message.clone());
        // The card caps at 320 px; a longer message clips with an
        // ellipsis instead of overflowing it.
        let max_text = 320.0 - 24.0;
        let message = if self.chrome_text_width(&message, 13.0) > max_text {
            clip_with_ellipsis(&message, max_text, |prefix| {
                self.chrome_text_width(prefix, 13.0)
            })
        } else {
            message
        };
        let text_width = self.chrome_text_width(&message, 13.0);
        let rect = violation_rect(control, text_width, self.viewport().width);
        self.violation_popup = Some(ViolationPopup {
            node,
            message,
            rect,
        });
        self.request_redraw();
    }

    /// Opens the swatch palette for a color input.
    fn open_color_popup(&mut self, node: usize) {
        let Some(rect) = self
            .session()
            .and_then(Session::page)
            .and_then(|page| page.layout.find_by_node(node))
            .map(|laid| laid.border_box())
        else {
            return;
        };
        let rows = COLOR_SWATCHES.len().div_ceil(SWATCH_COLUMNS);
        self.color_popup = Some(ColorPopup {
            node,
            rect: Rect {
                x: rect.x,
                y: rect.y + rect.height + 4.0,
                width: 16.0 + (SWATCH_SIZE + SWATCH_GAP) * SWATCH_COLUMNS as f32 - SWATCH_GAP,
                height: 16.0 + (SWATCH_SIZE + SWATCH_GAP) * rows as f32 - SWATCH_GAP,
            },
            hovered: None,
        });
        self.request_redraw();
    }

    /// The `<option>` at or above a hit node, when it belongs to `select`.
    fn option_at(&self, select: usize, node: usize) -> Option<usize> {
        let page = self.session().and_then(Session::page)?;
        let document = &page.document;
        std::iter::once(node)
            .chain(document.ancestors(node))
            .take_while(|candidate| *candidate != select)
            .find(|candidate| {
                document
                    .element(*candidate)
                    .is_some_and(|element| element.tag_name == "option")
            })
    }

    /// Moves a dragged range slider's thumb to the cursor's x position.
    fn drag_range_to_cursor(&mut self, control: usize) {
        let Some((x, _)) = self.page_cursor() else {
            return;
        };
        let Some(content) = self
            .session()
            .and_then(Session::page)
            .and_then(|page| page.layout.find_by_node(control))
            .map(|laid| laid.content_box())
        else {
            return;
        };
        let fraction = ((x - content.x) / content.width.max(1.0)).clamp(0.0, 1.0);
        if let SessionState::Ready(session) = &mut self.state {
            session.set_range_fraction(control, fraction);
        }
        self.invalidate_page();
        self.request_redraw();
    }

    /// The control's enclosing <form>, if any (submit buttons outside a
    /// form do nothing, like real browsers).
    fn enclosing_form(&self, control: usize) -> Option<usize> {
        let page = self.session().and_then(Session::page)?;
        let document = &page.document;
        std::iter::once(control)
            .chain(document.ancestors(control))
            .find(|node| {
                document
                    .element(*node)
                    .is_some_and(|element| element.tag_name == "form")
            })
    }

    /// Fires the `submit` event on the control's form. Returns the form
    /// when submission should proceed (a form exists and no handler
    /// called preventDefault).
    fn submit_allowed(&mut self, control: usize) -> bool {
        let Some(form) = self.enclosing_form(control) else {
            return false;
        };
        if let (Some(scripts), SessionState::Ready(session)) =
            (&mut self.page_scripts, &mut self.state)
        {
            let outcome = scripts.dispatch(session, form, "submit");
            if outcome.handled {
                self.invalidate_page();
                self.request_redraw();
            }
            if outcome.prevented {
                return false;
            }
        }
        true
    }

    /// Dispatches a DOM event to the page's scripts, repainting if a
    /// handler ran.
    fn dispatch_script_event(&mut self, node: usize, event: &str) {
        if let (Some(scripts), SessionState::Ready(session)) =
            (&mut self.page_scripts, &mut self.state)
            && scripts.dispatch(session, node, event).handled
        {
            self.invalidate_page();
            self.request_redraw();
        }
        self.follow_script_navigation();
        self.apply_script_focus();
    }

    /// Applies a script's focus()/blur() request, if one is pending.
    fn apply_script_focus(&mut self) {
        let request = self
            .page_scripts
            .as_mut()
            .and_then(lumen_browser::PageScripts::take_focus_request);
        if let Some(target) = request {
            self.focus_control(target);
        }
    }

    /// Performs a navigation a script requested (location.href/reload,
    /// history.back/forward).
    fn follow_script_navigation(&mut self) {
        let Some(target) = self
            .page_scripts
            .as_mut()
            .and_then(lumen_browser::PageScripts::take_navigation)
        else {
            return;
        };
        if target == "::reload" {
            self.start_nav(Nav::Refresh);
        } else if target == "::back" {
            self.start_nav(Nav::Back);
        } else if target == "::forward" {
            self.start_nav(Nav::Forward);
        } else {
            self.start_nav(Nav::Follow(target));
        }
    }

    /// The page's focusable controls in document order.
    fn focusable_controls(&self) -> Vec<usize> {
        let Some(page) = self.session().and_then(Session::page) else {
            return Vec::new();
        };
        let document = &page.document;
        document
            .descendants(document.root())
            .filter(|node| {
                document.element(*node).is_some_and(|element| {
                    match element.tag_name.as_str() {
                        "select" | "textarea" | "button" => true,
                        "input" => element.attributes.get("type") != Some("hidden"),
                        _ => false,
                    }
                })
            })
            .collect()
    }

    /// Moves focus to a control: blur/focus events fire, text controls
    /// begin editing with everything selected, and the page scrolls the
    /// control into view.
    fn focus_control(&mut self, target: Option<usize>) {
        let previous = self.session().and_then(Session::focused);
        if previous == target {
            return;
        }
        self.end_page_edit();
        if let SessionState::Ready(session) = &mut self.state {
            session.set_focused(target);
        }
        if let Some(previous) = previous {
            self.dispatch_script_event(previous, "blur");
        }
        if let Some(target) = target {
            let is_text = self.session().is_some_and(|session| {
                session.is_text_input(target) || session.is_textarea(target)
            });
            if is_text && let SessionState::Ready(session) = &mut self.state {
                session.begin_edit(target, None);
            }
            // Scroll the control into view when it sits outside.
            if let Some(rect) = self
                .session()
                .and_then(Session::page)
                .and_then(|page| page.layout.find_by_node(target))
                .map(|laid| laid.border_box())
            {
                let viewport_height = self.viewport().height;
                if rect.y < self.scroll_y || rect.y + rect.height > self.scroll_y + viewport_height
                {
                    self.set_scroll(rect.y - viewport_height / 3.0);
                }
            }
            self.dispatch_script_event(target, "focus");
        }
        self.invalidate_page();
        self.request_redraw();
    }

    /// Tab / Shift+Tab: focus the next / previous control.
    fn cycle_focus(&mut self, backward: bool) {
        let controls = self.focusable_controls();
        if controls.is_empty() {
            return;
        }
        let current = self
            .session()
            .and_then(Session::focused)
            .and_then(|node| controls.iter().position(|control| *control == node));
        let next = match (current, backward) {
            (Some(index), false) => controls[(index + 1) % controls.len()],
            (Some(index), true) => controls[(index + controls.len() - 1) % controls.len()],
            (None, false) => controls[0],
            (None, true) => *controls.last().expect("nonempty"),
        };
        self.focus_control(Some(next));
    }

    /// Ends in-page editing (the session restores the display).
    fn end_page_edit(&mut self) {
        self.input_drag = false;
        if let SessionState::Ready(session) = &mut self.state {
            session.end_edit();
        }
        self.invalidate_page();
    }

    /// The nearest form control at or above a hit node; labels resolve
    /// to their target control (for= id, else a wrapped control).
    fn form_control_at(&self, node: usize) -> Option<usize> {
        let page = self.session().and_then(Session::page)?;
        let document = &page.document;
        let control = std::iter::once(node)
            .chain(document.ancestors(node))
            .find(|candidate| {
                document.element(*candidate).is_some_and(|element| {
                    matches!(
                        element.tag_name.as_str(),
                        "input" | "select" | "textarea" | "button" | "label"
                    )
                })
            })?;
        let element = document.element(control)?;
        if element.tag_name != "label" {
            return Some(control);
        }
        // label: prefer for=<id>, else the first wrapped control.
        if let Some(target_id) = element.attributes.get("for") {
            return document.descendants(document.root()).find(|candidate| {
                document
                    .element(*candidate)
                    .is_some_and(|target| target.attributes.get("id") == Some(target_id))
            });
        }
        document.descendants(control).find(|candidate| {
            document.element(*candidate).is_some_and(|target| {
                matches!(target.tag_name.as_str(), "input" | "select" | "textarea")
            })
        })
    }

    fn chrome_click(&mut self, x: f32) {
        let width = self.viewport().width;
        match x {
            x if (8.0..32.0).contains(&x) => self.start_nav(Nav::Back),
            x if (32.0..56.0).contains(&x) => self.start_nav(Nav::Forward),
            x if (width - 54.0..width - 30.0).contains(&x) => self.toggle_current_bookmark(),
            x if x >= width - 30.0 => {
                if !self.bookmarks.items.is_empty() {
                    self.bookmarks_open = !self.bookmarks_open;
                    self.request_redraw();
                }
            }
            x if x >= 60.0 => self.focus_url_bar(),
            _ => {}
        }
    }

    fn focus_url_bar(&mut self) {
        // Focusing selects the whole URL, as browsers do.
        self.url_input = Some(TextInput::with_all_selected(
            self.session()
                .and_then(Session::current_url)
                .map_or_else(String::new, ToString::to_string),
        ));
        self.request_redraw();
    }

    fn submit_url_bar(&mut self) {
        let Some(input) = self.url_input.take() else {
            return;
        };
        let input = input.text.trim().to_string();
        if input.is_empty() {
            self.request_redraw();
            return;
        }
        self.start_nav(Nav::Load(resolve_omnibox(&input)));
    }

    /// Scrolls to the element whose `id` matches the current URL fragment.
    fn scroll_to_fragment(&mut self) {
        let Some(target_y) = self.session().and_then(|session| {
            let fragment = session.current_url()?.fragment()?.to_string();
            let page = session.page()?;
            let document = &page.document;
            let node = document.descendants(document.root()).find(|id| {
                document
                    .element(*id)
                    .is_some_and(|element| element.attributes.get("id") == Some(fragment.as_str()))
            })?;
            Some(page.layout.find_by_node(node)?.border_box().y)
        }) else {
            return;
        };
        self.set_scroll(target_y);
    }

    /// Recomputes find matches for the current query (ASCII
    /// case-insensitive, per text run — matches never span runs). The
    /// per-run lowercase text and char-boundary tables come from the
    /// generation cache, so a keystroke only pays for the actual scan.
    fn refresh_find_matches(&mut self) {
        self.find_matches.clear();
        self.find_index = 0;
        let Some(query) = self.find_input.as_ref().map(|input| input.text.clone()) else {
            return;
        };
        if query.is_empty() {
            return;
        }
        let Some(runs) = self.find_runs() else {
            return;
        };
        let needle = query.to_ascii_lowercase();
        self.find_matches = find_matches_in_runs(&runs, &needle);
    }

    /// Scrolls the current find match into view (upper third).
    fn scroll_to_find_match(&mut self) {
        let region = {
            let Some(selection) = self.find_matches.get(self.find_index).copied() else {
                return;
            };
            let Some(runs) = self.text_runs() else {
                return;
            };
            let measurer = self.measurer();
            highlight_rects(&runs, &selection, measurer.as_ref())
                .into_iter()
                .next()
        };
        let Some(region) = region else { return };
        let viewport_height = self.viewport().height;
        let visible = region.rect.y >= self.scroll_y
            && region.rect.y + region.rect.height <= self.scroll_y + viewport_height;
        if !visible {
            self.set_scroll(region.rect.y - viewport_height / 3.0);
        }
    }

    fn open_find_bar(&mut self) {
        match &mut self.find_input {
            // Reopening keeps the query and selects it (as Chrome does), so
            // typing replaces it and Enter reuses it.
            Some(input) => input.select_all(),
            None => self.find_input = Some(TextInput::empty()),
        }
        self.request_redraw();
    }

    fn close_find_bar(&mut self) {
        self.find_input = None;
        self.find_matches.clear();
        self.find_index = 0;
        self.request_redraw();
    }

    /// Cmd/Ctrl+A on the page: selects all text runs.
    fn select_all_page_text(&mut self) {
        let Some(runs) = self.text_runs() else {
            return;
        };
        let Some(last) = runs.last() else { return };
        self.selection = Some(Selection {
            anchor: Caret { run: 0, offset: 0 },
            focus: Caret {
                run: runs.len() - 1,
                offset: last.text.chars().count(),
            },
        });
        self.request_redraw();
    }

    /// Debug HUD paint commands: a translucent panel of live stats under
    /// the address bar, top-right.
    fn hud_commands(&mut self) -> Vec<DisplayCommand> {
        use lumen_css::Color;
        // Refresh RSS at most once a second (`ps` is not free).
        let now = Instant::now();
        if self
            .rss_checked
            .is_none_or(|checked| now - checked > Duration::from_secs(1))
        {
            self.rss_checked = Some(now);
            self.rss_megabytes = std::process::Command::new("ps")
                .args(["-o", "rss=", "-p"])
                .arg(std::process::id().to_string())
                .output()
                .ok()
                .and_then(|output| String::from_utf8(output.stdout).ok())
                .and_then(|text| text.trim().parse::<f64>().ok())
                .map(|kilobytes| kilobytes / 1024.0);
        }

        // Frames inside the last second → FPS; average + worst frame time.
        let window_start = now - Duration::from_secs(1);
        let recent: Vec<Duration> = self
            .frame_times
            .iter()
            .filter(|(finished, _)| *finished >= window_start)
            .map(|(_, took)| *took)
            .collect();
        let fps = recent.len();
        let average_ms = if recent.is_empty() {
            0.0
        } else {
            recent.iter().map(Duration::as_secs_f64).sum::<f64>() / recent.len() as f64 * 1000.0
        };
        let worst_ms = recent.iter().map(Duration::as_secs_f64).fold(0.0, f64::max) * 1000.0;

        let (commands_count, boxes, nodes) = match self.session().and_then(Session::page) {
            Some(page) => (
                page.display_list.len(),
                count_boxes(&page.layout),
                page.document.nodes().len(),
            ),
            None => (0, 0, 0),
        };

        let viewport = self.viewport();
        let lines = [
            format!("fps {fps}  frame {average_ms:.1} ms (max {worst_ms:.1})"),
            format!("last frame: {}", self.last_frame_kind),
            format!(
                "rss {}",
                self.rss_megabytes
                    .map_or_else(|| "?".to_string(), |mb| format!("{mb:.1} MB"))
            ),
            format!("display list {commands_count} cmds"),
            format!("layout {boxes} boxes / dom {nodes} nodes"),
            format!(
                "scroll {:.0}/{:.0}  viewport {:.0}x{:.0} @{:.0}%",
                self.scroll_y,
                self.max_scroll(),
                viewport.width,
                viewport.height,
                self.scale() * 100.0
            ),
            format!("generation {}", self.page_generation),
        ];

        let line_height = 16.0;
        let panel_width = 300.0;
        let panel_height = lines.len() as f32 * line_height + 12.0;
        let x = (viewport.width - panel_width - 8.0).max(0.0);
        let y = CHROME_HEIGHT + 8.0;
        let mut commands = vec![DisplayCommand::FillRect {
            rect: Rect {
                x,
                y,
                width: panel_width,
                height: panel_height,
            },
            color: Color::rgba(0x1c, 0x1a, 0x22, 245),
            radius: lumen_engine::Corners::uniform(6.0),
        }];
        for (index, line) in lines.iter().enumerate() {
            commands.push(DisplayCommand::DrawText {
                x: x + 10.0,
                y: y + 18.0 + index as f32 * line_height,
                text: line.clone(),
                color: Color::rgb(0xd8, 0xf0, 0xd0),
                font_size: 12.0,
                font_weight: 400,
                underline: false,
                italic: false,
                monospace: true,
                line_through: false,
                letter_spacing: 0.0,
                decoration_color: Color::rgb(0, 0, 0),
                decoration_style: lumen_engine::BorderStyle::Solid,
            });
        }
        commands
    }

    fn request_redraw(&self) {
        if let Some(window) = &self.window {
            window.request_redraw();
        }
    }

    /// The wall-clock instant at which a script timer due at `due_ms`
    /// (on the `started` clock) should wake the event loop.
    fn timer_wake(&self, due_ms: f64) -> Instant {
        self.started + Duration::from_secs_f64((due_ms / 1000.0).max(0.0))
    }

    fn update_title(&self) {
        if let Some(window) = &self.window {
            let label = match &self.state {
                SessionState::Loading { target } => format!("Lumen — loading {target}"),
                SessionState::Ready(session) => match session.title() {
                    Some(title) => format!("{title} — Lumen"),
                    None => format!("Lumen — {}", self.display_url(session)),
                },
            };
            window.set_title(&label);
        }
    }

    fn redraw(&mut self, event_loop: &ActiveEventLoop) {
        let frame_started = Instant::now();
        // Step running CSS transitions and script timers; keep redrawing
        // while any are live.
        let now_ms = self.started.elapsed().as_secs_f64() * 1000.0;
        if let SessionState::Ready(session) = &mut self.state {
            let transitioned = session.tick(now_ms);
            let scripted = self
                .page_scripts
                .as_mut()
                .is_some_and(|scripts| scripts.tick(session, now_ms));
            if transitioned || scripted {
                self.invalidate_page();
                self.request_redraw();
            } else if let Some(due_ms) = self
                .page_scripts
                .as_ref()
                .and_then(lumen_browser::PageScripts::next_timer_due_ms)
            {
                // A timer waits for a future deadline: sleep until then
                // instead of spinning redraws at 100% CPU.
                event_loop.set_control_flow(ControlFlow::WaitUntil(self.timer_wake(due_ms)));
            }
            self.follow_script_navigation();
        }
        let scale = self.scale();
        let chrome = self.chrome_commands();
        let Some(size) = self.window.as_ref().map(|window| window.inner_size()) else {
            return;
        };
        let (Some(width), Some(height)) =
            (NonZeroU32::new(size.width), NonZeroU32::new(size.height))
        else {
            return;
        };

        // Page first (offset below the bar via the scroll shift), then the
        // chrome painted over it. The page raster is cached so overlay-only
        // changes (selection drags, find typing, URL editing) skip the
        // expensive repaint; navigation/hover/resize bump the generation.
        let cache_key = (
            self.page_generation,
            self.scroll_y.to_bits(),
            size.width,
            size.height,
        );
        if self
            .page_frame
            .as_ref()
            .is_none_or(|(key, _)| *key != cache_key)
        {
            // Same page, same size, different scroll: hand the old buffer
            // to the blit, which shifts it in place instead of cloning.
            let blit_viable = matches!(
                &self.page_frame,
                Some((key, _))
                    if key.0 == self.page_generation
                        && key.2 == size.width
                        && key.3 == size.height
            );
            let blitted = if blit_viable {
                self.page_frame.take().and_then(|(key, frame)| {
                    self.blit_scrolled(frame, f32::from_bits(key.1), size.width, size.height, scale)
                })
            } else {
                None
            };
            let frame = match blitted {
                Some(frame) => {
                    self.last_frame_kind = "blit";
                    frame
                }
                None => {
                    self.last_frame_kind = "full";
                    match self.session().and_then(Session::page) {
                        Some(page) => rasterize_with(
                            &page.display_list,
                            size.width,
                            size.height,
                            self.scroll_y - CHROME_HEIGHT,
                            scale,
                            self.effective_font().as_deref(),
                        ),
                        None => lumen_engine::Framebuffer::new(size.width, size.height),
                    }
                }
            };
            self.page_frame = Some((cache_key, frame));
        } else {
            self.last_frame_kind = "cache";
        }
        let Some((_, base)) = &self.page_frame else {
            return;
        };
        // Reuse the composition buffer across redraws; only a resize forces
        // a reallocation. The cached raster is memcpy'd in, never mutated.
        let mut framebuffer = match self.compose_frame.take() {
            Some(mut frame) if frame.width == base.width && frame.height == base.height => {
                frame.pixels.copy_from_slice(&base.pixels);
                frame
            }
            _ => base.clone(),
        };
        let selection_runs = if self
            .selection
            .is_some_and(|selection| !selection.is_empty())
        {
            self.text_runs()
        } else {
            None
        };
        if let (Some(selection), Some(runs)) = (self.selection, selection_runs) {
            let measurer = self.measurer();
            for region in highlight_rects(&runs, &selection, measurer.as_ref()) {
                // ::selection backgrounds render stronger than the default
                // translucent blue.
                let (color, alpha) = match region.background {
                    Some(custom) => (custom, 150),
                    None => (lumen_css::Color::rgb(0x33, 0x8c, 0xff), 92),
                };
                framebuffer.blend_fill(
                    Rect {
                        x: region.rect.x * scale,
                        y: (region.rect.y - self.scroll_y + CHROME_HEIGHT) * scale,
                        width: region.rect.width * scale,
                        height: region.rect.height * scale,
                    },
                    color,
                    alpha,
                );
            }
        }
        // Find matches highlight in yellow; the current one in orange.
        let find_runs = if self.find_input.is_some() && !self.find_matches.is_empty() {
            self.text_runs()
        } else {
            None
        };
        if let Some(runs) = find_runs {
            let measurer = self.measurer();
            for (index, matched) in self.find_matches.iter().enumerate() {
                let (color, alpha) = if index == self.find_index {
                    (lumen_css::Color::rgb(0xff, 0x8c, 0x1a), 150)
                } else {
                    (lumen_css::Color::rgb(0xff, 0xd5, 0x4f), 110)
                };
                for region in highlight_rects(&runs, matched, measurer.as_ref()) {
                    framebuffer.blend_fill(
                        Rect {
                            x: region.rect.x * scale,
                            y: (region.rect.y - self.scroll_y + CHROME_HEIGHT) * scale,
                            width: region.rect.width * scale,
                            height: region.rect.height * scale,
                        },
                        color,
                        alpha,
                    );
                }
            }
        }
        // Focused in-page input: selection highlight + caret line (the
        // session owns the geometry).
        if let Some(overlay) = self.session().and_then(Session::edit_overlay) {
            let to_device = |rect: Rect| Rect {
                x: rect.x * scale,
                y: (rect.y - self.scroll_y + CHROME_HEIGHT) * scale,
                width: rect.width * scale,
                height: rect.height * scale,
            };
            if let Some(selection) = overlay.selection {
                framebuffer.blend_fill(
                    to_device(selection),
                    lumen_css::Color::rgb(0xb3, 0xd4, 0xfc),
                    140,
                );
            }
            if let Some(caret) = overlay.caret {
                framebuffer.blend_fill(
                    to_device(caret),
                    lumen_css::Color::rgb(0x20, 0x20, 0x20),
                    255,
                );
            }
        }
        // Inner scrollbars: a thin thumb on every scrollable box.
        if let SessionState::Ready(session) = &self.state
            && let Some(page) = session.page()
        {
            draw_inner_scrollbars(
                &mut framebuffer,
                &page.layout,
                session.scroll_offsets(),
                self.scroll_y,
                scale,
            );
        }
        // Open select dropdown: a native-looking card over the page —
        // soft shadow, rounded opaque panel, hover highlight, and a tick
        // on the selected option.
        if let Some(popup) = &self.select_popup {
            let mut commands: Vec<DisplayCommand> = Vec::new();
            let rect = popup.rect;
            commands.push(DisplayCommand::DrawShadow {
                rect: Rect {
                    x: rect.x,
                    y: rect.y + 3.0,
                    ..rect
                },
                radius: lumen_engine::Corners::uniform(8.0),
                blur: 14.0,
                color: lumen_css::Color::rgba(0x20, 0x1c, 0x2a, 70),
                inset: false,
            });
            commands.push(DisplayCommand::FillRect {
                rect,
                color: lumen_css::Color::rgb(0xff, 0xff, 0xff),
                radius: lumen_engine::Corners::uniform(8.0),
            });
            commands.push(DisplayCommand::StrokeRect {
                rect,
                widths: lumen_engine::EdgeSizes::uniform(1.0),
                colors: lumen_engine::EdgeSizes::uniform(lumen_css::Color::rgb(0xd6, 0xd1, 0xc6)),
                styles: lumen_engine::EdgeSizes::uniform(lumen_engine::BorderStyle::Solid),
                radius: lumen_engine::Corners::uniform(8.0),
            });
            for (index, (_, label)) in popup.options.iter().enumerate() {
                let row_y = rect.y + 6.0 + SELECT_ROW_HEIGHT * index as f32;
                if index == popup.hovered {
                    commands.push(DisplayCommand::FillRect {
                        rect: Rect {
                            x: rect.x + 4.0,
                            y: row_y,
                            width: rect.width - 8.0,
                            height: SELECT_ROW_HEIGHT,
                        },
                        color: lumen_css::Color::rgb(0xea, 0xf1, 0xf8),
                        radius: lumen_engine::Corners::uniform(5.0),
                    });
                }
                if index == popup.selected {
                    commands.push(DisplayCommand::DrawMark {
                        rect: Rect {
                            x: rect.x + 8.0,
                            y: row_y + (SELECT_ROW_HEIGHT - 12.0) / 2.0,
                            width: 12.0,
                            height: 12.0,
                        },
                        color: lumen_css::Color::rgb(0x22, 0x66, 0xaa),
                        mark: lumen_engine::Mark::Check,
                    });
                }
                commands.push(DisplayCommand::DrawText {
                    x: rect.x + 26.0,
                    y: row_y + SELECT_ROW_HEIGHT - 7.0,
                    text: label.clone(),
                    color: lumen_css::Color::rgb(0x23, 0x20, 0x19),
                    font_size: 13.0,
                    font_weight: if index == popup.selected { 600 } else { 400 },
                    underline: false,
                    italic: false,
                    monospace: false,
                    line_through: false,
                    letter_spacing: 0.0,
                    decoration_color: lumen_css::Color::rgb(0, 0, 0),
                    decoration_style: lumen_engine::BorderStyle::Solid,
                });
            }
            rasterize_over(
                &mut framebuffer,
                &commands,
                self.scroll_y - CHROME_HEIGHT,
                scale,
                self.effective_font().as_deref(),
            );
        }
        // Open color palette: the same card treatment with a grid of
        // swatches; the hovered one gets a blue ring.
        if let Some(popup) = &self.color_popup {
            let mut commands: Vec<DisplayCommand> = Vec::new();
            let rect = popup.rect;
            commands.push(DisplayCommand::DrawShadow {
                rect: Rect {
                    x: rect.x,
                    y: rect.y + 3.0,
                    ..rect
                },
                radius: lumen_engine::Corners::uniform(8.0),
                blur: 14.0,
                color: lumen_css::Color::rgba(0x20, 0x1c, 0x2a, 70),
                inset: false,
            });
            commands.push(DisplayCommand::FillRect {
                rect,
                color: lumen_css::Color::rgb(0xff, 0xff, 0xff),
                radius: lumen_engine::Corners::uniform(8.0),
            });
            commands.push(DisplayCommand::StrokeRect {
                rect,
                widths: lumen_engine::EdgeSizes::uniform(1.0),
                colors: lumen_engine::EdgeSizes::uniform(lumen_css::Color::rgb(0xd6, 0xd1, 0xc6)),
                styles: lumen_engine::EdgeSizes::uniform(lumen_engine::BorderStyle::Solid),
                radius: lumen_engine::Corners::uniform(8.0),
            });
            for (index, hex) in COLOR_SWATCHES.iter().enumerate() {
                let swatch = swatch_rect(rect, index);
                let Some(color) = lumen_css::Color::parse(hex) else {
                    continue;
                };
                commands.push(DisplayCommand::FillRect {
                    rect: swatch,
                    color,
                    radius: lumen_engine::Corners::uniform(4.0),
                });
                let ring = popup.hovered == Some(index);
                commands.push(DisplayCommand::StrokeRect {
                    rect: swatch,
                    widths: lumen_engine::EdgeSizes::uniform(if ring { 2.0 } else { 1.0 }),
                    colors: lumen_engine::EdgeSizes::uniform(if ring {
                        lumen_css::Color::rgb(0x22, 0x66, 0xaa)
                    } else {
                        lumen_css::Color::rgba(0, 0, 0, 40)
                    }),
                    styles: lumen_engine::EdgeSizes::uniform(lumen_engine::BorderStyle::Solid),
                    radius: lumen_engine::Corners::uniform(4.0),
                });
            }
            rasterize_over(
                &mut framebuffer,
                &commands,
                self.scroll_y - CHROME_HEIGHT,
                scale,
                self.effective_font().as_deref(),
            );
        }
        // Form-validation bubble: a dark Chrome-style card under the
        // violating control. It only draws while the session still
        // reports the violation — a cleared one closes the bubble.
        if let Some(popup) = &self.violation_popup
            && self
                .session()
                .and_then(Session::form_violation)
                .is_some_and(|violation| violation.node == popup.node)
        {
            let rect = popup.rect;
            let commands = vec![
                DisplayCommand::DrawShadow {
                    rect: Rect {
                        x: rect.x,
                        y: rect.y + 2.0,
                        ..rect
                    },
                    radius: lumen_engine::Corners::uniform(6.0),
                    blur: 12.0,
                    color: lumen_css::Color::rgba(0x20, 0x1c, 0x2a, 90),
                    inset: false,
                },
                DisplayCommand::FillRect {
                    rect,
                    color: lumen_css::Color::rgb(0x32, 0x2f, 0x35),
                    radius: lumen_engine::Corners::uniform(6.0),
                },
                DisplayCommand::DrawText {
                    x: rect.x + 12.0,
                    y: rect.y + rect.height - 10.0,
                    text: popup.message.clone(),
                    color: lumen_css::Color::rgb(0xff, 0xff, 0xff),
                    font_size: 13.0,
                    font_weight: 400,
                    underline: false,
                    italic: false,
                    monospace: false,
                    line_through: false,
                    letter_spacing: 0.0,
                    decoration_color: lumen_css::Color::rgb(0, 0, 0),
                    decoration_style: lumen_engine::BorderStyle::Solid,
                },
            ];
            rasterize_over(
                &mut framebuffer,
                &commands,
                self.scroll_y - CHROME_HEIGHT,
                scale,
                self.effective_font().as_deref(),
            );
        }
        // Scrollbar: a proportional overlay thumb on the right edge.
        let max_scroll = self.max_scroll();
        if max_scroll > 0.0 {
            let viewport = self.viewport();
            let content_height = viewport.height + max_scroll;
            let thumb_height = (viewport.height * viewport.height / content_height).max(24.0);
            let thumb_y = CHROME_HEIGHT
                + (viewport.height - thumb_height) * (self.scroll_y / max_scroll).clamp(0.0, 1.0);
            framebuffer.blend_fill(
                Rect {
                    x: (viewport.width - 8.0) * scale,
                    y: thumb_y * scale,
                    width: 5.0 * scale,
                    height: thumb_height * scale,
                },
                lumen_css::Color::rgb(0x55, 0x52, 0x5c),
                120,
            );
        }
        rasterize_over(
            &mut framebuffer,
            &chrome,
            0.0,
            scale,
            self.effective_font().as_deref(),
        );
        if self.debug_hud {
            let hud = self.hud_commands();
            rasterize_over(
                &mut framebuffer,
                &hud,
                0.0,
                scale,
                self.effective_font().as_deref(),
            );
        }

        let Some(surface) = self.surface.as_mut() else {
            return;
        };
        if surface.resize(width, height).is_err() {
            return;
        }
        let Ok(mut buffer) = surface.buffer_mut() else {
            return;
        };
        buffer.copy_from_slice(&framebuffer.pixels);
        let _ = buffer.present();
        self.compose_frame = Some(framebuffer);

        let now = Instant::now();
        self.frame_times.push_back((now, now - frame_started));
        while self.frame_times.len() > 240 {
            self.frame_times.pop_front();
        }
    }

    /// Applies a key to one text bar. Returns false when that bar is not
    /// active so the caller can fall through to the next input target.
    fn handle_bar_key(
        &mut self,
        bar: EditBar,
        key: &Key,
        command_held: bool,
        shift_held: bool,
        alt_held: bool,
    ) -> bool {
        let input = match bar {
            EditBar::Find => self.find_input.as_mut(),
            EditBar::Url => self.url_input.as_mut(),
        };
        let Some(input) = input else {
            return false;
        };
        match apply_edit(input, key, command_held, shift_held, alt_held) {
            EditOutcome::Changed => match bar {
                EditBar::Find => {
                    self.refresh_find_matches();
                    self.scroll_to_find_match();
                    self.request_redraw();
                }
                EditBar::Url => self.request_redraw(),
            },
            EditOutcome::Moved => self.request_redraw(),
            EditOutcome::Submit => match bar {
                EditBar::Find => {
                    if !self.find_matches.is_empty() {
                        // Shift+Enter walks backwards.
                        self.find_index = if shift_held {
                            (self.find_index + self.find_matches.len() - 1)
                                % self.find_matches.len()
                        } else {
                            (self.find_index + 1) % self.find_matches.len()
                        };
                        self.scroll_to_find_match();
                        self.request_redraw();
                    }
                }
                EditBar::Url => self.submit_url_bar(),
            },
            EditOutcome::Cancel => match bar {
                EditBar::Find => self.close_find_bar(),
                EditBar::Url => {
                    self.url_input = None;
                    self.request_redraw();
                }
            },
            EditOutcome::Copy => {
                // Copy the input selection, or the page selection when the
                // input has none.
                let selected = input.selected_text();
                if selected.is_empty() {
                    self.copy_selection();
                } else {
                    clipboard_set(&selected);
                }
            }
            EditOutcome::Cut => {
                clipboard_set(&input.selected_text());
                input.delete_selection();
                if bar == EditBar::Find {
                    self.refresh_find_matches();
                }
                self.request_redraw();
            }
            EditOutcome::Paste => {
                if let Some(pasted) = clipboard_get() {
                    input.insert(pasted.replace(['\n', '\r'], " ").as_str());
                    if bar == EditBar::Find {
                        self.refresh_find_matches();
                        self.scroll_to_find_match();
                    }
                    self.request_redraw();
                }
            }
            EditOutcome::Ignored => {}
        }
        true
    }

    /// Applies a key to the focused in-page input, syncing the live value
    /// into the session (relayout under the hood) on every change.
    fn handle_page_input_key(
        &mut self,
        key: &Key,
        command_held: bool,
        shift_held: bool,
        alt_held: bool,
    ) {
        let Some(control) = self.session().and_then(Session::editing) else {
            return;
        };
        // Number inputs: Up/Down step the value by `step` within min/max.
        if matches!(key, Key::Named(NamedKey::ArrowUp | NamedKey::ArrowDown))
            && self
                .session()
                .is_some_and(|session| session.is_number_input(control))
        {
            let direction = if matches!(key, Key::Named(NamedKey::ArrowUp)) {
                1.0
            } else {
                -1.0
            };
            let stepped = match &mut self.state {
                SessionState::Ready(session) => session.step_number_input(control, direction),
                SessionState::Loading { .. } => None,
            };
            if stepped.is_some() {
                self.dispatch_script_event(control, "input");
                self.invalidate_page();
                self.request_redraw();
            }
            return;
        }
        let Some(action) = page_edit_action(key, command_held, shift_held, alt_held) else {
            return;
        };
        match action {
            PageEdit::Submit => {
                // Enter inside a textarea inserts a newline instead.
                let is_textarea = self
                    .session()
                    .is_some_and(|session| session.is_textarea(control));
                if is_textarea {
                    if let SessionState::Ready(session) = &mut self.state {
                        session.edit(EditOp::Insert("\n".to_string()));
                    }
                    self.dispatch_script_event(control, "input");
                    self.invalidate_page();
                    self.request_redraw();
                } else if self.submit_allowed(control) {
                    self.end_page_edit();
                    self.start_nav(Nav::Submit(control));
                }
            }
            PageEdit::Cancel => {
                self.end_page_edit();
                if let SessionState::Ready(session) = &mut self.state
                    && session.set_focused(None)
                {
                    self.invalidate_page();
                }
                self.request_redraw();
            }
            PageEdit::Copy => {
                if let Some(buffer) = self.session().and_then(Session::edit_buffer) {
                    clipboard_set(&buffer.selected_text());
                }
            }
            PageEdit::Cut => {
                let selected = self
                    .session()
                    .and_then(Session::edit_buffer)
                    .filter(|buffer| buffer.has_selection())
                    .map(|buffer| buffer.selected_text());
                if let Some(selected) = selected {
                    clipboard_set(&selected);
                    if let SessionState::Ready(session) = &mut self.state {
                        session.edit(EditOp::DeleteForward);
                    }
                    self.dispatch_script_event(control, "input");
                    self.invalidate_page();
                    self.request_redraw();
                }
            }
            PageEdit::Paste => {
                if let Some(pasted) = clipboard_get() {
                    let multiline = self
                        .session()
                        .is_some_and(|session| session.is_textarea(control));
                    if let SessionState::Ready(session) = &mut self.state {
                        session.edit(EditOp::Insert(paste_text(pasted, multiline)));
                    }
                    self.dispatch_script_event(control, "input");
                    self.invalidate_page();
                    self.request_redraw();
                }
            }
            PageEdit::Op(op) => {
                let result = match &mut self.state {
                    SessionState::Ready(session) => session.edit(op),
                    SessionState::Loading { .. } => lumen_browser::EditResult::Ignored,
                };
                if result == lumen_browser::EditResult::Edited {
                    // Scripts hear about typing like real browsers.
                    self.dispatch_script_event(control, "input");
                }
                if result != lumen_browser::EditResult::Ignored {
                    self.invalidate_page();
                    self.request_redraw();
                }
            }
        }
    }

    /// The DOM `event.key` name for a winit key.
    fn dom_key_name(key: &Key) -> Option<String> {
        match key {
            Key::Character(text) => Some(text.to_string()),
            Key::Named(named) => {
                let name = match named {
                    NamedKey::Enter => "Enter",
                    NamedKey::Escape => "Escape",
                    NamedKey::Backspace => "Backspace",
                    NamedKey::Delete => "Delete",
                    NamedKey::Tab => "Tab",
                    NamedKey::Space => " ",
                    NamedKey::ArrowLeft => "ArrowLeft",
                    NamedKey::ArrowRight => "ArrowRight",
                    NamedKey::ArrowUp => "ArrowUp",
                    NamedKey::ArrowDown => "ArrowDown",
                    NamedKey::Home => "Home",
                    NamedKey::End => "End",
                    NamedKey::PageUp => "PageUp",
                    NamedKey::PageDown => "PageDown",
                    NamedKey::Shift => "Shift",
                    NamedKey::Control => "Control",
                    NamedKey::Alt => "Alt",
                    _ => return None,
                };
                Some(name.to_string())
            }
            _ => None,
        }
    }

    /// Dispatches keydown/keyup to the focused control (else the
    /// document). Returns whether a handler prevented the default.
    fn dispatch_key_event(&mut self, event: &str, key: &Key) -> bool {
        let Some(name) = Self::dom_key_name(key) else {
            return false;
        };
        let target = self
            .session()
            .and_then(Session::editing)
            .unwrap_or(0);
        let mut prevented = false;
        if let (Some(scripts), SessionState::Ready(session)) =
            (&mut self.page_scripts, &mut self.state)
        {
            let outcome = scripts.dispatch_with_key(session, target, event, Some(&name));
            if outcome.handled {
                self.invalidate_page();
                self.request_redraw();
            }
            prevented = outcome.prevented;
        }
        self.follow_script_navigation();
        prevented
    }

    fn handle_key(&mut self, key: &Key) {
        // Any key dismisses an open validation bubble; unlike the select
        // dropdown it never captures the key itself.
        if self.violation_popup.take().is_some() {
            self.request_redraw();
        }
        let command_held =
            self.modifiers.state().super_key() || self.modifiers.state().control_key();
        // Ctrl/Cmd+F toggles the find bar from anywhere.
        if command_held && matches!(key, Key::Character(text) if text.as_str() == "f") {
            self.open_find_bar();
            return;
        }
        // F12 toggles the debug HUD.
        if matches!(key, Key::Named(NamedKey::F12)) {
            self.debug_hud = !self.debug_hud;
            self.request_redraw();
            return;
        }
        // Ctrl/Cmd+D bookmarks the current page, as browsers do.
        if command_held && matches!(key, Key::Character(text) if text.as_str() == "d") {
            self.toggle_current_bookmark();
            return;
        }
        let shift_held = self.modifiers.state().shift_key();
        let alt_held = self.modifiers.state().alt_key();
        // Tab management shortcuts (before bar editing so they are not typed).
        if command_held {
            if matches!(key, Key::Named(NamedKey::Tab)) {
                let len = self.tabs.len();
                let next = if shift_held {
                    (self.active + len - 1) % len
                } else {
                    (self.active + 1) % len
                };
                self.switch_tab(next);
                return;
            }
            if let Key::Character(text) = key {
                match text.as_str() {
                    "t" => {
                        self.new_tab();
                        return;
                    }
                    "w" => {
                        self.close_tab(self.active);
                        return;
                    }
                    digit if digit.len() == 1 && digit.as_bytes()[0].is_ascii_digit() => {
                        let n = (digit.as_bytes()[0] - b'0') as usize;
                        if n >= 1 {
                            // Cmd+9 jumps to the last tab, as browsers do.
                            let index = if n == 9 {
                                self.tabs.len() - 1
                            } else {
                                (n - 1).min(self.tabs.len() - 1)
                            };
                            self.switch_tab(index);
                        }
                        return;
                    }
                    _ => {}
                }
            }
        }
        // Bar editing captures input first: the find bar, then the address
        // bar. Both share one handler; only Submit/Cancel and the post-edit
        // refresh differ per bar.
        for bar in [EditBar::Find, EditBar::Url] {
            if self.handle_bar_key(bar, key, command_held, shift_held, alt_held) {
                return;
            }
        }
        // Tab cycles focus through the page's controls.
        if matches!(key, Key::Named(NamedKey::Tab)) {
            self.cycle_focus(shift_held);
            return;
        }
        // Script keydown listeners see page-level keys first (the
        // chrome's own bars already returned above).
        if self.dispatch_key_event("keydown", key) {
            return;
        }
        // An open select dropdown or color palette: Escape closes it.
        if self.select_popup.is_some() || self.color_popup.is_some() {
            if matches!(key, Key::Named(NamedKey::Escape)) {
                self.select_popup = None;
                self.color_popup = None;
                self.request_redraw();
            }
            return;
        }
        // In-page form input editing.
        if self.session().and_then(Session::editing).is_some() {
            self.handle_page_input_key(key, command_held, shift_held, alt_held);
            return;
        }
        match key {
            Key::Named(NamedKey::ArrowDown) => self.scroll_by(SCROLL_STEP),
            Key::Named(NamedKey::ArrowUp) => self.scroll_by(-SCROLL_STEP),
            Key::Named(NamedKey::PageDown) | Key::Named(NamedKey::Space) => {
                self.scroll_by(self.viewport().height * 0.9);
            }
            Key::Named(NamedKey::PageUp) => self.scroll_by(-self.viewport().height * 0.9),
            Key::Named(NamedKey::Home) => {
                self.set_scroll(0.0);
                self.request_redraw();
            }
            Key::Character(text)
                if text.as_str() == "c"
                    && (self.modifiers.state().super_key()
                        || self.modifiers.state().control_key()) =>
            {
                self.copy_selection();
            }
            Key::Character(text) if text.as_str() == "a" && command_held => {
                self.select_all_page_text();
            }
            Key::Character(text) => match text.as_str() {
                "r" => self.start_nav(Nav::Refresh),
                "[" => self.start_nav(Nav::Back),
                "]" => self.start_nav(Nav::Forward),
                "l" => self.focus_url_bar(),
                _ => {}
            },
            _ => {}
        }
    }
}

impl ApplicationHandler<NavDone> for App {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.window.is_some() {
            return;
        }
        let attributes = Window::default_attributes()
            .with_title("Lumen")
            .with_min_inner_size(winit::dpi::LogicalSize::new(320.0, 240.0));
        let window = match event_loop.create_window(attributes) {
            Ok(window) => Rc::new(window),
            Err(error) => {
                eprintln!("error: cannot create window: {error}");
                event_loop.exit();
                return;
            }
        };
        let context = match softbuffer::Context::new(window.clone()) {
            Ok(context) => context,
            Err(error) => {
                eprintln!("error: cannot create draw context: {error}");
                event_loop.exit();
                return;
            }
        };
        match softbuffer::Surface::new(&context, window.clone()) {
            Ok(surface) => self.surface = Some(surface),
            Err(error) => {
                eprintln!("error: cannot create draw surface: {error}");
                event_loop.exit();
                return;
            }
        }
        self.window = Some(window);

        match url_from_user_input(&self.input) {
            Ok(url) => self.start_nav(Nav::Load(url)),
            Err(error) => {
                eprintln!("error: cannot load {}: {error}", self.input);
                event_loop.exit();
                return;
            }
        }
        self.update_title();
        self.request_redraw();
    }

    fn user_event(&mut self, _event_loop: &ActiveEventLoop, done: NavDone) {
        let failed = done.error.is_some();
        if let Some(error) = &done.error {
            eprintln!("navigation: {error}");
        }
        let Some(index) = tab_position(&self.tabs, done.tab) else {
            // The tab closed while loading; drop the result.
            return;
        };
        let mut session = done.session;
        // The window may have resized while the session was away.
        session.set_viewport(self.viewport());
        if index == self.active {
            if !failed {
                self.scroll_y = 0.0;
            }
            // A landed navigation gets a fresh script world (run here, on the
            // main thread); a failed one keeps the old page AND its scripts.
            if !failed || self.page_scripts.is_none() {
                self.page_scripts = lumen_browser::PageScripts::new(&mut session);
            }
            self.state = SessionState::Ready(session);
            self.follow_script_navigation();
            self.invalidate_page();
            self.scroll_to_fragment();
            self.refresh_find_matches();
            self.update_hover();
            // A blocked submit reports its violation on the session:
            // show the bubble next to the offending control.
            self.open_violation_popup();
        } else {
            // The user switched away while this tab was loading: the
            // result parks in its own slot instead of clobbering the
            // now-active tab. A script's follow-up navigation is dropped
            // — the parked tab is not live to perform it.
            let tab = &mut self.tabs[index];
            if !failed {
                tab.scroll_y = 0.0;
            }
            if !failed || tab.page_scripts.is_none() {
                tab.page_scripts = lumen_browser::PageScripts::new(&mut session);
            }
            tab.state = SessionState::Ready(session);
        }
        self.update_title();
        self.request_redraw();
    }

    fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
        // A pending script timer woke (or re-arms) the loop: fire it once
        // its deadline passed, sleep until then otherwise. No timers —
        // back to the default flow.
        let Some(due_ms) = self
            .page_scripts
            .as_ref()
            .and_then(lumen_browser::PageScripts::next_timer_due_ms)
        else {
            event_loop.set_control_flow(ControlFlow::Poll);
            return;
        };
        let wake = self.timer_wake(due_ms);
        if Instant::now() >= wake {
            self.request_redraw();
        } else {
            event_loop.set_control_flow(ControlFlow::WaitUntil(wake));
        }
    }

    fn window_event(
        &mut self,
        event_loop: &ActiveEventLoop,
        _window_id: WindowId,
        event: WindowEvent,
    ) {
        match event {
            WindowEvent::CloseRequested => event_loop.exit(),
            WindowEvent::Resized(_) => {
                let viewport = self.viewport();
                if let SessionState::Ready(session) = &mut self.state {
                    session.set_viewport(viewport);
                }
                self.set_scroll(self.scroll_y);
                self.invalidate_page();
                self.request_redraw();
            }
            WindowEvent::CursorMoved { position, .. } => {
                let scale = self.scale();
                self.cursor = Some((position.x as f32 / scale, position.y as f32 / scale));
                if let Some(popup) = &mut self.select_popup
                    && let Some((x, y)) = self
                        .cursor
                        .map(|(x, y)| (x, y - CHROME_HEIGHT + self.scroll_y))
                    && rect_contains(popup.rect, x, y)
                {
                    let row = (((y - popup.rect.y - 6.0).max(0.0) / SELECT_ROW_HEIGHT) as usize)
                        .min(popup.options.len().saturating_sub(1));
                    if row != popup.hovered {
                        popup.hovered = row;
                        self.request_redraw();
                    }
                }
                if let Some(popup) = &mut self.color_popup
                    && let Some((x, y)) = self
                        .cursor
                        .map(|(x, y)| (x, y - CHROME_HEIGHT + self.scroll_y))
                {
                    let hovered = (0..COLOR_SWATCHES.len())
                        .find(|index| rect_contains(swatch_rect(popup.rect, *index), x, y));
                    if hovered != popup.hovered {
                        popup.hovered = hovered;
                        self.request_redraw();
                    }
                }
                if let Some(control) = self.range_drag {
                    // Dragging a slider: the thumb tracks the pointer.
                    self.drag_range_to_cursor(control);
                } else if self.input_drag && self.press.is_some() {
                    // Drag-selecting inside the edited control: the caret
                    // extends the selection from the press anchor.
                    if let Some((x, _)) = self.page_cursor()
                        && let SessionState::Ready(session) = &mut self.state
                    {
                        session.edit_drag_to(x);
                        self.invalidate_page();
                        self.request_redraw();
                    }
                } else if self.press.is_some() {
                    // Dragging: extend the selection from the anchor.
                    if let (Some(anchor), Some(focus)) =
                        (self.select_anchor, self.caret_at_cursor())
                    {
                        self.selection = Some(Selection { anchor, focus });
                        self.request_redraw();
                    }
                } else {
                    self.update_hover();
                }
            }
            WindowEvent::CursorLeft { .. } => {
                self.cursor = None;
                if let SessionState::Ready(session) = &mut self.state
                    && session.set_hovered(None)
                {
                    self.invalidate_page();
                    self.request_redraw();
                }
            }
            WindowEvent::MouseInput {
                state: ElementState::Pressed,
                button: MouseButton::Left,
                ..
            } => {
                self.press = self.cursor;
                self.clear_selection();
                if self.cursor.is_some_and(|(_, y)| y >= CHROME_HEIGHT) {
                    self.select_anchor = self.caret_at_cursor();
                }
                // :active while the button is held.
                let hit = self.page_cursor().and_then(|(x, y)| {
                    self.session().and_then(Session::page).and_then(|page| {
                        page.layout
                            .hit_test_scrolled(x, y, session_offsets(&self.state))
                    })
                });
                if let SessionState::Ready(session) = &mut self.state
                    && session.set_active(hit)
                {
                    self.invalidate_page();
                    self.request_redraw();
                }
                // A press on a range slider grabs the thumb for dragging.
                let range = hit
                    .and_then(|node| self.form_control_at(node))
                    .filter(|control| {
                        self.session()
                            .and_then(Session::page)
                            .and_then(|page| page.document.element(*control))
                            .is_some_and(|element| {
                                element.tag_name == "input"
                                    && element.attributes.get("type") == Some("range")
                            })
                    });
                if let Some(control) = range {
                    self.range_drag = Some(control);
                    self.drag_range_to_cursor(control);
                }
                // A press inside the edited single-line control anchors a
                // caret drag-select (page text selection stays off).
                if let Some(control) = self.session().and_then(Session::editing) {
                    let over = hit.and_then(|node| self.form_control_at(node)) == Some(control)
                        && self
                            .session()
                            .is_some_and(|session| session.is_text_input(control));
                    if over && let Some((x, _)) = self.page_cursor() {
                        if let SessionState::Ready(session) = &mut self.state {
                            session.begin_edit(control, Some(x));
                        }
                        self.input_drag = true;
                        self.select_anchor = None;
                        self.invalidate_page();
                        self.request_redraw();
                    }
                }
            }
            WindowEvent::MouseInput {
                state: ElementState::Released,
                button: MouseButton::Left,
                ..
            } => {
                let press = self.press.take();
                self.range_drag = None;
                self.input_drag = false;
                self.select_anchor = None;
                if let SessionState::Ready(session) = &mut self.state
                    && session.set_active(None)
                {
                    self.invalidate_page();
                    self.request_redraw();
                }
                let moved = match (press, self.cursor) {
                    (Some((px, py)), Some((cx, cy))) => {
                        (px - cx).abs() > 3.0 || (py - cy).abs() > 3.0
                    }
                    _ => false,
                };
                if !moved {
                    // A stationary press is a click (links, chrome, focus).
                    self.clear_selection();
                    self.click();
                }
                self.update_hover();
            }
            WindowEvent::ModifiersChanged(modifiers) => {
                self.modifiers = modifiers;
            }
            WindowEvent::MouseWheel { delta, .. } => {
                let amount = match delta {
                    MouseScrollDelta::LineDelta(_, lines) => -lines * SCROLL_STEP,
                    MouseScrollDelta::PixelDelta(position) => -position.y as f32,
                };
                // Wheel over an overflow: scroll/auto box scrolls it;
                // everything else scrolls the page.
                let inner = self.page_cursor().and_then(|(x, y)| {
                    let session = self.session()?;
                    let page = session.page()?;
                    page.layout.scrollable_under(x, y, session.scroll_offsets())
                });
                if let Some((node, _)) = inner
                    && let SessionState::Ready(session) = &mut self.state
                    && session.scroll_inner(node, amount)
                {
                    self.invalidate_page();
                    self.request_redraw();
                } else {
                    self.scroll_by(amount);
                }
            }
            WindowEvent::KeyboardInput {
                event:
                    KeyEvent {
                        logical_key,
                        state: ElementState::Pressed,
                        ..
                    },
                ..
            } => self.handle_key(&logical_key),
            WindowEvent::KeyboardInput {
                event:
                    KeyEvent {
                        logical_key,
                        state: ElementState::Released,
                        ..
                    },
                ..
            } => {
                self.dispatch_key_event("keyup", &logical_key);
            }
            WindowEvent::RedrawRequested => self.redraw(event_loop),
            _ => {}
        }
    }
}

/// Turns address-bar text into a URL the way a browser omnibox does: an
/// explicit `http`/`https`/`file` URL is used verbatim, an existing local
/// path opens as a file, a bare dotted token (or `localhost`) becomes an
/// `https://` host, and anything else is a web search.
fn resolve_omnibox(input: &str) -> Url {
    if let Ok(url) = Url::parse(input)
        && matches!(url.scheme(), "http" | "https" | "file")
    {
        return url;
    }
    let single_token = input.split_whitespace().count() == 1;
    if single_token {
        // An existing local file (relative or absolute) opens directly.
        if let Ok(url) = url_from_user_input(input) {
            let on_disk = url
                .to_file_path()
                .map(|path| path.exists())
                .unwrap_or(false);
            if url.scheme() == "file" && on_disk {
                return url;
            }
        }
        // A dotted token or `localhost[:port]` is a bare hostname.
        let host_like = input.contains('.') || input == "localhost" || input.starts_with("localhost:");
        if host_like
            && let Ok(url) = Url::parse(&format!("https://{input}"))
            && url.host().is_some()
        {
            return url;
        }
    }
    // Fall back to a search; query_pairs_mut handles percent-encoding.
    let mut url = Url::parse("https://duckduckgo.com/html/").expect("static search URL");
    url.query_pairs_mut().append_pair("q", input);
    url
}

#[cfg(test)]
mod tests {
    use super::resolve_omnibox;

    #[test]
    fn omnibox_routes_urls_hosts_and_searches() {
        // Explicit scheme is kept.
        assert_eq!(
            resolve_omnibox("https://example.com/x").as_str(),
            "https://example.com/x"
        );
        // Bare dotted host gets https://.
        assert_eq!(resolve_omnibox("example.com").as_str(), "https://example.com/");
        assert_eq!(
            resolve_omnibox("localhost:8080").as_str(),
            "https://localhost:8080/"
        );
        // Free text becomes a search.
        let search = resolve_omnibox("rust async runtime");
        assert_eq!(search.host_str(), Some("duckduckgo.com"));
        assert_eq!(
            search.query_pairs().find(|(k, _)| k == "q").unwrap().1,
            "rust async runtime"
        );
        // A single word with no dot is a search, not a host.
        assert_eq!(
            resolve_omnibox("weather").host_str(),
            Some("duckduckgo.com")
        );
    }

    use super::{SessionState, Tab, paste_text, tab_position};

    fn parked_tab(id: u64) -> Tab {
        Tab {
            id,
            state: SessionState::Loading {
                target: String::new(),
            },
            scroll_y: 0.0,
            input: String::new(),
            page_scripts: None,
        }
    }

    #[test]
    fn nav_results_route_by_stable_tab_id() {
        let tabs = vec![parked_tab(1), parked_tab(2), parked_tab(3)];
        assert_eq!(tab_position(&tabs, 1), Some(0));
        assert_eq!(tab_position(&tabs, 3), Some(2));
        // A closed tab's late result is dropped, not misapplied.
        assert_eq!(tab_position(&tabs, 9), None);
    }

    #[test]
    fn paste_text_keeps_newlines_only_for_textareas() {
        // Single-line inputs flatten newlines; textareas keep them.
        assert_eq!(paste_text("a\nb\rc".to_string(), false), "a b c");
        assert_eq!(paste_text("a\nb".to_string(), true), "a\nb");
    }

    use super::{FindRun, cached, clip_with_ellipsis, find_matches_in_runs};

    #[test]
    fn generation_cache_rebuilds_only_on_bump() {
        let mut slot = None;
        let mut builds = 0;
        let value = cached(&mut slot, 7, || {
            builds += 1;
            "page-a"
        });
        assert_eq!(*value, "page-a");
        // Same generation: a hit, no rebuild.
        let value = cached(&mut slot, 7, || {
            builds += 1;
            "page-b"
        });
        assert_eq!(*value, "page-a");
        assert_eq!(builds, 1);
        // A bumped generation rebuilds exactly once.
        let value = cached(&mut slot, 8, || {
            builds += 1;
            "page-b"
        });
        assert_eq!(*value, "page-b");
        assert_eq!(builds, 2);
    }

    /// The pre-optimization clip loop, kept as the reference the
    /// binary-search version must match (monotone measures only).
    fn reference_clip(text: &str, max_width: f32, measure: impl Fn(&str) -> f32) -> String {
        if measure(text) <= max_width {
            return text.to_string();
        }
        let mut clipped = String::new();
        for ch in text.chars() {
            let candidate = format!("{clipped}{ch}…");
            if measure(&candidate) > max_width {
                break;
            }
            clipped.push(ch);
        }
        format!("{clipped}…")
    }

    #[test]
    fn clip_with_ellipsis_matches_reference_loop() {
        let measure = |text: &str| text.chars().count() as f32 * 10.0;
        let clip = |text: &str, max_width: f32| {
            if measure(text) <= max_width {
                text.to_string()
            } else {
                clip_with_ellipsis(text, max_width, measure)
            }
        };
        for text in [
            "",
            "short",
            "a much longer tab title",
            "ünïcödé başlık ✓",
            ".........",
        ] {
            for max_width in [0.0, 5.0, 10.0, 25.0, 55.0, 90.0, 200.0, 1000.0] {
                assert_eq!(
                    clip(text, max_width),
                    reference_clip(text, max_width, measure),
                    "clip({text:?}, {max_width})"
                );
            }
        }
    }

    /// The pre-optimization find scan (incremental byte→char conversion),
    /// kept as the reference for the boundary-table version.
    fn reference_find(runs: &[&str], needle: &str) -> Vec<(usize, usize, usize)> {
        let needle_chars = needle.chars().count();
        let mut out = Vec::new();
        for (index, text) in runs.iter().enumerate() {
            let haystack = text.to_ascii_lowercase();
            let mut prev_byte = 0;
            let mut prev_char = 0;
            for (byte_start, matched) in haystack.match_indices(needle) {
                prev_char += haystack[prev_byte..byte_start].chars().count();
                out.push((index, prev_char, prev_char + needle_chars));
                prev_char += needle_chars;
                prev_byte = byte_start + matched.len();
            }
        }
        out
    }

    #[test]
    fn find_matches_map_byte_offsets_to_char_offsets() {
        let runs = [
            "Hello hELLO",
            "dünya DÜNYA",
            "aa aa aa",
            "ünïcödé",
            "no match here",
            "",
        ];
        for needle in ["hello", "dünya", "aa", "é", "zzz", "n"] {
            let needle = needle.to_ascii_lowercase();
            let prepared: Vec<FindRun> = runs.iter().map(|text| FindRun::new(text)).collect();
            let matches = find_matches_in_runs(&prepared, &needle);
            let actual: Vec<(usize, usize, usize)> = matches
                .iter()
                .map(|found| (found.anchor.run, found.anchor.offset, found.focus.offset))
                .collect();
            assert_eq!(actual, reference_find(&runs, &needle), "needle {needle:?}");
        }
    }
}
