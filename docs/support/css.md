# CSS compatibility

✅ supported · ⚠️ partial (see note) · ❌ not supported.
Update the Lumen column whenever a feature lands. Unsupported CSS is always
ignored gracefully — ❌ never means "breaks the page".

Chrome/Firefox/Safari columns are current stable releases.

## Selectors

| Feature | Lumen | Chrome | Firefox | Safari | Note |
|---|:-:|:-:|:-:|:-:|---|
| Universal `*` | ✅ | ✅ | ✅ | ✅ | |
| Type `div` | ✅ | ✅ | ✅ | ✅ | |
| Class `.card` | ✅ | ✅ | ✅ | ✅ | |
| ID `#main` | ✅ | ✅ | ✅ | ✅ | |
| Compound `div.card#x` | ✅ | ✅ | ✅ | ✅ | |
| Descendant `A B` | ✅ | ✅ | ✅ | ✅ | |
| Selector list `h1, h2` | ✅ | ✅ | ✅ | ✅ | |
| Child `A > B` | ✅ | ✅ | ✅ | ✅ | |
| Siblings `A + B`, `A ~ B` | ✅ | ✅ | ✅ | ✅ | |
| Attribute `[href]`, `[type="x"]` | ✅ | ✅ | ✅ | ✅ | `=`, `^=`, `$=`, `*=`, `~=`, `|=`; no case flags |
| `:hover` | ✅ | ✅ | ✅ | ✅ | live hover chain in desktop shell |
| `:root` | ✅ | ✅ | ✅ | ✅ | |
| `:link` / `:visited` | ⚠️ | ✅ | ✅ | ✅ | always match; no visited state |
| `:active`, `:focus*` | ❌ | ✅ | ✅ | ✅ | |
| `:first/last/only-child` | ✅ | ✅ | ✅ | ✅ | of-type variants too |
| `:nth-child()` family | ⚠️ | ✅ | ✅ | ✅ | nth-child/nth-last-child + full of-type family with an+b/odd/even; no `of S` |
| `:not()`, `:is()`, `:where()` | ⚠️ | ✅ | ✅ | ✅ | compound arguments only (no combinators inside); :is takes max specificity, :where zero |
| `:has()` | ❌ | ✅ | ✅ | ✅ | |
| `::before` / `::after` + `content` | ⚠️ | ✅ | ✅ | ✅ | string content only (no counters/attr()/url()); flows as unselectable inline text; both colon forms |
| `::selection` | ⚠️ | ✅ | ✅ | ✅ | background-color works; color recorded but selected text keeps its color |
| `::first-line/-letter` | ❌ | ✅ | ✅ | ✅ | |

## Cascade & values

| Feature | Lumen | Chrome | Firefox | Safari | Note |
|---|:-:|:-:|:-:|:-:|---|
| Specificity | ✅ | ✅ | ✅ | ✅ | structural, lexicographic |
| Source-order tie-break | ✅ | ✅ | ✅ | ✅ | |
| Origins UA < author < inline | ✅ | ✅ | ✅ | ✅ | |
| Inheritance | ⚠️ | ✅ | ✅ | ✅ | 6 props; text-decoration approximated as inherited |
| `!important` | ✅ | ✅ | ✅ | ✅ | author important beats inline normal; inline important wins |
| `inherit`/`initial`/`unset`/`revert` | ⚠️ | ✅ | ✅ | ✅ | revert behaves as initial |
| Custom properties `--x` / `var()` | ⚠️ | ✅ | ✅ | ✅ | inherit + fallbacks; substituted before shorthand expansion; no invalid-at-computed-value handling |
| `calc()`, `min()`, `max()`, `clamp()` | ⚠️ | ✅ | ✅ | ✅ | calc() with px/em/rem/%/numbers, + - * /, parens; px+% mixing unsupported (dropped); no min/max/clamp |
| `@media` | ⚠️ | ✅ | ✅ | ✅ | screen/all + min-/max-width (and-combined, nesting intersects); restyles on resize; other queries skipped safely |
| `@import`, `@font-face`, `@supports`, `@layer` | ❌ | ✅ | ✅ | ✅ | skipped safely |

## Units

| Feature | Lumen | Chrome | Firefox | Safari | Note |
|---|:-:|:-:|:-:|:-:|---|
| `px` | ✅ | ✅ | ✅ | ✅ | |
| `em` | ✅ | ✅ | ✅ | ✅ | incl. font-size vs parent |
| `%` | ⚠️ | ✅ | ✅ | ✅ | widths/margins/paddings; % heights = auto |
| `vw` / `vh` | ✅ | ✅ | ✅ | ✅ | |
| Unitless `0` | ✅ | ✅ | ✅ | ✅ | |
| Unitless numbers | ✅ | ✅ | ✅ | ✅ | line-height, font-weight |
| `rem` | ✅ | ✅ | ✅ | ✅ | resolves against the html font-size |
| `vmin`/`vmax` | ✅ | ✅ | ✅ | ✅ | dvh/svh/lvh unsupported |
| `pt cm mm in pc Q` | ✅ | ✅ | ✅ | ✅ | folded to px at parse (96dpi) |
| `ch` / `ex` | ⚠️ | ✅ | ✅ | ✅ | approximated as 0.5em (matches the heuristic measurer) |

