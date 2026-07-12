//! HTML tokenizer implemented as an explicit state machine.
//!
//! The states follow a simplified subset of the WHATWG tokenizer. Full
//! compliance is a non-goal; instead the tokenizer is lenient and never
//! fails: malformed input is recovered from by dropping the incomplete
//! construct or by treating stray characters as text, mirroring how
//! browsers keep parsing.
//!
//! Supported: start/end tags, quoted/unquoted/boolean attributes,
//! self-closing syntax, comments, doctype, basic character references and
//! raw-text elements (`script`, `style`, `title`, `textarea`) whose content
//! is not scanned for markup.

/// A single `name="value"` pair as written in the source.
///
/// Duplicate names are preserved here; the tree builder keeps the first.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HtmlAttribute {
    /// Lowercase attribute name.
    pub name: String,
    /// Decoded attribute value; empty for boolean attributes.
    pub value: String,
}

/// Output of the tokenizer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HtmlToken {
    /// `<!doctype html>` — the content after `doctype`, trimmed.
    Doctype(String),
    StartTag {
        /// Lowercase tag name.
        name: String,
        attributes: Vec<HtmlAttribute>,
        self_closing: bool,
    },
    EndTag {
        /// Lowercase tag name.
        name: String,
    },
    /// Raw text with character references decoded. Whitespace is preserved;
    /// collapsing is a layout concern.
    Text(String),
    Comment(String),
}

/// Tokenizer states, named after their WHATWG counterparts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum State {
    Data,
    TagOpen,
    EndTagOpen,
    TagName,
    BeforeAttributeName,
    AttributeName,
    AfterAttributeName,
    BeforeAttributeValue,
    AttributeValueDoubleQuoted,
    AttributeValueSingleQuoted,
    AttributeValueUnquoted,
    SelfClosingStartTag,
    MarkupDeclarationOpen,
    Comment,
    Doctype,
    BogusComment,
    Rawtext,
}

/// Elements whose content is consumed as raw text until the matching end tag.
const RAWTEXT_ELEMENTS: [&str; 4] = ["script", "style", "textarea", "title"];

/// Tokenizes `source` into a flat list of [`HtmlToken`]s.
///
/// Never fails; see the module documentation for the recovery strategy.
#[must_use]
pub fn tokenize(source: &str) -> Vec<HtmlToken> {
    Tokenizer::new(source).run()
}

struct Tokenizer {
    chars: Vec<char>,
    position: usize,
    state: State,
    tokens: Vec<HtmlToken>,
    text: String,
    tag_name: String,
    tag_is_end: bool,
    self_closing: bool,
    attributes: Vec<HtmlAttribute>,
    attribute_name: String,
    attribute_value: String,
    comment: String,
    doctype: String,
    rawtext_tag: String,
}

impl Tokenizer {
    fn new(source: &str) -> Self {
        Self {
            chars: source.chars().collect(),
            position: 0,
            state: State::Data,
            tokens: Vec::new(),
            text: String::new(),
            tag_name: String::new(),
            tag_is_end: false,
            self_closing: false,
            attributes: Vec::new(),
            attribute_name: String::new(),
            attribute_value: String::new(),
            comment: String::new(),
            doctype: String::new(),
            rawtext_tag: String::new(),
        }
    }

    fn run(mut self) -> Vec<HtmlToken> {
        while let Some(current) = self.chars.get(self.position).copied() {
            match self.state {
                State::Data => self.step_data(current),
                State::TagOpen => self.step_tag_open(current),
                State::EndTagOpen => self.step_end_tag_open(current),
                State::TagName => self.step_tag_name(current),
                State::BeforeAttributeName => self.step_before_attribute_name(current),
                State::AttributeName => self.step_attribute_name(current),
                State::AfterAttributeName => self.step_after_attribute_name(current),
                State::BeforeAttributeValue => self.step_before_attribute_value(current),
                State::AttributeValueDoubleQuoted => self.step_attribute_value_quoted(current, '"'),
                State::AttributeValueSingleQuoted => {
                    self.step_attribute_value_quoted(current, '\'');
                }
                State::AttributeValueUnquoted => self.step_attribute_value_unquoted(current),
                State::SelfClosingStartTag => self.step_self_closing_start_tag(current),
                State::MarkupDeclarationOpen => self.step_markup_declaration_open(),
                State::Comment => self.step_comment(current),
                State::Doctype => self.step_doctype(current),
                State::BogusComment => self.step_bogus_comment(current),
                State::Rawtext => self.step_rawtext(),
            }
        }
        self.finish();
        self.tokens
    }

