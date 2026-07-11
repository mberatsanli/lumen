//! Hand-written JavaScript lexer: source text to a token stream with
//! line numbers for error messages. Comments and whitespace vanish here.

use std::fmt;

#[derive(Debug, Clone, PartialEq)]
pub enum Token {
    Number(f64),
    /// String literal, quotes removed and escapes resolved.
    Str(String),
    Ident(String),
    Keyword(Keyword),
    Punct(&'static str),
    /// `true` / `false`.
    Bool(bool),
    Null,
    Undefined,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Keyword {
    Var,
    Let,
    Const,
    Function,
    Return,
    If,
    Else,
    While,
    For,
    Break,
    Continue,
    Typeof,
    New,
    Of,
    In,
}

impl fmt::Display for Token {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Number(value) => write!(formatter, "{value}"),
            Self::Str(text) => write!(formatter, "\"{text}\""),
            Self::Ident(name) => write!(formatter, "{name}"),
            Self::Keyword(keyword) => write!(formatter, "{keyword:?}"),
            Self::Punct(punct) => write!(formatter, "{punct}"),
            Self::Bool(value) => write!(formatter, "{value}"),
            Self::Null => write!(formatter, "null"),
            Self::Undefined => write!(formatter, "undefined"),
        }
    }
}

/// A token plus the 1-based source line it started on.
#[derive(Debug, Clone, PartialEq)]
pub struct Spanned {
    pub token: Token,
    pub line: u32,
}

/// Multi-char punctuators first so maximal munch wins.
const PUNCTUATORS: [&str; 35] = [
    "===", "!==", "**=", "...", "=>", "==", "!=", "<=", ">=", "&&", "||", "??", "++", "--", "+=",
    "-=", "*=", "/=", "%=", "(", ")", "{", "}", "[", "]", ";", ",", ".", "?", ":", "<", ">", "=",
    "!", "%",
];

