//! The property registry: one row per CSS property that needs special
//! cascade or parse treatment. This is the single source of truth shared
//! by the parser (which values skip component parsing) and the engine's
//! cascade (which properties inherit) — adding a property means adding
//! one row here plus its apply logic, nothing to keep in sync.
//!
//! Properties without a row default to: not inherited, component-parsed.

/// Cascade/parse flags for one property.
pub struct PropertyMeta {
    pub name: &'static str,
    /// Inherits from the parent element per CSS.
    pub inherited: bool,
    /// Kept as raw text at parse time (component parsing would mangle
    /// the value — slashes, commas, function lists).
    pub keep_raw: bool,
}

const fn row(name: &'static str, inherited: bool, keep_raw: bool) -> PropertyMeta {
    PropertyMeta {
        name,
        inherited,
        keep_raw,
    }
}

/// Every property with non-default flags.
pub const PROPERTIES: &[PropertyMeta] = &[
    // Inherited typography and text properties.
    row("color", true, false),
    row("font-family", true, false),
    row("font-size", true, false),
    row("font-style", true, false),
    row("font-weight", true, false),
    row("letter-spacing", true, false),
    row("list-style", true, false),
    row("list-style-type", true, false),
    row("line-height", true, false),
    row("text-align", true, false),
    row("text-decoration", true, false),
    row("text-decoration-color", true, false),
    row("text-decoration-style", true, false),
    row("text-indent", true, false),
    row("text-transform", true, false),
    row("user-select", true, false),
    row("visibility", true, false),
    row("white-space", true, false),
    row("word-break", true, false),
    row("word-spacing", true, false),
    row("word-wrap", true, false),
    row("overflow-wrap", true, false),
    // Inherited per CSS: tab width, caret and accent colors, cell spacing.
    row("tab-size", true, false),
    row("caret-color", true, false),
    row("accent-color", true, false),
    // Internal carriers for `::selection` styling (treated as inherited
    // so descendants highlight consistently).
    row("::selection-background", true, false),
    row("::selection-color", true, false),
    // Inherited AND raw-kept (comma lists would not survive parsing).
    row("text-shadow", true, true),
    // Raw-kept only.
    row("animation", false, true),
    row("aspect-ratio", false, true),
    row("box-shadow", false, true),
    row("background-image", false, true),
    row("background-position", false, true),
    row("background-size", false, true),
    row("background-repeat", false, true),
    row("grid-template-columns", false, true),
    row("grid-template-rows", false, true),
    row("grid-column", false, true),
    row("grid-row", false, true),
    row("transform", false, true),
    row("transform-origin", false, true),
    row("transition", false, true),
    // Raw-kept only: function lists / multi-value forms the component
    // parser would mangle.
    row("filter", false, true),
    row("rotate", false, true),
    row("scale", false, true),
    row("translate", false, true),
    // Inherited AND raw-kept (two lengths).
    row("border-spacing", true, true),
];

/// Whether `name` inherits from the parent element.
#[must_use]
pub fn is_inherited(name: &str) -> bool {
    PROPERTIES
        .iter()
        .any(|meta| meta.inherited && meta.name == name)
}

/// Whether `name`'s value skips component parsing.
#[must_use]
pub fn keeps_raw(name: &str) -> bool {
    PROPERTIES
        .iter()
        .any(|meta| meta.keep_raw && meta.name == name)
}

/// The names of all inherited properties.
pub fn inherited() -> impl Iterator<Item = &'static str> {
    PROPERTIES
        .iter()
        .filter(|meta| meta.inherited)
        .map(|meta| meta.name)
}

/// Whether the engine reads this property at all — the question
/// `@supports` asks. Everything named here reaches a computed style;
/// anything else is parsed and then ignored, so a page is better served
/// by its fallback.
///
/// Directional longhands are matched by shape rather than listed one by
/// one: `margin-top` and `border-left-color` come from the same
/// expansion as the shorthands above them.
#[must_use]
pub fn is_supported(name: &str) -> bool {
    /// Properties the engine reads by name.
    const SUPPORTED: &[&str] = &[
        "accent-color",
        "align-content",
        "align-items",
        "align-self",
        "animation",
        "aspect-ratio",
        "background",
        "background-clip",
        "background-color",
        "background-image",
        "background-origin",
        "background-position",
        "background-repeat",
        "background-size",
        "border",
        "border-collapse",
        "border-radius",
        "border-spacing",
        "bottom",
        "box-shadow",
        "box-sizing",
        "caret-color",
        "clear",
        "color",
        "content",
        "cursor",
        "display",
        "filter",
        "flex",
        "flex-basis",
        "flex-direction",
        "flex-grow",
        "flex-shrink",
        "flex-wrap",
        "float",
        "font",
        "font-family",
        "font-size",
        "font-style",
        "font-weight",
        "gap",
        "grid-column",
        "grid-row",
        "grid-template-columns",
        "grid-template-rows",
        "height",
        "inset",
        "justify-content",
        "left",
        "letter-spacing",
        "line-height",
        "list-style",
        "list-style-type",
        "margin",
        "max-height",
        "max-width",
        "min-height",
        "min-width",
        "object-fit",
        "object-position",
        "opacity",
        "order",
        "outline",
        "outline-color",
        "outline-offset",
        "outline-style",
        "outline-width",
        "overflow",
        "overflow-wrap",
        "overflow-x",
        "overflow-y",
        "padding",
        "pointer-events",
        "position",
        "right",
        "rotate",
        "scale",
        "tab-size",
        "text-align",
        "text-decoration",
        "text-decoration-color",
        "text-decoration-style",
        "text-indent",
        "text-overflow",
        "text-shadow",
        "text-transform",
        "top",
        "transform",
        "transform-origin",
        "transition",
        "translate",
        "user-select",
        "vertical-align",
        "visibility",
        "white-space",
        "width",
        "word-break",
        "word-spacing",
        "word-wrap",
        "z-index",
    ];
    /// Longhands built from a shorthand plus a side or a corner.
    const SIDED: &[(&str, &[&str])] = &[
        (
            "margin-",
            &["top", "right", "bottom", "left", "inline", "block"],
        ),
        (
            "padding-",
            &["top", "right", "bottom", "left", "inline", "block"],
        ),
        ("inset-", &["inline", "block"]),
        (
            "border-",
            &[
                "top-width",
                "right-width",
                "bottom-width",
                "left-width",
                "top-style",
                "right-style",
                "bottom-style",
                "left-style",
                "top-color",
                "right-color",
                "bottom-color",
                "left-color",
                "top-left-radius",
                "top-right-radius",
                "bottom-left-radius",
                "bottom-right-radius",
                "top",
                "right",
                "bottom",
                "left",
                "width",
                "style",
                "color",
            ],
        ),
    ];

    let name = name.trim();
    // A custom property is always "supported": it holds whatever text
    // the page put in it.
    if name.starts_with("--") {
        return true;
    }
    if SUPPORTED.contains(&name) {
        return true;
    }
    SIDED.iter().any(|(prefix, suffixes)| {
        name.strip_prefix(prefix)
            .is_some_and(|rest| suffixes.contains(&rest))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn vertical_align_and_transition_are_not_inherited() {
        // Per CSS both are non-inherited; `transition` still keeps raw.
        assert!(!is_inherited("vertical-align"));
        assert!(!is_inherited("transition"));
        assert!(keeps_raw("transition"));
        assert!(!inherited().any(|name| name == "vertical-align" || name == "transition"));
        // Genuine inherited properties are unaffected.
        assert!(is_inherited("color"));
        assert!(is_inherited("font-size"));
    }
}
