//! Selector parsing on top of `parcel_selectors`.
//!
//! The selector *model* is parcel's own: rules store owned
//! (`'static`) [`Selector`]s and the engine matches them with
//! `parcel_selectors::matching`. This module only defines the
//! implementation glue parcel needs: the [`Selectors`] `SelectorImpl`
//! (string-based, no namespaces), our [`PseudoClass`] set (the dynamic
//! and form-state pseudo-classes the engine can answer) and the
//! [`PseudoElement`] set (`::before`/`::after`/`::selection`).
//!
//! Specificity is parcel's `u32` (`ids << 20 | classes << 10 | types`);
//! structural pseudo-classes (`:first-child`, `:nth-*`, `:empty`,
//! `:root`), `:is()`/`:where()`/`:not()` at full depth and `:has()`
//! parsing all come from parcel. Unsupported selectors are rejected and
//! the containing rule is dropped, matching browser behavior.

use cssparser::{CowRcStr, ParseError, Parser as CssParser, ParserInput, SourceLocation, ToCss};
use parcel_selectors::parser::{
    NestingRequirement, SelectorList as ParcelSelectorList, SelectorParseErrorKind,
};
use std::fmt;

/// A parsed complex selector, owned (`'static`) so rules can store it.
pub type Selector = parcel_selectors::parser::Selector<'static, Selectors>;

/// An owned identifier/string inside a parsed selector (tag, class, id,
/// attribute name/value). parcel's `SelectorImpl` bounds require
/// `ToCss` + `From<CowRcStr>`, which `String` does not implement.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Default)]
pub struct Ident(String);

impl Ident {
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl AsRef<str> for Ident {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

impl std::borrow::Borrow<str> for Ident {
    fn borrow(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for Ident {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl ToCss for Ident {
    fn to_css<W>(&self, dest: &mut W) -> fmt::Result
    where
        W: fmt::Write,
    {
        dest.write_str(&self.0)
    }
}

impl<'i> From<CowRcStr<'i>> for Ident {
    fn from(value: CowRcStr<'i>) -> Self {
        Self(value.to_string())
    }
}

impl<'any> static_self::IntoOwned<'any> for Ident {
    type Owned = Self;

    fn into_owned(self) -> Self {
        self
    }
}

/// The `SelectorImpl` the whole engine parses and matches with: plain
/// owned strings, no namespaces, our pseudo-class/pseudo-element sets.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Selectors;

impl<'i> parcel_selectors::parser::SelectorImpl<'i> for Selectors {
    type ExtraMatchingData = ();
    type AttrValue = Ident;
    type Identifier = Ident;
    type LocalName = Ident;
    type NamespaceUrl = Ident;
    type NamespacePrefix = Ident;
    type BorrowedNamespaceUrl = str;
    type BorrowedLocalName = str;
    type NonTSPseudoClass = PseudoClass;
    type VendorPrefix = Ident;
    type PseudoElement = PseudoElement;
}

impl<'any> static_self::IntoOwned<'any> for Selectors {
    type Owned = Selectors;

    fn into_owned(self) -> Selectors {
        self
    }
}

/// The non-tree-structural pseudo-classes the engine can match.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PseudoClass {
    /// Matches the engine's hover chain.
    Hover,
    /// Matches while the pointer is pressed on the element (chain).
    Active,
    /// Matches the focused element.
    Focus,
    /// Matches the focused element's chain.
    FocusWithin,
    /// Unvisited link (element with `href` not in the visited set).
    Link,
    /// Visited link (engine's visited set).
    Visited,
    /// Form control without a `disabled` attribute.
    Enabled,
    /// Form control with a `disabled` attribute.
    Disabled,
    /// Checked checkbox/radio (state supplied by the shell).
    Checked,
}

impl<'any> static_self::IntoOwned<'any> for PseudoClass {
    type Owned = Self;

    fn into_owned(self) -> Self {
        self
    }
}

impl ToCss for PseudoClass {
    fn to_css<W>(&self, dest: &mut W) -> fmt::Result
    where
        W: fmt::Write,
    {
        dest.write_str(match self {
            Self::Hover => "hover",
            Self::Active => "active",
            Self::Focus => "focus",
            Self::FocusWithin => "focus-within",
            Self::Link => "link",
            Self::Visited => "visited",
            Self::Enabled => "enabled",
            Self::Disabled => "disabled",
            Self::Checked => "checked",
        })
    }
}

impl<'i> parcel_selectors::parser::NonTSPseudoClass<'i> for PseudoClass {
    type Impl = Selectors;

    fn is_active_or_hover(&self) -> bool {
        matches!(self, Self::Active | Self::Hover)
    }

    fn is_user_action_state(&self) -> bool {
        matches!(
            self,
            Self::Active | Self::Hover | Self::Focus | Self::FocusWithin
        )
    }
}

/// The pseudo-elements the engine generates/styles.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PseudoElement {
    Before,
    After,
    /// Rules with `::selection` style the selection overlay of the
    /// matched element, not the element itself.
    Selection,
    /// `::first-letter`: styles the first letter of the matched block.
    FirstLetter,
    /// `::first-line`: styles the first formatted line of the block.
    FirstLine,
}

impl<'any> static_self::IntoOwned<'any> for PseudoElement {
    type Owned = Self;

    fn into_owned(self) -> Self {
        self
    }
}

impl ToCss for PseudoElement {
    fn to_css<W>(&self, dest: &mut W) -> fmt::Result
    where
        W: fmt::Write,
    {
        dest.write_str(match self {
            Self::Before => "::before",
            Self::After => "::after",
            Self::Selection => "::selection",
            Self::FirstLetter => "::first-letter",
            Self::FirstLine => "::first-line",
        })
    }
}

impl<'i> parcel_selectors::parser::PseudoElement<'i> for PseudoElement {
    type Impl = Selectors;
}

/// The error type parcel's `Parser` trait requires.
#[derive(Debug)]
#[allow(dead_code)] // The field is only carried, never read.
pub struct SelectorError<'i>(SelectorParseErrorKind<'i>);

impl<'i> From<SelectorParseErrorKind<'i>> for SelectorError<'i> {
    fn from(kind: SelectorParseErrorKind<'i>) -> Self {
        Self(kind)
    }
}

/// Teaches parcel which pseudo-classes/-elements exist in Lumen.
struct LumenSelectorParser;

impl<'i> parcel_selectors::parser::Parser<'i> for LumenSelectorParser {
    type Impl = Selectors;
    type Error = SelectorError<'i>;

