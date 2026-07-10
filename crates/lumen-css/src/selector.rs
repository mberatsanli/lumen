//! Selector model and parsing.
//!
//! Supported: universal `*`, tag `div`, class `.card`, id `#header`,
//! compound `div.card#x`, descendant combinator `.card p`, and selector
//! lists (handled by the rule parser). Child/sibling combinators and
//! pseudo-classes are not supported; such selectors are rejected and the
//! containing rule is dropped, matching browser behavior.

/// Cascade specificity, ordered lexicographically: ids > classes > types.
///
/// The universal selector contributes nothing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Default)]
pub struct Specificity {
    pub ids: u16,
    pub classes: u16,
    pub types: u16,
}

/// A compound selector: simple selectors that must all match one element.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct CompoundSelector {
    /// Lowercase tag name, if constrained.
    pub tag: Option<String>,
    pub id: Option<String>,
    pub classes: Vec<String>,
}

impl CompoundSelector {
    #[must_use]
    pub fn specificity(&self) -> Specificity {
        Specificity {
            ids: u16::from(self.id.is_some()),
            classes: self.classes.len() as u16,
            types: u16::from(self.tag.is_some()),
        }
    }
}

/// A complex selector: compounds joined by descendant combinators.
///
/// `compounds` is ordered outermost ancestor first; the last entry is the
/// subject (the element the rule applies to). A lone compound has one entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Selector {
    pub compounds: Vec<CompoundSelector>,
}

impl Selector {
    #[must_use]
    pub fn specificity(&self) -> Specificity {
        self.compounds
            .iter()
            .map(CompoundSelector::specificity)
            .fold(Specificity::default(), |sum, next| Specificity {
                ids: sum.ids + next.ids,
                classes: sum.classes + next.classes,
                types: sum.types + next.types,
            })
    }

    /// The compound the matched element itself must satisfy.
    #[must_use]
    pub fn subject(&self) -> &CompoundSelector {
        // Invariant: parse_selector never produces an empty compound list.
        &self.compounds[self.compounds.len() - 1]
    }
}

/// Parses one complex selector (no commas). Returns `None` if any part is
/// unsupported or malformed.
#[must_use]
pub fn parse_selector(source: &str) -> Option<Selector> {
    let compounds: Option<Vec<CompoundSelector>> =
        source.split_whitespace().map(parse_compound).collect();
    let compounds = compounds?;
    if compounds.is_empty() {
        return None;
    }
    Some(Selector { compounds })
}

fn parse_compound(source: &str) -> Option<CompoundSelector> {
    let mut compound = CompoundSelector::default();
    let mut rest = source;

    if rest == "*" {
        return Some(compound);
    }
    if !rest.starts_with(['.', '#']) {
        let end = rest.find(['.', '#']).unwrap_or(rest.len());
        let (tag, remainder) = rest.split_at(end);
        if !is_identifier(tag) {
            return None;
        }
        compound.tag = Some(tag.to_ascii_lowercase());
        rest = remainder;
    }

    while !rest.is_empty() {
        let (kind, remainder) = rest.split_at(1);
        let end = remainder.find(['.', '#']).unwrap_or(remainder.len());
        let (name, next) = remainder.split_at(end);
        if !is_identifier(name) {
            return None;
        }
        match kind {
            "." => compound.classes.push(name.to_string()),
            "#" => {
                if compound.id.is_some() {
                    return None;
                }
                compound.id = Some(name.to_string());
            }
            _ => return None,
        }
        rest = next;
    }

    Some(compound)
}

fn is_identifier(source: &str) -> bool {
    !source.is_empty()
        && source
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || matches!(character, '-' | '_'))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn compound(tag: Option<&str>, id: Option<&str>, classes: &[&str]) -> CompoundSelector {
        CompoundSelector {
            tag: tag.map(str::to_string),
            id: id.map(str::to_string),
            classes: classes.iter().map(|class| (*class).to_string()).collect(),
        }
    }

    #[test]
    fn parses_simple_selectors() {
        assert_eq!(
            parse_selector("div").unwrap().compounds,
            vec![compound(Some("div"), None, &[])]
        );
        assert_eq!(
            parse_selector(".card").unwrap().compounds,
            vec![compound(None, None, &["card"])]
        );
        assert_eq!(
            parse_selector("#header").unwrap().compounds,
            vec![compound(None, Some("header"), &[])]
        );
        assert_eq!(
            parse_selector("*").unwrap().compounds,
            vec![compound(None, None, &[])]
        );
    }

    #[test]
    fn parses_compound_selector() {
        assert_eq!(
            parse_selector("div.card.active#main").unwrap().compounds,
            vec![compound(Some("div"), Some("main"), &["card", "active"])]
        );
    }

    #[test]
    fn parses_descendant_selector() {
        assert_eq!(
            parse_selector(".card  p").unwrap().compounds,
            vec![
                compound(None, None, &["card"]),
                compound(Some("p"), None, &[])
            ]
        );
    }

    #[test]
    fn uppercase_tag_is_normalized() {
        assert_eq!(
            parse_selector("DIV").unwrap().compounds,
            vec![compound(Some("div"), None, &[])]
        );
    }

    #[test]
    fn rejects_unsupported_selectors() {
        assert!(parse_selector("").is_none());
        assert!(parse_selector("p > a").is_none());
        assert!(parse_selector("a:hover").is_none());
        assert!(parse_selector(".").is_none());
        assert!(parse_selector("#").is_none());
        assert!(parse_selector("div..x").is_none());
        assert!(parse_selector("[href]").is_none());
    }

    #[test]
    fn specificity_is_structural() {
        assert_eq!(
            parse_selector("#a").unwrap().specificity(),
            Specificity {
                ids: 1,
                classes: 0,
                types: 0
            }
        );
        assert_eq!(
            parse_selector("div.card p").unwrap().specificity(),
            Specificity {
                ids: 0,
                classes: 1,
                types: 2
            }
        );
        assert_eq!(
            parse_selector("*").unwrap().specificity(),
            Specificity::default()
        );
    }

    #[test]
    fn id_beats_any_number_of_classes() {
        let id = parse_selector("#a").unwrap().specificity();
        let classes = parse_selector(".a.b.c.d.e").unwrap().specificity();
        assert!(id > classes);
    }

    #[test]
    fn class_beats_any_number_of_types() {
        let class = parse_selector(".a").unwrap().specificity();
        let types = parse_selector("html body div p").unwrap().specificity();
        assert!(class > types);
    }
}
