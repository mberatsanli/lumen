# Style system

Lives in `crates/lumen-engine/src/style.rs` (it needs both the DOM from
`lumen-html` and the rule model from `lumen-css`).

## Cascade

Origins, weakest to strongest:

1. user-agent defaults (`user_agent_stylesheet()`, parsed once via `OnceLock`),
2. author stylesheet (embedded `<style>` contents),
3. inline `style="..."` attributes.

A stronger origin always wins per property, regardless of specificity — an
author `* { font-size: 20px }` beats the UA's `h1 { font-size: 32px }`.
Within one origin, the winner per property is the declaration with the
highest `(Specificity, source_order)` pair; later rules win ties.

## Selector matching

`selector_matches` checks the subject compound against the element, then
walks `Document::ancestors` right-to-left for the remaining compounds
(descendant combinator). Compound matching requires tag, id, and all
classes simultaneously.

## Inheritance

Inherited properties: `color`, `font-size`, `font-weight`, `line-height`,
`text-align`. Inheritance happens on **raw declared values** before typed
conversion, so a numeric `line-height: 1.5` inherits as the number and
re-resolves against each element's own font size, as CSS specifies.

## Typed computed styles

After the cascade, raw values convert once into `ComputedStyle`:

```rust
struct ComputedStyle {
    display: Display,               // Block | Inline | None
    color: Color,
    background_color: Option<Color>, // None = transparent
    width, height: Dimension,        // Auto | Px | Percent
    margin, padding: EdgeSizes<Dimension>,
    border_width: EdgeSizes<f32>,
    border_color: Color,             // defaults to `color` (currentColor)
    font_size: f32,                  // px
    font_weight: FontWeight,         // numeric, bold=700 normal=400
    line_height: f32,                // resolved px
    text_align: TextAlign,
}
```

Layout and paint consume only these typed fields — no string parsing after
this point.

## Display defaults

Per-tag defaults live in code (`default_display`): the usual block set
(html, body, div, p, h1-h6, ul, ol, li, section, ...), `display: none` for
head/style/script/title/meta/link/base, and inline for everything else
(including unknown tags, as in HTML). The UA stylesheet only carries
typography and spacing defaults.

## Known limitations

- `Display::Inline` still flows like a block (inline layout is a later
  milestone).
- No `!important`, no `em`/`rem`, no `inherit`/`initial` keywords.
- Percent heights are treated as auto; percent margins/paddings resolve
  against the containing width like widths do.
- `font-size` percent/number forms are unsupported (px only).
