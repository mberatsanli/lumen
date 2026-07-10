use std::collections::BTreeMap;
use std::fmt::Write as _;

pub type NodeId = usize;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NodeKind {
    Document,
    Element(ElementData),
    Text(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ElementData {
    pub tag_name: String,
    pub attributes: BTreeMap<String, String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Node {
    pub kind: NodeKind,
    pub parent: Option<NodeId>,
    pub children: Vec<NodeId>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Document {
    nodes: Vec<Node>,
    root: NodeId,
}

impl Document {
    #[must_use]
    pub fn new() -> Self {
        Self {
            nodes: vec![Node {
                kind: NodeKind::Document,
                parent: None,
                children: Vec::new(),
            }],
            root: 0,
        }
    }

    #[must_use]
    pub const fn root(&self) -> NodeId {
        self.root
    }

    #[must_use]
    pub fn node(&self, id: NodeId) -> &Node {
        &self.nodes[id]
    }

    #[must_use]
    pub fn nodes(&self) -> &[Node] {
        &self.nodes
    }

    pub fn append(&mut self, parent: NodeId, kind: NodeKind) -> NodeId {
        let id = self.nodes.len();
        self.nodes.push(Node {
            kind,
            parent: Some(parent),
            children: Vec::new(),
        });
        self.nodes[parent].children.push(id);
        id
    }

    #[must_use]
    pub fn text_content(&self, id: NodeId) -> String {
        let mut output = String::new();
        self.collect_text(id, &mut output);
        output
    }

    fn collect_text(&self, id: NodeId, output: &mut String) {
        match &self.node(id).kind {
            NodeKind::Text(text) => output.push_str(text),
            _ => {
                for child in &self.node(id).children {
                    self.collect_text(*child, output);
                }
            }
        }
    }

    #[must_use]
    pub fn dump(&self) -> String {
        let mut output = String::new();
        self.dump_node(self.root, 0, &mut output);
        output
    }

    fn dump_node(&self, id: NodeId, depth: usize, output: &mut String) {
        let indent = "  ".repeat(depth);
        match &self.node(id).kind {
            NodeKind::Document => {
                let _ = writeln!(output, "{indent}#document");
            }
            NodeKind::Text(text) => {
                let normalized = text.split_whitespace().collect::<Vec<_>>().join(" ");
                if !normalized.is_empty() {
                    let _ = writeln!(output, "{indent}\"{normalized}\"");
                }
            }
            NodeKind::Element(element) => {
                let mut attributes = String::new();
                for (name, value) in &element.attributes {
                    let _ = write!(attributes, " {name}=\"{value}\"");
                }
                let _ = writeln!(output, "{indent}<{}{}>", element.tag_name, attributes);
            }
        }

        for child in &self.node(id).children {
            self.dump_node(*child, depth + 1, output);
        }
    }
}

impl Default for Document {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Token {
    StartTag {
        name: String,
        attributes: BTreeMap<String, String>,
        self_closing: bool,
    },
    EndTag(String),
    Text(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HtmlError {
    UnterminatedTag,
    MissingTagName,
    UnterminatedAttributeValue,
}

impl std::fmt::Display for HtmlError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UnterminatedTag => write!(formatter, "unterminated HTML tag"),
            Self::MissingTagName => write!(formatter, "missing HTML tag name"),
            Self::UnterminatedAttributeValue => write!(formatter, "unterminated attribute value"),
        }
    }
}

impl std::error::Error for HtmlError {}

pub fn tokenize(source: &str) -> Result<Vec<Token>, HtmlError> {
    let chars: Vec<char> = source.chars().collect();
    let mut tokens = Vec::new();
    let mut index = 0;

    while index < chars.len() {
        if chars[index] != '<' {
            let start = index;
            while index < chars.len() && chars[index] != '<' {
                index += 1;
            }
            tokens.push(Token::Text(chars[start..index].iter().collect()));
            continue;
        }

        if starts_with(&chars, index, "<!--") {
            index += 4;
            while index + 2 < chars.len() && !starts_with(&chars, index, "-->") {
                index += 1;
            }
            index = (index + 3).min(chars.len());
            continue;
        }

        if starts_with(&chars, index, "<!") {
            while index < chars.len() && chars[index] != '>' {
                index += 1;
            }
            index = (index + 1).min(chars.len());
            continue;
        }

        index += 1;
        if index >= chars.len() {
            return Err(HtmlError::UnterminatedTag);
        }

        if chars[index] == '/' {
            index += 1;
            skip_whitespace(&chars, &mut index);
            let name = read_name(&chars, &mut index);
            if name.is_empty() {
                return Err(HtmlError::MissingTagName);
            }
            while index < chars.len() && chars[index] != '>' {
                index += 1;
            }
            if index >= chars.len() {
                return Err(HtmlError::UnterminatedTag);
            }
            index += 1;
            tokens.push(Token::EndTag(name.to_ascii_lowercase()));
            continue;
        }

        skip_whitespace(&chars, &mut index);
        let name = read_name(&chars, &mut index).to_ascii_lowercase();
        if name.is_empty() {
            return Err(HtmlError::MissingTagName);
        }

        let mut attributes = BTreeMap::new();
        let mut self_closing = false;

        loop {
            skip_whitespace(&chars, &mut index);
            if index >= chars.len() {
                return Err(HtmlError::UnterminatedTag);
            }
            if chars[index] == '>' {
                index += 1;
                break;
            }
            if chars[index] == '/' && chars.get(index + 1) == Some(&'>') {
                self_closing = true;
                index += 2;
                break;
            }

            let attribute_name = read_name(&chars, &mut index).to_ascii_lowercase();
            if attribute_name.is_empty() {
                index += 1;
                continue;
            }
            skip_whitespace(&chars, &mut index);

            let value = if chars.get(index) == Some(&'=') {
                index += 1;
                skip_whitespace(&chars, &mut index);
                read_attribute_value(&chars, &mut index)?
            } else {
                String::new()
            };
            attributes.insert(attribute_name, value);
        }

        tokens.push(Token::StartTag {
            name,
            attributes,
            self_closing,
        });
    }

    Ok(tokens)
}

pub fn parse_document(source: &str) -> Result<Document, HtmlError> {
    let tokens = tokenize(source)?;
    let mut document = Document::new();
    let mut stack = vec![document.root()];

    for token in tokens {
        match token {
            Token::StartTag {
                name,
                attributes,
                self_closing,
            } => {
                let parent = *stack.last().expect("document root exists");
                let id = document.append(
                    parent,
                    NodeKind::Element(ElementData {
                        tag_name: name.clone(),
                        attributes,
                    }),
                );
                if !self_closing && !is_void_element(&name) {
                    stack.push(id);
                }
            }
            Token::EndTag(name) => {
                if let Some(position) = stack.iter().rposition(|id| {
                    matches!(
                        &document.node(*id).kind,
                        NodeKind::Element(element) if element.tag_name == name
                    )
                }) {
                    stack.truncate(position);
                }
            }
            Token::Text(text) => {
                if !text.is_empty() {
                    let parent = *stack.last().expect("document root exists");
                    document.append(parent, NodeKind::Text(text));
                }
            }
        }
    }

    Ok(document)
}

fn starts_with(chars: &[char], index: usize, pattern: &str) -> bool {
    pattern
        .chars()
        .enumerate()
        .all(|(offset, expected)| chars.get(index + offset) == Some(&expected))
}

fn skip_whitespace(chars: &[char], index: &mut usize) {
    while chars
        .get(*index)
        .is_some_and(|character| character.is_whitespace())
    {
        *index += 1;
    }
}

fn read_name(chars: &[char], index: &mut usize) -> String {
    let start = *index;
    while chars.get(*index).is_some_and(|character| {
        character.is_ascii_alphanumeric() || matches!(character, '-' | '_' | ':')
    }) {
        *index += 1;
    }
    chars[start..*index].iter().collect()
}

fn read_attribute_value(chars: &[char], index: &mut usize) -> Result<String, HtmlError> {
    let Some(character) = chars.get(*index).copied() else {
        return Err(HtmlError::UnterminatedAttributeValue);
    };

    if matches!(character, '"' | '\'') {
        let quote = character;
        *index += 1;
        let start = *index;
        while chars.get(*index).is_some_and(|current| *current != quote) {
            *index += 1;
        }
        if *index >= chars.len() {
            return Err(HtmlError::UnterminatedAttributeValue);
        }
        let value = chars[start..*index].iter().collect();
        *index += 1;
        Ok(value)
    } else {
        let start = *index;
        while chars
            .get(*index)
            .is_some_and(|current| !current.is_whitespace() && !matches!(current, '>' | '/'))
        {
            *index += 1;
        }
        Ok(chars[start..*index].iter().collect())
    }
}

fn is_void_element(name: &str) -> bool {
    matches!(
        name,
        "area"
            | "base"
            | "br"
            | "col"
            | "embed"
            | "hr"
            | "img"
            | "input"
            | "link"
            | "meta"
            | "source"
            | "track"
            | "wbr"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_nested_elements_and_attributes() {
        let document = parse_document("<div class='card'><h1>Hello</h1></div>").unwrap();
        assert!(document.dump().contains("<div class=\"card\">"));
        assert!(document.dump().contains("\"Hello\""));
    }

    #[test]
    fn ignores_comments_and_doctype() {
        let document = parse_document("<!doctype html><!-- x --><p>ok</p>").unwrap();
        assert!(document.dump().contains("<p>"));
    }
}
