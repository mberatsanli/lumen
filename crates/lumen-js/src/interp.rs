//! Tree-walking evaluator: values, scopes, control flow, built-ins and
//! the host (DOM) bridge.
//!
//! Objects and arrays are shared mutable references (`Rc<RefCell<..>>`),
//! functions are closures over their defining scope. Built-ins are enum
//! tags dispatched in one match — no boxed closures to store, and every
//! native gets access to the interpreter (to call callbacks) and the
//! host (to touch the page).

use crate::ast::{Block, Expression, Statement};
use crate::parser::parse_program;
use std::cell::RefCell;
use std::collections::HashMap;
use std::fmt::Write as _;
use std::rc::Rc;

/// Element handles the host hands out (the engine's node ids).
pub type DomNode = u64;

/// What the page offers to scripts. The browser implements this; tests
/// use a stub.
pub trait Host {
    fn console_log(&mut self, message: &str);
    fn get_element_by_id(&mut self, id: &str) -> Option<DomNode>;
    fn query_selector_all(&mut self, selector: &str) -> Vec<DomNode>;
    fn get_text(&mut self, node: DomNode) -> String;
    fn set_text(&mut self, node: DomNode, text: &str);
    fn get_value(&mut self, node: DomNode) -> String;
    fn set_value(&mut self, node: DomNode, value: &str);
    fn get_attribute(&mut self, node: DomNode, name: &str) -> Option<String>;
    fn set_attribute(&mut self, node: DomNode, name: &str, value: &str);
    fn set_style(&mut self, node: DomNode, property: &str, value: &str);
    fn random(&mut self) -> f64;
}

/// A JavaScript value.
#[derive(Clone)]
pub enum Value {
    Undefined,
    Null,
    Bool(bool),
    Number(f64),
    Str(String),
    Object(Rc<RefCell<Object>>),
}

/// Object payload: plain objects, arrays, functions and DOM elements are
/// all objects with optional extras.
pub struct Object {
    pub properties: HashMap<String, Value>,
    /// `Some` for arrays: ordered element storage.
    pub array: Option<Vec<Value>>,
    /// `Some` for callables.
    pub call: Option<Callable>,
    /// `Some` for DOM element wrappers: the engine node id.
    pub dom_node: Option<DomNode>,
}

impl Object {
    fn plain() -> Self {
        Self {
            properties: HashMap::new(),
            array: None,
            call: None,
            dom_node: None,
        }
    }
}

#[derive(Clone)]
pub enum Callable {
    /// A script function: parameters, body, captured scope.
    Function {
        params: Rc<Vec<String>>,
        body: Rc<Block>,
        closure: Scope,
    },
    Native(Native),
}

/// Built-in functions, dispatched by tag.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Native {
    ConsoleLog,
    MathAbs,
    MathFloor,
    MathCeil,
    MathRound,
    MathSqrt,
    MathMin,
    MathMax,
    MathRandom,
    MathPow,
    JsonStringify,
    ParseInt,
    ParseFloat,
    NumberFn,
    StringFn,
    SetTimeout,
    SetInterval,
    // String methods (this = string).
    StrToUpperCase,
    StrToLowerCase,
    StrTrim,
    StrIncludes,
    StrIndexOf,
    StrSlice,
    StrSplit,
    StrCharAt,
    StrRepeat,
    StrReplace,
    // Array methods (this = array object).
    ArrPush,
    ArrPop,
    ArrJoin,
    ArrIndexOf,
    ArrIncludes,
    ArrSlice,
    ArrMap,
    ArrFilter,
    ArrForEach,
    // DOM.
    DocGetElementById,
    DocQuerySelector,
    DocQuerySelectorAll,
    ElAddEventListener,
    ElGetAttribute,
    ElSetAttribute,
}

/// Lexical scope: a variable map chained to its parent.
#[derive(Clone)]
pub struct Scope(Rc<RefCell<ScopeData>>);

struct ScopeData {
    variables: HashMap<String, Value>,
    parent: Option<Scope>,
}

impl Scope {
    fn new(parent: Option<Scope>) -> Self {
        Self(Rc::new(RefCell::new(ScopeData {
            variables: HashMap::new(),
            parent,
        })))
    }

    fn declare(&self, name: &str, value: Value) {
        self.0.borrow_mut().variables.insert(name.to_string(), value);
    }

    fn get(&self, name: &str) -> Option<Value> {
        let data = self.0.borrow();
        if let Some(value) = data.variables.get(name) {
            return Some(value.clone());
        }
        data.parent.as_ref().and_then(|parent| parent.get(name))
    }

    fn set(&self, name: &str, value: Value) -> bool {
        let mut data = self.0.borrow_mut();
        if let Some(slot) = data.variables.get_mut(name) {
            *slot = value;
            return true;
        }
        match &data.parent {
            Some(parent) => parent.set(name, value),
            None => false,
        }
    }
}

/// Why a block stopped evaluating.
enum Flow {
    Normal,
    Break,
    Continue,
    Return(Value),
}

/// A queued `addEventListener` registration.
struct Listener {
    node: DomNode,
    event: String,
    handler: Value,
}

/// A pending `setTimeout`/`setInterval`.
struct Timer {
    due_ms: f64,
    interval_ms: Option<f64>,
    handler: Value,
}

/// One page's script world: globals, event listeners and timers.
pub struct Runtime {
    globals: Scope,
    listeners: Vec<Listener>,
    timers: Vec<Timer>,
    /// Element wrappers by node, so listener identity works naturally.
    elements: HashMap<DomNode, Value>,
    now_ms: f64,
}

