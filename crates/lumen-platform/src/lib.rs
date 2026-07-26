//! Platform services for Lumen: resource loading and (eventually) window
//! surfaces. Engine crates never depend on this; orchestration code wires
//! platform services and the engine together.

pub mod encoding;
pub mod loader;

pub use loader::{
    DefaultLoader, FileLoader, HttpLoader, LoadError, ResourceLoader, ResourceRequest,
    ResourceResponse, USER_AGENT, Url, resolve, url_from_user_input,
};

/// Window size in physical pixels.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WindowSize {
    pub width: u32,
    pub height: u32,
}

/// A surface pixels can be presented to.
pub trait Surface {
    fn resize(&mut self, size: WindowSize);
    fn present(&mut self, pixels: &[u32]);
}

/// Surface that discards frames — for tests and headless runs.
#[derive(Debug, Default)]
pub struct HeadlessSurface {
    pub size: Option<WindowSize>,
    pub frames_presented: usize,
}

impl Surface for HeadlessSurface {
    fn resize(&mut self, size: WindowSize) {
        self.size = Some(size);
    }

    fn present(&mut self, _pixels: &[u32]) {
        self.frames_presented += 1;
    }
}