    // --- state steps -------------------------------------------------------

    fn step_data(&mut self, current: char) {
        match current {
            '<' => {
                self.position += 1;
                self.state = State::TagOpen;
            }
            '&' => {
                let decoded = self.consume_character_reference();
                self.text.push_str(&decoded);
            }
            _ => {
                self.text.push(current);
                self.position += 1;
            }
        }
    }

    fn step_tag_open(&mut self, current: char) {
        match current {
            '!' => {
                self.position += 1;
                self.state = State::MarkupDeclarationOpen;
            }
            '/' => {
                self.position += 1;
                self.state = State::EndTagOpen;
            }
            _ if current.is_ascii_alphabetic() => {
                self.flush_text();
                self.reset_tag(false);
                self.state = State::TagName;
            }
            // Recovery: `<` followed by anything else is literal text.
            _ => {
                self.text.push('<');
                self.state = State::Data;
            }
        }
    }

    fn step_end_tag_open(&mut self, current: char) {
        if current.is_ascii_alphabetic() {
            self.flush_text();
            self.reset_tag(true);
            self.state = State::TagName;
        } else if current == '>' {
            // Recovery: `</>` is dropped entirely.
            self.position += 1;
            self.state = State::Data;
        } else {
            // Recovery: `</...` with a non-letter becomes a bogus comment.
            self.comment.clear();
            self.state = State::BogusComment;
        }
    }

    fn step_tag_name(&mut self, current: char) {
        self.position += 1;
        match current {
            character if character.is_ascii_whitespace() => {
                self.state = State::BeforeAttributeName;
            }
            '/' => self.state = State::SelfClosingStartTag,
            '>' => self.emit_tag(),
            _ => self.tag_name.extend(current.to_lowercase()),
        }
    }

    fn step_before_attribute_name(&mut self, current: char) {
        match current {
            character if character.is_ascii_whitespace() => self.position += 1,
            '/' => {
                self.position += 1;
                self.state = State::SelfClosingStartTag;
            }
            '>' => {
                self.position += 1;
                self.emit_tag();
            }
            _ => {
                self.attribute_name.clear();
                self.attribute_value.clear();
                self.state = State::AttributeName;
            }
        }
    }

    fn step_attribute_name(&mut self, current: char) {
        match current {
            character if character.is_ascii_whitespace() => {
                self.position += 1;
                self.state = State::AfterAttributeName;
            }
            '=' => {
                self.position += 1;
                self.state = State::BeforeAttributeValue;
            }
            '/' | '>' => {
                self.commit_attribute();
                self.state = State::BeforeAttributeName;
            }
            _ => {
                self.attribute_name.extend(current.to_lowercase());
                self.position += 1;
            }
        }
    }

    fn step_after_attribute_name(&mut self, current: char) {
        match current {
            character if character.is_ascii_whitespace() => self.position += 1,
            '=' => {
                self.position += 1;
                self.state = State::BeforeAttributeValue;
            }
            '/' | '>' => {
                self.commit_attribute();
                self.state = State::BeforeAttributeName;
            }
            _ => {
                // A new attribute starts; the previous one was boolean.
                self.commit_attribute();
                self.state = State::AttributeName;
            }
        }
    }

    fn step_before_attribute_value(&mut self, current: char) {
        match current {
            character if character.is_ascii_whitespace() => self.position += 1,
            '"' => {
                self.position += 1;
                self.state = State::AttributeValueDoubleQuoted;
            }
            '\'' => {
                self.position += 1;
                self.state = State::AttributeValueSingleQuoted;
            }
            '>' => {
                // Recovery: `attr=>` commits an empty value.
                self.position += 1;
                self.commit_attribute();
                self.emit_tag();
            }
            _ => self.state = State::AttributeValueUnquoted,
        }
    }

    fn step_attribute_value_quoted(&mut self, current: char, quote: char) {
        if current == quote {
            self.position += 1;
            self.commit_attribute();
            self.state = State::BeforeAttributeName;
        } else if current == '&' {
            let decoded = self.consume_character_reference();
            self.attribute_value.push_str(&decoded);
        } else {
            self.attribute_value.push(current);
            self.position += 1;
        }
    }

