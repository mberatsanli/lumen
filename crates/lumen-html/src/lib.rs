//! HTML parsing for the Lumen browser engine.
//!
//! Parsing is delegated to [html5ever](https://github.com/servo/html5ever),
//! Servo's implementation of the WHATWG HTML5 parsing algorithm
//! ([`parse_document`], [`parse_fragment`]); [`sink`] only translates its
//! tree-builder callbacks into our arena-based [`Document`]. Parsing is
//! lenient by construction: malformed input is recovered from rather than
//! rejected, so parsing never fails.

pub mod dom;
mod sink;

pub use dom::{AttributeMap, Document, ElementData, Node, NodeId, NodeKind};
pub use sink::{StreamingParser, parse_document, parse_fragment};
