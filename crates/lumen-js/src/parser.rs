//! Recursive-descent parser: tokens to the [`ast`](crate::ast) tree.
//! Precedence climbing for binary operators; arrow functions are
//! recognized by lookahead from `(` or a bare identifier before `=>`.

use crate::ast::{Block, Expression, Statement};
use crate::lexer::{Keyword, Spanned, Token, tokenize};

pub struct Parser {
    tokens: Vec<Spanned>,
    index: usize,
}

/// Parses a whole program.
pub fn parse_program(source: &str) -> Result<Block, String> {
    let tokens = tokenize(source)?;
    let mut parser = Parser { tokens, index: 0 };
    let mut statements = Vec::new();
    while !parser.done() {
        statements.push(parser.statement()?);
    }
    Ok(statements)
}

impl Parser {
    fn done(&self) -> bool {
        self.index >= self.tokens.len()
    }

    fn peek(&self) -> Option<&Token> {
        self.tokens.get(self.index).map(|spanned| &spanned.token)
    }

    fn peek_at(&self, offset: usize) -> Option<&Token> {
        self.tokens
            .get(self.index + offset)
            .map(|spanned| &spanned.token)
    }

    fn line(&self) -> u32 {
        self.tokens
            .get(self.index.min(self.tokens.len().saturating_sub(1)))
            .map_or(0, |spanned| spanned.line)
    }

    fn advance(&mut self) -> Option<Token> {
        let token = self.tokens.get(self.index).map(|spanned| spanned.token.clone());
        self.index += 1;
        token
    }

    fn eat_punct(&mut self, punct: &'static str) -> bool {
        if self.peek() == Some(&Token::Punct(punct)) {
            // Compare against the table entry; the stored str is 'static.
            self.index += 1;
            return true;
        }
        false
    }

    fn expect_punct(&mut self, punct: &'static str) -> Result<(), String> {
        if self.eat_punct(punct) {
            Ok(())
        } else {
            Err(format!(
                "line {}: expected '{punct}', found {}",
                self.line(),
                self.peek().map_or("end of input".to_string(), ToString::to_string)
            ))
        }
    }

    fn eat_keyword(&mut self, keyword: Keyword) -> bool {
        if self.peek() == Some(&Token::Keyword(keyword)) {
            self.index += 1;
            return true;
        }
        false
    }

    fn expect_ident(&mut self) -> Result<String, String> {
        match self.advance() {
            Some(Token::Ident(name)) => Ok(name),
            other => Err(format!(
                "line {}: expected a name, found {}",
                self.line(),
                other.map_or("end of input".to_string(), |token| token.to_string())
            )),
        }
    }

    /// Semicolons are optional where a line break or `}` ends the
    /// statement (a practical ASI approximation).
    fn eat_semicolon(&mut self) {
        self.eat_punct(";");
    }

    fn statement(&mut self) -> Result<Statement, String> {
        match self.peek() {
            Some(Token::Keyword(Keyword::Var | Keyword::Let | Keyword::Const)) => {
                self.index += 1;
                let statement = self.declaration()?;
                self.eat_semicolon();
                Ok(statement)
            }
            Some(Token::Keyword(Keyword::Function)) => {
                self.index += 1;
                let name = self.expect_ident()?;
                let params = self.parameter_list()?;
                let body = self.braced_block()?;
                Ok(Statement::Function { name, params, body })
            }
            Some(Token::Keyword(Keyword::Return)) => {
                self.index += 1;
                let value = if self.peek() == Some(&Token::Punct(";"))
                    || self.peek() == Some(&Token::Punct("}"))
                    || self.done()
                {
                    None
                } else {
                    Some(self.expression()?)
                };
                self.eat_semicolon();
                Ok(Statement::Return(value))
            }
            Some(Token::Keyword(Keyword::If)) => {
                self.index += 1;
                self.expect_punct("(")?;
                let condition = self.expression()?;
                self.expect_punct(")")?;
                let then_branch = self.branch()?;
                let else_branch = if self.eat_keyword(Keyword::Else) {
                    Some(self.branch()?)
                } else {
                    None
                };
                Ok(Statement::If {
                    condition,
                    then_branch,
                    else_branch,
                })
            }
            Some(Token::Keyword(Keyword::While)) => {
                self.index += 1;
                self.expect_punct("(")?;
                let condition = self.expression()?;
                self.expect_punct(")")?;
                let body = self.branch()?;
                Ok(Statement::While { condition, body })
            }
            Some(Token::Keyword(Keyword::For)) => {
                self.index += 1;
                self.for_statement()
            }
            Some(Token::Keyword(Keyword::Break)) => {
                self.index += 1;
                self.eat_semicolon();
                Ok(Statement::Break)
            }
            Some(Token::Keyword(Keyword::Continue)) => {
                self.index += 1;
                self.eat_semicolon();
                Ok(Statement::Continue)
            }
            Some(Token::Punct("{")) => Ok(Statement::Block(self.braced_block()?)),
            Some(Token::Punct(";")) => {
                self.index += 1;
                Ok(Statement::Block(Vec::new()))
            }
            _ => {
                let expression = self.expression()?;
                self.eat_semicolon();
                Ok(Statement::Expression(expression))
            }
        }
    }

