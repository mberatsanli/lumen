# Lumen

Lumen is an experimental browser engine written from scratch in Rust.

It implements a small but complete rendering pipeline:

```text
HTML source -> tokenizer -> DOM -> CSS parser -> cascade -> block layout
            -> display list -> SVG renderer
```

The project is educational. It intentionally avoids production HTML/CSS parser and layout
libraries so the core algorithms remain visible and understandable.

## Current features

- Arena-based DOM representation
- HTML start tags, end tags, text nodes and attributes
- CSS tag, class and ID selectors
- Selector specificity and basic cascade
- Basic inherited text properties
- Vertical block layout
- Width, height, margin, padding and background colors
- Text display commands
- SVG output for visual inspection
- CLI commands for parsing, layout dumps and rendering
- Unit and integration tests

## Quick start

```bash
cargo test --workspace
cargo run -p lumen-cli -- render examples/card.html /tmp/lumen-card.svg
```

Then open `/tmp/lumen-card.svg` in a browser.

Other commands:

```bash
cargo run -p lumen-cli -- parse-html examples/card.html
cargo run -p lumen-cli -- parse-css fixtures/css/card.css
cargo run -p lumen-cli -- dump-layout examples/card.html
```

## Workspace

```text
crates/lumen-html      HTML tokenizer, parser and DOM
crates/lumen-css       CSS parser and selector model
crates/lumen-engine    Styling, layout, display list and SVG rendering
crates/lumen-platform  Platform abstraction placeholder
apps/lumen-cli         Developer CLI
apps/lumen-desktop     Desktop application placeholder
```

## Roadmap

See [ROADMAP.md](ROADMAP.md).

## Known limitations

- The HTML parser is not WHATWG compliant.
- Only simple selectors are supported.
- Layout is vertical block flow only.
- Text measurement is approximate.
- JavaScript, images, networking, flexbox and grid are intentionally not included yet.

## License

MIT
