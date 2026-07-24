# HTML parser

`crates/lumen-html` delegates parsing to
[html5ever](https://github.com/servo/html5ever) — Servo's implementation
of the WHATWG HTML5 parsing algorithm — and keeps the Lumen-specific
parts in two modules:

```text
sink.rs        html5ever TreeSink -> Document  (tree-builder callbacks -> arena ops)
dom.rs         arena-based Document / Node / AttributeMap
```

`parse_document(source) -> Document` runs html5ever's tokenizer and tree
builder over a `TreeSink` implementation that appends nodes into our
arena. Parsing never fails: malformed markup is recovered from by the
full WHATWG algorithm, and recoverable errors are merely counted
(`Document::parse_error_count`). `parse_fragment(context_tag, source)`
implements the innerHTML algorithm and backs `Document::set_inner_html`.

What the migration buys, for free:

- the `html`/`head`/`body` skeleton is always synthesized (even for
  bare fragments like `<p>x</p>`),
- WHATWG insertion modes: the adoption agency (`<b><i>x</b></i>`),
  foster parenting (stray text inside tables moves before the table),
  implied `<tbody>` around bare `<tr>`s, and the rest of the tree-
  construction rules,
- RCDATA (`title`, `textarea`) and raw-text (`script`, `style`, `xmp`,
  `iframe`, `noembed`, `noframes`, `plaintext`) states,
- the complete WHATWG named character reference table (`&copy;`,
  `&nbsp;`, `&auml;`, ...) plus numeric forms,
- spec-correct self-closing rules (`/>` is honored only on void and
  foreign elements; `<script/>` keeps consuming raw text).

Deliberate simplifications in the sink:

- comments, processing instructions and the doctype are dropped (the
  DOM has no node kinds for them),
- `<template>` contents become direct children of the `<template>`
  element instead of a separate "template contents" fragment,
- namespaces are flattened to local names (attributes keep
  `prefix:local`); foreign SVG/MathML content parses with correct tag
  names but no namespace distinction,
- quirks mode is ignored by rendering.

## DOM

Arena storage (see ADR 0001): all nodes in one `Vec<Node>`, referenced by
`NodeId` indices; root is always node `0`.

Helpers: `children`, `parent`, `element`, `descendants` (preorder),
`ancestors`, `get_element_by_id`, `text_content`, `dump`.
`ElementData` offers `id()`, `classes()`, `has_class()`; `AttributeMap.get`
does name lookup.

## Known limitations

- Input is assumed to be UTF-8 (html5ever is fed `&str`); byte-level
  encoding detection (BOM, `<meta charset>`, Encoding Standard) is
  future work (see ROADMAP).
- No namespace-aware processing beyond tag names (see above).
- Source spans / diagnostics are not recorded (only an error count).
