//! Selector model and parsing.
//!
//! Supported: universal `*`, tag `div`, class `.card`, id `#header`,
//! compound `div.card#x`, combinators (descendant, `>`, `+`, `~`),
//! attribute selectors (`[href]`, `[type="x"]`, `^=`, `$=`, `*=`),
//! structural pseudo-classes (`:first/last/only-child`, `:nth-child()`,
//! `:nth-last-child()`), `:not()` with a compound argument, the dynamic
//! `:link`/`:visited`/`:hover`, and selector lists (handled by the rule
//! parser). Unsupported selectors are rejected and the containing rule is
//! dropped, matching browser behavior.

/// Cascade specificity, ordered lexicographically: ids > classes > types.
///
/// The universal selector contributes nothing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Default)]
pub struct Specificity {
    pub ids: u16,
    pub classes: u16,
    pub types: u16,
}

/// How a compound connects to the compound on its right.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Combinator {
    /// Whitespace: any ancestor.
    #[default]
    Descendant,
    /// `>`: the parent.
    Child,
    /// `+`: the immediately preceding element sibling.
    NextSibling,
    /// `~`: any preceding element sibling.
    SubsequentSibling,
}

/// One `[attr]` / `[attr=value]` constraint.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AttributeSelector {
    /// Lowercase attribute name.
    pub name: String,
    pub operation: AttributeOperation,
}

/// The comparison an attribute selector performs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AttributeOperation {
    /// `[attr]` — present.
    Exists,
    /// `[attr=v]`
    Equals(String),
    /// `[attr^=v]`
    StartsWith(String),
    /// `[attr$=v]`
    EndsWith(String),
    /// `[attr*=v]`
    Contains(String),
    /// `[attr~=v]` — whitespace-separated word match.
    WordMatch(String),
    /// `[attr|=v]` — exact or `v-` prefix (language ranges).
    LangPrefix(String),
}

/// A parsed pseudo-class.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PseudoClass {
    /// Always-true (no visited state).
    Link,
    /// Always-true (no visited state).
    Visited,
    /// Matches the engine's hover chain.
    Hover,
    /// The document's root element (html).
    Root,
    FirstChild,
    LastChild,
    OnlyChild,
    /// `an+b` over the 1-based index among element siblings.
    NthChild(i32, i32),
    /// `an+b` counted from the end.
    NthLastChild(i32, i32),
    /// Negation of one compound (no combinators inside).
    Not(Box<CompoundSelector>),
    /// Matches when any listed compound matches. `:is()` takes its most
    /// specific argument's specificity; `:where()` contributes none.
    Is(Vec<CompoundSelector>),
    Where(Vec<CompoundSelector>),
    FirstOfType,
    LastOfType,
    OnlyOfType,
    NthOfType(i32, i32),
    NthLastOfType(i32, i32),
}

/// A compound selector: simple selectors that must all match one element.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct CompoundSelector {
    /// Lowercase tag name, if constrained.
    pub tag: Option<String>,
    pub id: Option<String>,
    pub classes: Vec<String>,
    pub attributes: Vec<AttributeSelector>,
    pub pseudo_classes: Vec<PseudoClass>,
    /// Supported pseudo-element (only `selection`). Rules with it style
    /// the selection overlay of the matched element, not the element.
    pub pseudo_element: Option<String>,
}

impl CompoundSelector {
    #[must_use]
    pub fn specificity(&self) -> Specificity {
        let mut specificity = Specificity {
            ids: u16::from(self.id.is_some()),
            classes: (self.classes.len() + self.attributes.len()) as u16,
            types: u16::from(self.tag.is_some()) + u16::from(self.pseudo_element.is_some()),
        };
        for pseudo in &self.pseudo_classes {
            match pseudo {
                // Per spec, :not() adds its argument's specificity.
                PseudoClass::Not(inner) => {
                    let inner = inner.specificity();
                    specificity.ids += inner.ids;
                    specificity.classes += inner.classes;
                    specificity.types += inner.types;
                }
                // :is() takes its most specific argument; :where() none.
                PseudoClass::Is(arguments) => {
                    if let Some(most) = arguments.iter().map(CompoundSelector::specificity).max() {
                        specificity.ids += most.ids;
                        specificity.classes += most.classes;
                        specificity.types += most.types;
                    }
                }
                PseudoClass::Where(_) => {}
                _ => specificity.classes += 1,
            }
        }
        specificity
    }
}

