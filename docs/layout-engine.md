# Layout engine

Lives in `crates/lumen-engine/src/layout.rs`, with geometry primitives in
`geometry.rs`. Input: DOM + `StyleMap` + viewport `Size`. Output: a
`LayoutBox` tree, separate from the DOM (linked back via `node_id`).

## Box model

```rust
struct Dimensions {
    content: Rect,        // page coordinates
    padding: Edges,
    border: Edges,
    margin: Edges,
}
```

Helpers `padding_box()`, `border_box()`, `margin_box()` expand outward from
the content rect. `width`/`height` in CSS refer to the **content box**
(`box-sizing: content-box` semantics only).

`LayoutBox` carries `box_type` (`Block`, `Inline`, `AnonymousBlock`,
`Replaced` — the last two are not generated yet), a `kind` (element tag or
text run), its `Dimensions`, the `ComputedStyle`, and children.

## Block layout algorithm

Per element, in order:

1. **Width** — explicit `width` sets the content width; `auto` fills the
   containing block minus margins, borders and paddings; `%` resolves
   against the containing block width.
2. **Horizontal position** — containing block left edge plus left margin,
   border and padding.
3. **Vertical position** — the running cursor plus top margin.
4. **Children** — laid out top-to-bottom inside this content box; the
   cursor advances by each child's margin-box height.
5. **Height** — explicit `height` wins; `auto` grows from the children.

Text nodes become `Inline` boxes occupying one full-width line of
`line_height` pixels — real text measurement and wrapping are milestone 11.

`layout_document` is a pure function; relayout on viewport resize is simply
calling it again with the new size (verified by test).

## Deliberate limitations

- **No margin collapsing** — adjacent vertical margins add up instead of
  collapsing. Documented deviation; revisit after inline layout.
- Percent heights are treated as `auto`; auto margins resolve to 0 (no
  `margin: 0 auto` centering yet).
- `Display::Inline` boxes still stack vertically like blocks.
- `default_min_height` gives empty non-container elements an 8px floor — a
  legacy hack to keep empty paragraphs visible until real text metrics land.

## Painting

`paint.rs` flattens the tree in paint order per box: background fill
(border box), border (`StrokeRect` with per-edge widths), then children.
The SVG backend draws borders as four inward edge strips, so output stays
plain rectangles and remains deterministic.