## Colors

| Feature | Lumen | Chrome | Firefox | Safari | Note |
|---|:-:|:-:|:-:|:-:|---|
| Named colors | ✅ | ✅ | ✅ | ✅ | full CSS table (148) |
| `#rgb` / `#rrggbb` | ✅ | ✅ | ✅ | ✅ | |
| `#rgba` / `#rrggbbaa` | ✅ | ✅ | ✅ | ✅ | |
| `rgb(r, g, b)` | ✅ | ✅ | ✅ | ✅ | |
| `rgba()` | ✅ | ✅ | ✅ | ✅ | comma syntax; no slash/space syntax |
| `hsl()` / `hsla()` | ✅ | ✅ | ✅ | ✅ | comma syntax |
| `hwb()` / `lab()` / `oklch()` | ❌ | ✅ | ✅ | ✅ | |
| `transparent` | ✅ | ✅ | ✅ | ✅ | |
| `currentColor` | ✅ | ✅ | ✅ | ✅ | keyword + border-color default |

## Box model

| Feature | Lumen | Chrome | Firefox | Safari | Note |
|---|:-:|:-:|:-:|:-:|---|
| `width` / `height` | ✅ | ✅ | ✅ | ✅ | |
| `min-/max-width/height` | ⚠️ | ✅ | ✅ | ✅ | clamp used size (min wins); % min/max-height ignored; not applied to flex base sizes |
| `margin` + longhands (1–4 values) | ✅ | ✅ | ✅ | ✅ | |
| `margin: auto` centering | ✅ | ✅ | ✅ | ✅ | horizontal only |
| `padding` + longhands | ✅ | ✅ | ✅ | ✅ | |
| `border-width` | ✅ | ✅ | ✅ | ✅ | per-side via shorthand |
| `border-color` | ✅ | ✅ | ✅ | ✅ | per side (1–4 values); defaults to text color |
| `border-style` | ⚠️ | ✅ | ✅ | ✅ | none/hidden hide; solid/dashed/dotted render (square borders; rounded rings stay solid); initial behaves as solid (deviation) |
| `border` shorthand | ✅ | ✅ | ✅ | ✅ | any order; missing width = 3px |
| Per-side `border-top/right/bottom/left` | ✅ | ✅ | ✅ | ✅ | width/style/color any order |
| `border-radius` | ⚠️ | ✅ | ✅ | ✅ | 1–4 value shorthand + per-corner longhands (px/em); rounds background + border; rounded border ring is single color/width; no % radii, no elliptical, no overflow clipping |
| `box-sizing` | ✅ | ✅ | ✅ | ✅ | border-box and content-box |
| `outline` | ⚠️ | ✅ | ✅ | ✅ | width/style/color outside the border box; no offset, square corners |
| `box-shadow` | ⚠️ | ✅ | ✅ | ✅ | first outer shadow; blur faked with 4 layered alpha rings; no inset/multiple |
| Margin collapsing | ⚠️ | ✅ | ✅ | ✅ | sibling + parent/first-child top; no bottom parent-child, no empty-block collapse-through |

## Layout

| Feature | Lumen | Chrome | Firefox | Safari | Note |
|---|:-:|:-:|:-:|:-:|---|
| `display: block` | ✅ | ✅ | ✅ | ✅ | |
| `display: inline` | ✅ | ✅ | ✅ | ✅ | shared line boxes; inline box edges (margin/padding/border/bg) ignored |
| `display: none` | ✅ | ✅ | ✅ | ✅ | |
| `display: inline-block` | ✅ | ✅ | ✅ | ✅ | atomic inline; auto width = crude shrink-to-fit (auto-width block children inflate to available) |
| Flexbox | ⚠️ | ✅ | ✅ | ✅ | direction row/column, flex-wrap, justify-content (start/center/end/space-between), align-items/align-self, gap (both axes), flex-grow/shrink, `flex` shorthand; no flex-basis/order/align-content/wrap-reverse |
| Grid | ❌ | ✅ | ✅ | ✅ | |
| Table layout | ❌ | ✅ | ✅ | ✅ | tables flow as plain blocks |
| `display: list-item` | ❌ | ✅ | ✅ | ✅ | no markers |
| `position` + offsets + `z-index` | ⚠️ | ✅ | ✅ | ✅ | relative/absolute/fixed; containing block = parent content box (not nearest positioned ancestor); absolute `bottom` unsupported; fixed scrolls with the page; z-index = simple sort, no stacking contexts |
| `float` / `clear` | ⚠️ | ✅ | ✅ | ✅ | simplified: floats narrow inline lines in the same container only; block siblings ignore floats except clear |
| `overflow` | ⚠️ | ✅ | ✅ | ✅ | hidden/scroll/auto/clip all clip to the padding box at paint time; no inner scrolling; hit testing unclipped |
| `vertical-align` | ❌ | ✅ | ✅ | ✅ | |
| `direction: rtl` / writing modes | ❌ | ✅ | ✅ | ✅ | |
| Multi-column | ❌ | ✅ | ✅ | ✅ | |
| `aspect-ratio` | ❌ | ✅ | ✅ | ✅ | |
| `gap` | ✅ | ✅ | ✅ | ✅ | flex containers |

