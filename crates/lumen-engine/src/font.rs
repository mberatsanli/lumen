//! Real font metrics and glyph rasterization behind [`TextMeasurer`].
//!
//! Wraps `fontdue` (a pure-Rust TTF/OTF rasterizer — font parsing is not
//! browser-engine core, like TLS it is deliberately a library). The engine
//! ships no font asset; callers load a font file (usually a system font via
//! [`SystemFont::load_default`]) and pass it in. Everything keeps working
//! without one: layout falls back to [`crate::HeuristicMeasurer`] and the
//! rasterizer to the built-in bitmap font.

use crate::text::{TextMeasurer, TextMetrics, TextStyle};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

/// A loaded scalable font usable for both measurement and rasterization.
///
/// Rasterized glyphs are cached per (character, size), which makes
/// repeated frames (scrolling, resizing) cheap. The cache sits behind a
/// `Mutex`, so the font is `Sync` and can be shared via `Arc` — including
/// with background loader threads.
pub struct SystemFont {
    font: fontdue::Font,
    /// Monospace face for `font-family: monospace`; falls back to the
    /// regular face when no monospace font was found.
    mono: Option<fontdue::Font>,
    /// Wide-coverage faces consulted when the chosen face lacks a glyph
    /// (symbols, exotic scripts) — otherwise text shows notdef boxes.
    /// One lazily-parsed slot per candidate path, tried in order, so a
    /// single uncovered glyph only pays for the faces up to the one
    /// that covers it (parsing all of macOS's fallback faces up front
    /// costs hundreds of MB in fontdue).
    fallbacks: Vec<std::sync::OnceLock<Option<fontdue::Font>>>,
    glyph_cache: Mutex<HashMap<(char, u32, bool), Arc<Glyph>>>,
}

/// A rasterized glyph: metrics plus an 8-bit coverage bitmap.
pub struct Glyph {
    pub metrics: fontdue::Metrics,
    pub coverage: Vec<u8>,
}

/// Common monospace font locations per platform, tried in order.
const MONO_CANDIDATE_PATHS: [&str; 8] = [
    // macOS
    "/System/Library/Fonts/Monaco.ttf",
    "/System/Library/Fonts/Supplemental/Courier New.ttf",
    "/System/Library/Fonts/Menlo.ttc",
    // Linux
    "/usr/share/fonts/truetype/dejavu/DejaVuSansMono.ttf",
    "/usr/share/fonts/TTF/DejaVuSansMono.ttf",
    "/usr/share/fonts/truetype/liberation/LiberationMono-Regular.ttf",
    // Windows
    "C:\\Windows\\Fonts\\consola.ttf",
    "C:\\Windows\\Fonts\\cour.ttf",
];

/// Wide-coverage fallback faces per platform, tried in order (all that
/// parse are kept).
const FALLBACK_CANDIDATE_PATHS: [&str; 5] = [
    // macOS — cheap first: Apple Symbols covers the common triggers
    // (arrows, math symbols) for ~2 MB; Arial Unicode is the last
    // resort for exotic scripts and is expensive to parse.
    "/System/Library/Fonts/Apple Symbols.ttf",
    "/System/Library/Fonts/Supplemental/Arial Unicode.ttf",
    // Linux
    "/usr/share/fonts/truetype/noto/NotoSansSymbols2-Regular.ttf",
    "/usr/share/fonts/truetype/dejavu/DejaVuSans.ttf",
    // Windows
    "C:\\Windows\\Fonts\\seguisym.ttf",
];

/// Common system font locations per platform, tried in order.
const CANDIDATE_PATHS: [&str; 8] = [
    // macOS
    "/System/Library/Fonts/Supplemental/Arial.ttf",
    "/System/Library/Fonts/Supplemental/Verdana.ttf",
    "/System/Library/Fonts/Supplemental/Tahoma.ttf",
    // Linux
    "/usr/share/fonts/truetype/dejavu/DejaVuSans.ttf",
    "/usr/share/fonts/TTF/DejaVuSans.ttf",
    "/usr/share/fonts/truetype/liberation/LiberationSans-Regular.ttf",
    // Windows
    "C:\\Windows\\Fonts\\arial.ttf",
    "C:\\Windows\\Fonts\\segoeui.ttf",
];

