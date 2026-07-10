//! Stylesheet parsing: rules, declarations and shorthand expansion.

use crate::selector::{Selector, parse_selector};
use crate::value::{CssValue, split_components};

/// A single longhand `name: value` pair. Shorthands are expanded at parse
/// time, so consumers never see `margin`, only `margin-top` etc.
#[derive(Debug, Clone, PartialEq)]
pub struct Declaration {
    /// Lowercase property name.
    pub name: String,
    pub value: CssValue,
}

/// One rule: a selector list and its declarations.
#[derive(Debug, Clone, PartialEq)]
pub struct Rule {
    pub selectors: Vec<Selector>,
    pub declarations: Vec<Declaration>,
    /// Position of the rule in the stylesheet, for cascade tie-breaking.
    pub source_order: usize,
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct Stylesheet {
    pub rules: Vec<Rule>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CssError {
    /// A `{` without a matching `}`.
    UnterminatedRule,
}

impl std::fmt::Display for CssError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UnterminatedRule => write!(formatter, "unterminated CSS rule"),
        }
    }
}

impl std::error::Error for CssError {}

/// Parses a stylesheet.
///
/// Lenient where browsers are lenient: comments are skipped, malformed
/// declarations and unsupported selectors are dropped (a rule whose selector
/// list contains any invalid selector is dropped entirely). The only hard
/// error is an unterminated `{` block.
pub fn parse_stylesheet(source: &str) -> Result<Stylesheet, CssError> {
    let source = strip_comments(source);
    let mut rules = Vec::new();
    let mut rest = source.as_str();
    let mut source_order = 0;

    while let Some(open) = rest.find('{') {
        let selector_source = rest[..open].trim();
        let after_open = &rest[open + 1..];
        let Some(close) = after_open.find('}') else {
            return Err(CssError::UnterminatedRule);
        };
        let declaration_source = &after_open[..close];
        rest = &after_open[close + 1..];

        let selectors: Option<Vec<Selector>> = selector_source
            .split(',')
            .map(str::trim)
            .filter(|selector| !selector.is_empty())
            .map(parse_selector)
            .collect();

        // A rule with an empty or invalid selector list is dropped.
        let Some(selectors) = selectors else { continue };
        if selectors.is_empty() {
            continue;
        }

        rules.push(Rule {
            selectors,
            declarations: parse_declarations(declaration_source),
            source_order,
        });
        source_order += 1;
    }

    Ok(Stylesheet { rules })
}

/// Parses a `;`-separated declaration list (also used for inline `style=`
/// attributes). Malformed entries are skipped; shorthands are expanded.
#[must_use]
pub fn parse_declarations(source: &str) -> Vec<Declaration> {
    let source = strip_comments(source);
    let mut declarations = Vec::new();
    for raw in source.split(';') {
        let Some((name, value)) = raw.split_once(':') else {
            continue; // Tolerates empty segments and extra semicolons.
        };
        let name = name.trim().to_ascii_lowercase();
        if name.is_empty() {
            continue;
        }
        let components: Vec<CssValue> = split_components(value)
            .iter()
            .filter_map(|component| CssValue::parse_component(component))
            .collect();
        if components.is_empty() {
            continue;
        }
        expand_declaration(&name, components, &mut declarations);
    }
    declarations
}

/// Expands `margin`/`padding` shorthands into longhands; every other
/// property keeps its first component (multi-value forms are unsupported).
fn expand_declaration(name: &str, mut components: Vec<CssValue>, output: &mut Vec<Declaration>) {
    match name {
        "margin" | "padding" => {
            let Some(edges) = edge_values(&components) else {
                return;
            };
            for (suffix, value) in ["top", "right", "bottom", "left"].iter().zip(edges) {
                output.push(Declaration {
                    name: format!("{name}-{suffix}"),
                    value,
                });
            }
        }
        _ => output.push(Declaration {
            name: name.to_string(),
            value: components.swap_remove(0),
        }),
    }
}

/// CSS 1-to-4 value expansion: top, right, bottom, left.
fn edge_values(components: &[CssValue]) -> Option<[CssValue; 4]> {
    let get = |index: usize| components[index].clone();
    match components.len() {
        1 => Some([get(0), get(0), get(0), get(0)]),
        2 => Some([get(0), get(1), get(0), get(1)]),
        3 => Some([get(0), get(1), get(2), get(1)]),
        4 => Some([get(0), get(1), get(2), get(3)]),
        _ => None,
    }
}

