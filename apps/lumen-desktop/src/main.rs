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
//! closes). F12 (or Cmd/Ctrl+D) toggles a debug HUD with FPS, frame
//! times, memory and page statistics. The address and find inputs support full editing: caret
//! movement, Shift+arrows selection, Home/End, Cmd/Ctrl+A/C/X/V.

use lumen_browser::Session;
use lumen_engine::{
    Caret, DisplayCommand, HeuristicMeasurer, Rect, Selection, Size, SystemFont, caret_at_point,
    collect_text_runs, highlight_rects, rasterize_over, rasterize_region, rasterize_with,
    selected_text,
};
use lumen_engine::{FontWeight, TextMeasurer, TextMetrics, TextStyle};
use lumen_platform::{DefaultLoader, Url, url_from_user_input};
use std::collections::VecDeque;
use std::num::NonZeroU32;
use std::rc::Rc;
use std::sync::Arc;
use std::time::{Duration, Instant};
use winit::application::ApplicationHandler;
use winit::event::Modifiers;
use winit::event::{ElementState, KeyEvent, MouseButton, MouseScrollDelta, WindowEvent};
use winit::event_loop::{ActiveEventLoop, EventLoop};
use winit::keyboard::{Key, NamedKey};
use winit::window::CursorIcon;
use winit::window::{Window, WindowId};

const SCROLL_STEP: f32 = 48.0;
/// Address-bar height in CSS pixels.
const BAR_HEIGHT: f32 = 36.0;

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
    session: Box<Session<DefaultLoader>>,
    error: Option<String>,
}

/// The session is either usable or away on a loader thread.
enum SessionState {
    Ready(Box<Session<DefaultLoader>>),
    Loading { target: String },
}