/// A complex selector: compounds joined by combinators.
///
/// `compounds` is ordered outermost first; the last entry is the subject
/// (the element the rule applies to). `combinators[i]` joins
/// `compounds[i]` to `compounds[i + 1]`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Selector {
    pub compounds: Vec<CompoundSelector>,
    pub combinators: Vec<Combinator>,
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
    let mut compounds = Vec::new();
    let mut combinators = Vec::new();
    let mut pending: Option<Combinator> = None;
    for token in tokenize_complex(source)? {
        match token {
            ComplexToken::Combinator(combinator) => {
                if compounds.is_empty() || pending.is_some() {
                    return None; // Leading or doubled combinator.
                }
                pending = Some(combinator);
            }
            ComplexToken::Compound(text) => {
                if !compounds.is_empty() {
                    combinators.push(pending.take().unwrap_or(Combinator::Descendant));
                }
                compounds.push(parse_compound(&text)?);
            }
        }
    }
    if compounds.is_empty() || pending.is_some() {
        return None; // Empty, or a trailing combinator.
    }
    // A pseudo-element is only valid on the subject.
    if compounds[..compounds.len() - 1]
        .iter()
        .any(|compound| compound.pseudo_element.is_some())
    {
        return None;
    }
    Some(Selector {
        compounds,
        combinators,
    })
}

enum ComplexToken {
    Compound(String),
    Combinator(Combinator),
}

/// Splits a complex selector into compound texts and combinators,
/// respecting `()`/`[]` nesting (so `:nth-child(2n+1)` keeps its `+`).
fn tokenize_complex(source: &str) -> Option<Vec<ComplexToken>> {
    let mut tokens = Vec::new();
    let mut current = String::new();
    let mut depth = 0usize;
    let flush = |current: &mut String, tokens: &mut Vec<ComplexToken>| {
        if !current.is_empty() {
            tokens.push(ComplexToken::Compound(std::mem::take(current)));
        }
    };
    for character in source.chars() {
        match character {
            '(' | '[' => {
                depth += 1;
                current.push(character);
            }
            ')' | ']' => {
                depth = depth.checked_sub(1)?;
                current.push(character);
            }
            _ if depth > 0 => current.push(character),
            character if character.is_whitespace() => flush(&mut current, &mut tokens),
            '>' => {
                flush(&mut current, &mut tokens);
                tokens.push(ComplexToken::Combinator(Combinator::Child));
            }
            '+' => {
                flush(&mut current, &mut tokens);
                tokens.push(ComplexToken::Combinator(Combinator::NextSibling));
            }
            '~' => {
                flush(&mut current, &mut tokens);
                tokens.push(ComplexToken::Combinator(Combinator::SubsequentSibling));
            }
            _ => current.push(character),
        }
    }
    if depth != 0 {
        return None;
    }
    flush(&mut current, &mut tokens);
    Some(tokens)
}

const SUPPORTED_PSEUDO_ELEMENTS: [&str; 3] = ["selection", "before", "after"];

/// Parses one compound selector with a character scanner.
fn parse_compound(source: &str) -> Option<CompoundSelector> {
    let mut compound = CompoundSelector::default();
    let chars: Vec<char> = source.chars().collect();
    let mut position = 0;

    let read_identifier = |position: &mut usize| -> Option<String> {
        let start = *position;
        while *position < chars.len()
            && (chars[*position].is_ascii_alphanumeric() || matches!(chars[*position], '-' | '_'))
        {
            *position += 1;
        }
        (*position > start).then(|| chars[start..*position].iter().collect())
    };

    // A tag (or `*`) is only allowed at the very start.
    if position < chars.len() {
        if chars[position] == '*' {
            position += 1;
        } else if chars[position].is_ascii_alphanumeric() {
            let tag = read_identifier(&mut position)?;
            compound.tag = Some(tag.to_ascii_lowercase());
        }
    }

    while position < chars.len() {
        match chars[position] {
            '.' => {
                position += 1;
                compound.classes.push(read_identifier(&mut position)?);
            }
            '#' => {
                position += 1;
                if compound.id.is_some() {
                    return None;
                }
                compound.id = Some(read_identifier(&mut position)?);
            }
            '[' => {
                let close = find_balanced(&chars, position, '[', ']')?;
                let inner: String = chars[position + 1..close].iter().collect();
                compound.attributes.push(parse_attribute(&inner)?);
                position = close + 1;
            }
            ':' if position + 1 < chars.len() && chars[position + 1] == ':' => {
                // Pseudo-element: must end the compound.
                position += 2;
                let name = read_identifier(&mut position)?;
                if !SUPPORTED_PSEUDO_ELEMENTS.contains(&name.as_str()) || position != chars.len() {
                    return None;
                }
                compound.pseudo_element = Some(name);
            }
            ':' => {
                position += 1;
                let name = read_identifier(&mut position)?;
                // Legacy single-colon pseudo-elements (`:before`).
                if matches!(name.as_str(), "before" | "after") {
                    if position != chars.len() {
                        return None;
                    }
                    compound.pseudo_element = Some(name);
                    continue;
                }
                let arguments = if position < chars.len() && chars[position] == '(' {
                    let close = find_balanced(&chars, position, '(', ')')?;
                    let inner: String = chars[position + 1..close].iter().collect();
                    position = close + 1;
                    Some(inner)
                } else {
                    None
                };
                compound
                    .pseudo_classes
                    .push(parse_pseudo_class(&name, arguments.as_deref())?);
            }
            _ => return None,
        }
    }

    // A bare empty compound (from `*` this is fine) must constrain
    // something or be the explicit universal selector.
    if compound == CompoundSelector::default() && source != "*" {
        return None;
    }
    Some(compound)
}