/// Tokenizes JavaScript source. Unknown characters are skipped (the
/// parser will complain about what is missing instead).
pub fn tokenize(source: &str) -> Result<Vec<Spanned>, String> {
    let chars: Vec<char> = source.chars().collect();
    let mut tokens = Vec::new();
    let mut index = 0;
    let mut line: u32 = 1;

    while index < chars.len() {
        let character = chars[index];
        if character == '\n' {
            line += 1;
            index += 1;
            continue;
        }
        if character.is_whitespace() {
            index += 1;
            continue;
        }
        // Comments.
        if character == '/' && chars.get(index + 1) == Some(&'/') {
            while index < chars.len() && chars[index] != '\n' {
                index += 1;
            }
            continue;
        }
        if character == '/' && chars.get(index + 1) == Some(&'*') {
            index += 2;
            while index < chars.len() && !(chars[index] == '*' && chars.get(index + 1) == Some(&'/')) {
                if chars[index] == '\n' {
                    line += 1;
                }
                index += 1;
            }
            index = (index + 2).min(chars.len());
            continue;
        }
        // String literals (single, double, and backtick without ${}).
        if character == '"' || character == '\'' || character == '`' {
            let quote = character;
            let start_line = line;
            index += 1;
            let mut text = String::new();
            while index < chars.len() && chars[index] != quote {
                let piece = chars[index];
                if piece == '\n' {
                    line += 1;
                }
                if piece == '\\' && index + 1 < chars.len() {
                    index += 1;
                    text.push(match chars[index] {
                        'n' => '\n',
                        't' => '\t',
                        'r' => '\r',
                        '0' => '\0',
                        other => other,
                    });
                } else {
                    text.push(piece);
                }
                index += 1;
            }
            if index >= chars.len() {
                return Err(format!("line {start_line}: unterminated string"));
            }
            index += 1; // closing quote
            tokens.push(Spanned {
                token: Token::Str(text),
                line: start_line,
            });
            continue;
        }
        // Numbers.
        if character.is_ascii_digit()
            || (character == '.' && chars.get(index + 1).is_some_and(char::is_ascii_digit))
        {
            let start = index;
            while index < chars.len()
                && (chars[index].is_ascii_alphanumeric() || chars[index] == '.' || chars[index] == '_')
            {
                // Stop before a second dot that starts a method call: 1.toFixed
                if chars[index] == '.' && chars[start..index].contains(&'.') {
                    break;
                }
                index += 1;
            }
            let text: String = chars[start..index].iter().collect();
            let value: f64 = if let Some(hex) = text.strip_prefix("0x").or_else(|| text.strip_prefix("0X")) {
                i64::from_str_radix(hex, 16)
                    .map_err(|_| format!("line {line}: bad number {text}"))? as f64
            } else {
                text.trim_end_matches('.')
                    .parse()
                    .map_err(|_| format!("line {line}: bad number {text}"))?
            };
            tokens.push(Spanned {
                token: Token::Number(value),
                line,
            });
            continue;
        }
        // Identifiers and keywords.
        if character.is_alphabetic() || character == '_' || character == '$' {
            let start = index;
            while index < chars.len()
                && (chars[index].is_alphanumeric() || chars[index] == '_' || chars[index] == '$')
            {
                index += 1;
            }
            let word: String = chars[start..index].iter().collect();
            let token = match word.as_str() {
                "var" => Token::Keyword(Keyword::Var),
                "let" => Token::Keyword(Keyword::Let),
                "const" => Token::Keyword(Keyword::Const),
                "function" => Token::Keyword(Keyword::Function),
                "return" => Token::Keyword(Keyword::Return),
                "if" => Token::Keyword(Keyword::If),
                "else" => Token::Keyword(Keyword::Else),
                "while" => Token::Keyword(Keyword::While),
                "for" => Token::Keyword(Keyword::For),
                "break" => Token::Keyword(Keyword::Break),
                "continue" => Token::Keyword(Keyword::Continue),
                "typeof" => Token::Keyword(Keyword::Typeof),
                "new" => Token::Keyword(Keyword::New),
                "of" => Token::Keyword(Keyword::Of),
                "in" => Token::Keyword(Keyword::In),
                "true" => Token::Bool(true),
                "false" => Token::Bool(false),
                "null" => Token::Null,
                "undefined" => Token::Undefined,
                _ => Token::Ident(word),
            };
            tokens.push(Spanned { token, line });
            continue;
        }
        // Punctuators (maximal munch). `/` doubles as division.
        let mut matched = false;
        for punct in PUNCTUATORS {
            let punct_chars: Vec<char> = punct.chars().collect();
            if chars[index..].starts_with(&punct_chars) {
                tokens.push(Spanned {
                    token: Token::Punct(punct),
                    line,
                });
                index += punct_chars.len();
                matched = true;
                break;
            }
        }
        if matched {
            continue;
        }
        if matches!(character, '/' | '+' | '-' | '*' | '&' | '|') {
            // Single-char operators not in the multi-char table.
            let punct: &'static str = match character {
                '/' => "/",
                '+' => "+",
                '-' => "-",
                '*' => "*",
                '&' => "&",
                _ => "|",
            };
            tokens.push(Spanned {
                token: Token::Punct(punct),
                line,
            });
            index += 1;
            continue;
        }
        return Err(format!("line {line}: unexpected character '{character}'"));
    }
    Ok(tokens)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kinds(source: &str) -> Vec<Token> {
        tokenize(source).unwrap().into_iter().map(|spanned| spanned.token).collect()
    }

    #[test]
    fn tokenizes_expressions() {
        assert_eq!(
            kinds("let x = 1 + 2.5; // note"),
            vec![
                Token::Keyword(Keyword::Let),
                Token::Ident("x".to_string()),
                Token::Punct("="),
                Token::Number(1.0),
                Token::Punct("+"),
                Token::Number(2.5),
                Token::Punct(";"),
            ]
        );
    }

    #[test]
    fn strings_resolve_escapes() {
        assert_eq!(kinds(r#" "a\nb" "#), vec![Token::Str("a\nb".to_string())]);
    }

    #[test]
    fn maximal_munch_operators() {
        assert_eq!(
            kinds("a === b => c ++"),
            vec![
                Token::Ident("a".to_string()),
                Token::Punct("==="),
                Token::Ident("b".to_string()),
                Token::Punct("=>"),
                Token::Ident("c".to_string()),
                Token::Punct("++"),
            ]
        );
    }

    #[test]
    fn line_numbers_track_newlines() {
        let tokens = tokenize("a\nb\n\nc").unwrap();
        let lines: Vec<u32> = tokens.iter().map(|spanned| spanned.line).collect();
        assert_eq!(lines, vec![1, 2, 4]);
    }
}
