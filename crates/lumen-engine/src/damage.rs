//! Paint-damage tracking for paint-only interaction repaints.
//!
//! When hover/:active/:focus rules touch only paint-level properties (see
//! [`crate::style::HoverImpact::PaintOnly`]), geometry is untouched, so a
//! restyle can only change pixels inside the restyled boxes (plus what
//! their shadows, outlines and blurs spill around them). [`paint_damage`]
//! diffs the old and new computed styles, maps the changed nodes to their
//! layout boxes and unions the resulting rects — the shell then re-rasterizes
//! just that region instead of the whole page.

use lumen_html::{Document, NodeId};

use crate::geometry::Rect;
use crate::layout::LayoutBox;
use crate::style::{ComputedStyle, FilterFunction, StyleMap};

/// The union of page-space rects whose paint can differ between the `old`
/// and `new` style maps over the same layout, grown by each box's paint
/// spill (shadows, outline, filter blur). `None` when nothing visible
/// changed.
///
/// Conservative fallbacks return the whole document box: nodes appearing
/// or disappearing (generated `content`), transformed boxes (paint can
/// land anywhere), and `html`/`body` background changes (which propagate
/// to the whole canvas).
#[must_use]
pub fn paint_damage(
    document: &Document,
    layout: &LayoutBox,
    old: &StyleMap,
    new: &StyleMap,
) -> Option<Rect> {
    let full = layout.border_box();
    let mut damage: Option<Rect> = None;
    for (node, new_style) in &new.by_node {
        let Some(old_style) = old.by_node.get(node) else {
            // A node without a previous style (new generated content):
            // its footprint is unknown, repaint everything.
            return Some(full);
        };
        if !paint_differs(old_style, new_style) {
            continue;
        }
        if spills_everywhere(document, *node, old_style, new_style) {
            return Some(full);
        }
        let Some(rect) = node_rect(document, layout, *node) else {
            // No box up the ancestor chain (display: none): nothing painted.
            continue;
        };
        let outset = paint_outset(old_style).max(paint_outset(new_style));
        let rect = rect.expanded_by(crate::geometry::Edges::uniform(outset));
        damage = Some(damage.map_or(rect, |so_far| union(so_far, rect)));
    }
    // A node that lost its style (removed generated content) may have
    // painted anywhere: repaint everything.
    if old
        .by_node
        .keys()
        .any(|node| !new.by_node.contains_key(node))
    {
        return Some(full);
    }
    damage
}

/// The rect two rects span together.
fn union(a: Rect, b: Rect) -> Rect {
    let x0 = a.x.min(b.x);
    let y0 = a.y.min(b.y);
    let x1 = (a.x + a.width).max(b.x + b.width);
    let y1 = (a.y + a.height).max(b.y + b.height);
    Rect {
        x: x0,
        y: y0,
        width: x1 - x0,
        height: y1 - y0,
    }
}

/// The border box of the nearest laid-out ancestor of `node` (text nodes
/// have no box of their own; their glyphs live inside the element's box).
fn node_rect(document: &Document, layout: &LayoutBox, node: NodeId) -> Option<Rect> {
    std::iter::once(node)
        .chain(document.ancestors(node))
        .find_map(|id| layout.find_by_node(id))
        .map(|laid| laid.border_box())
}

/// The damage rect for a node whose paint-only style is being mutated
/// in place (animation/transition ticks): its layout rect expanded by
/// the paint outset of `style`. `None` when the node paints nothing.
#[must_use]
pub fn node_damage(
    document: &Document,
    layout: &LayoutBox,
    node: NodeId,
    style: &ComputedStyle,
) -> Option<Rect> {
    let rect = node_rect(document, layout, node)?;
    Some(rect.expanded_by(crate::geometry::Edges::uniform(paint_outset(style))))
}