    fn parse_is_and_where(&self) -> bool {
        true
    }

    fn parse_non_ts_pseudo_class(
        &self,
        location: SourceLocation,
        name: CowRcStr<'i>,
    ) -> Result<PseudoClass, ParseError<'i, Self::Error>> {
        let pseudo = match () {
            () if name.eq_ignore_ascii_case("hover") => PseudoClass::Hover,
            () if name.eq_ignore_ascii_case("active") => PseudoClass::Active,
            // :focus-visible is approximated by :focus.
            () if name.eq_ignore_ascii_case("focus")
                || name.eq_ignore_ascii_case("focus-visible") =>
            {
                PseudoClass::Focus
            }
            () if name.eq_ignore_ascii_case("focus-within") => PseudoClass::FocusWithin,
            () if name.eq_ignore_ascii_case("link") => PseudoClass::Link,
            () if name.eq_ignore_ascii_case("visited") => PseudoClass::Visited,
            () if name.eq_ignore_ascii_case("enabled") => PseudoClass::Enabled,
            () if name.eq_ignore_ascii_case("disabled") => PseudoClass::Disabled,
            () if name.eq_ignore_ascii_case("checked") => PseudoClass::Checked,
            () => {
                return Err(
                    location.new_custom_error(SelectorParseErrorKind::UnsupportedPseudoClass(name))
                );
            }
        };
        Ok(pseudo)
    }

