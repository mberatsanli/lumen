//! The typed style model: every enum/struct a computed style is made
//! of, their defaults, and the value-level parsers (transforms,
//! gradients, grid tracks) that turn raw text into them.

use crate::geometry::{Corners, EdgeSizes};
use lumen_css::{Color, CssValue};

/// The subset of `display` the engine understands.
///
/// `Inline` elements currently still participate in block flow (inline
/// layout is a later milestone); `None` removes the subtree.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Display {
    Block,
    Inline,
    /// Atomic inline: flows in line boxes, lays out like a block inside.
    InlineBlock,
    /// Flex container (single-line; see layout docs for the subset).
    Flex,
    None,
    Grid,
}

/// A width/height/margin/padding value before resolution against the
/// containing block.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Dimension {
    Auto,
    Px(f32),
    Percent(f32),
    /// Percent of the viewport width / height.
    Vw(f32),
    Vh(f32),
    /// Percent of the smaller / larger viewport dimension.
    Vmin(f32),
    Vmax(f32),
}

impl Dimension {
    /// Resolves against the containing block size and the viewport;
    /// `Auto` resolves to `None`.
    #[must_use]
    pub fn resolve(&self, containing: f32, viewport: crate::geometry::Size) -> Option<f32> {
        match self {
            Self::Auto => None,
            Self::Px(value) => Some(*value),
            Self::Percent(percent) => Some(containing * percent / 100.0),
            Self::Vw(percent) => Some(viewport.width * percent / 100.0),
            Self::Vh(percent) => Some(viewport.height * percent / 100.0),
            Self::Vmin(percent) => Some(viewport.width.min(viewport.height) * percent / 100.0),
            Self::Vmax(percent) => Some(viewport.width.max(viewport.height) * percent / 100.0),
        }
    }

    /// Converts a declared value; `em` resolves against `font_size` here,
    /// so layout only ever sees px, percent or auto.
    pub(crate) fn from_value(value: &CssValue, font_size: f32) -> Option<Self> {
        match value {
            CssValue::Auto => Some(Self::Auto),
            CssValue::Length(pixels, lumen_css::Unit::Px) => Some(Self::Px(*pixels)),
            CssValue::Length(factor, lumen_css::Unit::Em) => Some(Self::Px(factor * font_size)),
            CssValue::Length(percent, lumen_css::Unit::Percent) => Some(Self::Percent(*percent)),
            CssValue::Length(percent, lumen_css::Unit::Vw) => Some(Self::Vw(*percent)),
            CssValue::Length(percent, lumen_css::Unit::Vh) => Some(Self::Vh(*percent)),
            CssValue::Length(percent, lumen_css::Unit::Vmin) => Some(Self::Vmin(*percent)),
            CssValue::Length(percent, lumen_css::Unit::Vmax) => Some(Self::Vmax(*percent)),
            _ => None,
        }
    }
}

/// Numeric font weight (400 = normal, 700 = bold).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FontWeight(pub u16);

impl Default for FontWeight {
    fn default() -> Self {
        Self(400)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum TextAlign {
    #[default]
    Left,
    Center,
    Right,
    /// Wrapped lines stretch to the full width.
    Justify,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum FlexDirection {
    #[default]
    Row,
    Column,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum JustifyContent {
    #[default]
    Start,
    Center,
    End,
    SpaceBetween,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum AlignItems {
    #[default]
    Stretch,
    Start,
    Center,
    End,
}

/// Border line style. Deviation from CSS: the initial value behaves as
/// `solid` (so `border-width` alone shows a border, as the project brief
/// expects); `none`/`hidden` suppress the border. `dashed`/`dotted` parse
/// but render solid for now.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum BorderStyle {
    #[default]
    Solid,
    Dashed,
    Dotted,
    None,
}

/// CSS positioning scheme.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Position {
    #[default]
    Static,
    Relative,
    Absolute,
    Fixed,
}

/// `float: left | right`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Float {
    #[default]
    None,
    Left,
    Right,
}

/// `clear: left | right | both`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Clear {
    #[default]
    None,
    Left,
    Right,
    Both,
}

/// What `width`/`height` refer to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum BoxSizing {
    /// The content box (CSS initial value).
    #[default]
    ContentBox,
    /// The border box: content shrinks by padding and border.
    BorderBox,
}

