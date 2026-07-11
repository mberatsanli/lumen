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
the content rect. `width`/`height` refer to the content box by default;
`box-sizing: border-box` makes them name the border box (the content
shrinks by padding and border, floored at zero).

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

Consecutive inline-level children (text nodes and inline elements with no
block descendants) form runs laid out by `inline.rs` into shared line
boxes inside an `AnonymousBlock`. Words carry the computed style of their
text node, so `<strong>`, `<a>`, `<em>` runs mix on one line with correct
weight/color/underline/italic per fragment. Whitespace collapses across
node boundaries (`foo<span>bar</span>` joins, `foo <span>bar</span>`
keeps the space); `<br>` forces a hard break; a word wider than the line
overflows alone. Line height is the tallest fragment's line-height (with
the container's as the strut); the baseline approximates the tallest font
size. `text-align` shifts whole lines at layout time. Measurement stays
behind `TextMeasurer` (`text.rs`), heuristic by default, real font in the
desktop shell. Deliberate simplifications: inline elements contribute no
box edges (margins/paddings/borders/backgrounds ignored) and there is no
`vertical-align`.

Atomic inlines (`display: inline-block`) flow in line boxes like single
big words: laid out in isolation (auto width via crude shrink-to-fit —
the widest used extent, so auto-width block children inflate to the
available width), bottom-aligned on the baseline. Floats use a simplified
model: a floated box leaves the flow, hugs its side at the current flow
position, narrows subsequent inline lines in the *same* container while
they overlap it vertically, and stretches the container's auto height;
block siblings ignore floats except via `clear`, and text in nested
blocks does not wrap around outer floats.

Flex containers (`display: flex`) implement a wrapping subset:
`flex-direction: row | column`, `flex-wrap: wrap` (greedy line filling),
`justify-content` (start/center/end/space-between), `align-items` with
per-item `align-self`, `gap` (used on both axes) and `flex-grow`/
`flex-shrink` (shrink weighted by base size). Base sizes come from
`width`/`height` (auto = shrink-to-fit on rows, fill on columns);
free space is distributed per line and adjusted items are laid out again
at their target size. No `flex-basis`, `order`, `align-content`
distribution or `wrap-reverse`. Bare text children become anonymous flex
items.

Positioning (simplified): `relative` translates the finished box without
affecting flow; `absolute`/`fixed` leave the flow, size shrink-to-fit for
auto widths, and resolve `top/left/right` against a containing block
simplified to the parent's content box (`fixed` uses the viewport, where
`bottom` also works; absolute `bottom` is unsupported). `z-index` sorts
sibling paint and hit-test order (stable; no real stacking contexts).
Fixed boxes scroll with the page — the paint pipeline has no layers yet.

The flex algorithm lives in `flex.rs` and float tracking in `float.rs`;
`layout.rs` owns the block/inline flow.

`layout_document` is a pure function; relayout on viewport resize is simply
calling it again with the new size (verified by test).

## Deliberate limitations

- **Margin collapsing is partial** — adjacent in-flow block siblings
  collapse (max of positives + min of negatives) and a parent with no top
  border/padding collapses with its first block child's top margin.
  Bottom parent-child collapsing and empty blocks collapsing through
  themselves are not implemented.
- Percent heights are treated as `auto`.
- Inline element box edges (margin/padding/border/background) are ignored;
  no `vertical-align`.
- Text measurement is heuristic, not shaped; real font metrics can slot in
  behind `TextMeasurer` without layout changes.

## Hover invalidation

Hover changes pick the cheapest reaction, decided once per page by
classifying the stylesheet's `:hover` rules (`hover_impact`): no hover
rules → nothing happens; paint-only rules (colors, decorations, opacity —
anything that cannot move geometry) → the existing layout tree keeps its
geometry and only swaps computed styles before the display list rebuilds
(`repaint_page_for_hover`); geometry-affecting rules → full restyle +
relayout, correctness first.

## Painting

`paint.rs` flattens the tree in paint order per box: background fill
(border box), border (`StrokeRect` with per-edge widths), then children.
The SVG backend draws borders as four inward edge strips, so output stays
plain rectangles and remains deterministic.