/// Index of the closing delimiter matching `chars[open]`.
fn find_balanced(chars: &[char], open: usize, opener: char, closer: char) -> Option<usize> {
    let mut depth = 0usize;
    for (index, character) in chars.iter().enumerate().skip(open) {
        if *character == opener {
            depth += 1;
        } else if *character == closer {
            depth -= 1;
            if depth == 0 {
                return Some(index);
            }
        }
    }
    None
}

/// Parses the inside of `[...]`: `name`, `name=v`, `name^=v`, `name$=v`,
/// `name*=v` (value optionally quoted).
fn parse_attribute(source: &str) -> Option<AttributeSelector> {
    let source = source.trim();
    let operator_at = source.find(['=', '^', '$', '*', '~', '|']);
    let Some(at) = operator_at else {
        let name = source.to_ascii_lowercase();
        return is_identifier(&name).then_some(AttributeSelector {
            name,
            operation: AttributeOperation::Exists,
        });
    };
    let (name, rest) = source.split_at(at);
    let name = name.trim().to_ascii_lowercase();
    if !is_identifier(&name) {
        return None;
    }
    let (operator, value) = if let Some(value) = rest.strip_prefix("^=") {
        ('^', value)
    } else if let Some(value) = rest.strip_prefix("$=") {
        ('$', value)
    } else if let Some(value) = rest.strip_prefix("*=") {
        ('*', value)
    } else if let Some(value) = rest.strip_prefix("~=") {
        ('~', value)
    } else if let Some(value) = rest.strip_prefix("|=") {
        ('|', value)
    } else if let Some(value) = rest.strip_prefix('=') {
        ('=', value)
    } else {
        return None;
    };
    let value = value.trim();
    let value = value
        .strip_prefix('"')
        .and_then(|value| value.strip_suffix('"'))
        .or_else(|| {
            value
                .strip_prefix('\'')
                .and_then(|value| value.strip_suffix('\''))
        })
        .unwrap_or(value)
        .to_string();
    let operation = match operator {
        '^' => AttributeOperation::StartsWith(value),
        '$' => AttributeOperation::EndsWith(value),
        '*' => AttributeOperation::Contains(value),
        '~' => AttributeOperation::WordMatch(value),
        '|' => AttributeOperation::LangPrefix(value),
        _ => AttributeOperation::Equals(value),
    };
    Some(AttributeSelector { name, operation })
}