impl Default for Runtime {
    fn default() -> Self {
        Self::new()
    }
}

impl Runtime {
    #[must_use]
    pub fn new() -> Self {
        let globals = Scope::new(None);
        install_globals(&globals);
        Self {
            globals,
            listeners: Vec::new(),
            timers: Vec::new(),
            elements: HashMap::new(),
            now_ms: 0.0,
        }
    }

    /// Runs a script in the global scope. Errors become messages (the
    /// page keeps working, like real browsers).
    pub fn run(&mut self, source: &str, host: &mut dyn Host) -> Result<(), String> {
        let program = parse_program(source)?;
        let scope = self.globals.clone();
        let mut interp = Interp {
            runtime: self,
            host,
            depth: 0,
        };
        match interp.run_block(&program, &scope) {
            Ok(_) => Ok(()),
            Err(error) => Err(error),
        }
    }

    /// Whether any listener is registered for (node, event).
    #[must_use]
    pub fn has_listener(&self, node: DomNode, event: &str) -> bool {
        self.listeners
            .iter()
            .any(|listener| listener.node == node && listener.event == event)
    }

    /// Whether any listeners at all are registered for an event type.
    #[must_use]
    pub fn has_any_listener(&self, event: &str) -> bool {
        self.listeners.iter().any(|listener| listener.event == event)
    }

    /// Dispatches an event to every matching listener. Returns whether
    /// any handler ran.
    pub fn dispatch_event(&mut self, node: DomNode, event: &str, host: &mut dyn Host) -> bool {
        let handlers: Vec<Value> = self
            .listeners
            .iter()
            .filter(|listener| listener.node == node && listener.event == event)
            .map(|listener| listener.handler.clone())
            .collect();
        if handlers.is_empty() {
            return false;
        }
        let target = self.element_value(node);
        for handler in handlers {
            let event_object = {
                let mut object = Object::plain();
                object
                    .properties
                    .insert("type".to_string(), Value::Str(event.to_string()));
                object.properties.insert("target".to_string(), target.clone());
                Value::Object(Rc::new(RefCell::new(object)))
            };
            let mut interp = Interp {
                runtime: self,
                host,
                depth: 0,
            };
            if let Err(error) = interp.call_value(&handler, vec![event_object]) {
                host.console_log(&format!("script error: {error}"));
            }
        }
        true
    }

    /// Advances the clock and runs due timers. Returns whether any ran.
    pub fn run_timers(&mut self, now_ms: f64, host: &mut dyn Host) -> bool {
        self.now_ms = now_ms;
        let mut ran = false;
        loop {
            let Some(index) = self
                .timers
                .iter()
                .position(|timer| timer.due_ms <= now_ms)
            else {
                break;
            };
            let timer = self.timers.remove(index);
            if let Some(interval) = timer.interval_ms {
                self.timers.push(Timer {
                    due_ms: now_ms + interval,
                    interval_ms: Some(interval),
                    handler: timer.handler.clone(),
                });
            }
            let mut interp = Interp {
                runtime: self,
                host,
                depth: 0,
            };
            if let Err(error) = interp.call_value(&timer.handler, Vec::new()) {
                host.console_log(&format!("script error: {error}"));
            }
            ran = true;
        }
        ran
    }

    /// Whether timers are waiting (the shell keeps frames coming).
    #[must_use]
    pub fn has_timers(&self) -> bool {
        !self.timers.is_empty()
    }

    /// The shared element wrapper for a node.
    fn element_value(&mut self, node: DomNode) -> Value {
        if let Some(value) = self.elements.get(&node) {
            return value.clone();
        }
        let mut object = Object::plain();
        object.dom_node = Some(node);
        for (name, native) in [
            ("addEventListener", Native::ElAddEventListener),
            ("getAttribute", Native::ElGetAttribute),
            ("setAttribute", Native::ElSetAttribute),
        ] {
            object
                .properties
                .insert(name.to_string(), native_value(native));
        }
        let value = Value::Object(Rc::new(RefCell::new(object)));
        self.elements.insert(node, value.clone());
        value
    }
}

fn native_value(native: Native) -> Value {
    let mut object = Object::plain();
    object.call = Some(Callable::Native(native));
    Value::Object(Rc::new(RefCell::new(object)))
}

fn object_with(entries: Vec<(&str, Value)>) -> Value {
    let mut object = Object::plain();
    for (name, value) in entries {
        object.properties.insert(name.to_string(), value);
    }
    Value::Object(Rc::new(RefCell::new(object)))
}

fn install_globals(globals: &Scope) {
    globals.declare(
        "console",
        object_with(vec![
            ("log", native_value(Native::ConsoleLog)),
            ("warn", native_value(Native::ConsoleLog)),
            ("error", native_value(Native::ConsoleLog)),
        ]),
    );
    globals.declare(
        "Math",
        object_with(vec![
            ("abs", native_value(Native::MathAbs)),
            ("floor", native_value(Native::MathFloor)),
            ("ceil", native_value(Native::MathCeil)),
            ("round", native_value(Native::MathRound)),
            ("sqrt", native_value(Native::MathSqrt)),
            ("min", native_value(Native::MathMin)),
            ("max", native_value(Native::MathMax)),
            ("random", native_value(Native::MathRandom)),
            ("pow", native_value(Native::MathPow)),
            ("PI", Value::Number(std::f64::consts::PI)),
        ]),
    );
    globals.declare(
        "JSON",
        object_with(vec![("stringify", native_value(Native::JsonStringify))]),
    );
    globals.declare(
        "document",
        object_with(vec![
            ("getElementById", native_value(Native::DocGetElementById)),
            ("querySelector", native_value(Native::DocQuerySelector)),
            ("querySelectorAll", native_value(Native::DocQuerySelectorAll)),
        ]),
    );
    globals.declare("parseInt", native_value(Native::ParseInt));
    globals.declare("parseFloat", native_value(Native::ParseFloat));
    globals.declare("Number", native_value(Native::NumberFn));
    globals.declare("String", native_value(Native::StringFn));
    globals.declare("setTimeout", native_value(Native::SetTimeout));
    globals.declare("setInterval", native_value(Native::SetInterval));
    globals.declare("NaN", Value::Number(f64::NAN));
    globals.declare("Infinity", Value::Number(f64::INFINITY));
}

