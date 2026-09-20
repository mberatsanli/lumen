//! The built-in user-agent defaults: the UA stylesheet (weakest cascade
//! origin) and the per-tag `display` defaults that live in code.

use crate::style::Display;
use lumen_css::{CssValue, Stylesheet, Unit};
use lumen_html::ElementData;
use std::sync::OnceLock;

/// The built-in user-agent stylesheet (weakest cascade origin).
///
/// Display defaults are code-side (see `default_display`); this sheet only
/// carries typography and spacing defaults. Form controls do not inherit
/// the document's font: they use the platform's small control face at a
/// fixed size, so a page-wide `font-family` never reshapes them.
///
/// The document-flow values are
/// the ones the HTML Standard's "Rendering" section suggests; pages are
/// authored against them, so a different default shifts every box below.
pub fn user_agent_stylesheet() -> &'static Stylesheet {
    static SHEET: OnceLock<Stylesheet> = OnceLock::new();
    SHEET.get_or_init(|| {
        let source = r"
            html { margin: 0; padding: 0; color: #000000; }
            body { margin: 8px; padding: 0; }
            h1 { font-size: 2em; font-weight: 700; margin-top: 0.67em; margin-bottom: 0.67em; }
            h2 { font-size: 1.5em; font-weight: 700; margin-top: 0.83em; margin-bottom: 0.83em; }
            h3 { font-size: 1.17em; font-weight: 700; margin-top: 1em; margin-bottom: 1em; }
            h4 { font-weight: 700; margin-top: 1.33em; margin-bottom: 1.33em; }
            h5 { font-size: 0.83em; font-weight: 700; margin-top: 1.67em; margin-bottom: 1.67em; }
            h6 { font-size: 0.67em; font-weight: 700; margin-top: 2.33em; margin-bottom: 2.33em; }
            p { margin-top: 1em; margin-bottom: 1em; }
            blockquote, figure { margin: 1em 40px; }
            dl { margin-top: 1em; margin-bottom: 1em; }
            dd { margin-left: 40px; }
            hr { border-top: 1px solid #808080; margin-top: 0.5em; margin-bottom: 0.5em; }
            a { color: #0000ee; text-decoration: underline; }
            strong, b { font-weight: 700; }
            em, i, cite, var, dfn, address { font-style: italic; }
            u, ins { text-decoration: underline; }
            s, strike, del { text-decoration: line-through; }
            mark { background-color: #ffff00; color: #000000; }
            small { font-size: 0.83em; }
            pre { white-space: pre; font-family: monospace;
                margin-top: 1em; margin-bottom: 1em; }
            code, kbd, samp, tt { font-family: monospace; }
            ul, ol { padding-left: 40px; margin-top: 1em; margin-bottom: 1em; }
            ul ul, ul ol, ol ul, ol ol { margin-top: 0; margin-bottom: 0; }
            center { text-align: center; }
            td, th { padding: 1px; }
            th { font-weight: 700; text-align: center; }
            sub { vertical-align: sub; font-size: 0.83em; }
            sup { vertical-align: super; font-size: 0.83em; }
            input, select, textarea, button { font-size: 13.3333px;
                font-family: sans-serif; box-sizing: border-box; margin: 0; }
            input, textarea { border: 2px inset #767676; background-color: #ffffff;
                padding: 1px 2px; box-sizing: content-box; }
            input { width: 153px; min-height: 1.1em; white-space: pre; overflow: hidden; }
            input[type=submit], input[type=button], input[type=reset], button {
                border: 2px outset #767676; background-color: #ebebeb;
                padding: 1px 6px; width: auto; box-sizing: border-box; }
            input[type=checkbox], input[type=radio] { width: 13px; height: 13px;
                padding: 0; min-height: 0; border: 1px solid #8a8a8a; border-radius: 3px;
                box-sizing: border-box; margin: 3px 3px 3px 4px; }
            input[type=radio] { border-radius: 7px; margin-left: 5px; }
            input[type=checkbox]:checked, input[type=radio]:checked {
                background-color: #2266aa; border-color: #2266aa; }
            input[type=checkbox]:checked { --lumen-mark: check; }
            input[type=radio]:checked { --lumen-mark: dot; }
            input[type=hidden] { display: none !important; }
            input[type=search] { box-sizing: border-box; }
            input[type=color] { width: 50px; height: 27px; padding: 1px 2px;
                border: 1px solid #767676; min-height: 0; box-sizing: border-box; }
            input[type=range] { width: 129px; height: 16px; padding: 0; min-height: 0;
                margin: 2px; border: 1px solid #b9b2a2; border-radius: 8px;
                background-color: #e8e4da; box-sizing: border-box; }
            select { border: 1px solid #767676; border-radius: 3px;
                padding: 0 18px 0 4px; background-color: #ffffff; height: 19px;
                width: auto; box-sizing: border-box; --lumen-mark: arrow; }
            option { display: none; }
            textarea { font-family: monospace; white-space: pre; overflow: auto;
                padding: 2px; border: 1px solid #767676; width: 177px; height: 30px; }
            fieldset { border: 2px groove #b9b2a2; padding: 0.35em 0.75em 0.625em;
                margin-inline: 2px; margin-top: 1em; margin-bottom: 1em; }
            legend { padding: 0 2px; }
            progress, meter { width: 160px; height: 16px; padding: 0; min-height: 0;
                border: 1px solid #b9b2a2; border-radius: 8px; background-color: #e8e4da;
                box-sizing: border-box; }
            meter { width: 80px; }
            label { color: inherit; }
            select[multiple] { display: inline-block; width: auto; max-height: 70px;
                overflow: auto; padding: 0; height: auto; --lumen-mark: none; }
            select[multiple] option { display: block; padding: 1px 2px; margin: 0;
                border-radius: 3px; }
            select[multiple] option:checked { background-color: #2266aa; color: #ffffff; }
            optgroup { display: none; }
            select[multiple] optgroup { display: block; padding: 1px 0; line-height: 15px;
                font-weight: 700; font-size: 0.85em; color: #6b675e; }
            select[multiple] optgroup option { font-weight: 400; font-size: 13.3333px;
                color: #232019; }
        ";
        lumen_css::parse_stylesheet(source)
    })
}

/// The width an element's attributes ask for, as a presentational hint:
/// a declaration that beats the UA stylesheet but loses to any author
/// rule. A text field is as wide as the text it is asked to hold, which
/// is what `size` states and what a number field's bound implies.
pub(crate) fn width_hint(element: &ElementData) -> Option<CssValue> {
    if element.tag_name != "input" {
        return None;
    }
    // One average character advance of the control font, and the room a
    // number field leaves beside its value for the stepper.
    const CHARACTER: f32 = 0.525;
    const STEPPER: f32 = 0.975;
    // The default 20 columns is what an unsized field shows.
    const COLUMNS: f32 = 20.0;

    let columns = |attribute: &str| -> Option<f32> {
        element
            .attributes
            .get(attribute)
            .and_then(|value| value.trim().parse::<f32>().ok())
            .filter(|columns| *columns >= 1.0)
    };
    match element.attributes.get("type").unwrap_or("text") {
        "text" | "search" | "url" | "tel" | "password" | "email" => Some(CssValue::Length(
            (columns("size").unwrap_or(COLUMNS) + 1.0) * CHARACTER,
            Unit::Em,
        )),
        // A number field is sized by the largest value it accepts, not
        // by `size`; unbounded, it falls back to the text default.
        "number" => {
            let digits = element
                .attributes
                .get("max")
                .map(|max| max.trim().chars().count() as f32)
                .filter(|digits| *digits >= 1.0);
            Some(match digits {
                Some(digits) => CssValue::Length((digits + 1.0) * CHARACTER + STEPPER, Unit::Em),
                None => CssValue::Length((COLUMNS + 1.0) * CHARACTER, Unit::Em),
            })
        }
        _ => None,
    }
}

/// Default `display` per tag, used when no declaration says otherwise.
/// Unknown tags default to inline, as in HTML.
pub(crate) fn default_display(tag: &str) -> Display {
    match tag {
        "html" | "body" | "div" | "p" | "h1" | "h2" | "h3" | "h4" | "h5" | "h6" | "ul" | "ol"
        | "li" | "section" | "article" | "header" | "footer" | "main" | "nav" | "aside"
        | "blockquote" | "pre" | "form" | "table" | "hr" | "center" => Display::Block,
        "input" | "button" | "select" | "textarea" | "progress" | "meter" => Display::InlineBlock,
        "head" | "style" | "script" | "title" | "meta" | "link" | "base" => Display::None,
        _ => Display::Inline,
    }
}
