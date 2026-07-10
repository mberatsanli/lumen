# ADR 0003: Display-list architecture

## Status

Accepted.

## Decision

Paint does not draw. It flattens the layout tree into a `Vec<DisplayCommand>`
(`FillRect`, `StrokeRect`, `DrawText`) in paint order; backends (SVG today,
a rasterizer later) consume the list without knowing about layout.

## Reason

A display list makes paint order explicit and testable, decouples backends
from the engine, and is the natural input for a future software rasterizer
and for debugging tools (`dump-display-list`).
