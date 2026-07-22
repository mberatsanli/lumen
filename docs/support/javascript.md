# JavaScript compatibility

✅ supported · ⚠️ partial · ❌ not supported.

**Lumen executes no JavaScript, by design** (see ADR 0005): script support
multiplies the complexity of every other component, and v0.x is about
getting parse → cascade → layout → paint right first. `<script>` content
is parsed as raw text and hidden — it never runs. This whole table is
therefore a roadmap, not a scorecard.

The one honest ✅: pages behave exactly as they would in a browser with
scripting disabled, including `<noscript>` content rendering.

## Engine

| Feature | Lumen | Chrome | Firefox | Safari | Note |
|---|:-:|:-:|:-:|:-:|---|
| Script execution (any) | ❌ | ✅ | ✅ | ✅ | deliberate — ADR 0005 |
| `<script>` inline | ❌ | ✅ | ✅ | ✅ | parsed + hidden, never run |
| `<script src>` | ❌ | ✅ | ✅ | ✅ | not fetched |
| `<script type="module">` / ESM | ❌ | ✅ | ✅ | ✅ | |
| `defer` / `async` | ❌ | ✅ | ✅ | ✅ | |
| `<noscript>` behavior | ✅ | ✅ | ✅ | ✅ | contents render, as with JS off |
| Event loop / microtasks | ❌ | ✅ | ✅ | ✅ | |
| Web Workers | ❌ | ✅ | ✅ | ✅ | |
| WebAssembly | ❌ | ✅ | ✅ | ✅ | |

## Language (when an interpreter lands)

| Feature | Lumen | Chrome | Firefox | Safari | Note |
|---|:-:|:-:|:-:|:-:|---|
| ES5 core (types, functions, prototypes) | ❌ | ✅ | ✅ | ✅ | likely first target |
| `let`/`const`, arrow functions, classes | ❌ | ✅ | ✅ | ✅ | |
| Template literals, destructuring, spread | ❌ | ✅ | ✅ | ✅ | |
| Iterators / generators | ❌ | ✅ | ✅ | ✅ | |
| Promises / `async`-`await` | ❌ | ✅ | ✅ | ✅ | |
| `Map`/`Set`, typed arrays | ❌ | ✅ | ✅ | ✅ | |
| Proxy / Reflect | ❌ | ✅ | ✅ | ✅ | |
| RegExp | ❌ | ✅ | ✅ | ✅ | |
| Intl | ❌ | ✅ | ✅ | ✅ | |

## DOM API

| Feature | Lumen | Chrome | Firefox | Safari | Note |
|---|:-:|:-:|:-:|:-:|---|
| `document.getElementById` / `querySelector*` | ❌ | ✅ | ✅ | ✅ | engine has the internals (id lookup, selector matching) but no JS binding |
| DOM tree mutation (`appendChild`, `remove`, ...) | ❌ | ✅ | ✅ | ✅ | would require style/layout invalidation |
| `innerHTML` / `textContent` | ❌ | ✅ | ✅ | ✅ | |
| `classList`, `getAttribute`/`setAttribute` | ❌ | ✅ | ✅ | ✅ | |
| `element.style` CSSOM | ❌ | ✅ | ✅ | ✅ | |
| `getComputedStyle` | ❌ | ✅ | ✅ | ✅ | computed styles exist internally |
| Geometry APIs (`getBoundingClientRect`) | ❌ | ✅ | ✅ | ✅ | layout boxes exist internally |
| Events (`addEventListener`, bubbling) | ❌ | ✅ | ✅ | ✅ | |
| `MutationObserver` / `ResizeObserver` / `IntersectionObserver` | ❌ | ✅ | ✅ | ✅ | |
| Shadow DOM | ❌ | ✅ | ✅ | ✅ | |

## Browser APIs

| Feature | Lumen | Chrome | Firefox | Safari | Note |
|---|:-:|:-:|:-:|:-:|---|
| `window`, `location`, `history` | ⚠️ | ✅ | ✅ | ✅ | pushState/replaceState without reload, back/forward with popstate |
| `console` | ❌ | ✅ | ✅ | ✅ | |
| `setTimeout` / `setInterval` / `requestAnimationFrame` | ❌ | ✅ | ✅ | ✅ | |
| `fetch` / `XMLHttpRequest` | ⚠️ | ✅ | ✅ | ✅ | fetch is GET-only; XHR covers open/send + onload/onerror |
| `localStorage` / `sessionStorage` / IndexedDB | ⚠️ | ✅ | ✅ | ✅ | storage works (localStorage persisted per origin); IndexedDB ❌ |
| Cookies (`document.cookie`) | ❌ | ✅ | ✅ | ✅ | |
| Canvas 2D / WebGL / WebGPU | ❌ | ✅ | ✅ | ⚠️ | WebGPU still rolling out in some Safari versions |
| Clipboard, Notifications, Geolocation | ❌ | ✅ | ✅ | ✅ | |
| Service Workers | ❌ | ✅ | ✅ | ✅ | |
| WebSocket / WebRTC | ❌ | ✅ | ✅ | ✅ | |
