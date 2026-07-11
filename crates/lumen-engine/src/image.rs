//! Decoded raster images for `<img>` rendering.
//!
//! The engine never fetches: navigation code loads the bytes and hands
//! them to [`RasterImage::decode`]. Decoding uses the `image` crate
//! (PNG/JPEG/GIF/WebP — animated formats keep their first frame) — codec
//! work, like TLS and font parsing, is deliberately a library rather than
//! part of the educational pipeline.

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
    /// MIME type of `encoded` (e.g. `image/png`).
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
    /// Decodes PNG, JPEG, GIF or WebP bytes (animated GIF/WebP decode to
    /// their first frame); `None` for anything else.
    #[must_use]
    pub fn decode(bytes: &[u8]) -> Option<Self> {
        let format = image::guess_format(bytes).ok()?;
        let mime = match format {
            image::ImageFormat::Png => "image/png",
            image::ImageFormat::Jpeg => "image/jpeg",
            image::ImageFormat::Gif => "image/gif",
            image::ImageFormat::WebP => "image/webp",
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

/// All `<img>` source references in document order. `srcset` (when
/// present) wins over `src`, picking the candidate whose density
/// descriptor is closest to 1x; width (`w`) descriptors fall back to the
/// first candidate.
#[must_use]
pub fn collect_image_sources(document: &Document) -> Vec<(NodeId, String)> {
    document
        .descendants(document.root())
        .filter_map(|id| {
            let element = document.element(id)?;
            if element.tag_name != "img" {
                return None;
            }
            let source = element
                .attributes
                .get("srcset")
                .and_then(pick_srcset_candidate)
                .or_else(|| {
                    element
                        .attributes
                        .get("src")
                        .filter(|src| !src.is_empty())
                        .map(String::from)
                })?;
            Some((id, source))
        })
        .collect()
}

/// Picks one URL from a `srcset` list: the candidate with density closest
/// to 1x, or the first candidate when only `w` descriptors are present.
fn pick_srcset_candidate(srcset: &str) -> Option<String> {
    let mut best: Option<(f32, String)> = None;
    let mut first: Option<String> = None;
    for candidate in srcset.split(',') {
        let mut parts = candidate.split_whitespace();
        let url = parts.next()?.to_string();
        if url.is_empty() {
            continue;
        }
        if first.is_none() {
            first = Some(url.clone());
        }
        let density = match parts.next() {
            None => 1.0,
            Some(descriptor) => match descriptor.strip_suffix('x') {
                Some(value) => value.parse::<f32>().ok().unwrap_or(f32::MAX),
                // `w` descriptors need layout knowledge; skip.
                None => continue,
            },
        };
        let distance = (density - 1.0).abs();
        if best.as_ref().is_none_or(|(current, _)| distance < *current) {
            best = Some((distance, url));
        }
    }
    best.map(|(_, url)| url).or(first)
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
    fn decodes_gif_first_frame() {
        let mut bytes = std::io::Cursor::new(Vec::new());
        image::RgbaImage::from_pixel(2, 3, image::Rgba([0, 255, 0, 255]))
            .write_to(&mut bytes, image::ImageFormat::Gif)
            .expect("in-memory gif encode");
        let decoded = RasterImage::decode(&bytes.into_inner()).expect("valid gif");
        assert_eq!((decoded.width, decoded.height), (2, 3));
        assert_eq!(decoded.mime, "image/gif");
    }

    #[test]
    fn decodes_webp() {
        let mut bytes = std::io::Cursor::new(Vec::new());
        image::RgbaImage::from_pixel(4, 2, image::Rgba([0, 0, 255, 255]))
            .write_to(&mut bytes, image::ImageFormat::WebP)
            .expect("in-memory webp encode");
        let decoded = RasterImage::decode(&bytes.into_inner()).expect("valid webp");
        assert_eq!((decoded.width, decoded.height), (4, 2));
        assert_eq!(decoded.mime, "image/webp");
    }

    #[test]
    fn srcset_picks_the_1x_candidate() {
        let document = parse_document(
            "<img srcset='small.png 1x, big.png 2x' src='fallback.png'>\
             <img srcset='w1.png 400w, w2.png 800w'>\
             <img src='plain.png'>",
        );
        let sources: Vec<String> = collect_image_sources(&document)
            .into_iter()
            .map(|(_, source)| source)
            .collect();
        assert_eq!(sources, vec!["small.png", "w1.png", "plain.png"]);
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
