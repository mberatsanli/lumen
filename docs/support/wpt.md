# Web-platform test compatibility

✅ supported · ⚠️ partial (see note) · ❌ not supported.

The [web-platform-tests project](https://web-platform-tests.org/) is the
shared cross-browser conformance suite. This document defines how Lumen will
measure compatibility; it does not treat a feature-table checkmark as proof.

## Current test infrastructure

| Capability | Lumen | Note |
|---|:-:|---|
| Rust unit tests | ✅ | Parser, selector, style, layout, loader and session behavior |
| Integration tests | ✅ | Cross-crate engine behavior |
| Golden SVG tests | ✅ | Deterministic display-list rendering |
| Pixel visual regression tests | ❌ | Planned for v0.3 |
| WPT testharness.js runner | ❌ | Requires JavaScript and browser harness APIs |
| WPT reftest runner | ❌ | Lumen has no WPT manifest/server integration |
| WPT parser-test adapter | ❌ | HTML tree-construction fixtures are not imported |
| WPT metadata/results export | ❌ | No machine-readable pass/fail report |
| Crash-test corpus | ❌ | Parser fuzz/crash inputs are not tracked as WPT results |

## Compatibility evidence

A Lumen feature should move to ✅ only when all applicable evidence exists:

1. The implemented behavior is linked to a WHATWG/W3C specification section.
2. Focused Rust tests cover normal, boundary and error-recovery behavior.
3. Rendering behavior has a deterministic golden or reference comparison.
4. Applicable WPT cases pass through an adapter, or the tracker documents why
   they cannot run yet.
5. Known deviations are recorded in the relevant support table.

⚠️ means the feature works only for a documented subset, is library-backed
without Lumen conformance tests, or cannot yet run the relevant WPT coverage.

## WPT adoption order

| Phase | Scope | Suitable test type | Status |
|---|---|---|:-:|
| 1 | URL parsing/resolution | Data-driven WPT fixtures through Rust tests | ❌ |
| 2 | HTML tokenizer | Data-driven tokenizer fixtures | ❌ |
| 3 | HTML tree construction | Parser fixtures compared with serialized DOM | ❌ |
| 4 | CSS syntax/selectors/values | Data-driven parsing and matching fixtures | ❌ |
| 5 | Block box model and text layout | Reftests rendered by Lumen and a reference page | ❌ |
| 6 | Fetch/encoding | Local WPT server plus response-header fixtures | ❌ |
| 7 | DOM and browser APIs | `testharness.js` after a JS runtime exists | ❌ |

## Result reporting

Results should be pinned to a WPT commit because the upstream suite changes
continuously. Reports should contain the commit hash, Lumen commit hash,
selected test paths, pass/fail/skip counts and explicit skip reasons. A raw
percentage without the selected test scope must not be presented as Lumen's
overall standards-compliance score.

