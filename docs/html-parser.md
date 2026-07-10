# HTML parser

`crates/lumen-html` is split into three modules:

```text
tokenizer.rs   source text -> Vec<HtmlToken>   (explicit state machine)
parser.rs      Vec<HtmlToken> -> Document      (open-element stack)
dom.rs         arena-based Document / Node / AttributeMap
```

## Tokenizer

An explicit state machine with states named after their WHATWG counterparts:
`Data`, `TagOpen`, `EndTagOpen`, `TagName`, `BeforeAttributeName`,
`AttributeName`, `AfterAttributeName`, `BeforeAttributeValue`,
`AttributeValue{Double,Single}Quoted`, `AttributeValueUnquoted`,
`SelfClosingStartTag`, `MarkupDeclarationOpen`, `Comment`, `Doctype`,
`BogusComment`, `Rawtext`.

Tokens: `Doctype`, `StartTag { name, attributes, self_closing }`,
`EndTag { name }`, `Text`, `Comment`.

Supported syntax:

- start/end tags, tag names normalized to lowercase,
- double-quoted, single-quoted, unquoted and boolean attributes,
- self-closing (`/>`) syntax,
- `<!-- comments -->` and `<!doctype ...>`,
- basic character references: `&amp; &lt; &gt; &quot; &apos; &nbsp;`,
  `&#nnn;` and `&#xhh;` (unknown references stay literal),
- raw-text elements: the content of `script`, `style`, `title` and
  `textarea` is not scanned for markup until the matching end tag.

### Error recovery

The tokenizer never fails. Recovery rules:

| Input | Behavior |
|---|---|
| `a < 5` | `<` followed by a non-letter is literal text |
| `</>` | empty end tag is dropped |
| `</3...>` | bogus content skipped up to `>` |
| `<div class="x` (EOF) | incomplete tag dropped, preceding text kept |
| `<!-- open` (EOF) | comment emitted with collected content |
| `<![CDATA[...]]>` | unknown markup declaration skipped like a comment |
| `<div / class=x>` | stray `/` inside a tag ignored |

## Tree builder

A simplified open-element stack; no WHATWG insertion modes, no implied
`<html>`/`<body>` synthesis.

- Void elements (`br`, `img`, `input`, `meta`, `link`, ...) and
  self-closing tags are never pushed onto the stack.
- An end tag closes the nearest matching open element **and** everything
  opened after it (`<div><p>a</div>` closes the `p` too).
- End tags with no matching open element are ignored.
- Elements still open at end of input are closed implicitly.
- Comments and doctype are dropped — they are not represented in the DOM.
- Duplicate attributes keep the first value.

Parsing therefore never returns an error; `parse_document(source) -> Document`.

## DOM

Arena storage (see ADR 0001): all nodes in one `Vec<Node>`, referenced by
`NodeId` indices; root is always node `0`.

Helpers: `children`, `parent`, `element`, `descendants` (preorder),
`ancestors`, `get_element_by_id`, `text_content`, `dump`.
`ElementData` offers `id()`, `classes()`, `has_class()`; `AttributeMap.get`
does name lookup.

## Known limitations

- Not WHATWG compliant: no insertion modes, no implied elements, no
  active-formatting reconstruction, no foreign content (SVG/MathML).
- Character reference support is a small practical subset.
- Source spans / diagnostics are not recorded yet (planned; the state
  machine keeps a single `position` so spans can be added cleanly).
