#[derive(Debug, Clone, Copy, PartialEq)]
pub struct WindowSize {
    pub width: u32,
    pub height: u32,
}

pub trait Surface {
    fn resize(&mut self, size: WindowSize);
    fn present(&mut self, pixels: &[u32]);
}

/// Placeholder for the native window implementation planned for v0.4.
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
