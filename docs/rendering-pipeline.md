# Rendering pipeline

```text
source HTML
  -> HTML parsing (html5ever: tokenizer + tree builder)
  -> DOM
  -> embedded stylesheet extraction
  -> CSS parser
  -> selector matching / cascade
  -> computed style map
  -> block layout tree
  -> display list
  -> SVG output
```

Each phase has a narrow input and output. This makes it possible to inspect intermediate states with
the CLI and test them independently.
