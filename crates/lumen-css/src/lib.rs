//! CSS parsing for the Lumen browser engine.
//!
//! Produces typed values ([`CssValue`], [`Color`]) and a structural
//! [`Specificity`], so later pipeline stages never re-parse strings.
//! Selector *matching* lives in `lumen-engine`, because it needs the DOM;
//! this crate only defines the selector model.

pub mod parser;
pub mod properties;
pub mod selector;
pub mod value;

pub use parser::{
    Declaration, FontFace, MediaQuery, Rule, Stylesheet, parse_declarations, parse_stylesheet,
};
pub use selector::{
    AttributeOperation, AttributeSelector, Combinator, CompoundSelector, PseudoClass, Selector,
    Specificity, parse_selector,
};
pub use value::{Color, CssValue, Unit, split_components};
