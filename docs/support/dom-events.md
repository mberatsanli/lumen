# DOM and events compatibility

✅ supported · ⚠️ partial (see note) · ❌ not supported.

This tracker separates the document and interaction model from JavaScript
language execution. Lumen can implement native interaction behavior before
those APIs are exposed to scripts. Normative references:
[DOM](https://dom.spec.whatwg.org/) and
[UI Events](https://www.w3.org/TR/uievents/).

## Document model

| Feature | Lumen | Note |
|---|:-:|---|
| Document, element and text nodes | ✅ | Arena-backed internal model |
| Parent/child/sibling traversal | ✅ | Internal Rust APIs |
| Element attributes | ✅ | String map with first duplicate winning |
| ID lookup | ✅ | Internal API |
| Selector matching | ⚠️ | Supported CSS selector subset only |
| Comment nodes | ❌ | Tokenized then dropped |
| Document type node | ❌ | Recognized then dropped |
| DOM mutation | ❌ | Tree is effectively immutable after parse |
| Style/layout invalidation | ❌ | Full page rebuild for hover/resize |
| `innerHTML` fragment parsing | ❌ | No fragment parser or scripting binding |
| Shadow DOM and slots | ❌ | |

## Events and interaction

| Feature | Lumen | Note |
|---|:-:|---|
| Pointer hit testing | ⚠️ | Layout-box hit test in the desktop shell |
| Link activation by mouse | ⚠️ | Navigates the nearest ancestor link |
| `:hover` state | ✅ | Hover chain triggers restyle and relayout |
| Scroll wheel and keyboard scrolling | ⚠️ | Basic vertical document scrolling |
| Event object and dispatch | ❌ | |
| Capture / target / bubble phases | ❌ | |
| Default actions and cancellation | ❌ | Link click is hard-coded, not event-driven |
| Pointer and mouse events | ❌ | Native window input is not exposed to the DOM |
| Keyboard events | ❌ | Shell shortcuts only |
| Touch events / Pointer Events | ❌ | |
| Drag and drop | ❌ | |

## Focus, selection and editing

| Feature | Lumen | Note |
|---|:-:|---|
| Focus model | ❌ | No focused element |
| Sequential keyboard navigation | ❌ | Tab does not move through controls/links |
| `:focus` / `:focus-visible` | ❌ | |
| Text selection | ❌ | |
| Clipboard operations | ❌ | |
| Editable content | ❌ | No controls or `contenteditable` |
| Selection and Range APIs | ❌ | |

