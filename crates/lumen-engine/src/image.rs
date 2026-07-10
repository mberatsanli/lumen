//! Decoded raster images for `<img>` rendering.
//!
//! The engine never fetches: navigation code loads the bytes and hands
//! them to [`RasterImage::decode`]. Decoding uses the `image` crate
//! (PNG/JPEG) — codec work, like TLS and font parsing, is deliberately a
//! library rather than part of the educational pipeline.

use lumen_html::{Document, NodeId};
use std::collections::HashMap;
use std::sync::Arc;

/// A decoded image plus its original encoded bytes (kept for the SVG
/// backend, which embeds them as a data URI).
pub struct RasterImage {
    pub width: u32,
    pub height: u32,
    /// Row-major RGBA, 4 bytes per pixel.
    pub rgba: Vec<u8>,
    pub encoded: Vec<u8>,
    /// MIME type of `encoded` (`image/png` or `image/jpeg`).
    pub mime: &'static str,
}

impl PartialEq for RasterImage {
    fn eq(&self, other: &Self) -> bool {
        self.width == other.width && self.height == other.height && self.encoded == other.encoded
    }
}

impl std::fmt::Debug for RasterImage {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("RasterImage")
            .field("width", &self.width)
            .field("height", &self.height)
            .field("mime", &self.mime)
            .finish_non_exhaustive()
    }
}

impl RasterImage {
    /// Decodes PNG or JPEG bytes; `None` for anything else.
    #[must_use]
    pub fn decode(bytes: &[u8]) -> Option<Self> {
        let format = image::guess_format(bytes).ok()?;
        let mime = match format {
            image::ImageFormat::Png => "image/png",
            image::ImageFormat::Jpeg => "image/jpeg",
            _ => return None,
        };
        let decoded = image::load_from_memory_with_format(bytes, format).ok()?;
        let rgba = decoded.to_rgba8();
        Some(Self {
            width: rgba.width(),
            height: rgba.height(),
            rgba: rgba.into_raw(),
            encoded: bytes.to_vec(),
            mime,
        })
    }
}

/// Decoded images per `<img>` node.
pub type ImageMap = HashMap<NodeId, Arc<RasterImage>>;

/// All `<img src>` references in document order.
#[must_use]
pub fn collect_image_sources(document: &Document) -> Vec<(NodeId, String)> {
    document
        .descendants(document.root())
        .filter_map(|id| {
            let element = document.element(id)?;
            if element.tag_name != "img" {
                return None;
            }
            let src = element.attributes.get("src")?;
            (!src.is_empty()).then(|| (id, src.to_string()))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use lumen_html::parse_document;

    /// Encodes a 1×1 red PNG with the same codec used for decoding.
    fn red_pixel_png() -> Vec<u8> {
        let mut bytes = std::io::Cursor::new(Vec::new());
        image::RgbaImage::from_pixel(1, 1, image::Rgba([255, 0, 0, 255]))
            .write_to(&mut bytes, image::ImageFormat::Png)
            .expect("in-memory png encode");
        bytes.into_inner()
    }

    #[test]
    fn decodes_png_and_reports_size() {
        let image = RasterImage::decode(&red_pixel_png()).expect("valid png");
        assert_eq!((image.width, image.height), (1, 1));
        assert_eq!(&image.rgba[..4], &[255, 0, 0, 255]);
        assert_eq!(image.mime, "image/png");
    }

    #[test]
    fn garbage_is_not_an_image() {
        assert!(RasterImage::decode(b"not an image").is_none());
    }

    #[test]
    fn collects_img_sources_in_order() {
        let document = parse_document("<img src='a.png'><p><img src='b.jpg'></p><img><img src=''>");
        let sources = collect_image_sources(&document);
        let srcs: Vec<&str> = sources.iter().map(|(_, src)| src.as_str()).collect();
        assert_eq!(srcs, vec!["a.png", "b.jpg"]);
    }
}