fn strip_comments(source: &str) -> String {
    let mut output = String::new();
    let mut rest = source;
    while let Some(start) = rest.find("/*") {
        output.push_str(&rest[..start]);
        let after_start = &rest[start + 2..];
        if let Some(end) = after_start.find("*/") {
            rest = &after_start[end + 2..];
        } else {
            return output;
        }
    }
    output.push_str(rest);
    output
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::value::{Color, Unit};

    fn px(value: f32) -> CssValue {
        CssValue::Length(value, Unit::Px)
    }

    #[test]
    fn parses_selector_list_and_declarations() {
        let sheet = parse_stylesheet(".card, #main { width: 400px; color: #222; }").unwrap();
        assert_eq!(sheet.rules.len(), 1);
        assert_eq!(sheet.rules[0].selectors.len(), 2);
        assert_eq!(
            sheet.rules[0].declarations,
            vec![
                Declaration {
                    name: "width".to_string(),
                    value: px(400.0)
                },
                Declaration {
                    name: "color".to_string(),
                    value: CssValue::Color(Color::rgb(0x22, 0x22, 0x22))
                },
            ]
        );
    }

    #[test]
    fn expands_margin_shorthand() {
        let cases: &[(&str, [f32; 4])] = &[
            ("margin: 8px", [8.0, 8.0, 8.0, 8.0]),
            ("margin: 8px 16px", [8.0, 16.0, 8.0, 16.0]),
            ("margin: 1px 2px 3px", [1.0, 2.0, 3.0, 2.0]),
            ("margin: 1px 2px 3px 4px", [1.0, 2.0, 3.0, 4.0]),
        ];
        for (source, [top, right, bottom, left]) in cases {
            let declarations = parse_declarations(source);
            let expect = |name: &str, value: f32| {
                assert!(
                    declarations
                        .iter()
                        .any(|declaration| declaration.name == name
                            && declaration.value == px(value)),
                    "{source}: expected {name}={value}px in {declarations:?}"
                );
            };
            expect("margin-top", *top);
            expect("margin-right", *right);
            expect("margin-bottom", *bottom);
            expect("margin-left", *left);
        }
    }

    #[test]
    fn longhand_overrides_survive_in_source_order() {
        let declarations = parse_declarations("margin: 8px; margin-left: 2px");
        let names: Vec<&str> = declarations
            .iter()
            .map(|declaration| declaration.name.as_str())
            .collect();
        assert_eq!(
            names,
            vec![
                "margin-top",
                "margin-right",
                "margin-bottom",
                "margin-left",
                "margin-left"
            ]
        );
    }

    #[test]
    fn skips_malformed_declarations_and_extra_semicolons() {
        let declarations =
            parse_declarations(";; color: red; oops; width: ; height: 10px; x: url(a);");
        assert_eq!(declarations.len(), 2);
        assert_eq!(declarations[0].name, "color");
        assert_eq!(declarations[1].name, "height");
    }

    #[test]
    fn drops_rule_with_invalid_selector() {
        let sheet =
            parse_stylesheet("p > a { color: red; } h1 { color: blue; } a:hover { color: red; }")
                .unwrap();
        assert_eq!(sheet.rules.len(), 1);
        assert_eq!(sheet.rules[0].source_order, 0);
    }

    #[test]
    fn skips_comments() {
        let sheet = parse_stylesheet("/* x */ p { /* y */ color: red; }").unwrap();
        assert_eq!(sheet.rules.len(), 1);
        assert_eq!(sheet.rules[0].declarations.len(), 1);
    }

    #[test]
    fn unterminated_rule_is_an_error() {
        assert_eq!(
            parse_stylesheet("p { color: red;"),
            Err(CssError::UnterminatedRule)
        );
    }

    #[test]
    fn preserves_source_order() {
        let sheet = parse_stylesheet("p { color: red; } div { color: blue; }").unwrap();
        assert_eq!(sheet.rules[0].source_order, 0);
        assert_eq!(sheet.rules[1].source_order, 1);
    }
}