/// Changes that can move pixels outside the node's own box, so no cheap
/// rect covers them: transformed output lands anywhere, and the html/body
/// background propagates to the whole canvas.
fn spills_everywhere(
    document: &Document,
    node: NodeId,
    old: &ComputedStyle,
    new: &ComputedStyle,
) -> bool {
    if old.transform.is_some()
        || new.transform.is_some()
        || old.individual_transform.is_some()
        || new.individual_transform.is_some()
    {
        return true;
    }
    let propagates = document
        .element(node)
        .is_some_and(|element| matches!(element.tag_name.as_str(), "html" | "body"));
    propagates
        && (old.background_color != new.background_color
            || old.background_layers != new.background_layers)
}

/// Whether repainting with `new` instead of `old` can change any pixel.
/// Only paint-level fields are compared — in the paint-only reaction path
/// geometry-affecting fields cannot change, and fields like `cursor`,
/// `transitions` or `selectable` never reach the raster.
fn paint_differs(old: &ComputedStyle, new: &ComputedStyle) -> bool {
    old.color != new.color
        || old.background_color != new.background_color
        || old.background_layers != new.background_layers
        || old.background_clip != new.background_clip
        || old.background_origin != new.background_origin
        || old.visible != new.visible
        || old.box_shadows != new.box_shadows
        || old.text_shadows != new.text_shadows
        || old.outline_width != new.outline_width
        || old.outline_color != new.outline_color
        || old.outline_style != new.outline_style
        || old.outline_offset != new.outline_offset
        || old.border_color != new.border_color
        || old.border_style != new.border_style
        || old.border_radius != new.border_radius
        || old.underline != new.underline
        || old.line_through != new.line_through
        || old.text_decoration_color != new.text_decoration_color
        || old.text_decoration_style != new.text_decoration_style
        || old.opacity != new.opacity
        || old.filters != new.filters
        || old.selection_background != new.selection_background
        || old.selection_color != new.selection_color
        || old.accent_color != new.accent_color
        || old.mark != new.mark
        || old.z_index != new.z_index
        || old.font_weight != new.font_weight
        || old.italic != new.italic
}

