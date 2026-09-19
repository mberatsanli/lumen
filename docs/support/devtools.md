# Devtools compatibility

✅ supported · ⚠️ partial (see note) · ❌ not supported.

Lumen has no in-browser DevTools panel yet. The current debugging surface is
split between CLI dumps, tests and the manual compatibility lab. This tracker
keeps that limitation explicit so browser behavior is not confused with
developer observability.

## Current inspection surfaces

| Capability | Lumen | Note |
|---|:-:|---|
| DOM tree dump | ⚠️ | CLI `dump-dom`; not live in the desktop shell |
| CSS parser dump | ⚠️ | CLI `parse-css`; no matched-rule UI |
| Computed style dump | ⚠️ | CLI `dump-style`; no cascade trace |
| Layout tree dump | ⚠️ | CLI `dump-layout`; no box overlay |
| Display-list dump | ⚠️ | CLI `dump-display-list` |
| Render snapshot | ⚠️ | CLI `render` emits SVG |
| Visual compatibility lab | ✅ | `docs/support/compat-lab/index.html` |
| Live element picker | ❌ | |
| Live style editing | ❌ | |
| Network request table | ❌ | |
| Console | ❌ | No JavaScript runtime |
| JavaScript debugger | ❌ | |
| Performance timeline | ❌ | |
| Accessibility tree inspector | ❌ | |

## First useful DevTools milestones

| Milestone | Why it matters | Status |
|---|---|:-:|
| Request log | Shows document, CSS, image and failed subresource loads | ❌ |
| DOM/layout inspector | Connects rendered pixels back to parsed nodes and layout boxes | ❌ |
| Matched CSS view | Explains cascade, specificity and ignored declarations | ❌ |
| Paint/display-list view | Debugs paint order and raster output | ❌ |
| Console/errors view | Needed only after JavaScript or richer diagnostics land | ❌ |