fn parse_pseudo_class(name: &str, arguments: Option<&str>) -> Option<PseudoClass> {
    match (name, arguments) {
        ("link", None) => Some(PseudoClass::Link),
        ("visited", None) => Some(PseudoClass::Visited),
        ("hover", None) => Some(PseudoClass::Hover),
        ("root", None) => Some(PseudoClass::Root),
        ("first-child", None) => Some(PseudoClass::FirstChild),
        ("last-child", None) => Some(PseudoClass::LastChild),
        ("only-child", None) => Some(PseudoClass::OnlyChild),
        ("nth-child", Some(arguments)) => {
            parse_nth(arguments).map(|(a, b)| PseudoClass::NthChild(a, b))
        }
        ("nth-last-child", Some(arguments)) => {
            parse_nth(arguments).map(|(a, b)| PseudoClass::NthLastChild(a, b))
        }
        ("first-of-type", None) => Some(PseudoClass::FirstOfType),
        ("last-of-type", None) => Some(PseudoClass::LastOfType),
        ("only-of-type", None) => Some(PseudoClass::OnlyOfType),
        ("nth-of-type", Some(arguments)) => {
            parse_nth(arguments).map(|(a, b)| PseudoClass::NthOfType(a, b))
        }
        ("nth-last-of-type", Some(arguments)) => {
            parse_nth(arguments).map(|(a, b)| PseudoClass::NthLastOfType(a, b))
        }
        ("is", Some(arguments)) | ("where", Some(arguments)) => {
            let compounds: Option<Vec<CompoundSelector>> = arguments
                .split(',')
                .map(|part| parse_compound(part.trim()))
                .collect();
            let compounds = compounds?;
            if compounds.is_empty()
                || compounds
                    .iter()
                    .any(|compound| compound.pseudo_element.is_some())
            {
                return None;
            }
            Some(if name == "is" {
                PseudoClass::Is(compounds)
            } else {
                PseudoClass::Where(compounds)
            })
        }
        ("not", Some(arguments)) => {
            let inner = parse_compound(arguments.trim())?;
            // No pseudo-elements inside :not().
            if inner.pseudo_element.is_some() {
                return None;
            }
            Some(PseudoClass::Not(Box::new(inner)))
        }
        _ => None,
    }
}

