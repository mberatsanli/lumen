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
//! Cmd/Ctrl+C copies the selection.

use lumen_browser::Session;
use lumen_engine::{
    Caret, DisplayCommand, HeuristicMeasurer, Rect, Selection, Size, SystemFont, caret_at_point,
    collect_text_runs, highlight_rects, rasterize_over, rasterize_with, selected_text,
};
use lumen_engine::{TextMeasurer, TextMetrics, TextStyle};
use lumen_platform::{DefaultLoader, Url, url_from_user_input};
use std::num::NonZeroU32;
use std::rc::Rc;
use std::sync::Arc;
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
    Load(Url),
    Follow(String),
    Back,
    Forward,
    Refresh,
}

impl Nav {
    fn label(&self) -> String {
        match self {
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
    url_input: Option<String>,
    /// Where the left button went down (CSS window coords), while held.
    press: Option<(f32, f32)>,
    /// Selection anchor caret while dragging.
    select_anchor: Option<Caret>,
    selection: Option<Selection>,
    modifiers: Modifiers,
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
        }
    }

    /// The measurer that produced the current layout — selection geometry
    /// must use the same one.
    fn measurer(&self) -> Box<dyn TextMeasurer + '_> {
        match &self.font {
            Some(font) => Box::new(SharedFont(font.clone())),
            None => Box::new(HeuristicMeasurer),
        }
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
        match arboard::Clipboard::new() {
            Ok(mut clipboard) => {
                if let Err(error) = clipboard.set_text(text) {
                    eprintln!("clipboard: {error}");
                }
            }
            Err(error) => eprintln!("clipboard: {error}"),
        }
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
                Nav::Load(url) => session.load(url).map(|_| ()),
                Nav::Follow(href) => session.follow(&href).map(|_| ()),
                Nav::Back => session.back().map(|_| ()),
                Nav::Forward => session.forward().map(|_| ()),
                Nav::Refresh => session.refresh().map(|_| ()),
            };
            let _ = proxy.send_event(NavDone {
                session,
                error: result.err().map(|error| error.to_string()),
            });
        });
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
            },
            DisplayCommand::FillRect {
                rect: bar(60.0, 6.0, (width - 68.0).max(40.0), BAR_HEIGHT - 12.0),
                color: Color::rgb(0xff, 0xff, 0xff),
                radius: lumen_engine::Corners::uniform(6.0),
            },
        ];
        let (text, color) = match (&self.url_input, &self.state) {
            (Some(input), _) => (format!("{input}_"), enabled),
            (None, SessionState::Loading { target }) => {
                (format!("Loading {target}…"), Color::rgb(0x6a, 0x66, 0x72))
            }
            (None, SessionState::Ready(session)) => (
                session
                    .current_url()
                    .map_or_else(|| self.input.clone(), ToString::to_string),
                Color::rgb(0x6a, 0x66, 0x72),
            ),
        };
        commands.push(DisplayCommand::DrawText {
            x: 68.0,
            y: 24.0,
            text,
            color,
            font_size: 14.0,
            font_weight: 400,
            underline: false,
            italic: false,
        });
        commands
    }

    fn max_scroll(&self) -> f32 {
        let content = self
            .session()
            .and_then(Session::page)
            .map_or(0.0, |page| page.layout.content_box().height);
        (content - self.viewport().height).max(0.0)
    }

    fn scroll_by(&mut self, delta: f32) {
        self.scroll_y = (self.scroll_y + delta).clamp(0.0, self.max_scroll());
        self.update_hover();
        self.request_redraw();
    }

    /// Hit-tests the current cursor position, updates `:hover` styling and
    /// the pointer shape, and redraws when the hovered node changed.
    fn update_hover(&mut self) {
        let hit = self.page_cursor().and_then(|(x, y)| {
            self.session()
                .and_then(Session::page)
                .and_then(|page| page.layout.hit_test(x, y))
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
        let Some(node) = self.page_cursor().and_then(|(x, y)| {
            self.session()
                .and_then(Session::page)
                .and_then(|page| page.layout.hit_test(x, y))
        }) else {
            return;
        };
        if let Some(href) = self.session().and_then(|session| session.link_target(node)) {
            self.start_nav(Nav::Follow(href));
        }
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
        self.url_input = Some(
            self.session()
                .and_then(Session::current_url)
                .map_or_else(String::new, ToString::to_string),
        );
        self.request_redraw();
    }

    fn submit_url_bar(&mut self) {
        let Some(input) = self.url_input.take() else {
            return;
        };
        let input = input.trim().to_string();
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

    fn request_redraw(&self) {
        if let Some(window) = &self.window {
            window.request_redraw();
        }
    }

    fn update_title(&self) {
        if let Some(window) = &self.window {
            let label = match &self.state {
                SessionState::Loading { target } => format!("Lumen — loading {target}"),
                SessionState::Ready(session) => format!(
                    "Lumen — {}",
                    session
                        .current_url()
                        .map_or_else(|| self.input.clone(), ToString::to_string)
                ),
            };
            window.set_title(&label);
        }
    }

    fn redraw(&mut self) {
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
        // chrome painted over it.
        let mut framebuffer = match self.session().and_then(Session::page) {
            Some(page) => rasterize_with(
                &page.display_list,
                size.width,
                size.height,
                self.scroll_y - BAR_HEIGHT,
                scale,
                self.font.as_deref(),
            ),
            None => lumen_engine::Framebuffer::new(size.width, size.height),
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
        rasterize_over(&mut framebuffer, &chrome, 0.0, scale, self.font.as_deref());

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
    }

    fn handle_key(&mut self, key: &Key) {
        // Address-bar editing captures all input first.
        if self.url_input.is_some() {
            match key {
                Key::Named(NamedKey::Enter) => self.submit_url_bar(),
                Key::Named(NamedKey::Escape) => {
                    self.url_input = None;
                    self.request_redraw();
                }
                Key::Named(NamedKey::Backspace) => {
                    if let Some(input) = &mut self.url_input {
                        input.pop();
                        self.request_redraw();
                    }
                }
                Key::Named(NamedKey::Space) => {
                    if let Some(input) = &mut self.url_input {
                        input.push(' ');
                        self.request_redraw();
                    }
                }
                Key::Character(text) => {
                    if let Some(input) = &mut self.url_input {
                        input.push_str(text);
                        self.request_redraw();
                    }
                }
                _ => {}
            }
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
                self.scroll_y = 0.0;
                self.request_redraw();
            }
            Key::Character(text)
                if text.as_str() == "c"
                    && (self.modifiers.state().super_key()
                        || self.modifiers.state().control_key()) =>
            {
                self.copy_selection();
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
                self.scroll_y = self.scroll_y.clamp(0.0, self.max_scroll());
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
            }
            WindowEvent::MouseInput {
                state: ElementState::Released,
                button: MouseButton::Left,
                ..
            } => {
                let press = self.press.take();
                self.select_anchor = None;
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
                self.scroll_by(amount);
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