/// Fully resolved style for one node. All fields are typed; nothing needs
/// re-parsing during layout or paint.
#[derive(Debug, Clone, PartialEq)]
pub struct ComputedStyle {
    pub display: Display,
    pub color: Color,
    pub background_color: Option<Color>,
    /// Background layers, first = topmost (painted last to first).
    pub background_layers: Vec<BackgroundLayer>,
    /// `visibility` (inherited): hidden boxes keep their space unpainted.
    pub visible: bool,
    pub box_shadows: Vec<BoxShadow>,
    pub outline_width: f32,
    /// `None` = currentColor.
    pub outline_color: Option<Color>,
    pub outline_style: BorderStyle,
    /// width / height; derives an auto height from the used width.
    pub aspect_ratio: Option<f32>,
    /// Column tracks for `display: grid` (empty = one auto column).
    pub grid_columns: Vec<GridTrack>,
    /// `grid-column: span N` on grid items.
    pub grid_span: usize,
    /// Paint-time 2D transform about `transform_origin`.
    pub transform: Option<Transform2D>,
    /// Origin as (x, y); percents resolve against the border box.
    pub transform_origin: (Dimension, Dimension),
    pub transitions: Vec<TransitionSpec>,
    /// `list-style(-type): none` suppresses the li marker.
    pub list_style_none: bool,
    /// Geometric control mark (from the internal `--lumen-mark` UA hook).
    pub mark: Option<Mark>,
    pub width: Dimension,
    pub height: Dimension,
    /// Size constraints; `Auto` means unconstrained.
    pub min_width: Dimension,
    pub max_width: Dimension,
    pub min_height: Dimension,
    pub max_height: Dimension,
    pub margin: EdgeSizes<Dimension>,
    pub padding: EdgeSizes<Dimension>,
    pub border_width: EdgeSizes<f32>,
    pub border_color: EdgeSizes<Color>,
    pub border_style: EdgeSizes<BorderStyle>,
    /// Corner radii in pixels, clockwise from top-left.
    pub border_radius: Corners<f32>,
    pub font_size: f32,
    pub font_weight: FontWeight,
    /// Resolved to pixels.
    pub line_height: f32,
    pub text_align: TextAlign,
    /// `text-decoration: underline`. Approximation: treated as inherited
    /// so text nodes inside links pick it up.
    pub underline: bool,
    /// `font-style: italic` (rendered as a synthetic shear).
    pub italic: bool,
    /// `font-family` collapsed to its generic: monospace or not.
    pub monospace: bool,
    pub white_space: WhiteSpace,
    pub text_transform: TextTransform,
    /// Extra advance per character, px.
    pub letter_spacing: f32,
    /// Extra width per inter-word space, px.
    pub word_spacing: f32,
    /// First-line indent, px.
    pub text_indent: f32,
    /// `text-overflow: ellipsis` (effective with nowrap + clipping).
    pub text_overflow_ellipsis: bool,
    /// `word-break: break-all` / `overflow-wrap: break-word`: over-wide
    /// words split at any character instead of overflowing.
    pub break_words: bool,
    /// `text-decoration: line-through` (inherited like underline).
    pub line_through: bool,
    /// Approximated as inherited so text inside `<sup>`/aligned spans
    /// picks it up (deviation, like text-decoration).
    pub vertical_align: VerticalAlign,
    /// Inherited, like all text properties.
    pub text_shadows: Vec<TextShadow>,
    /// `None` = currentColor.
    pub text_decoration_color: Option<Color>,
    /// Solid/dashed/dotted (double and wavy approximate as solid).
    pub text_decoration_style: BorderStyle,
    pub box_sizing: BoxSizing,
    pub float: Float,
    pub clear: Clear,
    pub overflow: Overflow,
    pub position: Position,
    /// `top`/`right`/`bottom`/`left` offsets for positioned boxes.
    pub offsets: EdgeSizes<Dimension>,
    pub z_index: Option<i32>,
    pub flex_direction: FlexDirection,
    /// `flex-wrap: wrap` (wrap-reverse is treated as wrap).
    pub flex_wrap: bool,
    pub justify_content: JustifyContent,
    pub align_items: AlignItems,
    /// Per-item `align-items` override; `None` is `auto`.
    pub align_self: Option<AlignItems>,
    /// Resolved to pixels.
    pub gap: f32,
    pub flex_grow: f32,
    pub flex_shrink: f32,
    /// Element opacity 0..=1, multiplied into every paint command of the
    /// subtree (an approximation of real group compositing).
    pub opacity: f32,
    /// `user-select: none` makes the element's text unselectable.
    pub selectable: bool,
    /// `::selection` overrides: highlight background and (recorded, not
    /// yet painted) text color.
    pub selection_background: Option<Color>,
    pub selection_color: Option<Color>,
}