    fn step_attribute_value_unquoted(&mut self, current: char) {
        match current {
            character if character.is_ascii_whitespace() => {
                self.position += 1;
                self.commit_attribute();
                self.state = State::BeforeAttributeName;
            }
            '>' => {
                self.position += 1;
                self.commit_attribute();
                self.emit_tag();
            }
            '&' => {
                let decoded = self.consume_character_reference();
                self.attribute_value.push_str(&decoded);
            }
            _ => {
                self.attribute_value.push(current);
                self.position += 1;
            }
        }
    }

    fn step_self_closing_start_tag(&mut self, current: char) {
        if current == '>' {
            self.position += 1;
            self.self_closing = true;
            self.emit_tag();
        } else {
            // Recovery: stray `/` inside a tag is ignored.
            self.state = State::BeforeAttributeName;
        }
    }

    fn step_markup_declaration_open(&mut self) {
        if self.lookahead_matches("--") {
            self.position += 2;
            self.flush_text();
            self.comment.clear();
            self.state = State::Comment;
        } else if self.lookahead_matches_ascii_case_insensitive("doctype") {
            self.position += "doctype".len();
            self.flush_text();
            self.doctype.clear();
            self.state = State::Doctype;
        } else {
            // Recovery: unknown `<!...` construct is skipped like a comment.
            self.flush_text();
            self.comment.clear();
            self.state = State::BogusComment;
        }
    }

    fn step_comment(&mut self, _current: char) {
        if self.lookahead_matches("-->") {
            self.position += 3;
            let comment = std::mem::take(&mut self.comment);
            self.tokens.push(HtmlToken::Comment(comment));
            self.state = State::Data;
        } else {
            self.comment.push(self.chars[self.position]);
            self.position += 1;
        }
    }

    fn step_doctype(&mut self, current: char) {
        self.position += 1;
        if current == '>' {
            let doctype = std::mem::take(&mut self.doctype);
            self.tokens
                .push(HtmlToken::Doctype(doctype.trim().to_string()));
            self.state = State::Data;
        } else {
            self.doctype.push(current);
        }
    }

    fn step_bogus_comment(&mut self, current: char) {
        self.position += 1;
        if current == '>' {
            self.state = State::Data;
        }
    }

    fn step_rawtext(&mut self) {
        let close = format!("</{}", self.rawtext_tag);
        if self.lookahead_matches_ascii_case_insensitive(&close) {
            let after = self.position + close.chars().count();
            // The closer must be followed by whitespace, `/` or `>`.
            let boundary = self.chars.get(after).copied();
            if boundary.is_none_or(|c| c.is_ascii_whitespace() || c == '/' || c == '>') {
                self.flush_text();
                self.position = after;
                while self
                    .chars
                    .get(self.position)
                    .is_some_and(|current| *current != '>')
                {
                    self.position += 1;
                }
                self.position = (self.position + 1).min(self.chars.len());
                let name = std::mem::take(&mut self.rawtext_tag);
                self.tokens.push(HtmlToken::EndTag { name });
                self.state = State::Data;
                return;
            }
        }
        self.text.push(self.chars[self.position]);
        self.position += 1;
    }

    // --- helpers -----------------------------------------------------------

    /// End-of-input handling: flush pending text, emit an unterminated
    /// comment, and drop any incomplete tag (recovery, mirroring browsers).
    fn finish(&mut self) {
        match self.state {
            State::Comment => {
                let comment = std::mem::take(&mut self.comment);
                self.tokens.push(HtmlToken::Comment(comment));
            }
            State::TagOpen => self.text.push('<'),
            _ => {}
        }
        self.flush_text();
    }

    fn flush_text(&mut self) {
        if !self.text.is_empty() {
            let text = std::mem::take(&mut self.text);
            self.tokens.push(HtmlToken::Text(text));
        }
    }

    fn reset_tag(&mut self, is_end: bool) {
        self.tag_name.clear();
        self.attributes.clear();
        self.attribute_name.clear();
        self.attribute_value.clear();
        self.self_closing = false;
        self.tag_is_end = is_end;
    }

