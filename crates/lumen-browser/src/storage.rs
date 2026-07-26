//! Persistent `localStorage`: one JSON file per origin under the
//! platform config directory (`<config>/lumen/storage/<origin>.json`).
//! Writes go through the whole map (the web Storage API is a flat
//! string map), so persistence is rewrite-the-file on change — cheap
//! at the 5 MiB cap. `sessionStorage` never touches this module; it
//! is the same map kept in memory only.

use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;

/// Most bytes one origin's serialized map may occupy on disk — a
/// runaway script filling storage is a disk-DoS vector. Oversized
/// maps stay in memory but are not persisted.
const MAX_STORAGE_BYTES: usize = 5 * 1024 * 1024;

/// Per-origin Web Storage maps, hydrated from disk on first use.
#[derive(Debug, Default)]
pub(crate) struct WebStorage {
    /// Storage root directory; `None` means memory-only (no disk).
    root: Option<PathBuf>,
    /// Origin -> live map, loaded from disk on first access.
    cache: HashMap<String, BTreeMap<String, String>>,
}

impl WebStorage {
    /// The production store: `<config dir>/lumen/storage`, falling
    /// back to memory-only when the platform has no config directory.
    pub(crate) fn for_config_dir() -> Self {
        Self {
            root: dirs::config_dir().map(|dir| dir.join("lumen").join("storage")),
            cache: HashMap::new(),
        }
    }

    /// Points the store at `root` (tests) and drops anything cached.
    pub(crate) fn set_root(&mut self, root: PathBuf) {
        self.root = Some(root);
        self.cache.clear();
    }

    fn file_for(&self, origin: &str) -> Option<PathBuf> {
        Some(self.root.as_ref()?.join(format!("{}.json", slug(origin))))
    }

    /// The live map for `origin`, hydrating from disk on first access.
    /// A missing or corrupt file starts empty — storage must never
    /// break page loads.
    pub(crate) fn load(&mut self, origin: &str) -> BTreeMap<String, String> {
        if let Some(map) = self.cache.get(origin) {
            return map.clone();
        }
        let map = self
            .file_for(origin)
            .and_then(|file| std::fs::read_to_string(file).ok())
            .and_then(|text| parse_map(&text).ok())
            .unwrap_or_default();
        self.cache.insert(origin.to_string(), map.clone());
        map
    }

    /// Replaces `origin`'s map and persists it (unless it exceeds the
    /// size cap — the map still updates in memory). I/O failures are
    /// swallowed: storage is best-effort, never fatal.
    pub(crate) fn save(&mut self, origin: &str, map: &BTreeMap<String, String>) {
        self.cache.insert(origin.to_string(), map.clone());
        let Some(file) = self.file_for(origin) else {
            return;
        };
        let text = serialize_map(map);
        if text.len() > MAX_STORAGE_BYTES {
            return;
        }
        if let Some(parent) = file.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let _ = std::fs::write(file, text);
    }
}

/// A filesystem-safe file stem for an origin string: alphanumerics,
/// `-` and `.` pass through, everything else becomes `_`
/// (`https://a.test:8443` -> `https___a.test_8443`).
fn slug(origin: &str) -> String {
    origin
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || matches!(character, '-' | '.') {
                character
            } else {
                '_'
            }
        })
        .collect()
}

/// The storage key scripts see for a page URL: the serialized origin
/// for http(s), `scheme://host` otherwise (`file://` pages share one
/// store, matching the opaque-origin treatment of cookies).
pub(crate) fn origin_key(url: &lumen_platform::Url) -> String {
    match url.scheme() {
        "http" | "https" => url.origin().ascii_serialization(),
        scheme => format!("{scheme}://{}", url.host_str().unwrap_or_default()),
    }
}

/// Serializes a flat string map as a JSON object.
fn serialize_map(map: &BTreeMap<String, String>) -> String {
    let mut output = String::from("{");
    for (index, (key, value)) in map.iter().enumerate() {
        if index > 0 {
            output.push(',');
        }
        write_json_string(&mut output, key);
        output.push(':');
        write_json_string(&mut output, value);
    }
    output.push('}');
    output
}

fn write_json_string(output: &mut String, value: &str) {
    output.push('"');
    for character in value.chars() {
        match character {
            '"' => output.push_str("\\\""),
            '\\' => output.push_str("\\\\"),
            '\n' => output.push_str("\\n"),
            '\r' => output.push_str("\\r"),
            '\t' => output.push_str("\\t"),
            character if (character as u32) < 0x20 => {
                output.push_str(&format!("\\u{:04x}", character as u32));
            }
            character => output.push(character),
        }
    }
    output.push('"');
}

/// Parses a flat JSON object of strings back into a map. Strict
/// enough to reject anything our serializer did not produce (a
/// hand-edited or corrupt file falls back to empty storage).
fn parse_map(text: &str) -> Result<BTreeMap<String, String>, ()> {
    let mut parser = Parser {
        bytes: text.as_bytes(),
        position: 0,
    };
    parser.skip_whitespace();
    parser.expect(b'{')?;
    let mut map = BTreeMap::new();
    parser.skip_whitespace();
    if parser.peek() == Some(b'}') {
        parser.position += 1;
    } else {
        loop {
            parser.skip_whitespace();
            let key = parser.string()?;
            parser.skip_whitespace();
            parser.expect(b':')?;
            parser.skip_whitespace();
            let value = parser.string()?;
            map.insert(key, value);
            parser.skip_whitespace();
            match parser.peek() {
                Some(b',') => parser.position += 1,
                Some(b'}') => {
                    parser.position += 1;
                    break;
                }
                _ => return Err(()),
            }
        }
    }
    parser.skip_whitespace();
    if parser.position != parser.bytes.len() {
        return Err(());
    }
    Ok(map)
}

