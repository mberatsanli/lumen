//! Real font metrics and glyph rasterization behind [`TextMeasurer`].
//!
//! Faces come from `fontdb` (which indexes the installed fonts and answers
//! `font-family` queries) and are rasterized by `fontdue` (a pure-Rust
//! TTF/OTF rasterizer — font parsing is not browser-engine core, like TLS
//! it is deliberately a library). The engine ships no font asset:
//! [`SystemFont::load_default`] indexes what the platform has, and
//! [`SystemFont::from_bytes`] wraps a single downloaded face. Everything
//! keeps working without either: layout falls back to
//! [`crate::HeuristicMeasurer`] and the rasterizer to the built-in bitmap
//! font.

use crate::text::{FaceKey, TextMeasurer, TextMetrics, TextStyle, families};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

/// The installed faces, plus the caches that keep repeated lookups and
/// glyph rasterization cheap.
///
/// Rasterized glyphs are cached per (character, size, face), which makes
/// repeated frames (scrolling, resizing) cheap. The caches sit behind a
/// `Mutex`, so the font is `Sync` and can be shared via `Arc` — including
/// with background loader threads.
pub struct SystemFont {
    /// Every installed face, queried by family name. Empty when this font
    /// wraps a single face loaded from bytes.
    database: fontdb::Database,
    /// The face for text whose family list matches nothing installed —
    /// and the only face when the font came from bytes.
    fallback_face: Arc<fontdue::Font>,
    /// Parsed faces per resolved query; a face parses once.
    faces: Mutex<HashMap<FaceKey, Arc<fontdue::Font>>>,
    /// Wide-coverage faces consulted when the chosen face lacks a glyph
    /// (symbols, exotic scripts) — otherwise text shows notdef boxes.
    /// One lazily-parsed slot per candidate path, tried in order, so a
    /// single uncovered glyph only pays for the faces up to the one
    /// that covers it (parsing all of macOS's fallback faces up front
    /// costs hundreds of MB in fontdue).
    fallbacks: Vec<std::sync::OnceLock<Option<fontdue::Font>>>,
    glyph_cache: Mutex<HashMap<(char, u32, FaceKey), Arc<Glyph>>>,
}

/// A rasterized glyph: metrics plus an 8-bit coverage bitmap.
pub struct Glyph {
    pub metrics: fontdue::Metrics,
    pub coverage: Vec<u8>,
}

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
    /// to TTF first. The result answers every family query with this one
    /// face. Returns `None` when the data is not a usable font.
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
        let face = fontdue::Font::from_bytes(data, fontdue::FontSettings::default()).ok()?;
        Some(Self::with_fallback_face(
            fontdb::Database::new(),
            face,
            Vec::new(),
        ))
    }

    fn with_fallback_face(
        database: fontdb::Database,
        fallback_face: fontdue::Font,
        fallbacks: Vec<std::sync::OnceLock<Option<fontdue::Font>>>,
    ) -> Self {
        Self {
            database,
            fallback_face: Arc::new(fallback_face),
            faces: Mutex::new(HashMap::new()),
            fallbacks,
            glyph_cache: Mutex::new(HashMap::new()),
        }
    }

    /// Indexes the platform's installed fonts, so `font-family` resolves
    /// the way the page asks. Returns `None` when no usable face exists at
    /// all — the caller then measures heuristically.
    #[must_use]
    pub fn load_default() -> Option<Self> {
        let mut database = fontdb::Database::new();
        database.load_system_fonts();
        let from_paths = |paths: &[&str]| {
            paths
                .iter()
                .filter_map(|path| std::fs::read(path).ok())
                .find_map(|data| {
                    fontdue::Font::from_bytes(data.as_slice(), fontdue::FontSettings::default())
                        .ok()
                })
        };
        let fallback_face = parse_query(
            &database,
            &FaceKey {
                families: families::initial(),
                weight: 400,
                italic: false,
            },
        )
        .map(|face| Arc::try_unwrap(face).unwrap_or_else(|shared| (*shared).clone()))
        .or_else(|| from_paths(&CANDIDATE_PATHS))?;
        // Fallback faces parse per-slot on the first uncovered glyph.
        let fallbacks = FALLBACK_CANDIDATE_PATHS
            .iter()
            .map(|_| std::sync::OnceLock::new())
            .collect();
        Some(Self::with_fallback_face(database, fallback_face, fallbacks))
    }

    /// The face `key` asks for, parsed once and then cached.
    fn face(&self, key: &FaceKey) -> Arc<fontdue::Font> {
        if let Ok(cache) = self.faces.lock()
            && let Some(face) = cache.get(key)
        {
            return face.clone();
        }
        let face = parse_query(&self.database, key).unwrap_or_else(|| self.fallback_face.clone());
        if let Ok(mut cache) = self.faces.lock() {
            cache.insert(key.clone(), face.clone());
        }
        face
    }

    /// The face that actually has a glyph for `character`: the requested
    /// face, else the first covering fallback — each fallback face is
    /// parsed only when reached.
    fn face_for(&self, character: char, key: &FaceKey) -> Arc<fontdue::Font> {
        let preferred = self.face(key);
        let has = |font: &fontdue::Font| font.lookup_glyph_index(character) != 0;
        if has(&preferred) {
            return preferred;
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
                // Parsed once and owned by the slot; cloning a fontdue
                // face copies its tables, so keep it behind its own Arc.
                return Arc::new(font.clone());
            }
        }
        preferred
    }

    /// Rasterizes one character of `face` at `font_size` (cached),
    /// returning metrics and an 8-bit coverage bitmap (row-major,
    /// `metrics.width` per row).
    #[must_use]
    pub fn rasterize(&self, character: char, font_size: f32, face: &FaceKey) -> Arc<Glyph> {
        let rasterize = || {
            let (metrics, coverage) = self
                .face_for(character, face)
                .rasterize(character, font_size);
            Arc::new(Glyph { metrics, coverage })
        };
        let key = (character, font_size.to_bits(), face.clone());
        match self.glyph_cache.lock() {
            Ok(mut cache) => cache.entry(key).or_insert_with(rasterize).clone(),
            // A poisoned cache just means uncached rasterization.
            Err(_) => rasterize(),
        }
    }

    /// Whether italics for `face` have to be faked by slanting the
    /// upright glyphs: true when the family has no italic face installed.
    #[must_use]
    pub fn synthesizes_italic(&self, face: &FaceKey) -> bool {
        let italic = FaceKey {
            italic: true,
            ..face.clone()
        };
        let Some(id) = query_id(&self.database, &italic) else {
            return true;
        };
        self.database
            .face(id)
            .is_none_or(|info| info.style == fontdb::Style::Normal)
    }

    /// The ascent (baseline distance from the top of the line) of `face`
    /// at `font_size`, approximated as `0.8 × font_size` if unavailable.
    #[must_use]
    pub fn ascent(&self, font_size: f32, face: &FaceKey) -> f32 {
        self.face(face)
            .horizontal_line_metrics(font_size)
            .map_or(font_size * 0.8, |metrics| metrics.ascent)
    }
}

