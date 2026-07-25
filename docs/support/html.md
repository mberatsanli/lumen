# HTML compatibility

✅ supported · ⚠️ partial (see note) · ❌ not supported.

The parser builds a DOM node for **any** element and never fails.
"Supported" means real engine behavior (styling, layout role,
interaction) — not just a DOM node.

## Parsing / tokenizer

Parsing is delegated to html5ever (the WHATWG HTML5 algorithm), so the
rows below mostly describe spec behavior; "Note" flags our deliberate
simplifications.

| Feature | Lumen | Chrome | Firefox | Safari | Note |
|---|:-:|:-:|:-:|:-:|---|
| Start/end tags, nesting | ✅ | ✅ | ✅ | ✅ | |
| Quoted/unquoted/boolean attributes | ✅ | ✅ | ✅ | ✅ | |
| Self-closing `<br />` | ✅ | ✅ | ✅ | ✅ | `/>` honored only on void/foreign elements, per spec |
| Void elements | ✅ | ✅ | ✅ | ✅ | |
| Comments | ✅ | ✅ | ✅ | ✅ | dropped from DOM |
| Doctype | ✅ | ✅ | ✅ | ✅ | recognized, dropped; no quirks mode |
| Character references | ✅ | ✅ | ✅ | ✅ | the full WHATWG named table (via html5ever) + numeric forms |
| Raw text (`script/style`), RCDATA (`title/textarea`), `xmp/iframe/noembed/noframes/plaintext` | ✅ | ✅ | ✅ | ✅ | |
| Error recovery | ✅ | ✅ | ✅ | ✅ | full WHATWG tree-construction recovery; errors counted, never fatal |
| Case normalization, duplicate attrs | ✅ | ✅ | ✅ | ✅ | first attribute wins |
| Implied `<html>/<head>/<body>` | ✅ | ✅ | ✅ | ✅ | always synthesized |
| WHATWG insertion modes | ✅ | ✅ | ✅ | ✅ | adoption agency, foster parenting, implied `<tbody>`, ... |
| Foreign content (SVG/MathML) | ⚠️ | ✅ | ✅ | ✅ | correct tag names; namespaces flattened to local names |
| Parse diagnostics / source spans | ⚠️ | — | — | — | error count only, no spans |

## Document metadata

| Feature | Lumen | Chrome | Firefox | Safari | Note |
|---|:-:|:-:|:-:|:-:|---|
| `<html> <head> <body>` | ✅ | ✅ | ✅ | ✅ | when present |
| `<title>` | ✅ | ✅ | ✅ | ✅ | shown as the desktop window title |
| `<style>` | ✅ | ✅ | ✅ | ✅ | document-order extraction |
| `<link rel="stylesheet">` | ✅ | ✅ | ✅ | ✅ | document order; relative hrefs vs final URL; failures skip the sheet |
| `<meta>` | ⚠️ | ✅ | ✅ | ✅ | hidden; charset drives decoding, viewport ignored |
| `<base>` | ❌ | ✅ | ✅ | ✅ | href not used |

## Sections & grouping

| Feature | Lumen | Chrome | Firefox | Safari | Note |
|---|:-:|:-:|:-:|:-:|---|
| `<h1>`–`<h6>` | ✅ | ✅ | ✅ | ✅ | UA sizes/weights/margins for all six |
| `<p> <div>` | ✅ | ✅ | ✅ | ✅ | |
| `<section> <article> <header> <footer> <main> <nav> <aside> <blockquote>` | ✅ | ✅ | ✅ | ✅ | block flow |
| `<ul> <ol> <li>` | ⚠️ | ✅ | ✅ | ✅ | bullets and 1. 2. numbering as inline markers + UA indent; `list-style: none` suppresses; no marker styling/`start` attr |
| `<dl> <dt> <dd>` | ❌ | ✅ | ✅ | ✅ | |
| `<pre>` | ⚠️ | ✅ | ✅ | ✅ | whitespace still collapses; not monospace |
| `<hr>` | ✅ | ✅ | ✅ | ✅ | UA: 1px solid top border |
| `<figure> <figcaption>` | ❌ | ✅ | ✅ | ✅ | |
| `<details> <summary>` | ❌ | ✅ | ✅ | ✅ | |
| `<dialog>` | ❌ | ✅ | ✅ | ✅ | |

## Text-level semantics

| Feature | Lumen | Chrome | Firefox | Safari | Note |
|---|:-:|:-:|:-:|:-:|---|
| `<a href>` | ✅ | ✅ | ✅ | ✅ | inline flow, clickable, :hover, pointer, UA blue underline |
| `<span> <strong> <em> <b> <i> <code> <small>` | ✅ | ✅ | ✅ | ✅ | inline flow; UA: strong/b bold, em/i synthetic italic, code 0.875em (no monospace yet) |
| `<br>` | ✅ | ✅ | ✅ | ✅ | hard line break in inline flow |
| `<sub> <sup>` | ❌ | ✅ | ✅ | ✅ | |
| `<mark> <ins> <del> <s> <u>` | ❌ | ✅ | ✅ | ✅ | |
| `<abbr title>` | ❌ | ✅ | ✅ | ✅ | |

## Embedded content

| Feature | Lumen | Chrome | Firefox | Safari | Note |
|---|:-:|:-:|:-:|:-:|---|
| `<img>` | ⚠️ | ✅ | ✅ | ✅ | PNG/JPEG/GIF/WebP (first frame, no animation); width/height attrs + CSS + intrinsic ratio; flows inline; max 32 images/page; no SVG-in-img, no lazy |
| `<picture> <source>` | ❌ | ✅ | ✅ | ✅ | |
| Inline `<svg>` | ❌ | ✅ | ✅ | ✅ | |
| `<video> <audio>` | ❌ | ✅ | ✅ | ✅ | out of scope |
| `<canvas>` | ❌ | ✅ | ✅ | ✅ | out of scope |
| `<iframe>` | ❌ | ✅ | ✅ | ✅ | out of scope |