/// One evaluation session: the runtime plus a host borrow.
struct Interp<'a> {
    runtime: &'a mut Runtime,
    host: &'a mut dyn Host,
    depth: usize,
}

// Tree-walking burns many (fat, debug-build) Rust frames per JS call
// and test threads get 2 MiB stacks, so the limit is conservative.
const MAX_DEPTH: usize = 64;

impl Interp<'_> {
    fn run_block(&mut self, block: &Block, scope: &Scope) -> Result<Flow, String> {
        // Function declarations hoist within their block.
        for statement in block {
            if let Statement::Function { name, params, body } = statement {
                let function = Callable::Function {
                    params: Rc::new(params.clone()),
                    body: Rc::new(body.clone()),
                    closure: scope.clone(),
                };
                let mut object = Object::plain();
                object.call = Some(function);
                scope.declare(name, Value::Object(Rc::new(RefCell::new(object))));
            }
        }
        for statement in block {
            match self.run_statement(statement, scope)? {
                Flow::Normal => {}
                other => return Ok(other),
            }
        }
        Ok(Flow::Normal)
    }

    fn run_statement(&mut self, statement: &Statement, scope: &Scope) -> Result<Flow, String> {
        match statement {
            Statement::Declare { name, value } => {
                let value = match value {
                    Some(expression) => self.eval(expression, scope)?,
                    None => Value::Undefined,
                };
                scope.declare(name, value);
                Ok(Flow::Normal)
            }
            Statement::Function { .. } => Ok(Flow::Normal), // hoisted
            Statement::Return(value) => {
                let value = match value {
                    Some(expression) => self.eval(expression, scope)?,
                    None => Value::Undefined,
                };
                Ok(Flow::Return(value))
            }
            Statement::If {
                condition,
                then_branch,
                else_branch,
            } => {
                if truthy(&self.eval(condition, scope)?) {
                    self.run_block(then_branch, &Scope::new(Some(scope.clone())))
                } else if let Some(else_branch) = else_branch {
                    self.run_block(else_branch, &Scope::new(Some(scope.clone())))
                } else {
                    Ok(Flow::Normal)
                }
            }
            Statement::While { condition, body } => {
                let mut guard = 0u32;
                while truthy(&self.eval(condition, scope)?) {
                    guard += 1;
                    if guard > 1_000_000 {
                        return Err("loop ran too long".to_string());
                    }
                    match self.run_block(body, &Scope::new(Some(scope.clone())))? {
                        Flow::Break => break,
                        Flow::Return(value) => return Ok(Flow::Return(value)),
                        Flow::Normal | Flow::Continue => {}
                    }
                }
                Ok(Flow::Normal)
            }
            Statement::For {
                init,
                condition,
                update,
                body,
            } => {
                let loop_scope = Scope::new(Some(scope.clone()));
                if let Some(init) = init {
                    self.run_statement(init, &loop_scope)?;
                }
                let mut guard = 0u32;
                loop {
                    if let Some(condition) = condition
                        && !truthy(&self.eval(condition, &loop_scope)?)
                    {
                        break;
                    }
                    guard += 1;
                    if guard > 1_000_000 {
                        return Err("loop ran too long".to_string());
                    }
                    match self.run_block(body, &Scope::new(Some(loop_scope.clone())))? {
                        Flow::Break => break,
                        Flow::Return(value) => return Ok(Flow::Return(value)),
                        Flow::Normal | Flow::Continue => {}
                    }
                    if let Some(update) = update {
                        self.eval(update, &loop_scope)?;
                    }
                }
                Ok(Flow::Normal)
            }
            Statement::ForOf {
                name,
                iterable,
                body,
            } => {
                let iterable = self.eval(iterable, scope)?;
                let items: Vec<Value> = match &iterable {
                    Value::Object(object) => {
                        object.borrow().array.clone().unwrap_or_default()
                    }
                    Value::Str(text) => {
                        text.chars().map(|c| Value::Str(c.to_string())).collect()
                    }
                    _ => Vec::new(),
                };
                for item in items {
                    let iteration = Scope::new(Some(scope.clone()));
                    iteration.declare(name, item);
                    match self.run_block(body, &iteration)? {
                        Flow::Break => break,
                        Flow::Return(value) => return Ok(Flow::Return(value)),
                        Flow::Normal | Flow::Continue => {}
                    }
                }
                Ok(Flow::Normal)
            }
            Statement::Break => Ok(Flow::Break),
            Statement::Continue => Ok(Flow::Continue),
            Statement::Expression(expression) => {
                self.eval(expression, scope)?;
                Ok(Flow::Normal)
            }
            Statement::Block(block) => self.run_block(block, &Scope::new(Some(scope.clone()))),
        }
    }

    fn eval(&mut self, expression: &Expression, scope: &Scope) -> Result<Value, String> {
        match expression {
            Expression::Number(value) => Ok(Value::Number(*value)),
            Expression::Str(text) => Ok(Value::Str(text.clone())),
            Expression::Bool(value) => Ok(Value::Bool(*value)),
            Expression::Null => Ok(Value::Null),
            Expression::Undefined => Ok(Value::Undefined),
            Expression::Ident(name) => scope
                .get(name)
                .ok_or_else(|| format!("{name} is not defined")),
            Expression::Array(items) => {
                let mut values = Vec::with_capacity(items.len());
                for item in items {
                    values.push(self.eval(item, scope)?);
                }
                let mut object = Object::plain();
                object.array = Some(values);
                Ok(Value::Object(Rc::new(RefCell::new(object))))
            }
            Expression::Object(entries) => {
                let mut object = Object::plain();
                for (key, value) in entries {
                    let value = self.eval(value, scope)?;
                    object.properties.insert(key.clone(), value);
                }
                Ok(Value::Object(Rc::new(RefCell::new(object))))
            }
            Expression::Function { params, body } => {
                let mut object = Object::plain();
                object.call = Some(Callable::Function {
                    params: Rc::new(params.clone()),
                    body: Rc::new(body.clone()),
                    closure: scope.clone(),
                });
                Ok(Value::Object(Rc::new(RefCell::new(object))))
            }
            Expression::Unary { op, operand } => {
                let value = self.eval(operand, scope)?;
                Ok(match *op {
                    "!" => Value::Bool(!truthy(&value)),
                    "-" => Value::Number(-to_number(&value)),
                    "+" => Value::Number(to_number(&value)),
                    "typeof" => Value::Str(type_of(&value).to_string()),
                    _ => return Err(format!("unknown unary {op}")),
                })
            }
            Expression::Update { op, prefix, target } => {
                let current = to_number(&self.eval(target, scope)?);
                let next = if *op == "++" { current + 1.0 } else { current - 1.0 };
                self.assign_to(target, Value::Number(next), scope)?;
                Ok(Value::Number(if *prefix { next } else { current }))
            }
            Expression::Binary { op, left, right } => {
                let left = self.eval(left, scope)?;
                let right = self.eval(right, scope)?;
                binary(op, &left, &right)
            }
            Expression::Logical { op, left, right } => {
                let left = self.eval(left, scope)?;
                match *op {
                    "&&" => {
                        if truthy(&left) {
                            self.eval(right, scope)
                        } else {
                            Ok(left)
                        }
                    }
                    "||" => {
                        if truthy(&left) {
                            Ok(left)
                        } else {
                            self.eval(right, scope)
                        }
                    }
                    "??" => {
                        if matches!(left, Value::Null | Value::Undefined) {
                            self.eval(right, scope)
                        } else {
                            Ok(left)
                        }
                    }
                    _ => Err(format!("unknown logical {op}")),
                }
            }
            Expression::Conditional {
                condition,
                then_value,
                else_value,
            } => {
                if truthy(&self.eval(condition, scope)?) {
                    self.eval(then_value, scope)
                } else {
                    self.eval(else_value, scope)
                }
            }
            Expression::Assign { op, target, value } => {
                let mut value = self.eval(value, scope)?;
                if !op.is_empty() {
                    let current = self.eval(target, scope)?;
                    value = binary(op, &current, &value)?;
                }
                self.assign_to(target, value.clone(), scope)?;
                Ok(value)
            }
            Expression::Call { callee, arguments } => {
                let mut args = Vec::with_capacity(arguments.len());
                for argument in arguments {
                    args.push(self.eval(argument, scope)?);
                }
                // Method calls carry their receiver as `this`.
                match callee.as_ref() {
                    Expression::Member { object, property } => {
                        let receiver = self.eval(object, scope)?;
                        let function = self.member_get(&receiver, property)?;
                        self.call_with_this(&function, Some(receiver), args)
                    }
                    _ => {
                        let function = self.eval(callee, scope)?;
                        self.call_with_this(&function, None, args)
                    }
                }
            }
            Expression::Member { object, property } => {
                let object = self.eval(object, scope)?;
                self.member_get(&object, property)
            }
            Expression::Index { object, index } => {
                let object = self.eval(object, scope)?;
                let index = self.eval(index, scope)?;
                match (&object, &index) {
                    (Value::Object(cell), Value::Number(position)) => {
                        let borrowed = cell.borrow();
                        if let Some(array) = &borrowed.array {
                            let position = *position as usize;
                            return Ok(array.get(position).cloned().unwrap_or(Value::Undefined));
                        }
                        drop(borrowed);
                        self.member_get(&object, &to_display(&index))
                    }
                    (Value::Str(text), Value::Number(position)) => Ok(text
                        .chars()
                        .nth(*position as usize)
                        .map_or(Value::Undefined, |c| Value::Str(c.to_string()))),
                    _ => self.member_get(&object, &to_display(&index)),
                }
            }
        }
    }

    /// Calls any callable value with no receiver.
    fn call_value(&mut self, function: &Value, args: Vec<Value>) -> Result<Value, String> {
        self.call_with_this(function, None, args)
    }

    fn call_with_this(
        &mut self,
        function: &Value,
        this: Option<Value>,
        args: Vec<Value>,
    ) -> Result<Value, String> {
        self.depth += 1;
        if self.depth > MAX_DEPTH {
            self.depth -= 1;
            return Err("call stack overflow".to_string());
        }
        let result = self.call_inner(function, this, args);
        self.depth -= 1;
        result
    }

    fn call_inner(
        &mut self,
        function: &Value,
        this: Option<Value>,
        args: Vec<Value>,
    ) -> Result<Value, String> {
        let Value::Object(cell) = function else {
            return Err(format!("{} is not a function", to_display(function)));
        };
        let callable = cell
            .borrow()
            .call
            .clone()
            .ok_or_else(|| "value is not a function".to_string())?;
        match callable {
            Callable::Function {
                params,
                body,
                closure,
            } => {
                let scope = Scope::new(Some(closure));
                for (position, name) in params.iter().enumerate() {
                    scope.declare(name, args.get(position).cloned().unwrap_or(Value::Undefined));
                }
                match self.run_block(&body, &scope)? {
                    Flow::Return(value) => Ok(value),
                    _ => Ok(Value::Undefined),
                }
            }
            Callable::Native(native) => self.native(native, this, args),
        }
    }

    /// Reads `object.property`, including DOM-backed and built-in
    /// properties of strings/arrays.
    fn member_get(&mut self, object: &Value, property: &str) -> Result<Value, String> {
        match object {
            Value::Str(text) => Ok(match property {
                "length" => Value::Number(text.chars().count() as f64),
                "toUpperCase" => native_value(Native::StrToUpperCase),
                "toLowerCase" => native_value(Native::StrToLowerCase),
                "trim" => native_value(Native::StrTrim),
                "includes" => native_value(Native::StrIncludes),
                "indexOf" => native_value(Native::StrIndexOf),
                "slice" => native_value(Native::StrSlice),
                "split" => native_value(Native::StrSplit),
                "charAt" => native_value(Native::StrCharAt),
                "repeat" => native_value(Native::StrRepeat),
                "replace" => native_value(Native::StrReplace),
                _ => Value::Undefined,
            }),
            Value::Object(cell) => {
                let borrowed = cell.borrow();
                // DOM-backed properties read live from the page.
                if let Some(node) = borrowed.dom_node {
                    match property {
                        "textContent" | "innerText" => {
                            drop(borrowed);
                            return Ok(Value::Str(self.host.get_text(node)));
                        }
                        "value" => {
                            drop(borrowed);
                            return Ok(Value::Str(self.host.get_value(node)));
                        }
                        "id" => {
                            drop(borrowed);
                            return Ok(self
                                .host
                                .get_attribute(node, "id")
                                .map_or(Value::Str(String::new()), Value::Str));
                        }
                        _ => {}
                    }
                }
                if let Some(array) = &borrowed.array {
                    match property {
                        "length" => return Ok(Value::Number(array.len() as f64)),
                        "push" => return Ok(native_value(Native::ArrPush)),
                        "pop" => return Ok(native_value(Native::ArrPop)),
                        "join" => return Ok(native_value(Native::ArrJoin)),
                        "indexOf" => return Ok(native_value(Native::ArrIndexOf)),
                        "includes" => return Ok(native_value(Native::ArrIncludes)),
                        "slice" => return Ok(native_value(Native::ArrSlice)),
                        "map" => return Ok(native_value(Native::ArrMap)),
                        "filter" => return Ok(native_value(Native::ArrFilter)),
                        "forEach" => return Ok(native_value(Native::ArrForEach)),
                        _ => {}
                    }
                }
                Ok(borrowed
                    .properties
                    .get(property)
                    .cloned()
                    .unwrap_or(Value::Undefined))
            }
            Value::Number(_) | Value::Bool(_) => Ok(Value::Undefined),
            Value::Null | Value::Undefined => Err(format!(
                "cannot read '{property}' of {}",
                to_display(object)
            )),
        }
    }

    /// Writes through an assignment target.
    fn assign_to(
        &mut self,
        target: &Expression,
        value: Value,
        scope: &Scope,
    ) -> Result<(), String> {
        match target {
            Expression::Ident(name) => {
                if !scope.set(name, value.clone()) {
                    // Implicit global, as sloppy-mode JS does.
                    scope.declare(name, value);
                }
                Ok(())
            }
            Expression::Member { object, property } => {
                let object = self.eval(object, scope)?;
                self.member_set(&object, property, value)
            }
            Expression::Index { object, index } => {
                let object = self.eval(object, scope)?;
                let index = self.eval(index, scope)?;
                if let (Value::Object(cell), Value::Number(position)) = (&object, &index) {
                    let mut borrowed = cell.borrow_mut();
                    if let Some(array) = &mut borrowed.array {
                        let position = *position as usize;
                        if position >= array.len() {
                            array.resize(position + 1, Value::Undefined);
                        }
                        array[position] = value;
                        return Ok(());
                    }
                }
                self.member_set(&object, &to_display(&index), value)
            }
            _ => Err("invalid assignment target".to_string()),
        }
    }

    fn member_set(&mut self, object: &Value, property: &str, value: Value) -> Result<(), String> {
        let Value::Object(cell) = object else {
            return Err(format!("cannot set '{property}' on {}", to_display(object)));
        };
        let dom_node = cell.borrow().dom_node;
        if let Some(node) = dom_node {
            match property {
                "textContent" | "innerText" => {
                    self.host.set_text(node, &to_display(&value));
                    return Ok(());
                }
                "value" => {
                    self.host.set_value(node, &to_display(&value));
                    return Ok(());
                }
                _ => {}
            }
            // element.style.color = ... : a magic style proxy.
            if property == "style" {
                return Err("assign to style properties instead (el.style.x = ...)".to_string());
            }
        }
        cell.borrow_mut()
            .properties
            .insert(property.to_string(), value);
        Ok(())
    }

    #[allow(clippy::too_many_lines)]
    fn native(
        &mut self,
        native: Native,
        this: Option<Value>,
        args: Vec<Value>,
    ) -> Result<Value, String> {
        let arg = |position: usize| args.get(position).cloned().unwrap_or(Value::Undefined);
        let number = |position: usize| to_number(&arg(position));
        match native {
            Native::ConsoleLog => {
                let message = args
                    .iter()
                    .map(to_display)
                    .collect::<Vec<_>>()
                    .join(" ");
                self.host.console_log(&message);
                Ok(Value::Undefined)
            }
            Native::MathAbs => Ok(Value::Number(number(0).abs())),
            Native::MathFloor => Ok(Value::Number(number(0).floor())),
            Native::MathCeil => Ok(Value::Number(number(0).ceil())),
            Native::MathRound => Ok(Value::Number(number(0).round())),
            Native::MathSqrt => Ok(Value::Number(number(0).sqrt())),
            Native::MathPow => Ok(Value::Number(number(0).powf(number(1)))),
            Native::MathMin => Ok(Value::Number(
                args.iter().map(to_number).fold(f64::INFINITY, f64::min),
            )),
            Native::MathMax => Ok(Value::Number(
                args.iter()
                    .map(to_number)
                    .fold(f64::NEG_INFINITY, f64::max),
            )),
            Native::MathRandom => Ok(Value::Number(self.host.random())),
            Native::JsonStringify => Ok(Value::Str(json_stringify(&arg(0)))),
            Native::ParseInt => Ok(Value::Number(
                to_display(&arg(0))
                    .trim()
                    .chars()
                    .take_while(|c| c.is_ascii_digit() || *c == '-' || *c == '+')
                    .collect::<String>()
                    .parse::<f64>()
                    .map(f64::trunc)
                    .unwrap_or(f64::NAN),
            )),
            Native::ParseFloat | Native::NumberFn => Ok(Value::Number(to_number(&arg(0)))),
            Native::StringFn => Ok(Value::Str(to_display(&arg(0)))),
            Native::SetTimeout | Native::SetInterval => {
                let delay = number(1).max(0.0);
                self.runtime.timers.push(Timer {
                    due_ms: self.runtime.now_ms + delay,
                    interval_ms: (native == Native::SetInterval).then_some(delay.max(1.0)),
                    handler: arg(0),
                });
                Ok(Value::Number(self.runtime.timers.len() as f64))
            }
            // ---- string methods ----
            Native::StrToUpperCase => Ok(Value::Str(this_string(&this)?.to_uppercase())),
            Native::StrToLowerCase => Ok(Value::Str(this_string(&this)?.to_lowercase())),
            Native::StrTrim => Ok(Value::Str(this_string(&this)?.trim().to_string())),
            Native::StrIncludes => Ok(Value::Bool(
                this_string(&this)?.contains(&to_display(&arg(0))),
            )),
            Native::StrIndexOf => {
                let text = this_string(&this)?;
                let needle = to_display(&arg(0));
                Ok(Value::Number(match text.find(&needle) {
                    Some(byte) => text[..byte].chars().count() as f64,
                    None => -1.0,
                }))
            }
            Native::StrSlice => {
                let text: Vec<char> = this_string(&this)?.chars().collect();
                let (start, end) = slice_bounds(&args, text.len());
                Ok(Value::Str(text[start..end].iter().collect()))
            }
            Native::StrSplit => {
                let text = this_string(&this)?;
                let separator = to_display(&arg(0));
                let pieces: Vec<Value> = if separator.is_empty() {
                    text.chars().map(|c| Value::Str(c.to_string())).collect()
                } else {
                    text.split(&separator)
                        .map(|piece| Value::Str(piece.to_string()))
                        .collect()
                };
                let mut object = Object::plain();
                object.array = Some(pieces);
                Ok(Value::Object(Rc::new(RefCell::new(object))))
            }
            Native::StrCharAt => Ok(this_string(&this)?
                .chars()
                .nth(number(0) as usize)
                .map_or(Value::Str(String::new()), |c| Value::Str(c.to_string()))),
            Native::StrRepeat => Ok(Value::Str(
                this_string(&this)?.repeat((number(0).max(0.0)) as usize),
            )),
            Native::StrReplace => Ok(Value::Str(this_string(&this)?.replacen(
                &to_display(&arg(0)),
                &to_display(&arg(1)),
                1,
            ))),
            // ---- array methods ----
            Native::ArrPush => {
                let cell = this_array(&this)?;
                let mut borrowed = cell.borrow_mut();
                let array = borrowed.array.as_mut().expect("checked");
                for value in args {
                    array.push(value);
                }
                Ok(Value::Number(array.len() as f64))
            }
            Native::ArrPop => {
                let cell = this_array(&this)?;
                let mut borrowed = cell.borrow_mut();
                Ok(borrowed
                    .array
                    .as_mut()
                    .expect("checked")
                    .pop()
                    .unwrap_or(Value::Undefined))
            }
            Native::ArrJoin => {
                let cell = this_array(&this)?;
                let separator = if args.is_empty() {
                    ",".to_string()
                } else {
                    to_display(&arg(0))
                };
                let joined = cell
                    .borrow()
                    .array
                    .as_ref()
                    .expect("checked")
                    .iter()
                    .map(to_display)
                    .collect::<Vec<_>>()
                    .join(&separator);
                Ok(Value::Str(joined))
            }
            Native::ArrIndexOf => {
                let cell = this_array(&this)?;
                let position = cell
                    .borrow()
                    .array
                    .as_ref()
                    .expect("checked")
                    .iter()
                    .position(|item| loose_equals(item, &arg(0)));
                Ok(Value::Number(position.map_or(-1.0, |p| p as f64)))
            }
            Native::ArrIncludes => {
                let cell = this_array(&this)?;
                let found = cell
                    .borrow()
                    .array
                    .as_ref()
                    .expect("checked")
                    .iter()
                    .any(|item| loose_equals(item, &arg(0)));
                Ok(Value::Bool(found))
            }
            Native::ArrSlice => {
                let cell = this_array(&this)?;
                let items = cell.borrow().array.clone().expect("checked");
                let (start, end) = slice_bounds(&args, items.len());
                let mut object = Object::plain();
                object.array = Some(items[start..end].to_vec());
                Ok(Value::Object(Rc::new(RefCell::new(object))))
            }
            Native::ArrMap | Native::ArrFilter | Native::ArrForEach => {
                let cell = this_array(&this)?;
                let items = cell.borrow().array.clone().expect("checked");
                let callback = arg(0);
                let mut mapped = Vec::new();
                for (position, item) in items.into_iter().enumerate() {
                    let result = self.call_value(
                        &callback,
                        vec![item.clone(), Value::Number(position as f64)],
                    )?;
                    match native {
                        Native::ArrMap => mapped.push(result),
                        Native::ArrFilter => {
                            if truthy(&result) {
                                mapped.push(item);
                            }
                        }
                        _ => {}
                    }
                }
                if native == Native::ArrForEach {
                    return Ok(Value::Undefined);
                }
                let mut object = Object::plain();
                object.array = Some(mapped);
                Ok(Value::Object(Rc::new(RefCell::new(object))))
            }
            // ---- DOM ----
            Native::DocGetElementById => {
                let id = to_display(&arg(0));
                Ok(match self.host.get_element_by_id(&id) {
                    Some(node) => self.runtime.element_value(node),
                    None => Value::Null,
                })
            }
            Native::DocQuerySelector => {
                let selector = to_display(&arg(0));
                Ok(match self.host.query_selector_all(&selector).first() {
                    Some(node) => self.runtime.element_value(*node),
                    None => Value::Null,
                })
            }
            Native::DocQuerySelectorAll => {
                let selector = to_display(&arg(0));
                let nodes = self.host.query_selector_all(&selector);
                let items: Vec<Value> = nodes
                    .into_iter()
                    .map(|node| self.runtime.element_value(node))
                    .collect();
                let mut object = Object::plain();
                object.array = Some(items);
                Ok(Value::Object(Rc::new(RefCell::new(object))))
            }
            Native::ElAddEventListener => {
                let node = this_dom(&this)?;
                self.runtime.listeners.push(Listener {
                    node,
                    event: to_display(&arg(0)),
                    handler: arg(1),
                });
                Ok(Value::Undefined)
            }
            Native::ElGetAttribute => {
                let node = this_dom(&this)?;
                let name = to_display(&arg(0));
                Ok(self
                    .host
                    .get_attribute(node, &name)
                    .map_or(Value::Null, Value::Str))
            }
            Native::ElSetAttribute => {
                let node = this_dom(&this)?;
                let name = to_display(&arg(0));
                let value = to_display(&arg(1));
                if let Some(property) = name.strip_prefix("style.") {
                    self.host.set_style(node, property, &value);
                } else {
                    self.host.set_attribute(node, &name, &value);
                }
                Ok(Value::Undefined)
            }
        }
    }
}