fn main() {
    let Some(input) = std::env::args().nth(1) else {
        eprintln!("usage: lumen-desktop <file-or-url>");
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

/// A single-line editable text field (address bar, find bar): a caret
/// and selection with the usual keyboard operations. Positions are in
/// chars.
struct TextInput {
    text: String,
    caret: usize,
    /// Selection anchor (== caret when nothing is selected).
    anchor: usize,
}

impl TextInput {
    fn with_all_selected(text: String) -> Self {
        let len = text.chars().count();
        Self {
            text,
            caret: len,
            anchor: 0,
        }
    }

    fn empty() -> Self {
        Self {
            text: String::new(),
            caret: 0,
            anchor: 0,
        }
    }

    fn char_count(&self) -> usize {
        self.text.chars().count()
    }

    /// Selection bounds in document order.
    fn selection(&self) -> (usize, usize) {
        (self.caret.min(self.anchor), self.caret.max(self.anchor))
    }

    fn has_selection(&self) -> bool {
        self.caret != self.anchor
    }

    fn slice(&self, start: usize, end: usize) -> String {
        self.text.chars().skip(start).take(end - start).collect()
    }

    fn selected_text(&self) -> String {
        let (start, end) = self.selection();
        self.slice(start, end)
    }

    fn byte_of(&self, char_index: usize) -> usize {
        self.text
            .char_indices()
            .nth(char_index)
            .map_or(self.text.len(), |(index, _)| index)
    }

    fn delete_selection(&mut self) {
        let (start, end) = self.selection();
        if start == end {
            return;
        }
        let (from, to) = (self.byte_of(start), self.byte_of(end));
        self.text.replace_range(from..to, "");
        self.caret = start;
        self.anchor = start;
    }

    fn insert(&mut self, input: &str) {
        self.delete_selection();
        let at = self.byte_of(self.caret);
        self.text.insert_str(at, input);
        self.caret += input.chars().count();
        self.anchor = self.caret;
    }

    fn backspace(&mut self) {
        if self.has_selection() {
            self.delete_selection();
            return;
        }
        if self.caret == 0 {
            return;
        }
        let (from, to) = (self.byte_of(self.caret - 1), self.byte_of(self.caret));
        self.text.replace_range(from..to, "");
        self.caret -= 1;
        self.anchor = self.caret;
    }

    fn delete_forward(&mut self) {
        if self.has_selection() {
            self.delete_selection();
            return;
        }
        if self.caret >= self.char_count() {
            return;
        }
        let (from, to) = (self.byte_of(self.caret), self.byte_of(self.caret + 1));
        self.text.replace_range(from..to, "");
    }

    /// Moves the caret by one; without `select`, a selection collapses to
    /// its matching edge first (as native inputs do).
    fn step(&mut self, forward: bool, select: bool) {
        if !select && self.has_selection() {
            let (start, end) = self.selection();
            self.caret = if forward { end } else { start };
        } else if forward {
            self.caret = (self.caret + 1).min(self.char_count());
        } else {
            self.caret = self.caret.saturating_sub(1);
        }
        if !select {
            self.anchor = self.caret;
        }
    }

    fn move_to(&mut self, index: usize, select: bool) {
        self.caret = index.min(self.char_count());
        if !select {
            self.anchor = self.caret;
        }
    }

    fn select_all(&mut self) {
        self.anchor = 0;
        self.caret = self.char_count();
    }

    /// Start of the word before the caret (Option+Left target).
    fn previous_word(&self) -> usize {
        let chars: Vec<char> = self.text.chars().collect();
        let mut index = self.caret.min(chars.len());
        while index > 0 && !chars[index - 1].is_alphanumeric() {
            index -= 1;
        }
        while index > 0 && chars[index - 1].is_alphanumeric() {
            index -= 1;
        }
        index
    }

    /// End of the word after the caret (Option+Right target).
    fn next_word(&self) -> usize {
        let chars: Vec<char> = self.text.chars().collect();
        let mut index = self.caret.min(chars.len());
        while index < chars.len() && !chars[index].is_alphanumeric() {
            index += 1;
        }
        while index < chars.len() && chars[index].is_alphanumeric() {
            index += 1;
        }
        index
    }
}

/// What an editing key did to a [`TextInput`].
enum EditOutcome {
    /// Text changed.
    Changed,
    /// Only the caret/selection moved.
    Moved,
    Submit,
    Cancel,
    Copy,
    Cut,
    Paste,
    Ignored,
}

/// Applies one key to a text input. Clipboard actions are reported, not
/// performed (the caller owns the clipboard).
fn apply_edit(
    input: &mut TextInput,
    key: &Key,
    command: bool,
    shift: bool,
    alt: bool,
) -> EditOutcome {
    match key {
        Key::Named(NamedKey::Enter) => EditOutcome::Submit,
        Key::Named(NamedKey::Escape) => EditOutcome::Cancel,
        Key::Named(NamedKey::Backspace) if alt => {
            // Option+Backspace deletes the previous word.
            if !input.has_selection() {
                let target = input.previous_word();
                input.anchor = input.caret;
                input.caret = target;
            }
            input.delete_selection();
            EditOutcome::Changed
        }
        Key::Named(NamedKey::Backspace) => {
            input.backspace();
            EditOutcome::Changed
        }
        Key::Named(NamedKey::Delete) => {
            input.delete_forward();
            EditOutcome::Changed
        }
        Key::Named(NamedKey::ArrowLeft) if command => {
            input.move_to(0, shift);
            EditOutcome::Moved
        }
        Key::Named(NamedKey::ArrowRight) if command => {
            input.move_to(usize::MAX, shift);
            EditOutcome::Moved
        }
        // Option+arrows step words (with Shift: extend the selection).
        Key::Named(NamedKey::ArrowLeft) if alt => {
            let target = input.previous_word();
            input.move_to(target, shift);
            EditOutcome::Moved
        }
        Key::Named(NamedKey::ArrowRight) if alt => {
            let target = input.next_word();
            input.move_to(target, shift);
            EditOutcome::Moved
        }
        Key::Named(NamedKey::ArrowLeft) => {
            input.step(false, shift);
            EditOutcome::Moved
        }
        Key::Named(NamedKey::ArrowRight) => {
            input.step(true, shift);
            EditOutcome::Moved
        }
        Key::Named(NamedKey::Home) => {
            input.move_to(0, shift);
            EditOutcome::Moved
        }
        Key::Named(NamedKey::End) => {
            input.move_to(usize::MAX, shift);
            EditOutcome::Moved
        }
        Key::Named(NamedKey::Space) => {
            input.insert(" ");
            EditOutcome::Changed
        }
        Key::Character(text) if command => match text.as_str() {
            "a" => {
                input.select_all();
                EditOutcome::Moved
            }
            "c" => EditOutcome::Copy,
            "x" => EditOutcome::Cut,
            "v" => EditOutcome::Paste,
            _ => EditOutcome::Ignored,
        },
        Key::Character(text) => {
            input.insert(text);
            EditOutcome::Changed
        }
        _ => EditOutcome::Ignored,
    }
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
                y: (y - page_scroll + BAR_HEIGHT) * scale,
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
    /// In-page text input being edited: (input element node, edit state).
    page_input: Option<(usize, TextInput)>,
    find_matches: Vec<Selection>,
    find_index: usize,
    /// Damage tracking: bumped whenever the page raster could change.
    page_generation: u64,
    /// Monotonic clock origin for animation ticks.
    started: Instant,
    /// Debug HUD (F12 / Cmd+D): FPS, memory, frame + page stats.
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
            page_input: None,
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

    fn caret_at_cursor(&self) -> Option<Caret> {
        let (x, y) = self.page_cursor()?;
        let page = self.session()?.page()?;
        let runs = collect_text_runs(&page.layout);
        caret_at_point(&runs, x, y, self.measurer().as_ref())
    }

    fn clear_selection(&mut self) {
        if self.selection.take().is_some() {
            self.request_redraw();
        }
        self.select_anchor = None;
    }

    fn copy_selection(&mut self) {
        let (Some(selection), Some(session)) = (self.selection, self.session()) else {
            return;
        };
        let Some(page) = session.page() else { return };
        let runs = collect_text_runs(&page.layout);
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
        self.selection = None;
        self.select_anchor = None;
        let SessionState::Ready(mut session) =
            std::mem::replace(&mut self.state, SessionState::Loading { target })
        else {
            return;
        };
        let viewport = self.viewport();
        let proxy = self.proxy.clone();
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
                    height: (size.height.max(1) as f32 / scale - BAR_HEIGHT).max(1.0),
                }
            },
        )
    }

    /// Cursor position translated into page coordinates, when it is over
    /// the page area (below the address bar).
    fn page_cursor(&self) -> Option<(f32, f32)> {
        let (x, y) = self.cursor?;
        (y >= BAR_HEIGHT).then_some((x, y - BAR_HEIGHT + self.scroll_y))
    }

    /// Paint commands for the browser chrome (address bar, nav buttons).
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
                rect: bar(60.0, 6.0, (width - 68.0).max(40.0), BAR_HEIGHT - 12.0),
                color: Color::rgb(0xff, 0xff, 0xff),
                radius: lumen_engine::Corners::uniform(6.0),
            },
        ];
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
                rect: bar(x, BAR_HEIGHT + 4.0, bar_width, 26.0),
                color: Color::rgb(0xfd, 0xf6, 0xd8),
                radius: lumen_engine::Corners::uniform(5.0),
            });
            commands.push(DisplayCommand::DrawText {
                x: x + 8.0,
                y: BAR_HEIGHT + 22.0,
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
                BAR_HEIGHT + 22.0,
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
                y: BAR_HEIGHT + 22.0,
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
        commands
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
        self.set_scroll(self.scroll_y + delta);
        self.update_hover();
        self.request_redraw();
    }

    /// Scroll-only cache reuse: shifts the cached page raster by the
    /// scroll delta and rasterizes only the exposed strip (plus the bar
    /// rows, whose shifted pixels are stale under the chrome).
    fn blit_scrolled(
        &self,
        frame: &lumen_engine::Framebuffer,
        old_scroll: f32,
        width: u32,
        height: u32,
        scale: f32,
    ) -> Option<lumen_engine::Framebuffer> {
        let delta = ((self.scroll_y - old_scroll) * scale).round() as i64;
        if delta == 0 || delta.abs() >= i64::from(height) {
            return None;
        }
        let page = self.session().and_then(Session::page)?;
        let mut shifted = frame.clone();
        let row = width as usize;
        let kept = (i64::from(height) - delta.abs()) as usize;
        let exposed = if delta > 0 {
            // Scrolled down: rows move up; the bottom strip is new.
            let from = delta as usize * row;
            shifted.pixels.copy_within(from..from + kept * row, 0);
            (height - delta as u32, height)
        } else {
            // Scrolled up: rows move down; the top strip is new.
            let up = (-delta) as usize;
            shifted.pixels.copy_within(0..kept * row, up * row);
            (0, (-delta) as u32)
        };
        let bar_rows = ((BAR_HEIGHT * scale).ceil() as u32).min(height);
        for region in [(0, exposed.0, width, exposed.1), (0, 0, width, bar_rows)] {
            if region.3 > region.1 {
                rasterize_region(
                    &mut shifted,
                    &page.display_list,
                    self.scroll_y - BAR_HEIGHT,
                    scale,
                    self.effective_font().as_deref(),
                    region,
                );
            }
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
            && self.caret_at_cursor().is_some_and(|_| {
                // Only show the I-beam when actually over a text run's rect.
                self.page_cursor().is_some_and(|(x, y)| {
                    self.session().and_then(Session::page).is_some_and(|page| {
                        collect_text_runs(&page.layout).iter().any(|run| {
                            x >= run.rect.x
                                && x < run.rect.x + run.rect.width
                                && y >= run.rect.y
                                && y < run.rect.y + run.rect.height
                        })
                    })
                })
            });
        if let Some(window) = &self.window {
            window.set_cursor(if over_link {
                CursorIcon::Pointer
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
        if let Some((x, y)) = self.cursor
            && y < BAR_HEIGHT
        {
            self.chrome_click(x);
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
            let kind = self
                .session()
                .and_then(Session::page)
                .and_then(|page| page.document.element(control))
                .and_then(|element| element.attributes.get("type"))
                .unwrap_or("text")
                .to_string();
            match kind.as_str() {
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
                    self.page_input = None;
                    self.start_nav(Nav::Submit(control));
                    return;
                }
                _ => {
                    let is_text = self
                        .session()
                        .is_some_and(|session| session.is_text_input(control));
                    if is_text {
                        let value = self
                            .session()
                            .map(|session| session.form_value(control))
                            .unwrap_or_default();
                        // Position the caret at the click (or select all on
                        // a fresh focus of another control).
                        let mut input = TextInput::with_all_selected(value);
                        if let Some(index) = self.caret_index_in_control(control) {
                            input.move_to(index, false);
                        }
                        self.page_input = Some((control, input));
                        self.invalidate_page();
                        self.request_redraw();
                        return;
                    }
                }
            }
        }
        self.page_input = None;
        let Some(node) = node else { return };
        if let Some(href) = self.session().and_then(|session| session.link_target(node)) {
            self.start_nav(Nav::Follow(href));
        }
    }

    /// What a control actually displays: passwords render as bullets, so
    /// caret math must measure bullets too.
    fn control_display_text(&self, control: usize, value: &str) -> String {
        let is_password = self
            .session()
            .and_then(Session::page)
            .and_then(|page| page.document.element(control))
            .and_then(|element| element.attributes.get("type"))
            == Some("password");
        if is_password {
            "\u{2022}".repeat(value.chars().count())
        } else {
            value.to_string()
        }
    }

    /// The nearest `<input>` element at or above a hit node.
    fn form_control_at(&self, node: usize) -> Option<usize> {
        let page = self.session().and_then(Session::page)?;
        let document = &page.document;
        std::iter::once(node)
            .chain(document.ancestors(node))
            .find(|candidate| {
                document
                    .element(*candidate)
                    .is_some_and(|element| element.tag_name == "input")
            })
    }

    /// Where in a text control's value the cursor points (char index).
    fn caret_index_in_control(&self, control: usize) -> Option<usize> {
        let (x, _) = self.page_cursor()?;
        let session = self.session()?;
        let page = session.page()?;
        let laid = page.layout.find_by_node(control)?;
        let content = laid.content_box();
        let value = self.control_display_text(control, &session.form_value(control));
        let style = page.styles.by_node.get(&control)?;
        let text_style = TextStyle {
            font_size: style.font_size,
            font_weight: style.font_weight,
            monospace: style.monospace,
            letter_spacing: style.letter_spacing,
        };
        let measurer = self.measurer();
        let relative = (x - content.x).max(0.0);
        let mut best = value.chars().count();
        for index in 0..=value.chars().count() {
            let prefix: String = value.chars().take(index).collect();
            if measurer.measure(&prefix, &text_style).width >= relative {
                best = index;
                break;
            }
        }
        Some(best)
    }

    fn chrome_click(&mut self, x: f32) {
        match x {
            x if (8.0..32.0).contains(&x) => self.start_nav(Nav::Back),
            x if (32.0..56.0).contains(&x) => self.start_nav(Nav::Forward),
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
        match url_from_user_input(&input) {
            Ok(url) => self.start_nav(Nav::Load(url)),
            Err(error) => {
                eprintln!("address bar: {error}");
                self.request_redraw();
            }
        }
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
    /// case-insensitive, per text run — matches never span runs).
    fn refresh_find_matches(&mut self) {
        self.find_matches.clear();
        self.find_index = 0;
        let Some(query) = self.find_input.as_ref().map(|input| input.text.clone()) else {
            return;
        };
        if query.is_empty() {
            return;
        }
        let Some(page) = self.session().and_then(Session::page) else {
            return;
        };
        let needle = query.to_ascii_lowercase();
        let needle_chars = needle.chars().count();
        for (index, run) in collect_text_runs(&page.layout).iter().enumerate() {
            let haystack = run.text.to_ascii_lowercase();
            // match_indices yields byte offsets; carets use char offsets, so
            // convert incrementally (matches come back in ascending order).
            let mut prev_byte = 0;
            let mut prev_char = 0;
            for (byte_start, matched) in haystack.match_indices(&needle) {
                prev_char += haystack[prev_byte..byte_start].chars().count();
                self.find_matches.push(Selection {
                    anchor: Caret {
                        run: index,
                        offset: prev_char,
                    },
                    focus: Caret {
                        run: index,
                        offset: prev_char + needle_chars,
                    },
                });
                prev_char += needle_chars;
                prev_byte = byte_start + matched.len();
            }
        }
    }

    /// Scrolls the current find match into view (upper third).
    fn scroll_to_find_match(&mut self) {
        let region = {
            let Some(selection) = self.find_matches.get(self.find_index).copied() else {
                return;
            };
            let Some(page) = self.session().and_then(Session::page) else {
                return;
            };
            let runs = collect_text_runs(&page.layout);
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
        let Some(page) = self.session().and_then(Session::page) else {
            return;
        };
        let runs = collect_text_runs(&page.layout);
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
        let y = BAR_HEIGHT + 8.0;
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

    fn redraw(&mut self) {
        let frame_started = Instant::now();
        // Step running CSS transitions; keep redrawing while any are live.
        let now_ms = self.started.elapsed().as_secs_f64() * 1000.0;
        if let SessionState::Ready(session) = &mut self.state
            && session.tick(now_ms)
        {
            self.invalidate_page();
            self.request_redraw();
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
            // Same page, same size, different scroll: blit instead of a
            // full re-rasterization.
            let blitted = match &self.page_frame {
                Some((key, frame))
                    if key.0 == self.page_generation
                        && key.2 == size.width
                        && key.3 == size.height =>
                {
                    self.blit_scrolled(frame, f32::from_bits(key.1), size.width, size.height, scale)
                }
                _ => None,
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
                            self.scroll_y - BAR_HEIGHT,
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
        if let (Some(selection), Some(page)) =
            (self.selection, self.session().and_then(Session::page))
            && !selection.is_empty()
        {
            let runs = collect_text_runs(&page.layout);
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
                        y: (region.rect.y - self.scroll_y + BAR_HEIGHT) * scale,
                        width: region.rect.width * scale,
                        height: region.rect.height * scale,
                    },
                    color,
                    alpha,
                );
            }
        }
        // Find matches highlight in yellow; the current one in orange.
        if self.find_input.is_some()
            && !self.find_matches.is_empty()
            && let Some(page) = self.session().and_then(Session::page)
        {
            let runs = collect_text_runs(&page.layout);
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
                            y: (region.rect.y - self.scroll_y + BAR_HEIGHT) * scale,
                            width: region.rect.width * scale,
                            height: region.rect.height * scale,
                        },
                        color,
                        alpha,
                    );
                }
            }
        }
        // Focused in-page input: selection highlight + caret line.
        if let Some((control, input)) = &self.page_input
            && let Some(page) = self.session().and_then(Session::page)
            && let Some(laid) = page.layout.find_by_node(*control)
        {
            let content = laid.content_box();
            if let Some(style) = page.styles.by_node.get(control) {
                let text_style = TextStyle {
                    font_size: style.font_size,
                    font_weight: style.font_weight,
                    monospace: style.monospace,
                    letter_spacing: style.letter_spacing,
                };
                let measurer = self.measurer();
                let display = self.control_display_text(*control, &input.text);
                let width_to = |index: usize| {
                    let prefix: String = display.chars().take(index).collect();
                    measurer.measure(&prefix, &text_style).width
                };
                let to_device = |x: f32, y: f32, w: f32, h: f32| Rect {
                    x: x * scale,
                    y: (y - self.scroll_y + BAR_HEIGHT) * scale,
                    width: w * scale,
                    height: h * scale,
                };
                let (start, end) = input.selection();
                if start != end {
                    let x0 = content.x + width_to(start);
                    let x1 = content.x + width_to(end);
                    framebuffer.blend_fill(
                        to_device(x0, content.y, x1 - x0, content.height),
                        lumen_css::Color::rgb(0xb3, 0xd4, 0xfc),
                        140,
                    );
                }
                let caret_x = content.x + width_to(input.caret);
                framebuffer.blend_fill(
                    to_device(caret_x, content.y, 1.5, content.height),
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
        // Scrollbar: a proportional overlay thumb on the right edge.
        let max_scroll = self.max_scroll();
        if max_scroll > 0.0 {
            let viewport = self.viewport();
            let content_height = viewport.height + max_scroll;
            let thumb_height = (viewport.height * viewport.height / content_height).max(24.0);
            let thumb_y = BAR_HEIGHT
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
        let Some((control, input)) = &mut self.page_input else {
            return;
        };
        let control = *control;
        let mut sync = false;
        match apply_edit(input, key, command_held, shift_held, alt_held) {
            EditOutcome::Changed => sync = true,
            EditOutcome::Moved => self.request_redraw(),
            EditOutcome::Submit => {
                self.page_input = None;
                self.start_nav(Nav::Submit(control));
                return;
            }
            EditOutcome::Cancel => {
                self.page_input = None;
                if let SessionState::Ready(session) = &mut self.state
                    && session.set_focused(None)
                {
                    self.invalidate_page();
                }
                self.request_redraw();
                return;
            }
            EditOutcome::Copy => {
                clipboard_set(&input.selected_text());
            }
            EditOutcome::Cut => {
                clipboard_set(&input.selected_text());
                input.delete_selection();
                sync = true;
            }
            EditOutcome::Paste => {
                if let Some(pasted) = clipboard_get() {
                    input.insert(pasted.replace(['\n', '\r'], " ").as_str());
                    sync = true;
                }
            }
            EditOutcome::Ignored => {}
        }
        if sync {
            let value = self.page_input.as_ref().unwrap().1.text.clone();
            if let SessionState::Ready(session) = &mut self.state {
                session.set_form_value(control, &value);
            }
            self.invalidate_page();
            self.request_redraw();
        }
    }

    fn handle_key(&mut self, key: &Key) {
        let command_held =
            self.modifiers.state().super_key() || self.modifiers.state().control_key();
        // Ctrl/Cmd+F toggles the find bar from anywhere.
        if command_held && matches!(key, Key::Character(text) if text.as_str() == "f") {
            self.open_find_bar();
            return;
        }
        // F12 (or Ctrl/Cmd+D) toggles the debug HUD.
        if matches!(key, Key::Named(NamedKey::F12))
            || (command_held && matches!(key, Key::Character(text) if text.as_str() == "d"))
        {
            self.debug_hud = !self.debug_hud;
            self.request_redraw();
            return;
        }
        let shift_held = self.modifiers.state().shift_key();
        let alt_held = self.modifiers.state().alt_key();
        // Bar editing captures input first: the find bar, then the address
        // bar. Both share one handler; only Submit/Cancel and the post-edit
        // refresh differ per bar.
        for bar in [EditBar::Find, EditBar::Url] {
            if self.handle_bar_key(bar, key, command_held, shift_held, alt_held) {
                return;
            }
        }
        // In-page form input editing.
        if self.page_input.is_some() {
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
        if let Some(error) = done.error {
            eprintln!("navigation: {error}");
        } else {
            self.scroll_y = 0.0;
        }
        let mut session = done.session;
        // The window may have resized while the session was away.
        session.set_viewport(self.viewport());
        self.state = SessionState::Ready(session);
        self.invalidate_page();
        self.scroll_to_fragment();
        self.refresh_find_matches();
        self.update_title();
        self.update_hover();
        self.request_redraw();
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
                if self.press.is_some() {
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
                if self.cursor.is_some_and(|(_, y)| y >= BAR_HEIGHT) {
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
            }
            WindowEvent::MouseInput {
                state: ElementState::Released,
                button: MouseButton::Left,
                ..
            } => {
                let press = self.press.take();
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
            WindowEvent::RedrawRequested => self.redraw(),
            _ => {}
        }
    }
}
