# Architecture

Lumen uses a one-way dependency flow:

```text
lumen-html ----\
                -> lumen-engine ---+-> lumen-cli
lumen-css -----/                   |   lumen-desktop
                                   |
lumen-platform -> lumen-browser ---+
```

Inside `lumen-engine`, the pipeline is split into modules
(`style`, `layout`, `paint`, `svg`, `geometry`, `text`) with four phases:

1. **Parse:** HTML becomes an arena-based DOM; CSS becomes rules and declarations.
2. **Style:** selectors are matched, specificity is calculated and computed properties are built.
3. **Layout:** visible nodes become rectangular layout boxes in vertical block flow.
4. **Paint:** layout boxes become display commands and are serialized to SVG.

The DOM uses numeric node identifiers instead of reference-counted parent/child pointers. This keeps
ownership simple and makes tree traversal explicit.
