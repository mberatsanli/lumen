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

## v0.2 — Resources and navigation

- [ ] ResourceLoader abstraction (file + HTTP)
- [ ] External stylesheets (`<link rel="stylesheet">`)
- [ ] Relative URL resolution
- [ ] Page/session model: load, refresh, back, forward

## v0.3 — Desktop shell

- [ ] Native window (winit)
- [ ] Software rasterizer for the display list
- [ ] Real font metrics behind TextMeasurer
- [ ] Scrolling, resize and relayout
- [ ] Address bar / file open

## v0.4 — Better rendering

- [ ] Inline flow and shared line boxes
- [ ] Margin collapsing
- [ ] Images (replaced boxes)
- [ ] Border radius
- [ ] Visual regression tests on pixels

## Future experiments

- [ ] Forms
- [ ] Small JavaScript interpreter
- [ ] DOM/layout inspector
