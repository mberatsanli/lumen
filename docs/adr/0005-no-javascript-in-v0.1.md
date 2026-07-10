# ADR 0005: No JavaScript before stable rendering

## Status

Accepted.

## Decision

Version 0.1 ships without any script execution. The DOM has no event or
mutation API surface beyond what parsing needs.

## Reason

Script support multiplies the surface area of every other component
(parser re-entrancy, DOM mutation, style invalidation, layout dirtying).
The educational core — parse, cascade, layout, paint — must be correct and
observable first.