/// Parses `an+b` micro-syntax: `odd`, `even`, `5`, `2n`, `2n+1`, `-n+3`, `n`.
fn parse_nth(source: &str) -> Option<(i32, i32)> {
    let source = source.trim().to_ascii_lowercase().replace(' ', "");
    match source.as_str() {
        "odd" => return Some((2, 1)),
        "even" => return Some((2, 0)),
        _ => {}
    }
    if let Some(at) = source.find('n') {
        let (a_text, b_text) = (&source[..at], &source[at + 1..]);
        let a = match a_text {
            "" | "+" => 1,
            "-" => -1,
            _ => a_text.parse().ok()?,
        };
        let b = if b_text.is_empty() {
            0
        } else {
            b_text.parse().ok()?
        };
        Some((a, b))
    } else {
        Some((0, source.parse().ok()?))
    }
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
            attributes: Vec::new(),
            pseudo_classes: Vec::new(),
            pseudo_element: None,
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
        let selector = parse_selector(".card  p").unwrap();
        assert_eq!(
            selector.compounds,
            vec![
                compound(None, None, &["card"]),
                compound(Some("p"), None, &[])
            ]
        );
        assert_eq!(selector.combinators, vec![Combinator::Descendant]);
    }

    #[test]
    fn parses_child_and_sibling_combinators() {
        let selector = parse_selector("ul > li + li ~ b").unwrap();
        assert_eq!(selector.compounds.len(), 4);
        assert_eq!(
            selector.combinators,
            vec![
                Combinator::Child,
                Combinator::NextSibling,
                Combinator::SubsequentSibling
            ]
        );
        // Whitespace around the symbol is optional.
        assert_eq!(
            parse_selector("ul>li").unwrap().combinators,
            vec![Combinator::Child]
        );
    }

    #[test]
    fn parses_attribute_selectors() {
        let selector = parse_selector("a[href]").unwrap();
        assert_eq!(
            selector.compounds[0].attributes,
            vec![AttributeSelector {
                name: "href".to_string(),
                operation: AttributeOperation::Exists,
            }]
        );
        let selector = parse_selector("input[type=\"text\"]").unwrap();
        assert_eq!(
            selector.compounds[0].attributes[0].operation,
            AttributeOperation::Equals("text".to_string())
        );
        let selector = parse_selector("a[href^='https']").unwrap();
        assert_eq!(
            selector.compounds[0].attributes[0].operation,
            AttributeOperation::StartsWith("https".to_string())
        );
        assert!(parse_selector("a[href$=.pdf]").is_some());
        assert!(parse_selector("a[href*=example]").is_some());
    }

    #[test]
    fn parses_structural_pseudo_classes() {
        assert_eq!(
            parse_selector("li:first-child").unwrap().compounds[0].pseudo_classes,
            vec![PseudoClass::FirstChild]
        );
        assert_eq!(
            parse_selector("li:nth-child(2n+1)").unwrap().compounds[0].pseudo_classes,
            vec![PseudoClass::NthChild(2, 1)]
        );
        assert_eq!(
            parse_selector("li:nth-child(odd)").unwrap().compounds[0].pseudo_classes,
            vec![PseudoClass::NthChild(2, 1)]
        );
        assert_eq!(
            parse_selector("li:nth-child(3)").unwrap().compounds[0].pseudo_classes,
            vec![PseudoClass::NthChild(0, 3)]
        );
        assert_eq!(
            parse_selector("li:nth-last-child(-n+2)").unwrap().compounds[0].pseudo_classes,
            vec![PseudoClass::NthLastChild(-1, 2)]
        );
    }

    #[test]
    fn parses_is_where_and_of_type() {
        let selector = parse_selector("p:is(.a, #b)").unwrap();
        let PseudoClass::Is(arguments) = &selector.compounds[0].pseudo_classes[0] else {
            panic!("expected :is");
        };
        assert_eq!(arguments.len(), 2);
        // :is takes its most specific argument: the id.
        assert_eq!(
            selector.specificity(),
            Specificity {
                ids: 1,
                classes: 0,
                types: 1
            }
        );
        // :where contributes nothing.
        assert_eq!(
            parse_selector("p:where(.a, #b)").unwrap().specificity(),
            Specificity {
                ids: 0,
                classes: 0,
                types: 1
            }
        );
        assert!(parse_selector("li:first-of-type").is_some());
        assert!(parse_selector("li:nth-of-type(2n)").is_some());
        assert!(parse_selector("a[rel~=nofollow]").is_some());
        assert!(parse_selector("p[lang|=en]").is_some());
    }

    #[test]
    fn parses_not_with_compound_argument() {
        let selector = parse_selector("p:not(.muted)").unwrap();
        let PseudoClass::Not(inner) = &selector.compounds[0].pseudo_classes[0] else {
            panic!("expected :not");
        };
        assert_eq!(inner.classes, vec!["muted"]);
        // :not() takes its argument's specificity: type + class.
        assert_eq!(
            selector.specificity(),
            Specificity {
                ids: 0,
                classes: 1,
                types: 1
            }
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
    fn supported_pseudo_classes_match_and_add_specificity() {
        let selector = parse_selector("a:link").unwrap();
        assert_eq!(selector.compounds[0].tag.as_deref(), Some("a"));
        assert_eq!(
            selector.compounds[0].pseudo_classes,
            vec![PseudoClass::Link]
        );
        assert_eq!(
            selector.specificity(),
            Specificity {
                ids: 0,
                classes: 1,
                types: 1
            }
        );
        assert!(parse_selector("a:visited").is_some());
        assert!(parse_selector("a:hover").is_some());
        assert!(parse_selector(".btn:hover").is_some());
    }

    #[test]
    fn selection_pseudo_element_parses_with_type_specificity() {
        let selector = parse_selector("p::selection").unwrap();
        assert_eq!(
            selector.compounds[0].pseudo_element.as_deref(),
            Some("selection")
        );
        assert_eq!(
            selector.specificity(),
            Specificity {
                ids: 0,
                classes: 0,
                types: 2
            }
        );
    }

    #[test]
    fn before_and_after_parse_in_both_colon_forms() {
        for source in ["p::before", "p:before", "p::after", "p:after"] {
            let selector = parse_selector(source).unwrap();
            let pseudo = selector.compounds[0].pseudo_element.as_deref().unwrap();
            assert!(matches!(pseudo, "before" | "after"), "{source}");
        }
    }

    #[test]
    fn rejects_unsupported_selectors() {
        assert!(parse_selector("").is_none());
        // Bare pseudo-classes are valid selectors now.
        assert!(parse_selector(":link").is_some());
        assert!(parse_selector(":focus").is_none()); // Unsupported pseudo.
        assert!(parse_selector(".").is_none());
        assert!(parse_selector("#").is_none());
        assert!(parse_selector("div..x").is_none());
        // Bare ::selection is the universal selector's selection.
        assert!(parse_selector("::selection").is_some());
        assert!(parse_selector("p >").is_none()); // Trailing combinator.
        assert!(parse_selector("> p").is_none()); // Leading combinator.
        assert!(parse_selector("a > > b").is_none()); // Doubled.
        assert!(parse_selector("p:nth-child(x)").is_none());
        assert!(parse_selector("p:has(a)").is_none());
        assert!(parse_selector("p::selection span").is_none()); // Non-subject pseudo-element.
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
            parse_selector("a[href]:first-child").unwrap().specificity(),
            Specificity {
                ids: 0,
                classes: 2,
                types: 1
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
