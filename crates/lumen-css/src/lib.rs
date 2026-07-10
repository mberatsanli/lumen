#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Selector {
    Tag(String),
    Class(String),
    Id(String),
}

impl Selector {
    #[must_use]
    pub const fn specificity(&self) -> u32 {
        match self {
            Self::Tag(_) => 1,
            Self::Class(_) => 10,
            Self::Id(_) => 100,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Declaration {
    pub name: String,
    pub value: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rule {
    pub selectors: Vec<Selector>,
    pub declarations: Vec<Declaration>,
    pub source_order: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Stylesheet {
    pub rules: Vec<Rule>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CssError {
    UnterminatedRule,
    InvalidSelector(String),
}

impl std::fmt::Display for CssError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UnterminatedRule => write!(formatter, "unterminated CSS rule"),
            Self::InvalidSelector(selector) => write!(formatter, "invalid selector: {selector}"),
        }
    }
}

impl std::error::Error for CssError {}

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

        let selectors = selector_source
            .split(',')
            .map(str::trim)
            .filter(|selector| !selector.is_empty())
            .map(parse_selector)
            .collect::<Result<Vec<_>, _>>()?;

        let declarations = declaration_source
            .split(';')
            .filter_map(|raw| {
                let (name, value) = raw.split_once(':')?;
                let name = name.trim();
                let value = value.trim();
                (!name.is_empty() && !value.is_empty()).then(|| Declaration {
                    name: name.to_ascii_lowercase(),
                    value: value.to_string(),
                })
            })
            .collect();

        if !selectors.is_empty() {
            rules.push(Rule {
                selectors,
                declarations,
                source_order,
            });
            source_order += 1;
        }
        rest = &after_open[close + 1..];
    }

    Ok(Stylesheet { rules })
}

fn parse_selector(source: &str) -> Result<Selector, CssError> {
    if let Some(id) = source.strip_prefix('#') {
        if !id.is_empty() {
            return Ok(Selector::Id(id.to_string()));
        }
    } else if let Some(class) = source.strip_prefix('.') {
        if !class.is_empty() {
            return Ok(Selector::Class(class.to_string()));
        }
    } else if !source.is_empty()
        && source
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || character == '-')
    {
        return Ok(Selector::Tag(source.to_ascii_lowercase()));
    }

    Err(CssError::InvalidSelector(source.to_string()))
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

    #[test]
    fn parses_selectors_and_declarations() {
        let sheet = parse_stylesheet(".card, #main { width: 400px; color: #222; }").unwrap();
        assert_eq!(sheet.rules.len(), 1);
        assert_eq!(sheet.rules[0].selectors.len(), 2);
        assert_eq!(sheet.rules[0].declarations.len(), 2);
    }
}
