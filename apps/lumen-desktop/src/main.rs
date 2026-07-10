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
//! `[` / `]` go back / forward.

use lumen_browser::Session;
use lumen_engine::{Size, rasterize};
use lumen_platform::{DefaultLoader, url_from_user_input};
use std::num::NonZeroU32;
use std::rc::Rc;
use winit::application::ApplicationHandler;
use winit::event::{ElementState, KeyEvent, MouseScrollDelta, WindowEvent};
use winit::event_loop::{ActiveEventLoop, EventLoop};
use winit::keyboard::{Key, NamedKey};
use winit::window::{Window, WindowId};

const SCROLL_STEP: f32 = 48.0;

fn main() {
    let Some(input) = std::env::args().nth(1) else {
        eprintln!("usage: lumen-desktop <file-or-url>");
        std::process::exit(2);
    };

    let event_loop = match EventLoop::new() {
        Ok(event_loop) => event_loop,
        Err(error) => {
            eprintln!("error: cannot start event loop: {error}");
            std::process::exit(1);
        }
    };

    let mut app = App::new(input);
    if let Err(error) = event_loop.run_app(&mut app) {
        eprintln!("error: {error}");
        std::process::exit(1);
    }
}

struct App {
    input: String,
    session: Session<DefaultLoader>,
    window: Option<Rc<Window>>,
    surface: Option<softbuffer::Surface<Rc<Window>, Rc<Window>>>,
    scroll_y: f32,
}

impl App {
    fn new(input: String) -> Self {
        Self {
            input,
            session: Session::new(
                DefaultLoader,
                Size {
                    width: 1024.0,
                    height: 768.0,
                },
            ),
            window: None,
            surface: None,
            scroll_y: 0.0,
        }
    }

    fn viewport(&self) -> Size {
        self.window.as_ref().map_or(
            Size {
                width: 1024.0,
                height: 768.0,
            },
            |window| {
                let size = window.inner_size();
                Size {
                    width: size.width.max(1) as f32,
                    height: size.height.max(1) as f32,
                }
            },
        )
    }

    fn max_scroll(&self) -> f32 {
        let content = self
            .session
            .page()
            .map_or(0.0, |page| page.layout.content_box().height);
        (content - self.viewport().height).max(0.0)
    }

    fn scroll_by(&mut self, delta: f32) {
        self.scroll_y = (self.scroll_y + delta).clamp(0.0, self.max_scroll());
        self.request_redraw();
    }

    fn request_redraw(&self) {
        if let Some(window) = &self.window {
            window.request_redraw();
        }
    }

    fn update_title(&self) {
        if let Some(window) = &self.window {
            let url = self
                .session
                .current_url()
                .map_or_else(|| self.input.clone(), ToString::to_string);
            window.set_title(&format!("Lumen — {url}"));
        }
    }

    fn redraw(&mut self) {
        let (Some(window), Some(surface)) = (&self.window, &mut self.surface) else {
            return;
        };
        let size = window.inner_size();
        let (Some(width), Some(height)) =
            (NonZeroU32::new(size.width), NonZeroU32::new(size.height))
        else {
            return;
        };
        if surface.resize(width, height).is_err() {
            return;
        }

        let framebuffer = self
            .session
            .page()
            .map(|page| rasterize(&page.display_list, size.width, size.height, self.scroll_y));
        let Ok(mut buffer) = surface.buffer_mut() else {
            return;
        };
        match framebuffer {
            Some(framebuffer) => buffer.copy_from_slice(&framebuffer.pixels),
            None => buffer.fill(0x00ff_ffff),
        }
        let _ = buffer.present();
    }

    fn handle_key(&mut self, key: &Key) {
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
            Key::Character(text) => match text.as_str() {
                "r" => self.navigate(|session| session.refresh().map(|_| ())),
                "[" => self.navigate(|session| session.back().map(|_| ())),
                "]" => self.navigate(|session| session.forward().map(|_| ())),
                _ => {}
            },
            _ => {}
        }
    }

    fn navigate(
        &mut self,
        action: impl FnOnce(&mut Session<DefaultLoader>) -> Result<(), lumen_platform::LoadError>,
    ) {
        if let Err(error) = action(&mut self.session) {
            eprintln!("navigation: {error}");
        } else {
            self.scroll_y = 0.0;
        }
        self.update_title();
        self.request_redraw();
    }
}

impl ApplicationHandler for App {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.window.is_some() {
            return;
        }
        let attributes = Window::default_attributes().with_title("Lumen");
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

        let input = self.input.clone();
        let result = url_from_user_input(&input).and_then(|url| self.session.load(url).map(|_| ()));
        if let Err(error) = result {
            eprintln!("error: cannot load {input}: {error}");
            event_loop.exit();
            return;
        }
        let viewport = self.viewport();
        let _ = self.session.set_viewport(viewport);
        self.update_title();
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
                if self.session.set_viewport(viewport).is_ok() {
                    self.scroll_y = self.scroll_y.clamp(0.0, self.max_scroll());
                }
                self.request_redraw();
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
