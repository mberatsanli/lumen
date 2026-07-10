# Media, images and fonts compatibility

✅ supported · ⚠️ partial (see note) · ❌ not supported.

These features cross HTML loading, CSS sizing, layout and paint, so they are
tracked together rather than only as individual HTML elements or properties.

## Images and replaced content

| Feature | Lumen | Chrome | Firefox | Safari | Note |
|---|:-:|:-:|:-:|:-:|---|
| `<img>` DOM parsing | ⚠️ | ✅ | ✅ | ✅ | Void element is parsed but does not produce a replaced box |
| Image resource fetching | ❌ | ✅ | ✅ | ✅ | |
| PNG / JPEG / GIF decoding | ❌ | ✅ | ✅ | ✅ | |
| WebP / AVIF decoding | ❌ | ✅ | ✅ | ✅ | |
| Intrinsic dimensions and aspect ratio | ❌ | ✅ | ✅ | ✅ | |
| CSS width/height on replaced elements | ❌ | ✅ | ✅ | ✅ | |
| `alt` fallback rendering | ❌ | ✅ | ✅ | ✅ | |
| `srcset`, `sizes` and `<picture>` | ❌ | ✅ | ✅ | ✅ | |
| CSS background images | ❌ | ✅ | ✅ | ✅ | |
| Broken-image state | ❌ | ✅ | ✅ | ✅ | |
| Lazy loading | ❌ | ✅ | ✅ | ✅ | |

## SVG, canvas and media

| Feature | Lumen | Chrome | Firefox | Safari | Note |
|---|:-:|:-:|:-:|:-:|---|
| SVG serialization output | ✅ | — | — | — | Developer renderer, not web SVG support |
| Inline SVG content | ❌ | ✅ | ✅ | ✅ | No foreign-content parsing or SVG layout |
| SVG image documents | ❌ | ✅ | ✅ | ✅ | |
| `<canvas>` | ❌ | ✅ | ✅ | ✅ | |
| `<audio>` / `<video>` | ❌ | ✅ | ✅ | ✅ | Out of current scope |
| `<iframe>` embedded documents | ❌ | ✅ | ✅ | ✅ | No nested browsing contexts |

## Fonts and text shaping

| Feature | Lumen | Chrome | Firefox | Safari | Note |
|---|:-:|:-:|:-:|:-:|---|
| System-font metrics | ⚠️ | ✅ | ✅ | ✅ | Desktop shell loads one platform font when available |
| Bitmap fallback font | ✅ | — | — | — | Deterministic built-in 8x8 font |
| Font-family selection | ❌ | ✅ | ✅ | ✅ | CSS `font-family` is ignored |
| Generic family mapping | ❌ | ✅ | ✅ | ✅ | No serif/sans-serif/monospace selection |
| Font fallback per glyph | ❌ | ✅ | ✅ | ✅ | |
| Web fonts (`@font-face`) | ❌ | ✅ | ✅ | ✅ | |
| Font weight/style face selection | ❌ | ✅ | ✅ | ✅ | Weight is approximated during drawing |
| Kerning and ligatures | ❌ | ✅ | ✅ | ✅ | |
| Complex-script shaping | ❌ | ✅ | ✅ | ✅ | |
| Emoji and color fonts | ❌ | ✅ | ✅ | ✅ | |
| Bidirectional text | ❌ | ✅ | ✅ | ✅ | |

