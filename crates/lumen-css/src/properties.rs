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