fn this_string(this: &Option<Value>) -> Result<String, String> {
    match this {
        Some(Value::Str(text)) => Ok(text.clone()),
        other => Err(format!(
            "string method on {}",
            other.as_ref().map_or("nothing".to_string(), to_display)
        )),
    }
}

fn this_array(this: &Option<Value>) -> Result<Rc<RefCell<Object>>, String> {
    if let Some(Value::Object(cell)) = this
        && cell.borrow().array.is_some()
    {
        return Ok(cell.clone());
    }
    Err("array method on a non-array".to_string())
}

fn this_dom(this: &Option<Value>) -> Result<DomNode, String> {
    if let Some(Value::Object(cell)) = this
        && let Some(node) = cell.borrow().dom_node
    {
        return Ok(node);
    }
    Err("element method on a non-element".to_string())
}

/// `slice(start, end)` bounds with negative indexing, clamped.
fn slice_bounds(args: &[Value], length: usize) -> (usize, usize) {
    let resolve = |value: Option<&Value>, default: i64| -> i64 {
        value.map_or(default, |v| to_number(v) as i64)
    };
    let clamp = |index: i64| -> usize {
        if index < 0 {
            (length as i64 + index).max(0) as usize
        } else {
            (index as usize).min(length)
        }
    };
    let start = clamp(resolve(args.first(), 0));
    let end = clamp(resolve(args.get(1), length as i64));
    (start, end.max(start))
}