    fn declaration(&mut self) -> Result<Statement, String> {
        let name = self.expect_ident()?;
        let value = if self.eat_punct("=") {
            Some(self.assignment()?)
        } else {
            None
        };
        Ok(Statement::Declare { name, value })
    }

    fn for_statement(&mut self) -> Result<Statement, String> {
        self.expect_punct("(")?;
        // for (let x of xs) { ... }
        if matches!(
            self.peek(),
            Some(Token::Keyword(Keyword::Var | Keyword::Let | Keyword::Const))
        ) && matches!(self.peek_at(2), Some(Token::Keyword(Keyword::Of | Keyword::In)))
        {
            self.index += 1;
            let name = self.expect_ident()?;
            self.index += 1; // of / in
            let iterable = self.expression()?;
            self.expect_punct(")")?;
            let body = self.branch()?;
            return Ok(Statement::ForOf {
                name,
                iterable,
                body,
            });
        }
        let init = if self.eat_punct(";") {
            None
        } else {
            let statement = if matches!(
                self.peek(),
                Some(Token::Keyword(Keyword::Var | Keyword::Let | Keyword::Const))
            ) {
                self.index += 1;
                self.declaration()?
            } else {
                Statement::Expression(self.expression()?)
            };
            self.expect_punct(";")?;
            Some(Box::new(statement))
        };
        let condition = if self.peek() == Some(&Token::Punct(";")) {
            None
        } else {
            Some(self.expression()?)
        };
        self.expect_punct(";")?;
        let update = if self.peek() == Some(&Token::Punct(")")) {
            None
        } else {
            Some(self.expression()?)
        };
        self.expect_punct(")")?;
        let body = self.branch()?;
        Ok(Statement::For {
            init,
            condition,
            update,
            body,
        })
    }

    /// A `{}` block or a single statement (if/while/for bodies).
    fn branch(&mut self) -> Result<Block, String> {
        if self.peek() == Some(&Token::Punct("{")) {
            self.braced_block()
        } else {
            Ok(vec![self.statement()?])
        }
    }

    fn braced_block(&mut self) -> Result<Block, String> {
        self.expect_punct("{")?;
        let mut statements = Vec::new();
        while self.peek() != Some(&Token::Punct("}")) {
            if self.done() {
                return Err(format!("line {}: unclosed block", self.line()));
            }
            statements.push(self.statement()?);
        }
        self.expect_punct("}")?;
        Ok(statements)
    }

    fn parameter_list(&mut self) -> Result<Vec<String>, String> {
        self.expect_punct("(")?;
        let mut params = Vec::new();
        while self.peek() != Some(&Token::Punct(")")) {
            params.push(self.expect_ident()?);
            if !self.eat_punct(",") {
                break;
            }
        }
        self.expect_punct(")")?;
        Ok(params)
    }

    // ---- expressions, lowest to highest precedence ----

    pub(crate) fn expression(&mut self) -> Result<Expression, String> {
        self.assignment()
    }

    fn assignment(&mut self) -> Result<Expression, String> {
        // Arrow functions: `x => ...` or `(a, b) => ...`.
        if let Some(arrow) = self.try_arrow_function()? {
            return Ok(arrow);
        }
        let target = self.conditional()?;
        for op in ["=", "+=", "-=", "*=", "/=", "%="] {
            if self.peek() == Some(&Token::Punct(op)) {
                self.index += 1;
                let value = self.assignment()?;
                return Ok(Expression::Assign {
                    op: match op {
                        "+=" => "+",
                        "-=" => "-",
                        "*=" => "*",
                        "/=" => "/",
                        "%=" => "%",
                        _ => "",
                    },
                    target: Box::new(target),
                    value: Box::new(value),
                });
            }
        }
        Ok(target)
    }