    fn commit_attribute(&mut self) {
        if !self.attribute_name.is_empty() {
            self.attributes.push(HtmlAttribute {
                name: std::mem::take(&mut self.attribute_name),
                value: std::mem::take(&mut self.attribute_value),
            });
        } else {
            self.attribute_name.clear();
            self.attribute_value.clear();
        }
    }

    fn emit_tag(&mut self) {
        self.commit_attribute();
        let name = std::mem::take(&mut self.tag_name);
        if name.is_empty() {
            self.state = State::Data;
            return;
        }
        if self.tag_is_end {
            self.tokens.push(HtmlToken::EndTag { name });
            self.state = State::Data;
        } else {
            if RAWTEXT_ELEMENTS.contains(&name.as_str()) && !self.self_closing {
                self.rawtext_tag = name.clone();
                self.state = State::Rawtext;
            } else {
                self.state = State::Data;
            }
            self.tokens.push(HtmlToken::StartTag {
                name,
                attributes: std::mem::take(&mut self.attributes),
                self_closing: self.self_closing,
            });
        }
    }

    fn lookahead_matches(&self, pattern: &str) -> bool {
        pattern
            .chars()
            .enumerate()
            .all(|(offset, expected)| self.chars.get(self.position + offset) == Some(&expected))
    }

    fn lookahead_matches_ascii_case_insensitive(&self, pattern: &str) -> bool {
        pattern.chars().enumerate().all(|(offset, expected)| {
            self.chars
                .get(self.position + offset)
                .is_some_and(|current| current.eq_ignore_ascii_case(&expected))
        })
    }

    /// Consumes a character reference starting at the current `&`.
    ///
    /// Recognizes a small named set and numeric forms. Anything else is
    /// returned literally, including the ampersand.
    fn consume_character_reference(&mut self) -> String {
        debug_assert_eq!(self.chars.get(self.position), Some(&'&'));
        let start = self.position;
        let mut end = self.position + 1;
        // The longest WHATWG name (CounterClockwiseContourIntegral) is
        // 31 chars; allow &name; up to 40.
        let limit = (start + 40).min(self.chars.len());
        while end < limit {
            let current = self.chars[end];
            if current == ';' {
                let body: String = self.chars[start + 1..end].iter().collect();
                if let Some(decoded) = decode_reference(&body) {
                    self.position = end + 1;
                    return decoded;
                }
                break;
            }
            if !(current.is_ascii_alphanumeric() || current == '#') {
                break;
            }
            end += 1;
        }
        self.position = start + 1;
        "&".to_string()
    }
}

fn decode_reference(body: &str) -> Option<String> {
    // Numeric references: &#38; and &#x26;.
    if let Some(digits) = body.strip_prefix('#') {
        let code = if let Some(hex) = digits.strip_prefix(['x', 'X']) {
            u32::from_str_radix(hex, 16).ok()?
        } else {
            digits.parse().ok()?
        };
        return char::from_u32(code).map(|character| character.to_string());
    }
    // Named references: the full WHATWG table via htmlize.
    let candidate = format!("&{body};");
    let unescaped = htmlize::unescape(&candidate);
    (unescaped != candidate).then(|| unescaped.into_owned())
}

#[cfg(test)]
mod tests {
    #[test]
    fn full_entity_table_decodes_exotic_names() {
        let tokens = tokenize("&alpha;&spades;&CounterClockwiseContourIntegral;&notin;");
        let HtmlToken::Text(text) = &tokens[0] else {
            panic!("expected text");
        };
        assert_eq!(text, "\u{3b1}\u{2660}\u{2233}\u{2209}");
    }

    use super::*;

    fn start_tag(name: &str, attributes: &[(&str, &str)], self_closing: bool) -> HtmlToken {
        HtmlToken::StartTag {
            name: name.to_string(),
            attributes: attributes
                .iter()
                .map(|(name, value)| HtmlAttribute {
                    name: name.to_string(),
                    value: value.to_string(),
                })
                .collect(),
            self_closing,
        }
    }

    fn end_tag(name: &str) -> HtmlToken {
        HtmlToken::EndTag {
            name: name.to_string(),
        }
    }

    fn text(value: &str) -> HtmlToken {
        HtmlToken::Text(value.to_string())
    }