impl SystemFont {
    /// Parses font bytes: TTF/OTF directly, WOFF and WOFF2 by unpacking
    /// to TTF first. Returns `None` when the data is not a usable font.
    #[must_use]
    pub fn from_bytes(data: &[u8]) -> Option<Self> {
        let unpacked: Vec<u8>;
        let data = match data.get(..4) {
            Some(b"wOF2") => {
                unpacked = woff2_patched::decode::convert_woff2_to_ttf(&mut &data[..]).ok()?;
                &unpacked[..]
            }
            Some(b"wOFF") => {
                unpacked = woff1_to_ttf(data)?;
                &unpacked[..]
            }
            _ => data,
        };
        fontdue::Font::from_bytes(data, fontdue::FontSettings::default())
            .ok()
            .map(|font| Self {
                font,
                mono: None,
                fallbacks: Vec::new(),
                glyph_cache: Mutex::new(HashMap::new()),
            })
    }

    /// Tries the well-known system font paths for this platform, plus a
    /// monospace companion face when one exists.
    #[must_use]
    pub fn load_default() -> Option<Self> {
        let load = |paths: &[&str]| {
            paths
                .iter()
                .filter_map(|path| std::fs::read(path).ok())
                .find_map(|data| {
                    fontdue::Font::from_bytes(data.as_slice(), fontdue::FontSettings::default())
                        .ok()
                })
        };
        let font = load(&CANDIDATE_PATHS)?;
        Some(Self {
            font,
            mono: load(&MONO_CANDIDATE_PATHS),
            // Fallback faces parse per-slot on the first uncovered glyph.
            fallbacks: FALLBACK_CANDIDATE_PATHS
                .iter()
                .map(|_| std::sync::OnceLock::new())
                .collect(),
            glyph_cache: Mutex::new(HashMap::new()),
        })
    }

    /// The face for a measurement/draw request.
    fn face(&self, monospace: bool) -> &fontdue::Font {
        if monospace {
            self.mono.as_ref().unwrap_or(&self.font)
        } else {
            &self.font
        }
    }

    /// The face that actually has a glyph for `character`: the requested
    /// face, else the other face, else the first covering fallback —
    /// each fallback face is parsed only when reached.
    fn face_for(&self, character: char, monospace: bool) -> &fontdue::Font {
        let preferred = self.face(monospace);
        let has = |font: &fontdue::Font| font.lookup_glyph_index(character) != 0;
        if has(preferred) {
            return preferred;
        }
        let other = self.face(!monospace);
        if has(other) {
            return other;
        }
        for (path, slot) in FALLBACK_CANDIDATE_PATHS.iter().zip(&self.fallbacks) {
            let font = slot.get_or_init(|| {
                std::fs::read(path).ok().and_then(|data| {
                    fontdue::Font::from_bytes(data.as_slice(), fontdue::FontSettings::default())
                        .ok()
                })
            });
            if let Some(font) = font.as_ref()
                && has(font)
            {
                return font;
            }
        }
        preferred
    }

    /// Rasterizes one character at `font_size` (cached), returning metrics
    /// and an 8-bit coverage bitmap (row-major, `metrics.width` per row).
    #[must_use]
    pub fn rasterize(&self, character: char, font_size: f32, monospace: bool) -> Arc<Glyph> {
        let rasterize = || {
            let (metrics, coverage) = self
                .face_for(character, monospace)
                .rasterize(character, font_size);
            Arc::new(Glyph { metrics, coverage })
        };
        match self.glyph_cache.lock() {
            Ok(mut cache) => cache
                .entry((character, font_size.to_bits(), monospace))
                .or_insert_with(rasterize)
                .clone(),
            // A poisoned cache just means uncached rasterization.
            Err(_) => rasterize(),
        }
    }

    /// The ascent (baseline distance from the top of the line) at
    /// `font_size`, approximated as `0.8 × font_size` if unavailable.
    #[must_use]
    pub fn ascent(&self, font_size: f32) -> f32 {
        self.font
            .horizontal_line_metrics(font_size)
            .map_or(font_size * 0.8, |metrics| metrics.ascent)
    }
}

impl TextMeasurer for SystemFont {
    fn measure(&self, text: &str, style: &TextStyle) -> TextMetrics {
        let width = text
            .chars()
            .map(|character| {
                self.face_for(character, style.monospace)
                    .metrics(character, style.font_size)
                    .advance_width
                    + style.letter_spacing
            })
            .sum();
        TextMetrics { width }
    }

    /// Ascent, descent and line gap each round to whole pixels before
    /// they add up, so lines always land on the pixel grid.
    fn normal_line_height(&self, style: &TextStyle) -> Option<f32> {
        let metrics = self
            .face(style.monospace)
            .horizontal_line_metrics(style.font_size)?;
        Some(metrics.ascent.round() + (-metrics.descent).round() + metrics.line_gap.round())
    }
}

