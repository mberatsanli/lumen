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
| Child `A > B` | ❌ | ✅ | ✅ | ✅ | |
| Siblings `A + B`, `A ~ B` | ❌ | ✅ | ✅ | ✅ | |
| Attribute `[href]`, `[type="x"]` | ❌ | ✅ | ✅ | ✅ | |
| `:hover` | ✅ | ✅ | ✅ | ✅ | live hover chain in desktop shell |
| `:link` / `:visited` | ⚠️ | ✅ | ✅ | ✅ | always match; no visited state |
| `:active`, `:focus*` | ❌ | ✅ | ✅ | ✅ | |
| `:first/last/only-child` | ❌ | ✅ | ✅ | ✅ | |
| `:nth-child()` family | ❌ | ✅ | ✅ | ✅ | |
| `:not()`, `:is()`, `:where()` | ❌ | ✅ | ✅ | ✅ | |
| `:has()` | ❌ | ✅ | ✅ | ✅ | |
| `::before` / `::after` + `content` | ❌ | ✅ | ✅ | ✅ | |
| `::first-line/-letter`, `::selection` | ❌ | ✅ | ✅ | ✅ | |

## Cascade & values

| Feature | Lumen | Chrome | Firefox | Safari | Note |
|---|:-:|:-:|:-:|:-:|---|
| Specificity | ✅ | ✅ | ✅ | ✅ | structural, lexicographic |
| Source-order tie-break | ✅ | ✅ | ✅ | ✅ | |
| Origins UA < author < inline | ✅ | ✅ | ✅ | ✅ | |
| Inheritance | ⚠️ | ✅ | ✅ | ✅ | 6 props; text-decoration approximated as inherited |
| `!important` | ❌ | ✅ | ✅ | ✅ | |
| `inherit`/`initial`/`unset`/`revert` | ❌ | ✅ | ✅ | ✅ | |
| Custom properties `--x` / `var()` | ❌ | ✅ | ✅ | ✅ | |
| `calc()`, `min()`, `max()`, `clamp()` | ❌ | ✅ | ✅ | ✅ | |
| `@media` | ❌ | ✅ | ✅ | ✅ | blocks skipped safely, never applied |
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
| `rem` | ❌ | ✅ | ✅ | ✅ | |
| `vmin`/`vmax`, `dvh`/`svh`/`lvh` | ❌ | ✅ | ✅ | ✅ | |
| `pt cm mm in pc ch ex Q` | ❌ | ✅ | ✅ | ✅ | |

## Colors

| Feature | Lumen | Chrome | Firefox | Safari | Note |
|---|:-:|:-:|:-:|:-:|---|
| Named colors | ⚠️ | ✅ | ✅ | ✅ | 11 names vs ~148 |
| `#rgb` / `#rrggbb` | ✅ | ✅ | ✅ | ✅ | |
| `#rgba` / `#rrggbbaa` | ❌ | ✅ | ✅ | ✅ | no alpha anywhere in the engine |
| `rgb(r, g, b)` | ✅ | ✅ | ✅ | ✅ | |
| `rgba()` / slash alpha | ❌ | ✅ | ✅ | ✅ | |
| `hsl()` / `hwb()` / `lab()` / `oklch()` | ❌ | ✅ | ✅ | ✅ | |
| `transparent` | ✅ | ✅ | ✅ | ✅ | |
| `currentColor` | ⚠️ | ✅ | ✅ | ✅ | border-color defaults to it; keyword itself doesn't parse |

## Box model

| Feature | Lumen | Chrome | Firefox | Safari | Note |
|---|:-:|:-:|:-:|:-:|---|
| `width` / `height` | ✅ | ✅ | ✅ | ✅ | |
| `min-/max-width/height` | ❌ | ✅ | ✅ | ✅ | parsed, ignored by layout |
| `margin` + longhands (1–4 values) | ✅ | ✅ | ✅ | ✅ | |
| `margin: auto` centering | ✅ | ✅ | ✅ | ✅ | horizontal only |
| `padding` + longhands | ✅ | ✅ | ✅ | ✅ | |
| `border-width` | ✅ | ✅ | ✅ | ✅ | per-side via shorthand |
| `border-color` | ✅ | ✅ | ✅ | ✅ | defaults to text color |
| `border-style` | ❌ | ✅ | ✅ | ✅ | always renders solid |
| `border` shorthand | ❌ | ✅ | ✅ | ✅ | |
| `border-radius` | ❌ | ✅ | ✅ | ✅ | |
| `box-sizing` | ✅ | ✅ | ✅ | ✅ | border-box and content-box |
| `outline` | ❌ | ✅ | ✅ | ✅ | |
| `box-shadow` | ❌ | ✅ | ✅ | ✅ | |
| Margin collapsing | ⚠️ | ✅ | ✅ | ✅ | sibling + parent/first-child top; no bottom parent-child, no empty-block collapse-through |

## Layout

