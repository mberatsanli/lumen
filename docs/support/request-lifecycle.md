# Request lifecycle compatibility

✅ supported · ⚠️ partial (see note) · ❌ not supported.

This document tracks what happens after Lumen is asked to open a URL. It is
separate from the broad network table because request ordering and
observability matter for browser compatibility.

## Document load path

| Step | Lumen | Chrome | Firefox | Safari | Note |
|---|:-:|:-:|:-:|:-:|---|
| User input to URL | ✅ | ✅ | ✅ | ✅ | Absolute URLs and local paths are accepted |
| Scheme dispatch | ✅ | ✅ | ✅ | ✅ | `file`, `http` and `https` only |
| HTTP redirect follow | ✅ | ✅ | ✅ | ✅ | Final URL becomes the document base |
| Response metadata capture | ⚠️ | ✅ | ✅ | ✅ | `Content-Type` stored but not used for parser selection |
| Body buffering | ✅ | ✅ | ✅ | ✅ | Whole body is loaded before parsing |
| Text decoding | ⚠️ | ✅ | ✅ | ✅ | UTF-8 lossy decode only |
| HTML parse | ✅ | ✅ | ✅ | ✅ | Lumen parser, not a WHATWG-complete tree builder |
| Embedded stylesheet collection | ✅ | ✅ | ✅ | ✅ | `<style>` in document order |
| External stylesheet collection | ⚠️ | ✅ | ✅ | ✅ | `<link rel="stylesheet">` fetched in document order |
| Image collection | ⚠️ | ✅ | ✅ | ✅ | `<img src>`-style resources are the current focus; no full image selection model |
| Style, layout and paint | ⚠️ | ✅ | ✅ | ✅ | Lumen subset of CSS and layout |
| History entry update | ⚠️ | ✅ | ✅ | ✅ | Linear history; documents are re-fetched |

## Subresource behavior

| Resource type | Lumen | Browser behavior gap |
|---|:-:|---|
| External CSS | ⚠️ | No media/type/CORS processing, preload scanner or CSSOM |
| Images | ⚠️ | No `srcset`, lazy loading, decoding policy or full replaced-element rules |
| Scripts | ❌ | `<script src>` is not fetched or executed |
| Fonts | ❌ | `@font-face` and font fetch are not implemented |
| CSS `@import` | ❌ | At-rules are skipped by the CSS parser |
| Fetch/XHR | ❌ | No JavaScript runtime or web-exposed network APIs |
| Forms | ❌ | No form submission algorithm |
| Iframes | ❌ | No nested browsing contexts |

## Observability gap

Without DevTools, request behavior is currently inferred from tests, CLI output
and manual pages such as `compat-lab`. A request log should eventually record:
URL, initiator, method, status, final URL, content type, byte count, timing,
error reason and whether the resource affected render output.

