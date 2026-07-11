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
    /// Declared with `!important`.
    pub important: bool,
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

/// Parses a stylesheet.
///
/// Lenient where browsers are lenient, and therefore infallible: comments
/// are skipped, malformed declarations and unsupported selectors are
/// dropped (a rule whose selector list contains any invalid selector is
/// dropped entirely), and an unterminated `{` block is closed at end of
/// input with its declarations kept.
#[must_use]
pub fn parse_stylesheet(source: &str) -> Stylesheet {
    let source = strip_comments(source);
    let mut rules = Vec::new();
    let mut rest = source.as_str();
    let mut source_order = 0;

    loop {
        rest = rest.trim_start();
        if rest.is_empty() {
            break;
        }
        // At-rules (`@media`, `@import`, ...) are unsupported: skip the
        // whole construct with balanced braces so nested rules inside the
        // block cannot desynchronize the parser.
        if rest.starts_with('@') {
            rest = skip_at_rule(rest);
            continue;
        }

        let Some(open) = rest.find('{') else { break };
        let selector_source = rest[..open].trim();
        let after_open = &rest[open + 1..];
        // Recovery: a missing `}` closes the block at end of input.
        let close = after_open.find('}').unwrap_or(after_open.len());
        let declaration_source = &after_open[..close];
        rest = &after_open[(close + 1).min(after_open.len())..];

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

    Stylesheet { rules }
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
        // `!important` peels off the end of the value (case-insensitive,
        // whitespace tolerated).
        let trimmed = value.trim_end();
        let (value, important) = match trimmed
            .to_ascii_lowercase()
            .strip_suffix("important")
            .map(|rest| rest.trim_end())
            .and_then(|rest| rest.strip_suffix('!').map(str::len))
        {
            Some(prefix_length) => (&trimmed[..prefix_length], true),
            None => (value, false),
        };
        let components: Vec<CssValue> = split_components(value)
            .iter()
            .filter_map(|component| CssValue::parse_component(component))
            .collect();
        if components.is_empty() {
            continue;
        }
        let start = declarations.len();
        expand_declaration(&name, components, &mut declarations);
        if important {
            for declaration in &mut declarations[start..] {
                declaration.important = true;
            }
        }
    }
    declarations
}