    /// Recognizes an arrow function by scanning ahead for `=>`.
    fn try_arrow_function(&mut self) -> Result<Option<Expression>, String> {
        // Bare identifier arrow: `x => body`.
        if let (Some(Token::Ident(name)), Some(Token::Punct("=>"))) =
            (self.peek(), self.peek_at(1))
        {
            let params = vec![name.clone()];
            self.index += 2;
            return Ok(Some(self.arrow_body(params)?));
        }
        // Parenthesized parameters: `(a, b) => body`.
        if self.peek() == Some(&Token::Punct("(")) {
            let mut depth = 0usize;
            let mut offset = 0usize;
            loop {
                match self.peek_at(offset) {
                    Some(Token::Punct("(")) => depth += 1,
                    Some(Token::Punct(")")) => {
                        depth -= 1;
                        if depth == 0 {
                            break;
                        }
                    }
                    None => return Ok(None),
                    _ => {}
                }
                offset += 1;
            }
            if self.peek_at(offset + 1) == Some(&Token::Punct("=>")) {
                let params = self.parameter_list()?;
                self.expect_punct("=>")?;
                return Ok(Some(self.arrow_body(params)?));
            }
        }
        Ok(None)
    }

    fn arrow_body(&mut self, params: Vec<String>) -> Result<Expression, String> {
        let body = if self.peek() == Some(&Token::Punct("{")) {
            self.braced_block()?
        } else {
            vec![Statement::Return(Some(self.assignment()?))]
        };
        Ok(Expression::Function { params, body })
    }

    fn conditional(&mut self) -> Result<Expression, String> {
        let condition = self.logical_or()?;
        if self.eat_punct("?") {
            let then_value = self.assignment()?;
            self.expect_punct(":")?;
            let else_value = self.assignment()?;
            return Ok(Expression::Conditional {
                condition: Box::new(condition),
                then_value: Box::new(then_value),
                else_value: Box::new(else_value),
            });
        }
        Ok(condition)
    }

    fn logical_or(&mut self) -> Result<Expression, String> {
        let mut left = self.logical_and()?;
        loop {
            let op = if self.eat_punct("||") {
                "||"
            } else if self.eat_punct("??") {
                "??"
            } else {
                break;
            };
            let right = self.logical_and()?;
            left = Expression::Logical {
                op,
                left: Box::new(left),
                right: Box::new(right),
            };
        }
        Ok(left)
    }

    fn logical_and(&mut self) -> Result<Expression, String> {
        let mut left = self.equality()?;
        while self.eat_punct("&&") {
            let right = self.equality()?;
            left = Expression::Logical {
                op: "&&",
                left: Box::new(left),
                right: Box::new(right),
            };
        }
        Ok(left)
    }

    fn equality(&mut self) -> Result<Expression, String> {
        let mut left = self.relational()?;
        loop {
            let op = if self.eat_punct("===") {
                "==="
            } else if self.eat_punct("!==") {
                "!=="
            } else if self.eat_punct("==") {
                "=="
            } else if self.eat_punct("!=") {
                "!="
            } else {
                break;
            };
            let right = self.relational()?;
            left = Expression::Binary {
                op,
                left: Box::new(left),
                right: Box::new(right),
            };
        }
        Ok(left)
    }

    fn relational(&mut self) -> Result<Expression, String> {
        let mut left = self.additive()?;
        loop {
            let op = if self.eat_punct("<=") {
                "<="
            } else if self.eat_punct(">=") {
                ">="
            } else if self.eat_punct("<") {
                "<"
            } else if self.eat_punct(">") {
                ">"
            } else {
                break;
            };
            let right = self.additive()?;
            left = Expression::Binary {
                op,
                left: Box::new(left),
                right: Box::new(right),
            };
        }
        Ok(left)
    }

    fn additive(&mut self) -> Result<Expression, String> {
        let mut left = self.multiplicative()?;
        loop {
            let op = if self.eat_punct("+") {
                "+"
            } else if self.eat_punct("-") {
                "-"
            } else {
                break;
            };
            let right = self.multiplicative()?;
            left = Expression::Binary {
                op,
                left: Box::new(left),
                right: Box::new(right),
            };
        }
        Ok(left)
    }

    fn multiplicative(&mut self) -> Result<Expression, String> {
        let mut left = self.unary()?;
        loop {
            let op = if self.eat_punct("*") {
                "*"
            } else if self.eat_punct("/") {
                "/"
            } else if self.eat_punct("%") {
                "%"
            } else {
                break;
            };
            let right = self.unary()?;
            left = Expression::Binary {
                op,
                left: Box::new(left),
                right: Box::new(right),
            };
        }
        Ok(left)
    }

