//! Encoding detection for text resources, following the HTML
//! standard's "determine the encoding" steps (simplified):
//!
//! 1. A byte-order mark (UTF-8, UTF-16LE/BE).
//! 2. A `charset` parameter on the Content-Type.
//! 3. A `<meta charset>` (or `http-equiv`) declaration prescanned
//!    from the first 1024 bytes.
//! 4. Fallback: UTF-8, lossily (deliberate deviation — the spec
//!    defaults text/html to windows-1252, but Lumen's local files and
//!    fixtures are UTF-8).

use encoding_rs::Encoding;

/// How many leading bytes the `<meta>` prescan looks at.
const PRESCAN_LEN: usize = 1024;

/// Decodes `body` to a `String`, sniffing the encoding from the BOM,
/// the Content-Type header and `<meta>` declarations, in that order.
#[must_use]
pub fn decode_text(body: &[u8], content_type: Option<&str>) -> String {
    if let Some((encoding, bom_len)) = Encoding::for_bom(body) {
        return decode_with(encoding, &body[bom_len..]);
    }
    if let Some(label) = content_type.and_then(charset_parameter)
        && let Some(encoding) = Encoding::for_label(label.as_bytes())
    {
        return decode_with(encoding, body);
    }
    if let Some(encoding) = prescan_meta(body) {
        return decode_with(encoding, body);
    }
    String::from_utf8_lossy(body).into_owned()
}

fn decode_with(encoding: &'static Encoding, body: &[u8]) -> String {
    // UTF-16 outputs go through the same replacement rules as UTF-8.
    encoding.decode(body).0.into_owned()
}

/// The `charset=...` parameter of a Content-Type value, if present.
fn charset_parameter(content_type: &str) -> Option<&str> {
    for parameter in content_type.split(';').skip(1) {
        let parameter = parameter.trim();
        let Some((name, value)) = parameter.split_once('=') else {
            continue;
        };
        if name.trim().eq_ignore_ascii_case("charset") {
            return Some(value.trim().trim_matches(['"', '\'']));
        }
    }
    None
}

/// Scans the first [`PRESCAN_LEN`] bytes for a `<meta>` charset
/// declaration. Simplified version of the HTML standard's prescan:
/// finds `<meta` (case-insensitive), then a `charset` attribute
/// (`charset=utf-8`, `charset="utf-8"`) or `content="...charset=..."`
/// within the same tag.
fn prescan_meta(body: &[u8]) -> Option<&'static Encoding> {
    let head = &body[..body.len().min(PRESCAN_LEN)];
    let haystack = to_ascii_lower(head);
    let mut search_from = 0;
    while let Some(found) = haystack[search_from..]
        .windows(5)
        .position(|window| window == b"<meta")
    {
        let tag_start = search_from + found;
        let tag_end = haystack[tag_start..]
            .iter()
            .position(|&byte| byte == b'>')
            .map(|position| tag_start + position)
            .unwrap_or(head.len());
        let tag = &haystack[tag_start..tag_end];
        if let Some(label) = charset_in_tag(tag)
            && let Some(encoding) = Encoding::for_label(&label)
        {
            // The standard maps x-user-defined to windows-1252 and
            // ignores utf-16 from meta (already handled by BOM);
            // encoding_rs labels cover the rest.
            return Some(encoding);
        }
        search_from = tag_end;
    }
    None
}

/// Extracts the charset label out of a (lowercased) `<meta ...>` tag.
fn charset_in_tag(tag: &[u8]) -> Option<Vec<u8>> {
    let text = std::str::from_utf8(tag).ok()?;
    // `charset=utf-8` attribute, or `content="text/html; charset=..."`.
    let mut rest = text;
    while let Some(position) = rest.find("charset") {
        rest = &rest[position + "charset".len()..];
        let rest_trimmed = rest.trim_start();
        let Some(after_eq) = rest_trimmed.strip_prefix('=') else {
            continue;
        };
        let after_eq = after_eq.trim_start();
        let label: String = match after_eq.chars().next() {
            Some(quote @ ('"' | '\'')) => after_eq[1..]
                .split(quote)
                .next()
                .unwrap_or("")
                .trim()
                .to_string(),
            _ => after_eq
                .split(|c: char| {
                    c.is_ascii_whitespace() || matches!(c, ';' | '/' | '>' | '"' | '\'')
                })
                .next()
                .unwrap_or("")
                .trim()
                .to_string(),
        };
        if !label.is_empty() {
            return Some(label.into_bytes());
        }
    }
    None
}