/// Expands `margin`/`padding`/`border-width` shorthands into longhands and
/// `background` to `background-color`; every other property keeps its first
/// component (multi-value forms of other properties are unsupported).
fn expand_declaration(name: &str, mut components: Vec<CssValue>, output: &mut Vec<Declaration>) {
    if name == "background" {
        // Supported parts of the shorthand: the first color (or
        // `transparent`/`none`) and the first image (url()/gradient).
        let color = components.iter().find_map(|component| match component {
            CssValue::Color(_) => Some(component.clone()),
            CssValue::Keyword(keyword) if keyword == "transparent" || keyword == "none" => {
                Some(CssValue::Keyword("transparent".to_string()))
            }
            _ => None,
        });
        if let Some(value) = color {
            output.push(Declaration {
                important: false,
                name: "background-color".to_string(),
                value,
            });
        }
        let image = components
            .iter()
            .find(|component| matches!(component, CssValue::Url(_) | CssValue::Function(..)));
        if let Some(value) = image {
            output.push(Declaration {
                important: false,
                name: "background-image".to_string(),
                value: value.clone(),
            });
        }
        return;
    }
    let expand_edges = |suffix_for: &dyn Fn(&str) -> String,
                        components: &[CssValue],
                        output: &mut Vec<Declaration>| {
        let Some(edges) = edge_values(components) else {
            return;
        };
        for (side, value) in SIDES.iter().zip(edges) {
            output.push(Declaration {
                important: false,
                name: suffix_for(side),
                value,
            });
        }
    };

    // One border side: width/style/color in any order; missing width is
    // 3px (CSS medium), missing style is solid, missing color falls back
    // to the element color at computed-style time.
    let expand_border_side =
        |side: &str, components: &[CssValue], output: &mut Vec<Declaration>| {
            let mut width = CssValue::Length(3.0, crate::value::Unit::Px);
            let mut style = CssValue::Keyword("solid".to_string());
            let mut color = None;
            for component in components {
                match component {
                    CssValue::Length(..) | CssValue::Number(_) => width = component.clone(),
                    CssValue::Color(_) => color = Some(component.clone()),
                    CssValue::Keyword(keyword) if BORDER_STYLES.contains(&keyword.as_str()) => {
                        style = component.clone();
                    }
                    _ => {}
                }
            }
            output.push(Declaration {
                important: false,
                name: format!("border-{side}-width"),
                value: width,
            });
            output.push(Declaration {
                important: false,
                name: format!("border-{side}-style"),
                value: style,
            });
            if let Some(color) = color {
                output.push(Declaration {
                    important: false,
                    name: format!("border-{side}-color"),
                    value: color,
                });
            }
        };

    match name {
        "margin" | "padding" => {
            expand_edges(&|side| format!("{name}-{side}"), &components, output);
        }
        "border-width" => {
            expand_edges(&|side| format!("border-{side}-width"), &components, output);
        }
        "border-style" => {
            expand_edges(&|side| format!("border-{side}-style"), &components, output);
        }
        "border-color" => {
            expand_edges(&|side| format!("border-{side}-color"), &components, output);
        }
        "border-radius" => {
            // 1-4 values, clockwise from top-left.
            let corners = ["top-left", "top-right", "bottom-right", "bottom-left"];
            let Some(values) = edge_values(&components) else {
                return;
            };
            for (corner, value) in corners.iter().zip(values) {
                output.push(Declaration {
                    important: false,
                    name: format!("border-{corner}-radius"),
                    value,
                });
            }
        }
        "border" => {
            for side in SIDES {
                expand_border_side(side, &components, output);
            }
        }
        "border-top" | "border-right" | "border-bottom" | "border-left" => {
            let side = &name["border-".len()..];
            expand_border_side(side, &components, output);
        }
        "font-family" => {
            // Only the generic family matters to the engine: the list
            // normalizes to `monospace` when any entry names a monospace
            // family, and `sans-serif` otherwise.
            let is_mono = components.iter().any(|component| match component {
                CssValue::Keyword(keyword) => {
                    let name = keyword
                        .trim_matches(|c: char| c == ',' || c == '"' || c == '\'')
                        .to_ascii_lowercase();
                    name.contains("mono")
                        || name.starts_with("courier")
                        || matches!(name.as_str(), "menlo" | "monaco" | "consolas")
                }
                _ => false,
            });
            output.push(Declaration {
                important: false,
                name: "font-family".to_string(),
                value: CssValue::Keyword(
                    if is_mono { "monospace" } else { "sans-serif" }.to_string(),
                ),
            });
        }
        "flex" => {
            // grow [shrink [basis]]; `none` = 0 0, `auto`/`initial` keep
            // defaults with grow 1/0. flex-basis is unsupported and ignored.
            let (grow, shrink) = match components.first() {
                Some(CssValue::Keyword(keyword)) if keyword == "none" => (0.0, 0.0),
                Some(CssValue::Keyword(keyword)) if keyword == "auto" => (1.0, 1.0),
                Some(CssValue::Keyword(keyword)) if keyword == "initial" => (0.0, 1.0),
                Some(CssValue::Number(grow)) => {
                    let shrink = match components.get(1) {
                        Some(CssValue::Number(shrink)) => *shrink,
                        _ => 1.0,
                    };
                    (*grow, shrink)
                }
                _ => return,
            };
            output.push(Declaration {
                important: false,
                name: "flex-grow".to_string(),
                value: CssValue::Number(grow),
            });
            output.push(Declaration {
                important: false,
                name: "flex-shrink".to_string(),
                value: CssValue::Number(shrink),
            });
        }
        _ => output.push(Declaration {
            name: name.to_string(),
            value: components.swap_remove(0),
            important: false,
        }),
    }
}

const SIDES: [&str; 4] = ["top", "right", "bottom", "left"];
const BORDER_STYLES: [&str; 5] = ["none", "hidden", "solid", "dashed", "dotted"];

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