    fn parse_pseudo_element(
        &self,
        location: SourceLocation,
        name: CowRcStr<'i>,
    ) -> Result<PseudoElement, ParseError<'i, Self::Error>> {
        let pseudo = match () {
            () if name.eq_ignore_ascii_case("before") => PseudoElement::Before,
            () if name.eq_ignore_ascii_case("after") => PseudoElement::After,
            () if name.eq_ignore_ascii_case("selection") => PseudoElement::Selection,
            () if name.eq_ignore_ascii_case("first-letter") => PseudoElement::FirstLetter,
            () if name.eq_ignore_ascii_case("first-line") => PseudoElement::FirstLine,
            () => {
                return Err(location
                    .new_custom_error(SelectorParseErrorKind::UnsupportedPseudoElement(name)));
            }
        };
        Ok(pseudo)
    }
}

/// Parses one complex selector (no commas). Returns `None` if any part is
/// unsupported or malformed.
#[must_use]
pub fn parse_selector(source: &str) -> Option<Selector> {
    let selectors = parse_selector_list(source)?;
    match selectors.len() {
        1 => selectors.into_iter().next(),
        _ => None,
    }
}

/// Parses a comma-separated selector list with the parcel selector parser.
/// Returns `None` when any selector in the list is malformed or
/// unsupported — the containing rule is then dropped whole, matching
/// browser behavior for invalid lists.
pub(crate) fn parse_selector_list(source: &str) -> Option<Vec<Selector>> {
    let mut input = ParserInput::new(source);
    let mut input = CssParser::new(&mut input);
    let list = ParcelSelectorList::parse(
        &LumenSelectorParser,
        &mut input,
        parcel_selectors::parser::ParseErrorRecovery::DiscardList,
        NestingRequirement::None,
    )
    .ok()?;
    input.expect_exhausted().ok()?;
    // Every string inside is owned, so the parse-time lifetime is
    // phantom; into_owned() restates the list as 'static safely.
    let list: ParcelSelectorList<'static, Selectors> = static_self::IntoOwned::into_owned(list);
    Some(list.0.into_vec())
}

#[cfg(test)]
mod tests {
    use super::*;
    use parcel_selectors::parser::{Component, SelectorImpl};

    fn specificity(source: &str) -> u32 {
        parse_selector(source).unwrap().specificity()
    }

    fn to_css(selector: &Selector) -> String {
        let list = ParcelSelectorList::from(selector.clone());
        let mut out = String::new();
        Selectors::to_css(&list, &mut out).unwrap();
        out
    }

    #[test]
    fn parses_simple_selectors() {
        assert_eq!(to_css(&parse_selector("div").unwrap()), "div");
        assert_eq!(to_css(&parse_selector(".card").unwrap()), ".card");
        assert_eq!(to_css(&parse_selector("#header").unwrap()), "#header");
        assert_eq!(to_css(&parse_selector("*").unwrap()), "*");
        assert_eq!(
            to_css(&parse_selector("div.card#main").unwrap()),
            "div.card#main"
        );
        assert_eq!(
            to_css(&parse_selector("ul > li + li ~ b").unwrap()),
            "ul > li + li ~ b"
        );
        assert_eq!(to_css(&parse_selector(".card  p").unwrap()), ".card p");
    }

    #[test]
    fn uppercase_tag_is_normalized() {
        // Matching lowercases for HTML elements; serialization keeps the
        // parsed name.
        let selector = parse_selector("DIV").unwrap();
        assert!(selector.iter_raw_match_order().any(|component| matches!(
            component,
            Component::LocalName(name) if name.lower_name.as_str() == "div"
        )));
    }

    #[test]
    fn parses_attribute_selectors() {
        assert!(parse_selector("a[href]").is_some());
        assert!(parse_selector("input[type=\"text\"]").is_some());
        assert!(parse_selector("a[href^='https']").is_some());
        assert!(parse_selector("a[href$=\".pdf\"]").is_some());
        assert!(parse_selector("a[href*=example]").is_some());
        assert!(parse_selector("a[rel~=nofollow]").is_some());
        assert!(parse_selector("p[lang|=en]").is_some());
        // The `i` case-insensitivity flag is honored by the matcher now.
        assert!(parse_selector("a[href=\"X\" i]").is_some());
    }