/// How far beyond its border box a box's paint can reach: drop shadows,
/// outlines and filter blur spill outward. Text decorations and marks
/// stay inside.
fn paint_outset(style: &ComputedStyle) -> f32 {
    let mut outset = (style.outline_width + style.outline_offset.max(0.0)).max(0.0);
    for shadow in &style.box_shadows {
        if shadow.inset {
            continue;
        }
        outset = outset
            .max(shadow.offset_x.abs() + shadow.blur + shadow.spread)
            .max(shadow.offset_y.abs() + shadow.blur + shadow.spread);
    }
    for shadow in &style.text_shadows {
        outset = outset
            .max(shadow.offset_x.abs() + shadow.blur)
            .max(shadow.offset_y.abs() + shadow.blur);
    }
    for filter in &style.filters {
        if let FilterFunction::Blur(radius) = filter {
            // The box blur samples neighbors this far out.
            outset = outset.max(radius.max(0.0));
        }
    }
    outset
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{InteractionState, Size, build_page_full};

    const VIEWPORT: Size = Size {
        width: 800.0,
        height: 600.0,
    };

    fn find_tag(page: &crate::Page, tag: &str) -> NodeId {
        page.document
            .descendants(page.document.root())
            .find(|id| {
                page.document
                    .element(*id)
                    .is_some_and(|element| element.tag_name == tag)
            })
            .unwrap_or_else(|| panic!("no <{tag}>"))
    }

    #[test]
    fn unchanged_styles_mean_no_damage() {
        let page = build_page_full(
            "<style>a:hover { color: #ff0000; }</style><a href='#'>link</a>",
            VIEWPORT,
            &crate::HeuristicMeasurer,
            None,
        );
        let interaction = InteractionState::new(&page.document, None, None, None);
        let styles = crate::style::compute_styles_interactive(
            &page.document,
            &page.stylesheet,
            &interaction,
        );
        assert_eq!(
            paint_damage(&page.document, &page.layout, &page.styles, &styles),
            None
        );
    }

    #[test]
    fn hover_damage_is_the_hovered_box() {
        let page = build_page_full(
            "<style>a { display: block; width: 120px; height: 20px; }\
                    a:hover { color: #ff0000; }</style>\
             <div style='margin: 50px;'><a href='#'>link</a></div>",
            VIEWPORT,
            &crate::HeuristicMeasurer,
            None,
        );
        let anchor = find_tag(&page, "a");
        let interaction = InteractionState::new(&page.document, Some(anchor), None, None);
        let styles = crate::style::compute_styles_interactive(
            &page.document,
            &page.stylesheet,
            &interaction,
        );
        let damage = paint_damage(&page.document, &page.layout, &page.styles, &styles)
            .expect("color change damages");
        let anchor_box = page.layout.find_by_node(anchor).unwrap().border_box();
        assert_eq!(damage, anchor_box);
    }

    #[test]
    fn damage_grows_to_cover_shadow_spill() {
        let page = build_page_full(
            "<style>div { box-shadow: 10px 20px 5px #000; width: 100px; height: 40px; }\
                    div:hover { background-color: #ff0000; }</style>\
             <div></div>",
            VIEWPORT,
            &crate::HeuristicMeasurer,
            None,
        );
        let div = find_tag(&page, "div");
        let interaction = InteractionState::new(&page.document, Some(div), None, None);
        let styles = crate::style::compute_styles_interactive(
            &page.document,
            &page.stylesheet,
            &interaction,
        );
        let damage = paint_damage(&page.document, &page.layout, &page.styles, &styles)
            .expect("background change damages");
        let box_rect = page.layout.find_by_node(div).unwrap().border_box();
        // max(|10| + 5, |20| + 5) = 25px of shadow spill on every side.
        let expected = box_rect.expanded_by(crate::geometry::Edges::uniform(25.0));
        assert_eq!(damage, expected);
    }

    #[test]
    fn body_background_change_damages_everything() {
        let page = build_page_full(
            "<style>body:hover { background-color: #ff0000; }</style><p>x</p>",
            VIEWPORT,
            &crate::HeuristicMeasurer,
            None,
        );
        let body = find_tag(&page, "body");
        let interaction = InteractionState::new(&page.document, Some(body), None, None);
        let styles = crate::style::compute_styles_interactive(
            &page.document,
            &page.stylesheet,
            &interaction,
        );
        let damage = paint_damage(&page.document, &page.layout, &page.styles, &styles)
            .expect("canvas background change damages");
        assert_eq!(damage, page.layout.border_box());
    }

    #[test]
    fn unrelated_hover_rule_change_unions_boxes() {
        // :hover on the div restyles the link inside it too: both boxes
        // (here nested, so the union is the div) end up in the damage.
        let page = build_page_full(
            "<style>div:hover { opacity: 0.5; } div:hover a { color: #ff0000; }</style>\
             <div style='width: 200px; height: 60px;'><a href='#'>link</a></div>",
            VIEWPORT,
            &crate::HeuristicMeasurer,
            None,
        );
        let div = find_tag(&page, "div");
        let interaction = InteractionState::new(&page.document, Some(div), None, None);
        let styles = crate::style::compute_styles_interactive(
            &page.document,
            &page.stylesheet,
            &interaction,
        );
        let damage = paint_damage(&page.document, &page.layout, &page.styles, &styles)
            .expect("hover restyle damages");
        let div_box = page.layout.find_by_node(div).unwrap().border_box();
        assert!(damage.x <= div_box.x && damage.y <= div_box.y);
        assert!(damage.x + damage.width >= div_box.x + div_box.width);
        assert!(damage.y + damage.height >= div_box.y + div_box.height);
    }
}