## Tables & forms

| Feature | Lumen | Chrome | Firefox | Safari | Note |
|---|:-:|:-:|:-:|:-:|---|
| `<table>` family | ⚠️ | ✅ | ✅ | ✅ | real column/row layout with colspan/rowspan; thead/tbody/tfoot flattened; caption ignored; no border-collapse |
| `<form>` submission | ⚠️ | ✅ | ✅ | ✅ | GET and POST (urlencoded body): name=value pairs from all controls (multiple selects submit one pair per selection); history re-GETs POSTed pages |
| `<progress>` / `<meter>` / range | ⚠️ | ✅ | ✅ | ✅ | vector value bars; range thumb click-to-set and draggable |
| `<input>` (all types) | ⚠️ | ✅ | ✅ | ✅ | text inputs editable (caret/selection/clipboard, click-to-position; long values clip and scroll horizontally to follow the caret), checkbox/radio toggle, submit buttons submit, number steps with ↑/↓ (step/min/max), color opens a swatch palette; date/file etc render as text boxes; live typing reuses the cascade and only re-lays out, skipping selector matching per keystroke |
| `<textarea> <select> <button>` | ⚠️ | ✅ | ✅ | ✅ | select opens a shell-drawn dropdown (`multiple` renders an inline list box: click selects, Cmd/Ctrl+click toggles, rows highlight via `:checked`), `<optgroup>` labels shown; textarea edits multiline (Enter = newline, inner scroll follows the caret line), button submits; label clicks focus their control |
| `<label> <fieldset> <progress> <meter>` | ⚠️ | ✅ | ✅ | ✅ | label click focuses its control; fieldset/legend framed; progress/meter as vector bars |
| Focus / validation | ⚠️ | ✅ | ✅ | ✅ | Tab/Shift+Tab cycles controls (:focus styles apply, text controls open for editing); el.focus()/blur() from scripts; no validation |

## Scripting-related elements

| Feature | Lumen | Chrome | Firefox | Safari | Note |
|---|:-:|:-:|:-:|:-:|---|
| `<script>` | ⚠️ | ✅ | ✅ | ✅ | inline and `src=` scripts execute on the Boa engine (see [js.md](js.md)); runs once after first render |
| `<noscript>` | ⚠️ | ✅ | ✅ | ✅ | contents still render (no-JS fallback not yet suppressed) |
| `<template>` | ❌ | ✅ | ✅ | ✅ | contents should be inert; currently normal children |
| Custom elements / `<slot>` | ❌ | ✅ | ✅ | ✅ | |

## Attributes with behavior

| Feature | Lumen | Chrome | Firefox | Safari | Note |
|---|:-:|:-:|:-:|:-:|---|
| `id` | ✅ | ✅ | ✅ | ✅ | selectors + lookup |
| `class` | ✅ | ✅ | ✅ | ✅ | |
| `style` | ✅ | ✅ | ✅ | ✅ | strongest cascade origin |
| `href` (on `<a>`) | ✅ | ✅ | ✅ | ✅ | relative resolution + navigation |
| `src` / `srcset` / `alt` | ⚠️ | ✅ | ✅ | ✅ | src works; srcset picks the candidate nearest 1x (w descriptors → first); alt ignored |
| `title` tooltip | ❌ | ✅ | ✅ | ✅ | |
| `hidden` | ❌ | ✅ | ✅ | ✅ | |
| `target` / `rel` / `download` | ❌ | ✅ | ✅ | ✅ | |
| `colspan` / `rowspan` | ❌ | ✅ | ✅ | ✅ | |
| ARIA attributes | ❌ | ✅ | ✅ | ✅ | |

## Navigation / loading

| Feature | Lumen | Chrome | Firefox | Safari | Note |
|---|:-:|:-:|:-:|:-:|---|
| `file://` documents | ✅ | ✅ | ✅ | ✅ | |
| `http(s)://` + redirects | ✅ | ✅ | ✅ | ✅ | 5s connect / 20s request timeouts |
| Relative URL resolution | ✅ | ✅ | ✅ | ✅ | against final URL |
| History back/forward/refresh | ⚠️ | ✅ | ✅ | ✅ | re-fetches; no cache; desktop: address bar + nav buttons, loads on background thread |
| Omnibox (URL vs search) | ⚠️ | ✅ | ✅ | ✅ | desktop: explicit scheme/existing file/dotted host load directly, free text searches DuckDuckGo |
| Tabs | ⚠️ | ✅ | ✅ | ✅ | desktop: strip below the address bar; Cmd/Ctrl+T new, +W close, +1–9 switch, +Tab cycle; each tab its own session/cookies/scroll |
| Bookmarks | ⚠️ | ✅ | ✅ | ✅ | desktop: star toggles (Cmd/Ctrl+D), ▾ opens a dropdown; persisted as JSON in the platform config dir |
| Fragment `#anchor` scroll | ⚠️ | ✅ | ✅ | ✅ | scrolls to `id` targets after navigation; no :target styling |
| Cookies | ⚠️ | ✅ | ✅ | ✅ | session jar (RFC 6265 core: domain/path/Secure, Max-Age deletion via the cookie crate); document.cookie read/write; no persistence, no HttpOnly distinction for scripts |
| Cache / compression | ❌ | ✅ | ✅ | ✅ | |
