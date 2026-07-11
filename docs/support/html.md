# HTML compatibility

✅ supported · ⚠️ partial (see note) · ❌ not supported.

The parser builds a DOM node for **any** element and never fails.
"Supported" means real engine behavior (styling, layout role,
interaction) — not just a DOM node.

## Parsing / tokenizer

| Feature | Lumen | Chrome | Firefox | Safari | Note |
|---|:-:|:-:|:-:|:-:|---|
| Start/end tags, nesting | ✅ | ✅ | ✅ | ✅ | |
| Quoted/unquoted/boolean attributes | ✅ | ✅ | ✅ | ✅ | |
| Self-closing `<br />` | ✅ | ✅ | ✅ | ✅ | |
| Void elements | ✅ | ✅ | ✅ | ✅ | 13-element set |
| Comments | ✅ | ✅ | ✅ | ✅ | dropped from DOM |
| Doctype | ✅ | ✅ | ✅ | ✅ | recognized, dropped; no quirks mode |
| Character references | ⚠️ | ✅ | ✅ | ✅ | ~80 common named + numeric forms; full 2200-entry table missing |
| Raw text (`script/style/title/textarea`) | ✅ | ✅ | ✅ | ✅ | |
| Error recovery | ✅ | ✅ | ✅ | ✅ | mismatched tags, stray `<`, EOF cases |
| Case normalization, duplicate attrs | ✅ | ✅ | ✅ | ✅ | first attribute wins |
| Implied `<html>/<head>/<body>` | ❌ | ✅ | ✅ | ✅ | not synthesized |
| WHATWG insertion modes | ❌ | ✅ | ✅ | ✅ | no foster parenting etc. |
| Foreign content (SVG/MathML) | ❌ | ✅ | ✅ | ✅ | |
| Parse diagnostics / source spans | ❌ | — | — | — | devtools concern |

## Document metadata

| Feature | Lumen | Chrome | Firefox | Safari | Note |
|---|:-:|:-:|:-:|:-:|---|
| `<html> <head> <body>` | ✅ | ✅ | ✅ | ✅ | when present |
| `<title>` | ✅ | ✅ | ✅ | ✅ | shown as the desktop window title |
| `<style>` | ✅ | ✅ | ✅ | ✅ | document-order extraction |
| `<link rel="stylesheet">` | ✅ | ✅ | ✅ | ✅ | document order; relative hrefs vs final URL; failures skip the sheet |
| `<meta>` | ⚠️ | ✅ | ✅ | ✅ | hidden; viewport/charset ignored |
| `<base>` | ❌ | ✅ | ✅ | ✅ | href not used |

## Sections & grouping

| Feature | Lumen | Chrome | Firefox | Safari | Note |
|---|:-:|:-:|:-:|:-:|---|
| `<h1>`–`<h6>` | ✅ | ✅ | ✅ | ✅ | UA sizes/weights/margins for all six |
| `<p> <div>` | ✅ | ✅ | ✅ | ✅ | |
| `<section> <article> <header> <footer> <main> <nav> <aside> <blockquote>` | ✅ | ✅ | ✅ | ✅ | block flow |
| `<ul> <ol> <li>` | ⚠️ | ✅ | ✅ | ✅ | block only; no bullets/numbers/indent |
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
| `<form>` submission | ❌ | ✅ | ✅ | ✅ | |
| `<input>` (all types) | ⚠️ | ✅ | ✅ | ✅ | visual only: bordered box with value/placeholder text (password bulleted, hidden hidden, checkbox/radio as small squares); not interactive |
| `<textarea> <select> <button>` | ⚠️ | ✅ | ✅ | ✅ | visual only: bordered boxes, button labels render; not interactive |
| `<label> <fieldset> <progress> <meter>` | ❌ | ✅ | ✅ | ✅ | |
| Focus / validation | ❌ | ✅ | ✅ | ✅ | |

## Scripting-related elements

| Feature | Lumen | Chrome | Firefox | Safari | Note |
|---|:-:|:-:|:-:|:-:|---|
| `<script>` | ⚠️ | ✅ | ✅ | ✅ | raw-text parsed, hidden, **never executed** (deliberate) |
| `<noscript>` | ⚠️ | ✅ | ✅ | ✅ | contents render — correct for a no-JS browser |
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
| Fragment `#anchor` scroll | ⚠️ | ✅ | ✅ | ✅ | scrolls to `id` targets after navigation; no :target styling |
| Cookies / cache / compression | ❌ | ✅ | ✅ | ✅ | |
