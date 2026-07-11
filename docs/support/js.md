# JavaScript support

Lumen runs page scripts through `lumen-js`, a hand-written lexer +
recursive-descent parser + tree-walking interpreter (no engine
libraries). Objects live in an arena inside the per-page runtime; the
whole world is dropped on navigation.

## Language

| Feature | Lumen | Note |
|---|:-:|---|
| `let` / `const` / `var` | ⚠️ | all lexically scoped the same way (no hoisting of `var`, no TDZ) |
| functions, closures | ✅ | declarations hoist within their block |
| arrow functions | ✅ | expression and block bodies; no `this` binding (none needed — no `this` at all yet) |
| `if` / `else`, `while`, classic `for` | ✅ | loops guarded against runaway iteration |
| `for...of` | ⚠️ | arrays and strings; `for...in` parses but iterates the same values |
| `break` / `continue` / `return` | ✅ | |
| objects / arrays | ✅ | literals, member + index access, shorthand `{ x }`; shared by reference |
| `++` `--`, compound assignment | ✅ | |
| ternary, `&&` `\|\|` `??` | ✅ | short-circuiting |
| `==` `===` and friends | ⚠️ | practical coercion subset (number↔string, bool→number) |
| `typeof` | ✅ | |
| template literals | ⚠️ | backtick strings work; `${}` interpolation not yet |
| classes, `this`, `new`, prototypes | ❌ | |
| exceptions (`try` / `throw`) | ❌ | runtime errors abort the current handler with a console message |
| async / promises / modules | ❌ | |

## Built-ins

| API | Lumen | Note |
|---|:-:|---|
| `console.log/warn/error` | ✅ | printed to the shell's terminal as `[js] …` |
| `Math` | ⚠️ | abs floor ceil round sqrt min max pow random PI |
| `JSON.stringify` | ⚠️ | no indent/replacer; keys sorted |
| `parseInt` / `parseFloat` / `Number` / `String` | ⚠️ | |
| string methods | ⚠️ | length toUpperCase toLowerCase trim includes indexOf slice split charAt repeat replace |
| array methods | ⚠️ | length push pop join indexOf includes slice map filter forEach |
| `setTimeout` / `setInterval` | ⚠️ | driven by the shell's frame clock; no clearTimeout yet |

## DOM

| API | Lumen | Note |
|---|:-:|---|
| `document.getElementById` | ✅ | |
| `document.querySelector(All)` | ⚠️ | `#id`, `.class`, `tag`, `tag.class` |
| `element.textContent` / `innerText` | ✅ | live read/write; writes relayout the page |
| `element.value` | ✅ | reads live form state, writes update the control |
| `element.getAttribute` / `setAttribute` | ✅ | `setAttribute('style.color', …)` merges into the style attribute |
| `element.addEventListener` | ⚠️ | `click` (bubbles to ancestors) and `input`; the handler gets `{ type, target }` |
| `createElement` / `appendChild` / `remove` | ❌ | structural DOM edits are the next milestone |
| `preventDefault` / `stopPropagation` | ❌ | |

`<script>` elements (inline or `src=`) run once after the page first
renders, in document order, sharing one global scope.
