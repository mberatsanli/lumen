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
| Character references | ⚠️ | ✅ | ✅ | ✅ | named subset + numeric; full table missing |
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
| `<title>` | ⚠️ | ✅ | ✅ | ✅ | hidden from layout; not used as window title |
| `<style>` | ✅ | ✅ | ✅ | ✅ | document-order extraction |
| `<link rel="stylesheet">` | ❌ | ✅ | ✅ | ✅ | next up: loader is ready |
| `<meta>` | ⚠️ | ✅ | ✅ | ✅ | hidden; viewport/charset ignored |
| `<base>` | ❌ | ✅ | ✅ | ✅ | href not used |

## Sections & grouping

| Feature | Lumen | Chrome | Firefox | Safari | Note |
|---|:-:|:-:|:-:|:-:|---|
| `<h1>`–`<h6>` | ⚠️ | ✅ | ✅ | ✅ | block; UA sizes only for h1/h2 |
| `<p> <div>` | ✅ | ✅ | ✅ | ✅ | |
| `<section> <article> <header> <footer> <main> <nav> <aside> <blockquote>` | ✅ | ✅ | ✅ | ✅ | block flow |
| `<ul> <ol> <li>` | ⚠️ | ✅ | ✅ | ✅ | block only; no bullets/numbers/indent |
| `<dl> <dt> <dd>` | ❌ | ✅ | ✅ | ✅ | |
| `<pre>` | ⚠️ | ✅ | ✅ | ✅ | whitespace still collapses; not monospace |
| `<hr>` | ⚠️ | ✅ | ✅ | ✅ | void+block but invisible (no default border) |
| `<figure> <figcaption>` | ❌ | ✅ | ✅ | ✅ | |
| `<details> <summary>` | ❌ | ✅ | ✅ | ✅ | |
| `<dialog>` | ❌ | ✅ | ✅ | ✅ | |

## Text-level semantics

| Feature | Lumen | Chrome | Firefox | Safari | Note |
|---|:-:|:-:|:-:|:-:|---|
| `<a href>` | ⚠️ | ✅ | ✅ | ✅ | clickable, :hover, pointer, UA underline — but no inline flow |
| `<span> <strong> <em> <b> <i> <code> <small>` | ⚠️ | ✅ | ✅ | ✅ | inline computed + inheritance; still stack vertically; no UA bold/italic/mono |
| `<br>` | ❌ | ✅ | ✅ | ✅ | parsed, produces no line break |
| `<sub> <sup>` | ❌ | ✅ | ✅ | ✅ | |
| `<mark> <ins> <del> <s> <u>` | ❌ | ✅ | ✅ | ✅ | |
| `<abbr title>` | ❌ | ✅ | ✅ | ✅ | |

## Embedded content

| Feature | Lumen | Chrome | Firefox | Safari | Note |
|---|:-:|:-:|:-:|:-:|---|
| `<img>` | ❌ | ✅ | ✅ | ✅ | parsed as void, not rendered — next big item |
| `<picture> <source>` | ❌ | ✅ | ✅ | ✅ | |
| Inline `<svg>` | ❌ | ✅ | ✅ | ✅ | |
| `<video> <audio>` | ❌ | ✅ | ✅ | ✅ | out of scope |
| `<canvas>` | ❌ | ✅ | ✅ | ✅ | out of scope |
| `<iframe>` | ❌ | ✅ | ✅ | ✅ | out of scope |

## Tables & forms

| Feature | Lumen | Chrome | Firefox | Safari | Note |
|---|:-:|:-:|:-:|:-:|---|
| `<table>` family | ⚠️ | ✅ | ✅ | ✅ | parses; flows as plain blocks, no table layout |
| `<form>` submission | ❌ | ✅ | ✅ | ✅ | |
| `<input>` (all types) | ❌ | ✅ | ✅ | ✅ | |
| `<textarea> <select> <button>` | ❌ | ✅ | ✅ | ✅ | |
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
| `src` / `srcset` / `alt` | ❌ | ✅ | ✅ | ✅ | |
| `title` tooltip | ❌ | ✅ | ✅ | ✅ | |
| `hidden` | ❌ | ✅ | ✅ | ✅ | |
| `target` / `rel` / `download` | ❌ | ✅ | ✅ | ✅ | |
| `colspan` / `rowspan` | ❌ | ✅ | ✅ | ✅ | |
| ARIA attributes | ❌ | ✅ | ✅ | ✅ | |

## Navigation / loading

| Feature | Lumen | Chrome | Firefox | Safari | Note |
|---|:-:|:-:|:-:|:-:|---|
| `file://` documents | ✅ | ✅ | ✅ | ✅ | |
| `http(s)://` + redirects | ✅ | ✅ | ✅ | ✅ | |
| Relative URL resolution | ✅ | ✅ | ✅ | ✅ | against final URL |
| History back/forward/refresh | ⚠️ | ✅ | ✅ | ✅ | re-fetches; no cache |
| Fragment `#anchor` scroll | ❌ | ✅ | ✅ | ✅ | |
| Cookies / cache / compression | ❌ | ✅ | ✅ | ✅ | |
