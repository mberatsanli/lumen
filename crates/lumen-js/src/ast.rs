//! The syntax tree the parser produces and the interpreter walks.

/// A whole program (or function body): a statement list.
pub type Block = Vec<Statement>;

#[derive(Debug, Clone)]
pub enum Statement {
    /// `let`/`const`/`var` — the engine scopes them all the same way
    /// (function/block scope refinements are out of subset).
    Declare {
        name: String,
        value: Option<Expression>,
    },
    Function {
        name: String,
        params: Vec<String>,
        body: Block,
    },
    Return(Option<Expression>),
    If {
        condition: Expression,
        then_branch: Block,
        else_branch: Option<Block>,
    },
    While {
        condition: Expression,
        body: Block,
    },
    /// Classic `for (init; condition; update)`.
    For {
        init: Option<Box<Statement>>,
        condition: Option<Expression>,
        update: Option<Expression>,
        body: Block,
    },
    /// `for (let name of iterable)`.
    ForOf {
        name: String,
        iterable: Expression,
        body: Block,
    },
    Break,
    Continue,
    Expression(Expression),
    Block(Block),
}

#[derive(Debug, Clone)]
pub enum Expression {
    Number(f64),
    Str(String),
    Bool(bool),
    Null,
    Undefined,
    Ident(String),
    Array(Vec<Expression>),
    /// Object literal: (key, value) pairs.
    Object(Vec<(String, Expression)>),
    /// Anonymous function or arrow function.
    Function {
        params: Vec<String>,
        body: Block,
    },
    Unary {
        op: &'static str,
        operand: Box<Expression>,
    },
    /// Prefix or postfix `++`/`--` on an assignable target.
    Update {
        op: &'static str,
        prefix: bool,
        target: Box<Expression>,
    },
    Binary {
        op: &'static str,
        left: Box<Expression>,
        right: Box<Expression>,
    },
    /// `&&`, `||`, `??` — short-circuiting.
    Logical {
        op: &'static str,
        left: Box<Expression>,
        right: Box<Expression>,
    },
    Conditional {
        condition: Box<Expression>,
        then_value: Box<Expression>,
        else_value: Box<Expression>,
    },
    /// `target op= value` (op empty for plain `=`).
    Assign {
        op: &'static str,
        target: Box<Expression>,
        value: Box<Expression>,
    },
    Call {
        callee: Box<Expression>,
        arguments: Vec<Expression>,
    },
    /// `object.property`.
    Member {
        object: Box<Expression>,
        property: String,
    },
    /// `object[index]`.
    Index {
        object: Box<Expression>,
        index: Box<Expression>,
    },
}