/// Rebuilds a TTF from a WOFF1 container: the sfnt header plus each
/// table, zlib-inflating the ones stored compressed.
fn woff1_to_ttf(data: &[u8]) -> Option<Vec<u8>> {
    use std::io::Read as _;

    /// A decompressed table claims its size in the (untrusted) directory;
    /// refuse absurd claims instead of pre-allocating them.
    const MAX_TABLE_LENGTH: usize = 64 * 1024 * 1024; // 64 MiB

    let u32_at = |at: usize| -> Option<u32> {
        data.get(at..at + 4)
            .map(|bytes| u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
    };
    let u16_at = |at: usize| -> Option<u16> {
        data.get(at..at + 2)
            .map(|bytes| u16::from_be_bytes([bytes[0], bytes[1]]))
    };
    let flavor = u32_at(4)?;
    let table_count = u16_at(12)? as usize;
    if table_count == 0 || table_count > 64 {
        return None;
    }

    // sfnt header bookkeeping.
    let mut search_range: u16 = 1;
    let mut entry_selector: u16 = 0;
    while u32::from(search_range) * 2 <= table_count as u32 {
        search_range *= 2;
        entry_selector += 1;
    }
    let search_range = search_range * 16;
    let range_shift = table_count as u16 * 16 - search_range;

    let mut header = Vec::with_capacity(12 + table_count * 16);
    header.extend_from_slice(&flavor.to_be_bytes());
    header.extend_from_slice(&(table_count as u16).to_be_bytes());
    header.extend_from_slice(&search_range.to_be_bytes());
    header.extend_from_slice(&entry_selector.to_be_bytes());
    header.extend_from_slice(&range_shift.to_be_bytes());

    let mut body: Vec<u8> = Vec::new();
    let mut directory: Vec<u8> = Vec::new();
    let body_base = 12 + table_count * 16;
    for index in 0..table_count {
        let entry = 44 + index * 20;
        let tag = data.get(entry..entry + 4)?;
        let offset = u32_at(entry + 4)? as usize;
        let compressed_length = u32_at(entry + 8)? as usize;
        let original_length = u32_at(entry + 12)? as usize;
        if original_length > MAX_TABLE_LENGTH {
            return None;
        }
        let raw = data.get(offset..offset + compressed_length)?;
        let table = if compressed_length < original_length {
            let mut inflated = Vec::with_capacity(original_length);
            // Bound the inflate: a hostile stream may expand past the
            // declared length, so stop one byte beyond it (the length
            // check below then rejects the table).
            flate2::read::ZlibDecoder::new(raw)
                .take(original_length as u64 + 1)
                .read_to_end(&mut inflated)
                .ok()?;
            inflated
        } else {
            raw.to_vec()
        };
        if table.len() != original_length {
            return None;
        }
        let checksum = u32_at(entry + 16)?;
        directory.extend_from_slice(tag);
        directory.extend_from_slice(&checksum.to_be_bytes());
        directory.extend_from_slice(&((body_base + body.len()) as u32).to_be_bytes());
        directory.extend_from_slice(&(original_length as u32).to_be_bytes());
        body.extend_from_slice(&table);
        while !body.len().is_multiple_of(4) {
            body.push(0); // tables are 4-byte aligned
        }
    }
    header.extend_from_slice(&directory);
    header.extend_from_slice(&body);
    Some(header)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::style::FontWeight;

    #[test]
    fn garbage_bytes_are_not_a_font() {
        assert!(SystemFont::from_bytes(b"definitely not a font").is_none());
    }

    /// Runs only when the sample exists (developer machines); CI-safe.
    #[test]
    fn woff2_bytes_unpack_into_a_usable_font() {
        let Ok(data) = std::fs::read(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/lato.woff2"
        )) else {
            return;
        };
        let font = SystemFont::from_bytes(&data).expect("woff2 should unpack");
        let style = TextStyle {
            font_size: 16.0,
            font_weight: FontWeight(400),
            monospace: false,
            letter_spacing: 0.0,
        };
        assert!(font.measure("Merhaba", &style).width > 10.0);
    }

    /// Wraps a TTF into a (stored, uncompressed) WOFF1 container and
    /// checks the unpacker reproduces a parseable font.
    #[test]
    fn woff1_container_round_trips() {
        let Ok(data) = std::fs::read(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/lato.woff2"
        )) else {
            return;
        };
        let ttf = woff2_patched::decode::convert_woff2_to_ttf(&mut &data[..]).unwrap();
        let table_count = u16::from_be_bytes([ttf[4], ttf[5]]) as usize;
        let mut woff: Vec<u8> = Vec::new();
        woff.extend_from_slice(b"wOFF");
        woff.extend_from_slice(&ttf[0..4]); // flavor
        woff.extend_from_slice(&0u32.to_be_bytes()); // length (unused)
        woff.extend_from_slice(&(table_count as u16).to_be_bytes());
        woff.extend_from_slice(&0u16.to_be_bytes());
        woff.extend_from_slice(&[0; 28]); // totalSfntSize..privLength
        let body_offset = 44 + table_count * 20;
        let mut body: Vec<u8> = Vec::new();
        for index in 0..table_count {
            let entry = 12 + index * 16;
            let tag = &ttf[entry..entry + 4];
            let checksum = &ttf[entry + 4..entry + 8];
            let offset =
                u32::from_be_bytes(ttf[entry + 8..entry + 12].try_into().unwrap()) as usize;
            let length =
                u32::from_be_bytes(ttf[entry + 12..entry + 16].try_into().unwrap()) as usize;
            woff.extend_from_slice(tag);
            woff.extend_from_slice(&((body_offset + body.len()) as u32).to_be_bytes());
            woff.extend_from_slice(&(length as u32).to_be_bytes()); // compLength
            woff.extend_from_slice(&(length as u32).to_be_bytes()); // origLength
            woff.extend_from_slice(checksum);
            body.extend_from_slice(&ttf[offset..offset + length]);
            while !body.len().is_multiple_of(4) {
                body.push(0);
            }
        }
        woff.extend_from_slice(&body);
        assert!(SystemFont::from_bytes(&woff).is_some());
    }

    #[test]
    fn woff1_rejects_absurd_table_lengths() {
        // A hostile directory claims a 200 MiB table: the unpacker must
        // refuse instead of pre-allocating it.
        let mut woff: Vec<u8> = Vec::new();
        woff.extend_from_slice(b"wOFF");
        woff.extend_from_slice(&0u32.to_be_bytes()); // flavor
        woff.extend_from_slice(&0u32.to_be_bytes()); // length (unused)
        woff.extend_from_slice(&1u16.to_be_bytes()); // numTables
        woff.extend_from_slice(&0u16.to_be_bytes()); // reserved
        woff.extend_from_slice(&[0; 28]); // totalSfntSize..privLength
        woff.extend_from_slice(b"head");
        woff.extend_from_slice(&64u32.to_be_bytes()); // offset (bogus)
        woff.extend_from_slice(&4u32.to_be_bytes()); // compLength
        woff.extend_from_slice(&(200u32 * 1024 * 1024).to_be_bytes()); // origLength
        woff.extend_from_slice(&0u32.to_be_bytes()); // checksum
        assert!(woff1_to_ttf(&woff).is_none());
    }

    /// Runs only where a system font exists (macOS/Linux/Windows dev boxes
    /// and most CI images); silently passes elsewhere.
    #[test]
    fn proportional_metrics_when_a_system_font_is_available() {
        let Some(font) = SystemFont::load_default() else {
            return;
        };
        let style = TextStyle {
            font_size: 16.0,
            font_weight: FontWeight(400),
            monospace: false,
            letter_spacing: 0.0,
        };
        let narrow = font.measure("iiii", &style).width;
        let wide = font.measure("MMMM", &style).width;
        assert!(
            narrow < wide,
            "expected proportional widths: {narrow} vs {wide}"
        );
        assert!(font.ascent(16.0) > 8.0);
    }

    #[test]
    fn fallback_faces_load_lazily() {
        // macOS's Arial Unicode costs hundreds of MB to parse in
        // fontdue; no page should pay that unless it renders a glyph
        // the main faces lack — and then only up to the covering face.
        let Some(font) = SystemFont::load_default() else {
            return; // no system fonts on this machine
        };
        assert!(
            font.fallbacks.iter().all(|slot| slot.get().is_none()),
            "fallback faces must not load eagerly"
        );
        // Apple Symbols covers ★: only that slot should initialize.
        let _ = font.rasterize('★', 16.0, false);
        let loaded = font
            .fallbacks
            .iter()
            .filter(|slot| slot.get().is_some())
            .count();
        assert_eq!(loaded, 1, "only the first covering fallback loads");
    }
}

#[cfg(test)]
mod normal_line_height_tests {
    use super::*;

    #[test]
    fn normal_line_height_is_a_whole_pixel_sum_of_the_font_metrics() {
        let Some(font) = SystemFont::load_default() else {
            return; // No system font on this machine.
        };
        let style = |font_size| TextStyle {
            font_size,
            font_weight: crate::style::FontWeight(400),
            monospace: false,
            letter_spacing: 0.0,
        };
        let height = font.normal_line_height(&style(16.0)).expect("line metrics");
        assert_eq!(height, height.round());
        assert!((17.0..=20.0).contains(&height), "16px text: {height}");
    }
}