    #[test]
    fn tokenizes_simple_element() {
        assert_eq!(
            tokenize("<div>Hello</div>"),
            vec![start_tag("div", &[], false), text("Hello"), end_tag("div")]
        );
    }

    #[test]
    fn boolean_attribute() {
        assert_eq!(
            tokenize("<input disabled>"),
            vec![start_tag("input", &[("disabled", "")], false)]
        );
    }

    #[test]
    fn self_closing_with_quoted_attribute() {
        assert_eq!(
            tokenize("<img src=\"image.png\" />"),
            vec![start_tag("img", &[("src", "image.png")], true)]
        );
    }

    #[test]
    fn mixed_quote_styles_and_multiple_attributes() {
        assert_eq!(
            tokenize("<div class=\"card active\" id='main'>"),
            vec![start_tag(
                "div",
                &[("class", "card active"), ("id", "main")],
                false
            )]
        );
    }

    #[test]
    fn unquoted_attribute_value() {
        assert_eq!(
            tokenize("<div id=main>"),
            vec![start_tag("div", &[("id", "main")], false)]
        );
    }

    #[test]
    fn comment_token() {
        assert_eq!(
            tokenize("<!-- comment -->"),
            vec![HtmlToken::Comment(" comment ".to_string())]
        );
    }

    #[test]
    fn doctype_token() {
        assert_eq!(
            tokenize("<!DOCTYPE html>"),
            vec![HtmlToken::Doctype("html".to_string())]
        );
    }

    #[test]
    fn uppercase_names_are_normalized() {
        assert_eq!(
            tokenize("<DIV CLASS='x'></DIV>"),
            vec![start_tag("div", &[("class", "x")], false), end_tag("div")]
        );
    }

    #[test]
    fn character_references_in_text_and_attributes() {
        assert_eq!(
            tokenize("a &amp; b &#65;&#x42;<p title=\"5 &lt; 6\">"),
            vec![
                text("a & b AB"),
                start_tag("p", &[("title", "5 < 6")], false)
            ]
        );
    }

    #[test]
    fn unknown_character_reference_is_literal() {
        assert_eq!(tokenize("fish &chips; ok"), vec![text("fish &chips; ok")]);
    }

    #[test]
    fn stray_less_than_is_text() {
        assert_eq!(tokenize("if a < 5 then"), vec![text("if a < 5 then")]);
    }

    #[test]
    fn unterminated_tag_is_dropped() {
        assert_eq!(tokenize("ok<div class=\"x"), vec![text("ok")]);
    }

    #[test]
    fn unterminated_comment_is_emitted() {
        assert_eq!(
            tokenize("<!-- open"),
            vec![HtmlToken::Comment(" open".to_string())]
        );
    }

    #[test]
    fn empty_end_tag_is_ignored() {
        assert_eq!(tokenize("a</>b"), vec![text("ab")]);
    }

    #[test]
    fn duplicate_attributes_are_preserved_in_order() {
        assert_eq!(
            tokenize("<div a=1 a=2>"),
            vec![start_tag("div", &[("a", "1"), ("a", "2")], false)]
        );
    }

    #[test]
    fn stray_slash_inside_tag_is_ignored() {
        assert_eq!(
            tokenize("<div / class=x>"),
            vec![start_tag("div", &[("class", "x")], false)]
        );
    }

    #[test]
    fn style_content_is_raw_text() {
        assert_eq!(
            tokenize("<style>p > a { color: red; }</style>"),
            vec![
                start_tag("style", &[], false),
                text("p > a { color: red; }"),
                end_tag("style"),
            ]
        );
    }

    #[test]
    fn script_content_with_markup_like_text() {
        assert_eq!(
            tokenize("<script>if (a<b) { x = \"</div>\"; }</script>"),
            vec![
                start_tag("script", &[], false),
                text("if (a<b) { x = \"</div>\"; }"),
                end_tag("script"),
            ]
        );
    }

    #[test]
    fn bogus_markup_declaration_is_skipped() {
        assert_eq!(tokenize("a<![CDATA[x]]>b"), vec![text("a"), text("b")]);
    }

    #[test]
    fn whitespace_between_attributes() {
        assert_eq!(
            tokenize("<div   id = \"a\"   class = b >"),
            vec![start_tag("div", &[("id", "a"), ("class", "b")], false)]
        );
    }
}