/// `vertical-align` subset for inline-level content.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum VerticalAlign {
    #[default]
    Baseline,
    Top,
    Middle,
    Bottom,
    /// Baseline shifted down ~0.25em.
    Sub,
    /// Baseline shifted up ~0.4em.
    Super,
}

/// `text-transform` subset.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum TextTransform {
    #[default]
    None,
    Uppercase,
    Lowercase,
    Capitalize,
}

/// A background image layer (single layer only).
#[derive(Debug, Clone, PartialEq)]
pub enum BackgroundImage {
    /// Fetched by navigation code; placement follows background-position/
    /// -size/-repeat.
    Url(String),
    LinearGradient(LinearGradient),
    /// Center-anchored ellipse with normalized stops.
    RadialGradient(Vec<(Color, f32)>),
    /// Center-anchored sweep (0 at top, clockwise) with normalized stops.
    ConicGradient(Vec<(Color, f32)>),
}

/// One box shadow of a possibly comma-separated list.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct BoxShadow {
    pub offset_x: f32,
    pub offset_y: f32,
    pub blur: f32,
    pub spread: f32,
    pub color: Color,
    /// Shades inward from the box edge instead of dropping behind it.
    pub inset: bool,
}

/// One text shadow of a possibly comma-separated list.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TextShadow {
    pub offset_x: f32,
    pub offset_y: f32,
    pub blur: f32,
    pub color: Color,
}

/// A 2D affine transform (column-major CSS matrix(a, b, c, d, e, f)):
/// x' = a·x + c·y + e, y' = b·x + d·y + f.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Transform2D {
    pub a: f32,
    pub b: f32,
    pub c: f32,
    pub d: f32,
    pub e: f32,
    pub f: f32,
}

impl Transform2D {
    pub const IDENTITY: Self = Self {
        a: 1.0,
        b: 0.0,
        c: 0.0,
        d: 1.0,
        e: 0.0,
        f: 0.0,
    };

    #[must_use]
    pub fn multiply(self, other: Self) -> Self {
        Self {
            a: self.a * other.a + self.c * other.b,
            b: self.b * other.a + self.d * other.b,
            c: self.a * other.c + self.c * other.d,
            d: self.b * other.c + self.d * other.d,
            e: self.a * other.e + self.c * other.f + self.e,
            f: self.b * other.e + self.d * other.f + self.f,
        }
    }

    #[must_use]
    pub fn translate(x: f32, y: f32) -> Self {
        Self {
            e: x,
            f: y,
            ..Self::IDENTITY
        }
    }

    #[must_use]
    pub fn apply(&self, x: f32, y: f32) -> (f32, f32) {
        (
            self.a * x + self.c * y + self.e,
            self.b * x + self.d * y + self.f,
        )
    }

    #[must_use]
    pub fn is_identity(&self) -> bool {
        *self == Self::IDENTITY
    }

    /// Component-wise interpolation (matches CSS for translate/scale).
    #[must_use]
    pub fn lerp(from: Self, to: Self, t: f32) -> Self {
        let mix = |a: f32, b: f32| a + (b - a) * t;
        Self {
            a: mix(from.a, to.a),
            b: mix(from.b, to.b),
            c: mix(from.c, to.c),
            d: mix(from.d, to.d),
            e: mix(from.e, to.e),
            f: mix(from.f, to.f),
        }
    }
}