/// Lowercases ASCII in place-free fashion, tolerating arbitrary bytes.
fn to_ascii_lower(bytes: &[u8]) -> Vec<u8> {
    bytes.iter().map(u8::to_ascii_lowercase).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn utf8_bom_is_sniffed_and_stripped() {
        let body = b"\xEF\xBB\xBF<p>hi</p>";
        assert_eq!(decode_text(body, None), "<p>hi</p>");
    }

    #[test]
    fn utf16_boms_are_decoded() {
        let mut le = vec![0xFF, 0xFE];
        le.extend("<p>ğ</p>".encode_utf16().flat_map(u16::to_le_bytes));
        assert_eq!(decode_text(&le, None), "<p>ğ</p>");

        let mut be = vec![0xFE, 0xFF];
        be.extend("<p>ğ</p>".encode_utf16().flat_map(u16::to_be_bytes));
        assert_eq!(decode_text(&be, None), "<p>ğ</p>");
    }

    #[test]
    fn content_type_charset_wins_over_meta() {
        // "Ğ" in windows-1254 is 0xD0; in UTF-8 lossy it would break.
        let body = b"<meta charset=\"utf-8\"><p>\xD0</p>";
        assert_eq!(
            decode_text(body, Some("text/html; charset=windows-1254")),
            "<meta charset=\"utf-8\"><p>Ğ</p>"
        );
    }

    #[test]
    fn meta_charset_is_prescanned() {
        let body = b"<html><head><meta charset=\"windows-1254\"></head><body>\xFD</body>";
        assert_eq!(
            decode_text(body, Some("text/html")),
            "<html><head><meta charset=\"windows-1254\"></head><body>ı</body>"
        );
    }

    #[test]
    fn meta_http_equiv_content_attribute_is_prescanned() {
        let body = b"<meta http-equiv=\"Content-Type\" content=\"text/html; charset=iso-8859-9\"><p>\xFD";
        assert_eq!(
            decode_text(body, None),
            "<meta http-equiv=\"Content-Type\" content=\"text/html; charset=iso-8859-9\"><p>ı"
        );
    }

    #[test]
    fn meta_beyond_the_prescan_window_is_ignored() {
        let mut body = vec![b' '; 2048];
        body.extend_from_slice(b"<meta charset=\"windows-1254\">\xFD");
        let decoded = decode_text(&body, None);
        // 0xFD is invalid UTF-8 -> replacement character, not "ı".
        assert!(decoded.ends_with('\u{FFFD}'));
    }

    #[test]
    fn unquoted_and_latin_labels_work() {
        // 0xF0 is "ğ" in ISO-8859-9/windows-1254.
        let body = b"<meta charset=iso-8859-9><p>\xF0";
        let decoded = decode_text(body, None);
        assert!(decoded.ends_with('ğ'));
    }

    #[test]
    fn plain_utf8_without_hints_decodes_as_utf8() {
        let body = "<p>şçöğüı</p>".as_bytes();
        assert_eq!(decode_text(body, None), "<p>şçöğüı</p>");
        assert_eq!(decode_text(body, Some("text/html")), "<p>şçöğüı</p>");
    }

    #[test]
    fn unknown_charset_labels_fall_back_to_utf8() {
        let body = b"<meta charset=\"klingon\">hi";
        assert_eq!(decode_text(body, Some("text/html; charset=klingon")), "<meta charset=\"klingon\">hi");
    }
}