/// Skips one at-rule: either a statement ending in `;` (`@import ...;`) or
/// a block with balanced braces (`@media ... { ... }`). Returns the rest.
fn skip_at_rule(source: &str) -> &str {
    let mut depth = 0usize;
    for (index, character) in source.char_indices() {
        match character {
            ';' if depth == 0 => return &source[index + 1..],
            '{' => depth += 1,
            '}' => {
                depth = depth.saturating_sub(1);
                if depth == 0 {
                    return &source[index + 1..];
                }
            }
            _ => {}
        }
    }
    ""
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
        let sheet = parse_stylesheet(".card, #main { width: 400px; color: #222; }");
        assert_eq!(sheet.rules.len(), 1);
        assert_eq!(sheet.rules[0].selectors.len(), 2);
        assert_eq!(
            sheet.rules[0].declarations,
            vec![
                Declaration {
                    name: "width".to_string(),
                    value: px(400.0),
                    important: false,
                },
                Declaration {
                    name: "color".to_string(),
                    value: CssValue::Color(Color::rgb(0x22, 0x22, 0x22)),
                    important: false,
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
    fn expands_flex_shorthand() {
        let sheet = parse_stylesheet("a { flex: 2; } b { flex: none; } c { flex: 1 3; }");
        let decls = |index: usize| -> Vec<(String, String)> {
            sheet.rules[index]
                .declarations
                .iter()
                .map(|d| (d.name.clone(), format!("{:?}", d.value)))
                .collect()
        };
        assert_eq!(
            decls(0),
            vec![
                ("flex-grow".to_string(), "Number(2.0)".to_string()),
                ("flex-shrink".to_string(), "Number(1.0)".to_string()),
            ]
        );
        assert_eq!(
            decls(1),
            vec![
                ("flex-grow".to_string(), "Number(0.0)".to_string()),
                ("flex-shrink".to_string(), "Number(0.0)".to_string()),
            ]
        );
        assert_eq!(
            decls(2),
            vec![
                ("flex-grow".to_string(), "Number(1.0)".to_string()),
                ("flex-shrink".to_string(), "Number(3.0)".to_string()),
            ]
        );
    }

    #[test]
    fn expands_border_width_shorthand() {
        let declarations = parse_declarations("border-width: 1px 2px");
        let names: Vec<&str> = declarations
            .iter()
            .map(|declaration| declaration.name.as_str())
            .collect();
        assert_eq!(
            names,
            vec![
                "border-top-width",
                "border-right-width",
                "border-bottom-width",
                "border-left-width"
            ]
        );
        assert_eq!(declarations[1].value, px(2.0));
    }

    #[test]
    fn border_shorthand_expands_width_style_color() {
        let declarations = parse_declarations("border: 1px solid #cccccc");
        assert_eq!(declarations.len(), 12);
        let find = |name: &str| {
            declarations
                .iter()
                .find(|declaration| declaration.name == name)
                .map(|declaration| declaration.value.clone())
        };
        assert_eq!(find("border-top-width"), Some(px(1.0)));
        assert_eq!(
            find("border-left-style"),
            Some(CssValue::Keyword("solid".to_string()))
        );
        assert_eq!(
            find("border-bottom-color"),
            Some(CssValue::Color(Color::rgb(0xcc, 0xcc, 0xcc)))
        );
        // Order-independent, defaults for missing parts (medium = 3px).
        let declarations = parse_declarations("border: red");
        let find = |name: &str| {
            declarations
                .iter()
                .find(|declaration| declaration.name == name)
                .map(|declaration| declaration.value.clone())
        };
        assert_eq!(find("border-top-width"), Some(px(3.0)));
        assert_eq!(
            find("border-top-color"),
            Some(CssValue::Color(Color::rgb(255, 0, 0)))
        );
    }

    #[test]
    fn border_radius_expands_clockwise_from_top_left() {
        let declarations = parse_declarations("border-radius: 1px 2px");
        let names: Vec<&str> = declarations
            .iter()
            .map(|declaration| declaration.name.as_str())
            .collect();
        assert_eq!(
            names,
            vec![
                "border-top-left-radius",
                "border-top-right-radius",
                "border-bottom-right-radius",
                "border-bottom-left-radius"
            ]
        );
        assert_eq!(declarations[2].value, px(1.0));
        assert_eq!(declarations[3].value, px(2.0));
    }

    #[test]
    fn border_side_shorthand_targets_one_side() {
        let declarations = parse_declarations("border-top: 2px dashed #112233");
        let names: Vec<&str> = declarations
            .iter()
            .map(|declaration| declaration.name.as_str())
            .collect();
        assert_eq!(
            names,
            vec!["border-top-width", "border-top-style", "border-top-color"]
        );
    }

    #[test]
    fn border_color_and_style_expand_per_side() {
        let declarations = parse_declarations("border-color: red blue; border-style: solid none");
        let find = |name: &str| {
            declarations
                .iter()
                .find(|declaration| declaration.name == name)
                .map(|declaration| declaration.value.clone())
        };
        assert_eq!(
            find("border-right-color"),
            Some(CssValue::Color(Color::rgb(0, 0, 255)))
        );
        assert_eq!(
            find("border-bottom-color"),
            Some(CssValue::Color(Color::rgb(255, 0, 0)))
        );
        assert_eq!(
            find("border-left-style"),
            Some(CssValue::Keyword("none".to_string()))
        );
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
            parse_declarations(";; color: red; oops; width: ; height: 10px; x: !!;");
        assert_eq!(declarations.len(), 2);
        assert_eq!(declarations[0].name, "color");
        assert_eq!(declarations[1].name, "height");
    }

    #[test]
    fn drops_rule_with_invalid_selector() {
        let sheet =
            parse_stylesheet("p ! a { color: red; } h1 { color: blue; } a:focus { color: red; }");
        assert_eq!(sheet.rules.len(), 1);
        assert_eq!(sheet.rules[0].source_order, 0);
    }

    #[test]
    fn skips_comments() {
        let sheet = parse_stylesheet("/* x */ p { /* y */ color: red; }");
        assert_eq!(sheet.rules.len(), 1);
        assert_eq!(sheet.rules[0].declarations.len(), 1);
    }

    #[test]
    fn unterminated_rule_recovers_at_end_of_input() {
        let sheet = parse_stylesheet("p { color: red;");
        assert_eq!(sheet.rules.len(), 1);
        assert_eq!(sheet.rules[0].declarations.len(), 1);
    }

    #[test]
    fn background_shorthand_keeps_color_and_image() {
        let declarations = parse_declarations("background: #fdfcff left top no-repeat");
        assert_eq!(declarations.len(), 1);
        assert_eq!(declarations[0].name, "background-color");
        assert_eq!(
            declarations[0].value,
            CssValue::Color(Color::rgb(0xfd, 0xfc, 0xff))
        );
        let none = parse_declarations("background: none");
        assert_eq!(none[0].value, CssValue::Keyword("transparent".to_string()));
        let image = parse_declarations("background: url(x.png)");
        assert_eq!(image.len(), 1);
        assert_eq!(image[0].name, "background-image");
        assert_eq!(image[0].value, CssValue::Url("x.png".to_string()));
    }

    #[test]
    fn media_block_is_skipped_without_desync() {
        // Regression: the parser used to close the @media block at the
        // first `}`, swallowing every rule that followed.
        let sheet = parse_stylesheet(
            "p { color: red; }
             @media (max-width: 700px) {
                 div { margin: 0; }
                 body { background-color: white; }
             }
             h1 { color: blue; }",
        );
        assert_eq!(sheet.rules.len(), 2);
        assert_eq!(
            sheet.rules[1].selectors[0].compounds[0].tag.as_deref(),
            Some("h1")
        );
    }

    #[test]
    fn statement_at_rules_are_skipped() {
        let sheet = parse_stylesheet("@charset \"utf-8\"; @import url(x.css); p { color: red; }");
        assert_eq!(sheet.rules.len(), 1);
    }

    #[test]
    fn unterminated_at_rule_consumes_rest() {
        let sheet = parse_stylesheet("p { color: red; } @media (x) { div { color: blue; }");
        assert_eq!(sheet.rules.len(), 1);
    }

    #[test]
    fn preserves_source_order() {
        let sheet = parse_stylesheet("p { color: red; } div { color: blue; }");
        assert_eq!(sheet.rules[0].source_order, 0);
        assert_eq!(sheet.rules[1].source_order, 1);
    }
}
