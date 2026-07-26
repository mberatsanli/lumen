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
| `document.getElementsByClassName/TagName` | ✅ | |
| `document.body` / `document.head` / `document.documentElement` | ✅ | fall back to the root when the literal element is missing |
| `document.readyState` | ⚠️ | always `"complete"` (scripts only run after the parse) |
| `document.addEventListener` / `removeEventListener` | ✅ | window/document listeners land on the root; `DOMContentLoaded` and `load` fire after page scripts run |
| `localStorage` / `sessionStorage` | ✅ | localStorage persists per origin on disk (`<config>/lumen/storage/`, 5 MiB cap); sessionStorage lives with the page's script world; named-property access (`storage.x`) and unbound methods work |
| `navigator` | ⚠️ | `userAgent`, `language`/`languages`, `platform`, `onLine` (always true) |
| `matchMedia` / `requestAnimationFrame` / `getComputedStyle` | ⚠️ | survival stubs: matchMedia never matches, rAF is a 16ms timeout |
| `element.textContent` / `innerText` | ✅ | live accessor properties; writes relayout the page |
| `element.value` | ✅ | reads live form state, writes update the control |
| `element.getAttribute` / `setAttribute` | ✅ | `setAttribute('style.color', …)` merges into the style attribute |
| `element.addEventListener` / `removeEventListener` | ⚠️ | `click` (bubbles to ancestors), `input`, and `submit` on forms; the handler gets `{ type, target, preventDefault, stopPropagation }`; null listeners are ignored and the options argument (boolean or `{capture, passive, once}`) is tolerated — capture/passive are ignored, `once` works |
| `event.target.closest(selector)` | ✅ | the target is a full element wrapper; `closest` walks self + ancestors with the selector subset |
| `event.preventDefault` / `stopPropagation` | ✅ | cancels link follows, form submits and control activation |
| `element.classList` / `className` | ✅ | add/remove/toggle/contains, writing through to the class attribute |
| `document.createElement` / `el.appendChild` / `el.insertBefore` / `el.removeChild` / `el.remove` | ✅ | detached nodes render nothing until appended; appends/inserts refuse cycles; `insertBefore(new, null)` appends |
| `el.parentNode` / `childNodes` / `firstChild` / `lastChild` / `nextSibling` / `previousSibling` | ✅ | node-level traversal (text nodes included), null at the edges |
| `iframe.contentDocument` / `contentWindow` | ⚠️ | always null (no nested browsing contexts), so frame feature-detection bails out cleanly |
| `el.innerHTML` | ✅ | set parses the fragment with the engine's own HTML parser (fragment scripts stay inert); get serializes |
| `el.style.x` / `el.dataset.x` | ✅ | live proxies writing through to the style / data-* attributes (camelCase → kebab-case) |
| `el.parentElement` / `el.children` / element `querySelector(All)` | ✅ | subtree-scoped queries |
| `el.getBoundingClientRect` | ✅ | real border-box geometry from the layout tree |
| `el.focus()` / `el.blur()` + `focus`/`blur` events | ✅ | shell applies the change; Tab/Shift+Tab cycles focusable controls |
| `keydown` / `keyup` | ⚠️ | dispatched to the focused control (else the document) with `event.key`; preventDefault skips shell defaults |
| `fetch` | ⚠️ | GET only; blocking under the hood, resolved between script entries; `response.text()`/`.json()`; no headers/status detail |
| `XMLHttpRequest` | ⚠️ | `open`/`setRequestHeader`/`send`, `readyState`/`status`/`responseText`, `onreadystatechange`/`onload`/`onerror`; rides the same fetch pump (cookies + file:// gate + same-origin policy included); `withCredentials` sends cookies cross-origin (an ACAO `*` grant then no longer suffices, per spec); non-simple cross-origin requests (custom header, method beyond GET/HEAD/POST, non-simple Content-Type) run a CORS preflight (OPTIONS) first, cached per origin+URL for the session; only Content-Type reaches the wire |
| `document.cookie` | ⚠️ | reads the jar for the page URL, writes store through it (Path/Domain/Max-Age honored); HttpOnly not hidden |
| `window` / `location` | ⚠️ | `window` aliases the global object; `location.href` read/write (write navigates) and `location.reload()` |
| `history` | ⚠️ | `pushState`/`replaceState` rewrite the URL without reloading (same-origin enforced), `back`/`forward`/`go` traverse via the shell and fire `popstate`; `state` is JSON-serializable values |

`<script>` elements (inline or `src=`) run once after the page first
renders, in document order, sharing one global scope. Classic scripts
run; `defer` scripts and `type="module"` scripts (static imports are
fetched level by level, runtime `import()` goes through the network
queue) run after parsing in document order. `type="application/ld+json"`,
templates and import maps are skipped. Runtime errors abort the current
script/handler with a `[js] script error: …` message and the page keeps
working. One distinct error message prints at most four times (three
plain, one with a "repeated; further occurrences suppressed" note), so a
misfiring handler cannot flood the terminal.

Known module limit: a **top-level `await` on the page's own `fetch()`
promise never settles**. Boa's async-job future borrows the context for
the duration of one job-drain call and cannot be stored across script
entries (see the `BoundedJobExecutor` notes in `scripting.rs`), so the
parked module evaluation is dropped. Top-level awaits on
module-internal promises (imports, already-resolved values) do settle —
put the fetch in a `.then()` chain instead.

## History

The first JS milestone shipped a hand-written interpreter (`lumen-js`,
commit `9a4ffd5`). The project then pivoted from "educational,
everything from scratch" to a hobby project that prefers existing
crates, and the engine was replaced with Boa — the `Host`-style bridge
design made the swap local to `lumen-browser/src/scripting.rs`.
