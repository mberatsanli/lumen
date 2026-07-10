# ADR 0001: Arena-based DOM

## Status

Accepted.

## Decision

Store DOM nodes in a `Vec<Node>` and reference them using `NodeId` indices.

## Reason

A browser tree contains parent and child relationships. Rust ownership becomes unnecessarily complex
when nodes directly own shared references to each other. An arena keeps ownership centralized,
provides stable identifiers and simplifies tests.
