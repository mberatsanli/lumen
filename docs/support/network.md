# Network and navigation compatibility

✅ supported · ⚠️ partial (see note) · ❌ not supported.

This tracker covers the browser behavior around URLs, resource fetching,
HTTP and navigation. Transport-library support does not count as browser
support unless Lumen applies the corresponding web-platform rules.

Normative references: [URL](https://url.spec.whatwg.org/),
[Fetch](https://fetch.spec.whatwg.org/) and
[HTML navigation](https://html.spec.whatwg.org/multipage/browsing-the-web.html).

## URLs

| Feature | Lumen | Note |
|---|:-:|---|
| Absolute `http:` / `https:` URLs | ✅ | Parsed by the `url` crate |
| `file:` URLs and local paths | ✅ | Local paths are converted to absolute `file:` URLs |
| Relative URL resolution | ✅ | Resolved against the final response URL |
| Dot segments and root-relative paths | ✅ | |
| Percent encoding and IDNA hosts | ⚠️ | Library-backed; no Lumen conformance suite yet |
| Query strings | ✅ | Preserved during loading and navigation |
| Fragment identifiers | ⚠️ | Preserved in URLs; no target scrolling |
| `<base href>` | ❌ | Document base always remains the response URL |
| `data:` / `blob:` / `about:` URLs | ❌ | Loader accepts only file and HTTP(S) |
| Origin calculation | ❌ | No origin or site model |

## Fetching and HTTP

| Feature | Lumen | Note |
|---|:-:|---|
| HTTP/HTTPS GET | ✅ | `ureq` transport with Rustls |
| Redirect following | ✅ | Followed by hand (up to 10 hops); every hop's `Set-Cookie` is captured, 301/302/303 downgrade to GET; final URL recorded |
| Response body bytes | ✅ | Fully buffered in memory |
| `Content-Type` capture | ⚠️ | Stored but not used to choose a parser |
| HTTP error pages | ❌ | Non-success responses surface as load errors |
| Content encoding (`gzip`, Brotli) | ✅ | ureq sends `Accept-Encoding: gzip, br` and decodes the body; the size cap applies after decompression |
| External stylesheets | ⚠️ | Matching `<link>` elements are fetched in document order; no media/type/CORS processing |
| Images, fonts and scripts | ❌ | Subresource fetching is not orchestrated |
| Request headers and user agent | ❌ | No browser-level header policy |
| MIME sniffing | ❌ | All loaded documents are treated as HTML |
| HTTP cache and validation | ❌ | Back/forward/refresh re-fetch |
| Cookies | ⚠️ | Session jar; redirect-hop `Set-Cookie` captured so POST-login flows keep their session |
| Authentication | ⚠️ | Form-based login works (POST + session cookie); no HTTP Basic/Digest |
| Proxy support | ❌ | |

## Navigation and security

| Feature | Lumen | Note |
|---|:-:|---|
| Link click navigation | ⚠️ | Primary-button hit testing; no modifier behavior |
| Back / forward / refresh | ⚠️ | Linear URL history; documents are not retained |
| Redirect URL in history | ✅ | Final response URL is stored |
| Address bar | ❌ | Command-line input only |
| Download navigation | ❌ | |
| Same-origin policy and CORS | ❌ | Required before exposing network APIs to JS |
| Referrer policy | ❌ | |
| Content Security Policy | ❌ | |
| Mixed-content blocking | ❌ | |
| TLS certificate UI/errors | ❌ | Transport failures are plain load errors |
| Sandboxed browsing contexts | ❌ | No iframe/browsing-context model |
