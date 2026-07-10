# Lumen

Lumen is an experimental browser engine written from scratch in Rust.

It is **educational**: the HTML parser, CSS parser, style system, layout
engine and paint pipeline are all implemented in this repository, on
purpose, instead of using production libraries — so the core algorithms
stay visible and understandable. It is not, and does not aim to be,
standards-compliant.

## Pipeline

```text
HTML source -> tokenizer -> DOM ─┐
                                 ├-> cascade -> computed styles
embedded CSS  -> CSS parser ─────┘        |
                                          v
                        block layout (box model, line boxes)
                                          |
                                          v
                            display list -> SVG renderer
```

## Implemented

- State-machine HTML tokenizer (WHATWG-style states, comments, doctype,
  character references, raw-text elements) with lenient error recovery
- Arena-based DOM with traversal and attribute helpers
- CSS parser with typed values (`px`, `%`, colors, keywords), selector
  lists, compound and descendant selectors, shorthand expansion
- Structural specificity and origin-aware cascade (UA < author < inline)
- Inheritance (color, font-size, font-weight, line-height, text-align)
- Typed computed styles — no string parsing during layout
- CSS box model (content/padding/border/margin) with exact geometry
- Vertical block layout: auto and explicit widths/heights, percentages
  against the containing block, borders taking real space
- Text layout: whitespace collapsing, greedy word wrap into line boxes,
  `text-align`, pluggable `TextMeasurer` (heuristic metrics by default)
- Display list (`FillRect`, `StrokeRect`, `DrawText`) with defined paint
  order and deterministic SVG output
- Display-list software rasterizer (pixel buffer + bitmap font)
- Resource loading (file/http/https) and a navigation session with history
- Desktop shell: native window (winit + softbuffer), address bar with
  navigation buttons, clickable links with :hover, scrolling, resize
- CLI that inspects every pipeline stage and accepts URLs
- Golden-file SVG tests and 120+ unit/integration tests

## Not implemented (yet or on purpose)

JavaScript, inline flow (inline elements still stack vertically), margin
collapsing, flexbox/grid, images, real font metrics (the desktop shell
draws a scaled 8×8 bitmap font on purpose), external stylesheets,
`@media` and other at-rules, `!important`, `em`/`rem` units.

## Quick start

```bash
cargo test --workspace
cargo run -p lumen-cli -- render examples/card.html output/card.svg
open output/card.svg

# or a native window:
cargo run -p lumen-desktop -- examples/card.html
cargo run -p lumen-desktop -- https://example.com
```

## CLI

```bash
cargo run -p lumen-cli -- parse-html <file>          # token stream
cargo run -p lumen-cli -- parse-css <file>           # stylesheet rules
cargo run -p lumen-cli -- dump-dom <file>            # DOM tree
cargo run -p lumen-cli -- dump-style <file>          # computed styles
cargo run -p lumen-cli -- dump-layout <file>         # box geometry
cargo run -p lumen-cli -- dump-display-list <file>   # paint commands
cargo run -p lumen-cli -- render <file> <out.svg>    # SVG output
```

## Workspace

```text
crates/lumen-html      HTML tokenizer, tree builder and DOM
crates/lumen-css       CSS parser, typed values and selector model
crates/lumen-engine    style system, layout, display list, SVG renderer
crates/lumen-platform  resource loading (file/http) and surfaces
crates/lumen-browser   sessions, navigation and history
apps/lumen-cli         developer CLI
apps/lumen-desktop     native window shell (winit + softbuffer)
```

Architecture details: [docs/architecture.md](docs/architecture.md), decision
records under [docs/adr/](docs/adr/).

Feature-by-feature compatibility trackers (Lumen vs Chrome/Firefox/Safari):
[compat lab](docs/support/compat-lab/index.html) ·
[HTML](docs/support/html.md) · [CSS](docs/support/css.md) ·
[JavaScript](docs/support/javascript.md) ·
[network/navigation](docs/support/network.md) ·
[request lifecycle](docs/support/request-lifecycle.md) ·
[encoding/MIME](docs/support/encoding.md) ·
[DOM/events](docs/support/dom-events.md) ·
[media/fonts](docs/support/media-fonts.md) ·
[accessibility](docs/support/accessibility.md) · [devtools](docs/support/devtools.md) ·
[web-platform tests](docs/support/wpt.md).

## Examples

Three demo pages live in `examples/`: `hello.html`, `card.html` and
`nested.html`. Their golden SVG renders are committed under
`crates/lumen-engine/tests/golden/` and verified in CI.

## Roadmap

See [ROADMAP.md](ROADMAP.md).

## Contributing

See [CONTRIBUTING.md](CONTRIBUTING.md). CI requires `cargo fmt --check`,
`cargo clippy -D warnings` and `cargo test` to pass.

## License

MIT
