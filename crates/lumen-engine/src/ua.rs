//! The built-in user-agent defaults: the UA stylesheet (weakest cascade
//! origin) and the per-tag `display` defaults that live in code.

use crate::style::Display;
use lumen_css::Stylesheet;
use std::sync::OnceLock;

/// The built-in user-agent stylesheet (weakest cascade origin).
///
/// Display defaults are code-side (see `default_display`); this sheet only
/// carries typography and spacing defaults.
pub fn user_agent_stylesheet() -> &'static Stylesheet {
    static SHEET: OnceLock<Stylesheet> = OnceLock::new();
    SHEET.get_or_init(|| {
        let source = r"
            html, body { margin: 0; padding: 0; color: #111111; font-size: 16px; }
            h1 { font-size: 32px; font-weight: 700; margin-top: 12px; margin-bottom: 12px; }
            h2 { font-size: 24px; font-weight: 700; margin-top: 10px; margin-bottom: 10px; }
            h3 { font-size: 19px; font-weight: 700; margin-top: 9px; margin-bottom: 9px; }
            h4 { font-size: 16px; font-weight: 700; margin-top: 11px; margin-bottom: 11px; }
            h5 { font-size: 13px; font-weight: 700; margin-top: 11px; margin-bottom: 11px; }
            h6 { font-size: 11px; font-weight: 700; margin-top: 12px; margin-bottom: 12px; }
            p { font-size: 16px; margin-top: 8px; margin-bottom: 8px; }
            hr { border-top: 1px solid #808080; margin-top: 8px; margin-bottom: 8px; }
            a { color: #0000ee; text-decoration: underline; }
            strong, b { font-weight: 700; }
            em, i { font-style: italic; }
            pre { white-space: pre; font-family: monospace; margin-top: 8px; margin-bottom: 8px; }
            code, kbd, samp, tt { font-family: monospace; font-size: 0.875em; }
            ul, ol { padding-left: 22px; margin-top: 8px; margin-bottom: 8px; }
            center { text-align: center; }
            td, th { padding: 2px 4px; }
            th { font-weight: 700; text-align: center; }
            sub { vertical-align: sub; font-size: 0.8em; }
            sup { vertical-align: super; font-size: 0.8em; }
            input, select, textarea, button { border: 1px solid #767676; border-radius: 3px;
                background-color: #ffffff; padding: 3px 8px; font-size: 13px; margin: 2px; }
            input { width: 170px; min-height: 1.1em; white-space: pre;
                overflow: hidden; }
            input[type=submit], input[type=button], button { background-color: #ebebeb;
                width: auto; padding: 3px 12px; }
            input[type=checkbox], input[type=radio] { width: 13px; height: 13px; padding: 0;
                min-height: 0; border-radius: 3px; border-color: #8a8a8a; }
            input[type=radio] { border-radius: 7px; }
            select { border: 1px solid #767676; border-radius: 3px; padding: 3px 24px 3px 8px;
                background-color: #ffffff; font-size: 13px; min-height: 1.1em;
                --lumen-mark: arrow; }
            option { display: none; }
            textarea { white-space: pre; overflow: auto; }
            fieldset { border: 1px solid #b9b2a2; border-radius: 4px;
                padding: 8px 12px; margin-top: 8px; margin-bottom: 8px; }
            legend { font-weight: 700; font-size: 0.9em; }
            progress, meter, input[type=range] { width: 160px; height: 10px; padding: 0;
                border: 1px solid #b9b2a2; border-radius: 5px; background-color: #e8e4da;
                min-height: 0; }
            input[type=range] { height: 14px; border-radius: 7px; }
            label { color: inherit; }
            input[type=checkbox]:checked, input[type=radio]:checked {
                background-color: #2266aa; border-color: #2266aa; }
            input[type=checkbox]:checked { --lumen-mark: check; }
            input[type=radio]:checked { --lumen-mark: dot; }
            input[type=hidden] { display: none !important; }
            select { width: auto; }
            textarea { width: 300px; height: 64px; }
            select[multiple] { display: inline-block; width: 200px; max-height: 108px;
                overflow: auto; padding: 4px; --lumen-mark: none; }
            select[multiple] option { display: block; padding: 2px 8px; margin: 1px 0;
                border-radius: 3px; min-height: 1.1em; }
            select[multiple] option:checked { background-color: #2266aa; color: #ffffff; }
            optgroup { display: none; }
            select[multiple] optgroup { display: block; padding: 2px 4px;
                font-weight: 700; font-size: 0.85em; color: #6b675e; }
            select[multiple] optgroup option { font-weight: 400; font-size: 13px;
                color: #232019; }
            input[type=color] { width: 44px; height: 26px; padding: 2px; min-height: 0;
                border-color: #8a8a8a; }
            input[type=number] { width: 80px; }
        ";
        lumen_css::parse_stylesheet(source)
    })
}

/// Default `display` per tag, used when no declaration says otherwise.
/// Unknown tags default to inline, as in HTML.
pub(crate) fn default_display(tag: &str) -> Display {
    match tag {
        "html" | "body" | "div" | "p" | "h1" | "h2" | "h3" | "h4" | "h5" | "h6" | "ul" | "ol"
        | "li" | "section" | "article" | "header" | "footer" | "main" | "nav" | "aside"
        | "blockquote" | "pre" | "form" | "table" | "hr" | "center" => Display::Block,
        "input" | "button" | "select" | "textarea" => Display::InlineBlock,
        "head" | "style" | "script" | "title" | "meta" | "link" | "base" => Display::None,
        _ => Display::Inline,
    }
}