    fn unary(&mut self) -> Result<Expression, String> {
        for op in ["!", "-", "+"] {
            if self.peek() == Some(&Token::Punct(op)) {
                self.index += 1;
                let operand = self.unary()?;
                return Ok(Expression::Unary {
                    op,
                    operand: Box::new(operand),
                });
            }
        }
        if self.eat_keyword(Keyword::Typeof) {
            let operand = self.unary()?;
            return Ok(Expression::Unary {
                op: "typeof",
                operand: Box::new(operand),
            });
        }
        for op in ["++", "--"] {
            if self.peek() == Some(&Token::Punct(op)) {
                self.index += 1;
                let target = self.unary()?;
                return Ok(Expression::Update {
                    op,
                    prefix: true,
                    target: Box::new(target),
                });
            }
        }
        self.postfix()
    }

    fn postfix(&mut self) -> Result<Expression, String> {
        let mut expression = self.call_or_member()?;
        for op in ["++", "--"] {
            if self.peek() == Some(&Token::Punct(op)) {
                self.index += 1;
                expression = Expression::Update {
                    op,
                    prefix: false,
                    target: Box::new(expression),
                };
            }
        }
        Ok(expression)
    }

    fn call_or_member(&mut self) -> Result<Expression, String> {
        let mut expression = self.primary()?;
        loop {
            if self.eat_punct(".") {
                let property = self.expect_ident()?;
                expression = Expression::Member {
                    object: Box::new(expression),
                    property,
                };
            } else if self.eat_punct("[") {
                let index = self.expression()?;
                self.expect_punct("]")?;
                expression = Expression::Index {
                    object: Box::new(expression),
                    index: Box::new(index),
                };
            } else if self.eat_punct("(") {
                let mut arguments = Vec::new();
                while self.peek() != Some(&Token::Punct(")")) {
                    arguments.push(self.assignment()?);
                    if !self.eat_punct(",") {
                        break;
                    }
                }
                self.expect_punct(")")?;
                expression = Expression::Call {
                    callee: Box::new(expression),
                    arguments,
                };
            } else {
                break;
            }
        }
        Ok(expression)
    }

    fn primary(&mut self) -> Result<Expression, String> {
        match self.advance() {
            Some(Token::Number(value)) => Ok(Expression::Number(value)),
            Some(Token::Str(text)) => Ok(Expression::Str(text)),
            Some(Token::Bool(value)) => Ok(Expression::Bool(value)),
            Some(Token::Null) => Ok(Expression::Null),
            Some(Token::Undefined) => Ok(Expression::Undefined),
            Some(Token::Ident(name)) => Ok(Expression::Ident(name)),
            Some(Token::Keyword(Keyword::Function)) => {
                // Anonymous function expression (a name is allowed and
                // ignored).
                if matches!(self.peek(), Some(Token::Ident(_))) {
                    self.index += 1;
                }
                let params = self.parameter_list()?;
                let body = self.braced_block()?;
                Ok(Expression::Function { params, body })
            }
            Some(Token::Punct("(")) => {
                let expression = self.expression()?;
                self.expect_punct(")")?;
                Ok(expression)
            }
            Some(Token::Punct("[")) => {
                let mut items = Vec::new();
                while self.peek() != Some(&Token::Punct("]")) {
                    items.push(self.assignment()?);
                    if !self.eat_punct(",") {
                        break;
                    }
                }
                self.expect_punct("]")?;
                Ok(Expression::Array(items))
            }
            Some(Token::Punct("{")) => {
                let mut entries = Vec::new();
                while self.peek() != Some(&Token::Punct("}")) {
                    let key = match self.advance() {
                        Some(Token::Ident(name)) => name,
                        Some(Token::Str(text)) => text,
                        Some(Token::Number(value)) => value.to_string(),
                        other => {
                            return Err(format!(
                                "line {}: bad object key {:?}",
                                self.line(),
                                other
                            ));
                        }
                    };
                    let value = if self.eat_punct(":") {
                        self.assignment()?
                    } else {
                        // Shorthand { x } — the key doubles as a variable.
                        Expression::Ident(key.clone())
                    };
                    entries.push((key, value));
                    if !self.eat_punct(",") {
                        break;
                    }
                }
                self.expect_punct("}")?;
                Ok(Expression::Object(entries))
            }
            other => Err(format!(
                "line {}: unexpected {}",
                self.line(),
                other.map_or("end of input".to_string(), |token| token.to_string())
            )),
        }
    }
}