/// Parses a transform list: translate/translateX/translateY (px/%/em),
/// scale/scaleX/scaleY, rotate(deg) and matrix(); composed left to right.
pub(crate) fn parse_transform(source: &str, font_size: f32) -> Option<Transform2D> {
    let mut matrix = Transform2D::IDENTITY;
    let mut rest = source.trim();
    if rest == "none" {
        return None;
    }
    while !rest.is_empty() {
        let open = rest.find('(')?;
        let name = rest[..open].trim().to_ascii_lowercase();
        let after = &rest[open + 1..];
        let close = find_balanced_paren(after)?;
        let arguments: Vec<f32> = after[..close]
            .split(',')
            .filter_map(|argument| {
                let argument = argument.trim();
                if let Some(number) = argument.strip_suffix("deg") {
                    return number.trim().parse().ok();
                }
                match CssValue::parse_component(argument)? {
                    CssValue::Length(px, lumen_css::Unit::Px) => Some(px),
                    CssValue::Length(em, lumen_css::Unit::Em) => Some(em * font_size),
                    CssValue::Length(percent, lumen_css::Unit::Percent) => Some(percent),
                    CssValue::Number(number) => Some(number),
                    _ => None,
                }
            })
            .collect();
        let step = match name.as_str() {
            "translate" => Transform2D::translate(
                *arguments.first()?,
                arguments.get(1).copied().unwrap_or(0.0),
            ),
            "translatex" => Transform2D::translate(*arguments.first()?, 0.0),
            "translatey" => Transform2D::translate(0.0, *arguments.first()?),
            "scale" => {
                let sx = *arguments.first()?;
                let sy = arguments.get(1).copied().unwrap_or(sx);
                Transform2D {
                    a: sx,
                    d: sy,
                    ..Transform2D::IDENTITY
                }
            }
            "scalex" => Transform2D {
                a: *arguments.first()?,
                ..Transform2D::IDENTITY
            },
            "scaley" => Transform2D {
                d: *arguments.first()?,
                ..Transform2D::IDENTITY
            },
            "rotate" => {
                let radians = arguments.first()?.to_radians();
                Transform2D {
                    a: radians.cos(),
                    b: radians.sin(),
                    c: -radians.sin(),
                    d: radians.cos(),
                    ..Transform2D::IDENTITY
                }
            }
            "matrix" if arguments.len() == 6 => Transform2D {
                a: arguments[0],
                b: arguments[1],
                c: arguments[2],
                d: arguments[3],
                e: arguments[4],
                f: arguments[5],
            },
            _ => return None,
        };
        matrix = matrix.multiply(step);
        rest = after[close + 1..].trim_start();
    }
    Some(matrix)
}

/// One transition: property (or "all"), duration and delay in seconds,
/// plus whether the ease timing curve applies (else linear).
#[derive(Debug, Clone, PartialEq)]
pub struct TransitionSpec {
    pub property: String,
    pub duration: f32,
    pub delay: f32,
    pub ease: bool,
}

/// A vector-drawn control mark.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Mark {
    /// A check tick (checkboxes).
    Check,
    /// A filled dot (radios).
    Dot,
    /// A horizontal value bar filled to the fraction (progress/meter/
    /// range; range also gets a thumb).
    Fraction(FractionMark),
    /// A small dropdown arrow at the right edge (select).
    Arrow,
}

/// A 0..=1 fill fraction with optional slider thumb.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct FractionMark {
    pub fraction: f32,
    pub thumb: bool,
}

/// One grid column track.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum GridTrack {
    Px(f32),
    /// Fraction of the leftover space.
    Fr(f32),
    Percent(f32),
    /// Behaves like `1fr` (simplification).
    Auto,
}

/// One background layer (image + placement). Lists cycle per CSS when
/// shorter than the image list.
#[derive(Debug, Clone, PartialEq)]
pub struct BackgroundLayer {
    pub image: BackgroundImage,
    pub position: (Dimension, Dimension),
    pub size: BackgroundSize,
    /// Tiling along x / y.
    pub repeat: (bool, bool),
}

/// `background-size` subset.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub enum BackgroundSize {
    /// Intrinsic image size.
    #[default]
    Auto,
    Cover,
    Contain,
    /// Explicit width/height (Auto keeps the aspect ratio).
    Explicit(Dimension, Dimension),
}

/// `linear-gradient()`: an angle (CSS convention, 0deg = to top) and
/// normalized color stops (position 0..=1).
#[derive(Debug, Clone, PartialEq)]
pub struct LinearGradient {
    pub angle_degrees: f32,
    pub stops: Vec<(Color, f32)>,
}