## Backgrounds

| Feature | Lumen | Chrome | Firefox | Safari | Note |
|---|:-:|:-:|:-:|:-:|---|
| `background-color` | ✅ | ✅ | ✅ | ✅ | |
| `background` shorthand | ⚠️ | ✅ | ✅ | ✅ | color + image components only |
| Body/root background → canvas | ✅ | ✅ | ✅ | ✅ | |
| `background-image: url()` | ⚠️ | ✅ | ✅ | ✅ | position/size (auto/cover/contain/lengths)/repeat honored, clipped to the border box; shares the 32-image page cap |
| `linear-gradient()` | ⚠️ | ✅ | ✅ | ✅ | angles + to-side/corner, %-positioned stops; no repeating, no interpolation hints |
| `radial-gradient()` / `conic-gradient()` | ⚠️ | ✅ | ✅ | ✅ | radial as a centered ellipse (shape/position prelude ignored); no conic |
| position/size/repeat/attachment/clip | ⚠️ | ✅ | ✅ | ✅ | position (keywords/lengths/%), size (auto/cover/contain/lengths), repeat/no-repeat/-x/-y; no attachment/clip |
| Multiple backgrounds | ❌ | ✅ | ✅ | ✅ | |

## Typography

| Feature | Lumen | Chrome | Firefox | Safari | Note |
|---|:-:|:-:|:-:|:-:|---|
| `font-size` | ✅ | ✅ | ✅ | ✅ | px, em, % |
| `font-weight` | ✅ | ✅ | ✅ | ✅ | numeric+keywords; drawn as double-strike ≥600 |
| `line-height` | ✅ | ✅ | ✅ | ✅ | number, px, em |
| `text-align: left/center/right` | ✅ | ✅ | ✅ | ✅ | |
| `text-align: justify` | ❌ | ✅ | ✅ | ✅ | |
| `text-decoration` | ⚠️ | ✅ | ✅ | ✅ | underline, line-through, none; no color/style/wavy; propagation ≈ inheritance |
| `font-family` | ⚠️ | ✅ | ✅ | ✅ | collapsed to generic: monospace vs everything else (one face each) |
| `font-style: italic` | ✅ | ✅ | ✅ | ✅ | synthetic shear (no real italic face) |
| `font` shorthand | ⚠️ | ✅ | ✅ | ✅ | style/weight/size/line-height/family; small-caps and system fonts ignored |
| `letter-/word-spacing` | ✅ | ✅ | ✅ | ✅ | px/em |
| `text-transform` | ✅ | ✅ | ✅ | ✅ | uppercase/lowercase/capitalize |
| `text-indent` | ✅ | ✅ | ✅ | ✅ | first line of the block |
| `white-space` / `pre` | ⚠️ | ✅ | ✅ | ✅ | normal, nowrap, pre (pre-wrap/pre-line treated as pre) |
| `word-break` / `overflow-wrap` | ❌ | ✅ | ✅ | ✅ | over-wide words overflow |
| `text-overflow: ellipsis` | ❌ | ✅ | ✅ | ✅ | |
| `text-shadow` | ❌ | ✅ | ✅ | ✅ | |
| Web fonts `@font-face` | ❌ | ✅ | ✅ | ✅ | |

## Visual effects & motion

| Feature | Lumen | Chrome | Firefox | Safari | Note |
|---|:-:|:-:|:-:|:-:|---|
| `opacity` | ⚠️ | ✅ | ✅ | ✅ | per-command alpha multiply (approximation of group compositing; overlapping children double-blend) |
| `visibility: hidden` | ❌ | ✅ | ✅ | ✅ | |
| `transform` | ❌ | ✅ | ✅ | ✅ | |
| `filter` / `backdrop-filter` | ❌ | ✅ | ✅ | ✅ | |
| `clip-path` / `mask` | ❌ | ✅ | ✅ | ✅ | |
| `transition` | ❌ | ✅ | ✅ | ✅ | |
| `animation` / `@keyframes` | ❌ | ✅ | ✅ | ✅ | |
| `cursor` | ⚠️ | ✅ | ✅ | ✅ | pointer over links only; property ignored |
| `user-select: none` | ✅ | ✅ | ✅ | ✅ | treated as inherited |
| `pointer-events` | ❌ | ✅ | ✅ | ✅ | |