pub fn truthy(value: &Value) -> bool {
    match value {
        Value::Undefined | Value::Null => false,
        Value::Bool(value) => *value,
        Value::Number(value) => *value != 0.0 && !value.is_nan(),
        Value::Str(text) => !text.is_empty(),
        Value::Object(_) => true,
    }
}

fn type_of(value: &Value) -> &'static str {
    match value {
        Value::Undefined => "undefined",
        Value::Null => "object",
        Value::Bool(_) => "boolean",
        Value::Number(_) => "number",
        Value::Str(_) => "string",
        Value::Object(cell) => {
            if cell.borrow().call.is_some() {
                "function"
            } else {
                "object"
            }
        }
    }
}

pub fn to_number(value: &Value) -> f64 {
    match value {
        Value::Undefined => f64::NAN,
        Value::Null => 0.0,
        Value::Bool(value) => f64::from(*value),
        Value::Number(value) => *value,
        Value::Str(text) => {
            let trimmed = text.trim();
            if trimmed.is_empty() {
                0.0
            } else {
                trimmed.parse().unwrap_or(f64::NAN)
            }
        }
        Value::Object(_) => f64::NAN,
    }
}

/// Human/DOM display form (what `console.log` and string coercion show).
pub fn to_display(value: &Value) -> String {
    match value {
        Value::Undefined => "undefined".to_string(),
        Value::Null => "null".to_string(),
        Value::Bool(value) => value.to_string(),
        Value::Number(value) => format_number(*value),
        Value::Str(text) => text.clone(),
        Value::Object(cell) => {
            let borrowed = cell.borrow();
            if borrowed.call.is_some() {
                return "function".to_string();
            }
            if let Some(array) = &borrowed.array {
                return array.iter().map(to_display).collect::<Vec<_>>().join(",");
            }
            if borrowed.dom_node.is_some() {
                return "[object Element]".to_string();
            }
            "[object Object]".to_string()
        }
    }
}

