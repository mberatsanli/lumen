//! HTML parsing for the Lumen browser engine.
//!
//! Pipeline: [`tokenize`] turns source text into [`HtmlToken`]s (also
//! available as a streaming iterator), [`parse_document`] builds an
//! arena-based [`Document`] from them. Both are lenient: malformed input is
//! recovered from rather than rejected, so parsing never fails.

pub mod dom;
pub mod parser;
pub mod tokenizer;

pub use dom::{AttributeMap, Document, ElementData, Node, NodeId, NodeKind};
pub use parser::parse_document;
pub use tokenizer::{HtmlAttribute, HtmlToken, tokenize};
