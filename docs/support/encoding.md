# Encoding and MIME compatibility

✅ supported · ⚠️ partial (see note) · ❌ not supported.

Decoding happens before HTML tokenization, so parser compatibility depends on
this layer. The reference algorithms live in the
[Encoding Standard](https://encoding.spec.whatwg.org/) and HTML's
[character encoding declaration](https://html.spec.whatwg.org/multipage/semantics.html#charset).

## Document decoding

| Feature | Lumen | Chrome | Firefox | Safari | Note |
|---|:-:|:-:|:-:|:-:|---|
| UTF-8 input | ✅ | ✅ | ✅ | ✅ | Valid UTF-8 is decoded correctly |
| Invalid UTF-8 recovery | ⚠️ | ✅ | ✅ | ✅ | Uses replacement characters via lossy Rust decoding |
| UTF-8 BOM | ⚠️ | ✅ | ✅ | ✅ | Decoded as text rather than used as an encoding signature |
| HTTP `Content-Type` charset | ❌ | ✅ | ✅ | ✅ | Header is captured but charset is ignored |
| `<meta charset>` | ❌ | ✅ | ✅ | ✅ | Parsed as an attribute; does not affect decoding |
| Legacy encoding labels | ❌ | ✅ | ✅ | ✅ | Windows-1252, Shift_JIS and others are unsupported |
| Encoding restart after `<meta>` | ❌ | ✅ | ✅ | ✅ | Input is decoded completely before parsing |
| Replacement decoding on errors | ⚠️ | ✅ | ✅ | ✅ | Similar outcome for malformed UTF-8, not the full algorithm |

## Unicode text

| Feature | Lumen | Chrome | Firefox | Safari | Note |
|---|:-:|:-:|:-:|:-:|---|
| Unicode scalar values in DOM text | ✅ | ✅ | ✅ | ✅ | Rust `String` storage |
| Numeric character references | ✅ | ✅ | ✅ | ✅ | Decimal and hexadecimal |
| Named character references | ⚠️ | ✅ | ✅ | ✅ | Small built-in subset, not the complete HTML table |
| Null and control-code handling | ⚠️ | ✅ | ✅ | ✅ | Tokenizer recovery is incomplete |
| Grapheme-cluster measurement | ❌ | ✅ | ✅ | ✅ | Text is not shaped as grapheme clusters |
| Bidirectional text | ❌ | ✅ | ✅ | ✅ | No Unicode bidi layout |
| Unicode line breaking | ❌ | ✅ | ✅ | ✅ | Wrapping is whitespace/word based |
| Language-sensitive shaping | ❌ | ✅ | ✅ | ✅ | No shaping engine |

## MIME handling

| Feature | Lumen | Chrome | Firefox | Safari | Note |
|---|:-:|:-:|:-:|:-:|---|
| Read HTTP `Content-Type` | ✅ | ✅ | ✅ | ✅ | Stored as an opaque string |
| File-extension MIME guess | ⚠️ | ✅ | ✅ | ✅ | HTML, CSS, SVG and plain text only |
| Select parser by MIME type | ❌ | ✅ | ✅ | ✅ | Every top-level response is parsed as HTML |
| MIME parameter parsing | ❌ | ✅ | ✅ | ✅ | Includes `charset` parameters |
| MIME sniffing | ❌ | ✅ | ✅ | ✅ | |
| `nosniff` enforcement | ❌ | ✅ | ✅ | ✅ | |
| Unsupported-document fallback | ❌ | ✅ | ✅ | ✅ | No image/text/download document viewers |

