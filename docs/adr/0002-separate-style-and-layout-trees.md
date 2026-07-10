# ADR 0002: Separate style map and layout tree

## Status

Accepted.

## Decision

Computed styles live in a `StyleMap` keyed by `NodeId`; layout produces its
own `LayoutBox` tree that owns a clone of each node's `ComputedStyle` and
points back to the DOM via `node_id`. Neither structure borrows from the
DOM or from each other.

## Reason

Keeping the three trees (DOM, style, layout) independent avoids shared
mutable references across pipeline stages, keeps each stage a pure
function, and lets tests exercise any stage in isolation. The cost — one
`ComputedStyle` clone per box — is acceptable at this scale.