/// Parses `linear-gradient(...)` arguments; `None` when unsupported.
pub(crate) fn parse_linear_gradient(arguments: &str) -> Option<LinearGradient> {
    let parts: Vec<&str> = split_top_level_commas(arguments);
    if parts.is_empty() {
        return None;
    }
    let mut index = 0;
    let first = parts[0].trim();
    let angle_degrees = if let Some(direction) = first.strip_prefix("to ") {
        index = 1;
        match direction
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ")
            .as_str()
        {
            "top" => 0.0,
            "right" => 90.0,
            "bottom" => 180.0,
            "left" => 270.0,
            "top right" | "right top" => 45.0,
            "bottom right" | "right bottom" => 135.0,
            "bottom left" | "left bottom" => 225.0,
            "top left" | "left top" => 315.0,
            _ => return None,
        }
    } else if let Some(number) = first.strip_suffix("deg") {
        index = 1;
        number.trim().parse().ok()?
    } else {
        180.0 // Default: to bottom.
    };
    let stops = parse_gradient_stops(&parts[index..])?;
    Some(LinearGradient {
        angle_degrees,
        stops,
    })
}

/// Parses gradient color stops and normalizes their positions: first
/// defaults to 0, last to 1, unpositioned middles spread evenly.
pub(crate) fn parse_gradient_stops(parts: &[&str]) -> Option<Vec<(Color, f32)>> {
    let mut stops: Vec<(Color, Option<f32>)> = Vec::new();
    for part in parts {
        let mut pieces = part.split_whitespace();
        let color = Color::parse(pieces.next()?)?;
        let position = match pieces.next() {
            Some(position) => Some(match position.strip_suffix('%') {
                Some(percent) => percent.parse::<f32>().ok()? / 100.0,
                None => position.strip_suffix("deg")?.parse::<f32>().ok()? / 360.0,
            }),
            None => None,
        };
        stops.push((color, position));
    }
    if stops.len() < 2 {
        return None;
    }
    let count = stops.len();
    if stops[0].1.is_none() {
        stops[0].1 = Some(0.0);
    }
    if stops[count - 1].1.is_none() {
        stops[count - 1].1 = Some(1.0);
    }
    let mut resolved: Vec<(Color, f32)> = Vec::with_capacity(count);
    let mut position_so_far = 0.0f32;
    for (offset, (color, position)) in stops.iter().enumerate() {
        let position = match position {
            Some(position) => position.max(position_so_far),
            None => {
                let (steps_to_next, next_position) = stops[offset + 1..]
                    .iter()
                    .enumerate()
                    .find_map(|(ahead, (_, position))| {
                        position.map(|position| (ahead + 1, position))
                    })
                    .unwrap_or((1, 1.0));
                position_so_far
                    + (next_position.max(position_so_far) - position_so_far)
                        / (steps_to_next + 1) as f32
            }
        };
        position_so_far = position;
        resolved.push((*color, position));
    }
    Some(resolved)
}

/// Parses a grid track list: lengths, percents, `Nfr`, `auto` and
/// `repeat(N, tracks)`.
pub(crate) fn parse_grid_tracks(source: &str, font_size: f32) -> Vec<GridTrack> {
    let mut tracks = Vec::new();
    let mut rest = source.trim();
    while !rest.is_empty() {
        if let Some(after) = rest.strip_prefix("repeat(") {
            let Some(close) = find_balanced_paren(after) else {
                break;
            };
            let inner = &after[..close];
            if let Some((count, list)) = inner.split_once(',')
                && let Ok(count) = count.trim().parse::<usize>()
            {
                let inner_tracks = parse_grid_tracks(list, font_size);
                for _ in 0..count.min(64) {
                    tracks.extend(inner_tracks.iter().copied());
                }
            }
            rest = after[close + 1..].trim_start();
            continue;
        }
        let end = rest.find(char::is_whitespace).unwrap_or(rest.len());
        let token = &rest[..end];
        rest = rest[end..].trim_start();
        if token == "auto" {
            tracks.push(GridTrack::Auto);
        } else if let Some(number) = token.strip_suffix("fr") {
            if let Ok(value) = number.parse::<f32>() {
                tracks.push(GridTrack::Fr(value.max(0.0)));
            }
        } else if let Some(dimension) = CssValue::parse_component(token)
            .and_then(|value| Dimension::from_value(&value, font_size))
        {
            match dimension {
                Dimension::Px(value) => tracks.push(GridTrack::Px(value)),
                Dimension::Percent(value) => tracks.push(GridTrack::Percent(value)),
                _ => {}
            }
        }
    }
    tracks
}

/// Index of the `)` matching an already-consumed `(`.
pub(crate) fn find_balanced_paren(source: &str) -> Option<usize> {
    let mut depth = 1usize;
    for (index, character) in source.char_indices() {
        match character {
            '(' => depth += 1,
            ')' => {
                depth -= 1;
                if depth == 0 {
                    return Some(index);
                }
            }
            _ => {}
        }
    }
    None
}

