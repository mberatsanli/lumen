# Roadmap

## v0.1 — Core pipeline (done)

- [x] State-machine HTML tokenizer
- [x] DOM tree builder with recovery
- [x] CSS parser with typed values
- [x] Compound/descendant selectors, structural specificity
- [x] Origin-aware cascade and inheritance
- [x] Typed computed styles
- [x] CSS box model with borders
- [x] Vertical block layout
- [x] Text wrapping and line boxes
- [x] Display list (fill, stroke, text)
- [x] SVG output with golden tests
- [x] CLI for every pipeline stage

## v0.1 — Resources, navigation, desktop shell (done)

- [x] ResourceLoader abstraction (file + HTTP/HTTPS with redirects)
- [x] Relative URL resolution
- [x] Session model: load, refresh, back, forward
- [x] Native window (winit) with software rasterizer
- [x] Scrolling, resize and relayout

## v0.2 — Better rendering and navigation

- [ ] External stylesheets (`<link rel="stylesheet">`)
- [ ] Real font metrics behind TextMeasurer
- [ ] Link hit testing and click navigation
- [ ] Address bar

## v0.3 — Better rendering

- [ ] Inline flow and shared line boxes
- [ ] Margin collapsing
- [ ] Images (replaced boxes)
- [ ] Border radius
- [ ] Visual regression tests on pixels

## Future experiments

- [ ] Forms
- [ ] Small JavaScript interpreter
- [ ] DOM/layout inspector

## Parser migration (done)

Maintaining hand-written HTML/CSS parsers costs more than it returns.
`lumen-html` now delegates parsing to [html5ever](https://crates.io/crates/html5ever)
and `lumen-css` to [lightningcss](https://crates.io/crates/lightningcss),
both behind the existing crate APIs, so the engine keeps working unchanged.

Encoding detection is handled in `lumen-platform::encoding` (BOM,
Content-Type charset, `<meta>` prescan, UTF-8 fallback) via
[encoding_rs](https://crates.io/crates/encoding_rs), so byte streams
reach html5ever already decoded.
