# Accessibility compatibility

✅ supported · ⚠️ partial (see note) · ❌ not supported.

Parsing semantic elements is not sufficient for accessibility support. A
browser must derive accessible roles, names, states and relationships and
expose them through the host platform. References:
[WAI-ARIA](https://www.w3.org/TR/wai-aria-1.2/) and
[Accessible Name and Description Computation](https://www.w3.org/TR/accname-1.2/).

## Semantics and accessibility tree

| Feature | Lumen | Note |
|---|:-:|---|
| Semantic HTML in DOM | ⚠️ | Tag names are preserved; behavior varies by element |
| Accessibility tree | ❌ | No separate accessible representation |
| Implicit HTML roles | ❌ | |
| Accessible-name computation | ❌ | `alt`, labels and ARIA are not interpreted |
| Accessible descriptions | ❌ | |
| State/property exposure | ❌ | |
| Hidden/inert subtree handling | ❌ | `hidden`, `aria-hidden` and `inert` have no behavior |
| Live regions | ❌ | |

## Keyboard and focus

| Feature | Lumen | Note |
|---|:-:|---|
| Keyboard document scrolling | ⚠️ | Shell-level arrows, page keys, Home and Space |
| Focusable elements | ❌ | |
| Tab navigation and focus order | ❌ | |
| Keyboard link activation | ❌ | Links require a pointer click |
| Visible focus indicator | ❌ | |
| Form-control keyboard behavior | ❌ | Forms are not implemented |
| Access keys | ❌ | |

## Host integration and user preferences

| Feature | Lumen | Note |
|---|:-:|---|
| Platform accessibility API | ❌ | No AX/UIA/AT-SPI bridge |
| Screen-reader navigation | ❌ | |
| Text zoom / page zoom | ❌ | HiDPI scaling is supported, user zoom is not |
| Minimum readable font handling | ❌ | |
| Forced colors / high contrast | ❌ | |
| Reduced motion preference | ❌ | No media queries or motion yet |
| Color-scheme preference | ❌ | |
| Caret and selection visibility | ❌ | |