fn format_number(value: f64) -> String {
    if value.is_nan() {
        return "NaN".to_string();
    }
    if value.is_infinite() {
        return if value > 0.0 { "Infinity" } else { "-Infinity" }.to_string();
    }
    if value == value.trunc() && value.abs() < 1e15 {
        format!("{}", value as i64)
    } else {
        format!("{value}")
    }
}

fn binary(op: &str, left: &Value, right: &Value) -> Result<Value, String> {
    Ok(match op {
        "+" => match (left, right) {
            (Value::Str(_), _) | (_, Value::Str(_)) => {
                Value::Str(format!("{}{}", to_display(left), to_display(right)))
            }
            _ => Value::Number(to_number(left) + to_number(right)),
        },
        "-" => Value::Number(to_number(left) - to_number(right)),
        "*" => Value::Number(to_number(left) * to_number(right)),
        "/" => Value::Number(to_number(left) / to_number(right)),
        "%" => Value::Number(to_number(left) % to_number(right)),
        "==" => Value::Bool(loose_equals(left, right)),
        "!=" => Value::Bool(!loose_equals(left, right)),
        "===" => Value::Bool(strict_equals(left, right)),
        "!==" => Value::Bool(!strict_equals(left, right)),
        "<" | ">" | "<=" | ">=" => {
            let result = if let (Value::Str(a), Value::Str(b)) = (left, right) {
                match op {
                    "<" => a < b,
                    ">" => a > b,
                    "<=" => a <= b,
                    _ => a >= b,
                }
            } else {
                let (a, b) = (to_number(left), to_number(right));
                match op {
                    "<" => a < b,
                    ">" => a > b,
                    "<=" => a <= b,
                    _ => a >= b,
                }
            };
            Value::Bool(result)
        }
        _ => return Err(format!("unknown operator {op}")),
    })
}

