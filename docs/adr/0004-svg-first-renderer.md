# ADR 0004: SVG as the first renderer

## Status

Accepted.

## Decision

The first rendering backend serializes the display list to SVG text.
Borders are drawn as four inward edge rectangles rather than stroked paths.

## Reason

SVG output is deterministic, diffable and viewable in any browser, which
enables golden-file testing long before a pixel rasterizer or window
exists. Plain rectangles keep the output byte-stable across platforms.