struct Parser<'a> {
    bytes: &'a [u8],
    position: usize,
}

impl Parser<'_> {
    fn peek(&self) -> Option<u8> {
        self.bytes.get(self.position).copied()
    }

    fn skip_whitespace(&mut self) {
        while matches!(self.peek(), Some(b' ' | b'\n' | b'\r' | b'\t')) {
            self.position += 1;
        }
    }

    fn expect(&mut self, byte: u8) -> Result<(), ()> {
        if self.peek() == Some(byte) {
            self.position += 1;
            Ok(())
        } else {
            Err(())
        }
    }

    fn string(&mut self) -> Result<String, ()> {
        self.expect(b'"')?;
        let mut output = String::new();
        loop {
            match self.peek() {
                None => return Err(()),
                Some(b'"') => {
                    self.position += 1;
                    return Ok(output);
                }
                Some(b'\\') => {
                    self.position += 1;
                    match self.peek() {
                        Some(b'"') => output.push('"'),
                        Some(b'\\') => output.push('\\'),
                        Some(b'/') => output.push('/'),
                        Some(b'n') => output.push('\n'),
                        Some(b'r') => output.push('\r'),
                        Some(b't') => output.push('\t'),
                        Some(b'b') => output.push('\u{8}'),
                        Some(b'f') => output.push('\u{c}'),
                        Some(b'u') => {
                            let hex = self
                                .bytes
                                .get(self.position + 1..self.position + 5)
                                .ok_or(())?;
                            let code =
                                u32::from_str_radix(std::str::from_utf8(hex).map_err(|_| ())?, 16)
                                    .map_err(|_| ())?;
                            output.push(char::from_u32(code).ok_or(())?);
                            self.position += 4;
                        }
                        _ => return Err(()),
                    }
                    self.position += 1;
                }
                Some(_) => {
                    // Consume one UTF-8 scalar.
                    let rest = std::str::from_utf8(&self.bytes[self.position..]).map_err(|_| ())?;
                    let character = rest.chars().next().ok_or(())?;
                    output.push(character);
                    self.position += character.len_utf8();
                }
            }
        }
    }
}

/// Test hook: a unique temporary storage root per call, so parallel
/// tests never share files.
#[cfg(test)]
pub(crate) fn temp_root(tag: &str) -> PathBuf {
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let root = std::env::temp_dir().join(format!(
        "lumen-storage-test-{}-{}-{tag}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    let _ = std::fs::remove_dir_all(&root);
    root
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn serialize_round_trips_arbitrary_strings() {
        let mut map = BTreeMap::new();
        map.insert("plain".to_string(), "değer".to_string());
        map.insert("quo\"te".to_string(), "sla\\sh".to_string());
        map.insert("new\nline".to_string(), "tab\there".to_string());
        map.insert(String::new(), String::new());
        let text = serialize_map(&map);
        assert_eq!(parse_map(&text).unwrap(), map);
    }

    #[test]
    fn corrupt_files_are_rejected() {
        assert!(parse_map("not json").is_err());
        assert!(parse_map(r#"{"a": 1}"#).is_err());
        assert!(parse_map(r#"{"a": "b""#).is_err());
        assert!(parse_map(r#"{"a": "b"} trailing"#).is_err());
        assert_eq!(parse_map("{}").unwrap(), BTreeMap::new());
    }

    #[test]
    fn save_then_load_persists_across_instances() {
        let root = temp_root("persist");
        let origin = "https://a.test";
        let mut map = BTreeMap::new();
        map.insert("k".to_string(), "v".to_string());
        let mut first = WebStorage::for_config_dir();
        first.set_root(root.clone());
        first.save(origin, &map);
        drop(first);

        let mut second = WebStorage::for_config_dir();
        second.set_root(root.clone());
        assert_eq!(second.load(origin).get("k"), Some(&"v".to_string()));
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn origins_map_to_distinct_safe_files() {
        assert_eq!(slug("https://a.test"), "https___a.test");
        assert_eq!(slug("https://a.test:8443"), "https___a.test_8443");
        assert_eq!(slug("file://"), "file___");
    }

    #[test]
    fn oversized_maps_stay_in_memory_only() {
        let root = temp_root("cap");
        let origin = "https://big.test";
        let mut map = BTreeMap::new();
        map.insert("huge".to_string(), "x".repeat(MAX_STORAGE_BYTES));
        let mut storage = WebStorage::for_config_dir();
        storage.set_root(root.clone());
        storage.save(origin, &map);
        // In memory the value is there…
        assert_eq!(
            storage.load(origin).get("huge").map(String::len),
            Some(MAX_STORAGE_BYTES)
        );
        // …but nothing was written to disk.
        assert!(!root.join(format!("{}.json", slug(origin))).exists());
        let _ = std::fs::remove_dir_all(root);
    }
}