    #[test]
    fn parses_pseudo_classes() {
        assert!(parse_selector("li:first-child").is_some());
        assert!(parse_selector("li:nth-child(2n+1)").is_some());
        assert!(parse_selector("li:nth-last-child(-n+2)").is_some());
        assert!(parse_selector("li:first-of-type").is_some());
        assert!(parse_selector("li:nth-of-type(2n)").is_some());
        assert!(parse_selector(":link").is_some());
        assert!(parse_selector("a:visited").is_some());
        assert!(parse_selector("a:hover").is_some());
        assert!(parse_selector("input:checked").is_some());
        assert!(parse_selector("input:enabled").is_some());
        assert!(parse_selector("input:disabled").is_some());
        assert!(parse_selector("p:empty").is_some());
        assert!(parse_selector(":root").is_some());
    }

    #[test]
    fn parses_is_where_not_at_full_depth() {
        // Multi-argument :not() and complex arguments are supported now.
        assert!(parse_selector("p:not(.muted)").is_some());
        assert!(parse_selector("p:not(.a, #b)").is_some());
        assert!(parse_selector("p:not(div > span)").is_some());
        assert!(parse_selector("p:is(.a, #b)").is_some());
        assert!(parse_selector("p:where(.a, #b)").is_some());
        // Deep nesting: parcel manages its own recursion limit.
        let deep = format!("p{}.a{}", ":not(".repeat(8), ")".repeat(8));
        assert!(parse_selector(&deep).is_some());
    }

    #[test]
    fn parses_has() {
        assert!(parse_selector("div:has(> img)").is_some());
        assert!(parse_selector("div:has(img)").is_some());
    }

    #[test]
    fn pseudo_elements_parse_in_both_colon_forms() {
        for (source, expected) in [
            ("p::before", PseudoElement::Before),
            ("p:before", PseudoElement::Before),
            ("p::after", PseudoElement::After),
            ("p:after", PseudoElement::After),
            ("p::selection", PseudoElement::Selection),
            ("::selection", PseudoElement::Selection),
            ("p::first-letter", PseudoElement::FirstLetter),
            ("p:first-letter", PseudoElement::FirstLetter),
            ("p::first-line", PseudoElement::FirstLine),
            ("p:first-line", PseudoElement::FirstLine),
        ] {
            let selector = parse_selector(source).unwrap();
            assert_eq!(selector.pseudo_element(), Some(&expected), "{source}");
        }
    }

    #[test]
    fn rejects_unsupported_selectors() {
        assert!(parse_selector("").is_none());
        assert!(parse_selector(":blur").is_none()); // Unsupported pseudo.
        assert!(parse_selector(".").is_none());
        assert!(parse_selector("#").is_none());
        assert!(parse_selector("div..x").is_none());
        assert!(parse_selector("p >").is_none()); // Trailing combinator.
        assert!(parse_selector("> p").is_none()); // Leading combinator.
        assert!(parse_selector("a > > b").is_none()); // Doubled.
        assert!(parse_selector("p:nth-child(x)").is_none());
        assert!(parse_selector("p::selection span").is_none()); // Non-subject pseudo-element.
    }

    #[test]
    fn specificity_is_layered() {
        let id = 1 << 20;
        let class = 1 << 10;
        assert_eq!(specificity("#a"), id);
        assert_eq!(specificity("div.card p"), class + 2);
        assert_eq!(specificity("a[href]:first-child"), 2 * class + 1);
        assert_eq!(specificity("*"), 0);
        // :is() takes its most specific argument; :where() none;
        // :not() its argument's.
        assert_eq!(specificity("p:is(.a, #b)"), id + 1);
        assert_eq!(specificity("p:where(.a, #b)"), 1);
        assert_eq!(specificity("p:not(.muted)"), class + 1);
        assert_eq!(specificity("a:link"), class + 1);
        // Pseudo-elements count as element selectors.
        assert_eq!(specificity("p::selection"), 2);
    }

    #[test]
    fn id_beats_any_number_of_classes() {
        assert!(specificity("#a") > specificity(".a.b.c.d.e"));
        assert!(specificity(".a") > specificity("html body div p"));
    }

    #[test]
    fn huge_selector_specificity_does_not_overflow() {
        // Parcel clamps each specificity layer at 10 bits.
        let selector = parse_selector(&format!("p{}", ":hover".repeat(70_000))).unwrap();
        assert_eq!(selector.specificity(), (1023 << 10) | 1);
    }
}
