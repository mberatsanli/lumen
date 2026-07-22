# CSS parser

`crates/lumen-css` is split into three modules:

```text
value.rs      typed values: CssValue, Color, Unit
selector.rs   selector parsing (parcel_selectors) + Lumen's pseudo-class/-element impl
parser.rs     stylesheet/declaration parsing, shorthand expansion
```

The crate has no DOM dependency. Selector *matching* lives in
`lumen-engine`, which owns both the DOM and the style computation.

## Values

```rust
enum CssValue { Keyword(String), Length(f32, Unit), Color(Color), Number(f32), Auto }
enum Unit { Px, Percent }
struct Color { r: u8, g: u8, b: u8 }
```

- Lengths: `px`, `%` (parsed; layout only resolves `px` for now) and
  unitless zero (treated as `0px`).
- Colors: `#rgb`, `#rrggbb`, `rgb(r, g, b)` and a small named set
  (black, white, red, green, blue, yellow, orange, purple, gray/grey,
  silver). `transparent` stays a keyword.
- Bare numbers stay `Number` (font-weight, line-height).
- Anything unparseable makes the whole declaration get dropped.

`Color` displays as `#rrggbb`, so paint output is normalized.

## Selectors

Rules store parcel_selectors selectors directly
(`parcel_selectors::parser::Selector<'static, Selectors>`): `*`, `div`,
`.card`, `#header`, compounds, all combinators (`A B`, `>`, `+`, `~`),
attribute selectors (incl. the `i` case flag), structural pseudo-classes
(`:first-child`, the full `:nth-*` family incl. `of S`, `:empty`, `:root`),
`:is()`/`:where()`/`:not()` at full depth, `:has()`, selector lists.

`selector.rs` defines the `SelectorImpl` glue: owned strings, the
non-tree-structural pseudo-classes the engine can answer (`:hover`,
`:active`, `:focus`, `:focus-within`, `:link`, `:visited`, `:enabled`,
`:disabled`, `:checked`) and the pseudo-elements (`::before`, `::after`,
`::selection`).

Unsupported syntax fails selector parsing, and per CSS error handling the
**entire rule** is dropped when any selector in its list is invalid.

## Specificity

parcel's layered `u32` (`ids << 20 | classes << 10 | types`, each layer
clamped to 10 bits). One id outranks any number of classes; one class
outranks any number of type selectors. The universal selector adds nothing.
Ties are broken by rule source order (stored on each `Rule`).

## Declarations and shorthands

`parse_declarations` handles `;`-separated lists and is also used for
inline `style="..."` attributes. `margin` and `padding` shorthands are
expanded at parse time with the standard 1/2/3/4-value rules, so consumers
only ever see longhands (`margin-top`, ...). Other properties keep their
first value component; multi-value forms of other properties are not
supported yet.

Leniency: comments are stripped, extra semicolons tolerated, malformed
declarations skipped. The only hard error is `CssError::UnterminatedRule`
(a `{` without `}`).

## Known limitations

- No child/sibling combinators, pseudo-classes/elements, or attribute
  selectors.
- No `@media` or other at-rules (an at-rule's block will be misparsed as a
  rule; avoid them in fixtures for now).
- `%` lengths are not resolved by layout.
- No `!important`.
- No source spans/diagnostics yet.