| Feature | Lumen | Chrome | Firefox | Safari | Note |
|---|:-:|:-:|:-:|:-:|---|
| `display: block` | ✅ | ✅ | ✅ | ✅ | |
| `display: inline` | ✅ | ✅ | ✅ | ✅ | shared line boxes; inline box edges (margin/padding/border/bg) ignored |
| `display: none` | ✅ | ✅ | ✅ | ✅ | |
| `display: inline-block` | ✅ | ✅ | ✅ | ✅ | atomic inline; auto width = crude shrink-to-fit (auto-width block children inflate to available) |
| Flexbox | ❌ | ✅ | ✅ | ✅ | |
| Grid | ❌ | ✅ | ✅ | ✅ | |
| Table layout | ❌ | ✅ | ✅ | ✅ | tables flow as plain blocks |
| `display: list-item` | ❌ | ✅ | ✅ | ✅ | no markers |
| `position` + offsets + `z-index` | ❌ | ✅ | ✅ | ✅ | |
| `float` / `clear` | ⚠️ | ✅ | ✅ | ✅ | simplified: floats narrow inline lines in the same container only; block siblings ignore floats except clear |
| `overflow` | ❌ | ✅ | ✅ | ✅ | boxes never clip/scroll |
| `vertical-align` | ❌ | ✅ | ✅ | ✅ | |
| `direction: rtl` / writing modes | ❌ | ✅ | ✅ | ✅ | |
| Multi-column | ❌ | ✅ | ✅ | ✅ | |
| `aspect-ratio` | ❌ | ✅ | ✅ | ✅ | |
| `gap` | ❌ | ✅ | ✅ | ✅ | |

## Backgrounds

| Feature | Lumen | Chrome | Firefox | Safari | Note |
|---|:-:|:-:|:-:|:-:|---|
| `background-color` | ✅ | ✅ | ✅ | ✅ | |
| `background` shorthand | ⚠️ | ✅ | ✅ | ✅ | color component only |
| Body/root background → canvas | ✅ | ✅ | ✅ | ✅ | |
| `background-image: url()` | ❌ | ✅ | ✅ | ✅ | |
| `linear-gradient()` | ❌ | ✅ | ✅ | ✅ | |
| `radial-gradient()` / `conic-gradient()` | ❌ | ✅ | ✅ | ✅ | |
| position/size/repeat/attachment/clip | ❌ | ✅ | ✅ | ✅ | |
| Multiple backgrounds | ❌ | ✅ | ✅ | ✅ | |

## Typography

| Feature | Lumen | Chrome | Firefox | Safari | Note |
|---|:-:|:-:|:-:|:-:|---|
| `font-size` | ✅ | ✅ | ✅ | ✅ | px, em, % |
| `font-weight` | ✅ | ✅ | ✅ | ✅ | numeric+keywords; drawn as double-strike ≥600 |
| `line-height` | ✅ | ✅ | ✅ | ✅ | number, px, em |
| `text-align: left/center/right` | ✅ | ✅ | ✅ | ✅ | |
| `text-align: justify` | ❌ | ✅ | ✅ | ✅ | |
| `text-decoration: underline/none` | ⚠️ | ✅ | ✅ | ✅ | no line-through/color/style; propagation ≈ inheritance |
| `font-family` | ❌ | ✅ | ✅ | ✅ | one system font, no fallback lists |
| `font-style: italic` | ✅ | ✅ | ✅ | ✅ | synthetic shear (no real italic face) |
| `font` shorthand | ❌ | ✅ | ✅ | ✅ | |
| `letter-/word-spacing` | ❌ | ✅ | ✅ | ✅ | |
| `text-transform` | ❌ | ✅ | ✅ | ✅ | |
| `text-indent` | ❌ | ✅ | ✅ | ✅ | |
| `white-space` / `pre` | ❌ | ✅ | ✅ | ✅ | whitespace always collapses |
| `word-break` / `overflow-wrap` | ❌ | ✅ | ✅ | ✅ | over-wide words overflow |
| `text-overflow: ellipsis` | ❌ | ✅ | ✅ | ✅ | |
| `text-shadow` | ❌ | ✅ | ✅ | ✅ | |
| Web fonts `@font-face` | ❌ | ✅ | ✅ | ✅ | |

## Visual effects & motion

| Feature | Lumen | Chrome | Firefox | Safari | Note |
|---|:-:|:-:|:-:|:-:|---|
| `opacity` | ❌ | ✅ | ✅ | ✅ | |
| `visibility: hidden` | ❌ | ✅ | ✅ | ✅ | |
| `transform` | ❌ | ✅ | ✅ | ✅ | |
| `filter` / `backdrop-filter` | ❌ | ✅ | ✅ | ✅ | |
| `clip-path` / `mask` | ❌ | ✅ | ✅ | ✅ | |
| `transition` | ❌ | ✅ | ✅ | ✅ | |
| `animation` / `@keyframes` | ❌ | ✅ | ✅ | ✅ | |
| `cursor` | ⚠️ | ✅ | ✅ | ✅ | pointer over links only; property ignored |
| `pointer-events` / `user-select` | ❌ | ✅ | ✅ | ✅ | |
