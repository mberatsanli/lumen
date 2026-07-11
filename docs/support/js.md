# JavaScript support

Lumen runs page scripts on [Boa](https://github.com/boa-dev/boa)
(`boa_engine`), a JavaScript engine written in pure Rust. The language
itself is therefore essentially complete — classes, closures, template
literals, destructuring, try/catch, regex, Promises (microtasks run when
the job queue is driven), `Math`/`JSON`/`Date` and the rest of the
standard library all come from Boa.

What Lumen itself provides is the **web platform layer**: the DOM
bindings, events and timers below. One `boa_engine::Context` lives per
page (on the shell's main thread — Boa's GC handles cannot cross the
loader thread); the whole world drops on navigation.

## Platform APIs

| API | Lumen | Note |
|---|:-:|---|
| `console.log/warn/error` | ✅ | printed to the shell's terminal as `[js] …` |
| `setTimeout` / `setInterval` / `clearTimeout` / `clearInterval` | ✅ | driven by the shell's frame clock |
| `document.getElementById` | ✅ | |
| `document.querySelector(All)` | ⚠️ | `#id`, `.class`, `tag`, `tag.class` |
| `element.textContent` / `innerText` | ✅ | live accessor properties; writes relayout the page |
| `element.value` | ✅ | reads live form state, writes update the control |
| `element.getAttribute` / `setAttribute` | ✅ | `setAttribute('style.color', …)` merges into the style attribute |
| `element.addEventListener` | ⚠️ | `click` (bubbles to ancestors), `input`, and `submit` on forms; the handler gets `{ type, target, preventDefault, stopPropagation }` |
| `event.preventDefault` / `stopPropagation` | ✅ | cancels link follows, form submits and control activation |
| `element.classList` / `className` | ✅ | add/remove/toggle/contains, writing through to the class attribute |
| `document.createElement` / `el.appendChild` / `el.remove` | ✅ | detached nodes render nothing until appended; appends refuse cycles |
| `fetch` | ⚠️ | GET only; blocking under the hood, resolved between script entries; `response.text()`/`.json()`; no headers/status detail |
| `window` / `location` | ⚠️ | `window` aliases the global object; `location.href` read/write (write navigates) and `location.reload()`; no `history` |

`<script>` elements (inline or `src=`) run once after the page first
renders, in document order, sharing one global scope. Runtime errors
abort the current script/handler with a `[js] script error: …` message
and the page keeps working.

## History

The first JS milestone shipped a hand-written interpreter (`lumen-js`,
commit `9a4ffd5`). The project then pivoted from "educational,
everything from scratch" to a hobby project that prefers existing
crates, and the engine was replaced with Boa — the `Host`-style bridge
design made the swap local to `lumen-browser/src/scripting.rs`.