impl TextMeasurer for SystemFont {
    fn measure(&self, text: &str, style: &TextStyle) -> TextMetrics {
        let face = style.face();
        let width = text
            .chars()
            .map(|character| {
                self.face_for(character, &face)
                    .metrics(character, style.font_size)
                    .advance_width
                    + style.letter_spacing
            })
            .sum();
        TextMetrics { width }
    }

    /// `normal` is the font's own content area: nothing is added, so
    /// text sits in its line with no leading.
    fn normal_line_height(&self, style: &TextStyle) -> Option<f32> {
        let (ascent, descent) = self.content_extent(style)?;
        Some(ascent + descent)
    }

    fn x_height(&self, style: &TextStyle) -> Option<f32> {
        let face = self.face_for('x', &style.face());
        Some(f32::from(face.metrics('x', style.font_size).height as u16))
    }

    /// The line gap belongs below the text, and both edges round to
    /// whole pixels, so every line lands on the pixel grid.
    fn content_extent(&self, style: &TextStyle) -> Option<(f32, f32)> {
        let metrics = self
            .face(&style.face())
            .horizontal_line_metrics(style.font_size)?;
        Some((
            metrics.ascent.round(),
            (-metrics.descent + metrics.line_gap).round(),
        ))
    }
}

/// The installed face `key` resolves to, if any.
fn query_id(database: &fontdb::Database, key: &FaceKey) -> Option<fontdb::ID> {
    let names: Vec<&str> = key.families.split(',').map(str::trim).collect();
    let families: Vec<fontdb::Family> = names
        .iter()
        .map(|name| match *name {
            families::SERIF => fontdb::Family::Serif,
            families::SANS_SERIF => fontdb::Family::SansSerif,
            families::MONOSPACE => fontdb::Family::Monospace,
            "cursive" => fontdb::Family::Cursive,
            "fantasy" => fontdb::Family::Fantasy,
            name => fontdb::Family::Name(name),
        })
        .collect();
    database.query(&fontdb::Query {
        families: &families,
        weight: fontdb::Weight(key.weight),
        stretch: fontdb::Stretch::Normal,
        style: if key.italic {
            fontdb::Style::Italic
        } else {
            fontdb::Style::Normal
        },
    })
}

/// Resolves `key` against `database` and parses the winning face.
fn parse_query(database: &fontdb::Database, key: &FaceKey) -> Option<Arc<fontdue::Font>> {
    let id = query_id(database, key)?;
    database.with_face_data(id, |data, face_index| {
        fontdue::Font::from_bytes(
            data,
            fontdue::FontSettings {
                collection_index: face_index,
                ..fontdue::FontSettings::default()
            },
        )
        .ok()
        .map(Arc::new)
    })?
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
            families: crate::text::families::sans_serif(),
            italic: false,
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
            families: crate::text::families::sans_serif(),
            italic: false,
            letter_spacing: 0.0,
        };
        let narrow = font.measure("iiii", &style).width;
        let wide = font.measure("MMMM", &style).width;
        assert!(
            narrow < wide,
            "expected proportional widths: {narrow} vs {wide}"
        );
        assert!(font.ascent(16.0, &style.face()) > 8.0);
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
        let _ = font.rasterize(
            '★',
            16.0,
            &FaceKey {
                families: families::initial(),
                weight: 400,
                italic: false,
            },
        );
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
            families: crate::text::families::sans_serif(),
            italic: false,
            letter_spacing: 0.0,
        };
        let height = font.normal_line_height(&style(16.0)).expect("line metrics");
        assert_eq!(height, height.round());
        assert!((17.0..=20.0).contains(&height), "16px text: {height}");
    }
}