/// Splits at commas outside parentheses (rgb() stays whole).
pub(crate) fn split_top_level_commas(source: &str) -> Vec<&str> {
    let mut parts = Vec::new();
    let mut depth = 0usize;
    let mut start = 0;
    for (index, character) in source.char_indices() {
        match character {
            '(' => depth += 1,
            ')' => depth = depth.saturating_sub(1),
            ',' if depth == 0 => {
                parts.push(&source[start..index]);
                start = index + 1;
            }
            _ => {}
        }
    }
    parts.push(&source[start..]);
    parts
}

/// `overflow` subset: anything that is not `visible` clips children to
/// the padding box at paint time (no inner scrolling).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Overflow {
    #[default]
    Visible,
    /// Clips without user scrolling (`hidden`/`clip`).
    Hidden,
    /// Clips and scrolls on wheel input (`scroll`/`auto`).
    Scroll,
}

impl Overflow {
    /// Whether children clip to the padding box.
    #[must_use]
    pub fn clips(self) -> bool {
        self != Self::Visible
    }
}

/// `white-space` subset: `pre` preserves spaces and newlines and never
/// wraps (pre-wrap/pre-line are approximated as pre).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum WhiteSpace {
    #[default]
    Normal,
    Pre,
    /// Collapses whitespace but never wraps.
    Nowrap,
}

pub const DEFAULT_FONT_SIZE: f32 = 16.0;
/// Used when no `line-height` is declared or inherited.
pub const DEFAULT_LINE_HEIGHT_FACTOR: f32 = 1.4;
const DEFAULT_COLOR: Color = Color::rgb(0x11, 0x11, 0x11);

impl Default for ComputedStyle {
    fn default() -> Self {
        Self {
            display: Display::Inline,
            color: DEFAULT_COLOR,
            background_color: None,
            background_layers: Vec::new(),
            visible: true,
            box_shadows: Vec::new(),
            outline_width: 0.0,
            outline_color: None,
            outline_style: BorderStyle::None,
            aspect_ratio: None,
            grid_columns: Vec::new(),
            grid_span: 1,
            transform: None,
            transform_origin: (Dimension::Percent(50.0), Dimension::Percent(50.0)),
            transitions: Vec::new(),
            mark: None,
            width: Dimension::Auto,
            height: Dimension::Auto,
            min_width: Dimension::Auto,
            max_width: Dimension::Auto,
            list_style_none: false,
            min_height: Dimension::Auto,
            max_height: Dimension::Auto,
            margin: EdgeSizes::uniform(Dimension::Px(0.0)),
            padding: EdgeSizes::uniform(Dimension::Px(0.0)),
            border_width: EdgeSizes::uniform(0.0),
            border_color: EdgeSizes::uniform(DEFAULT_COLOR),
            border_style: EdgeSizes::uniform(BorderStyle::Solid),
            border_radius: Corners::uniform(0.0),
            font_size: DEFAULT_FONT_SIZE,
            font_weight: FontWeight::default(),
            line_height: DEFAULT_FONT_SIZE * DEFAULT_LINE_HEIGHT_FACTOR,
            text_align: TextAlign::Left,
            underline: false,
            italic: false,
            monospace: false,
            white_space: WhiteSpace::Normal,
            text_transform: TextTransform::None,
            letter_spacing: 0.0,
            word_spacing: 0.0,
            text_indent: 0.0,
            text_overflow_ellipsis: false,
            break_words: false,
            line_through: false,
            vertical_align: VerticalAlign::Baseline,
            text_shadows: Vec::new(),
            text_decoration_color: None,
            text_decoration_style: BorderStyle::Solid,
            box_sizing: BoxSizing::default(),
            float: Float::None,
            clear: Clear::None,
            overflow: Overflow::Visible,
            position: Position::Static,
            offsets: EdgeSizes::uniform(Dimension::Auto),
            z_index: None,
            flex_direction: FlexDirection::default(),
            flex_wrap: false,
            justify_content: JustifyContent::default(),
            align_items: AlignItems::default(),
            align_self: None,
            gap: 0.0,
            flex_grow: 0.0,
            flex_shrink: 1.0,
            opacity: 1.0,
            selectable: true,
            selection_background: None,
            selection_color: None,
        }
    }
}