fn strict_equals(left: &Value, right: &Value) -> bool {
    match (left, right) {
        (Value::Undefined, Value::Undefined) | (Value::Null, Value::Null) => true,
        (Value::Bool(a), Value::Bool(b)) => a == b,
        (Value::Number(a), Value::Number(b)) => a == b,
        (Value::Str(a), Value::Str(b)) => a == b,
        (Value::Object(a), Value::Object(b)) => Rc::ptr_eq(a, b),
        _ => false,
    }
}

fn loose_equals(left: &Value, right: &Value) -> bool {
    match (left, right) {
        (Value::Null | Value::Undefined, Value::Null | Value::Undefined) => true,
        (Value::Number(_), Value::Str(_)) | (Value::Str(_), Value::Number(_)) => {
            to_number(left) == to_number(right)
        }
        (Value::Bool(_), _) | (_, Value::Bool(_)) => to_number(left) == to_number(right),
        _ => strict_equals(left, right),
    }
}

fn json_stringify(value: &Value) -> String {
    match value {
        Value::Undefined => "null".to_string(),
        Value::Null => "null".to_string(),
        Value::Bool(value) => value.to_string(),
        Value::Number(value) => format_number(*value),
        Value::Str(text) => format!("{text:?}"),
        Value::Object(cell) => {
            let borrowed = cell.borrow();
            if let Some(array) = &borrowed.array {
                let items: Vec<String> = array.iter().map(json_stringify).collect();
                return format!("[{}]", items.join(","));
            }
            if borrowed.call.is_some() {
                return "null".to_string();
            }
            let mut output = String::from("{");
            let mut entries: Vec<(&String, &Value)> = borrowed.properties.iter().collect();
            entries.sort_by_key(|(key, _)| (*key).clone());
            for (position, (key, value)) in entries.into_iter().enumerate() {
                if position > 0 {
                    output.push(',');
                }
                let _ = write!(output, "{key:?}:{}", json_stringify(value));
            }
            output.push('}');
            output
        }
    }
}
