# markdown

**An incremental Markdown parser and streaming terminal renderer, in Rust.**

Parse incrementally. Render immediately. Keep memory bounded.

A Rust port of [`codewandler/markdown-go`](https://github.com/codewandler/markdown-go) — built for
rendering streaming output (e.g. an LLM's tokens) to a terminal as it arrives, without waiting for the
whole document.

> **Status:** under active construction. The workspace, parser contract, and the CommonMark/GFM
> compliance harness are in place; the parser and terminal renderer are being ported milestone by
> milestone (see [the roadmap](#roadmap)).

## Why

- **Streaming** — feed bytes in any chunking; the event stream is identical regardless of how input is
  split (split-equivalence). Blocks are emitted as soon as they close.
- **Bounded memory** — proportional to unresolved (open) state, not document size.
- **Terminal-first** — themed ANSI output with width-aware wrapping, syntax-highlighted code, and a
  live renderer that redraws as content streams in. HTML output exists primarily to measure compliance.
- **Compliance-driven** — developed against the CommonMark 0.31.2 (652) and GFM (728) spec corpora.

## Workspace

| Crate | What |
|---|---|
| `markdown-stream` | the incremental parser + event model (pure `std`) |
| `markdown-html` | events → HTML (the compliance oracle) |
| `markdown-terminal` | events → styled terminal output (ANSI) + the live renderer |
| `markdown` | the top-level facade (`render_string`, `html_string`, `parse`) |

## Roadmap

- **M0** — workspace + parser contract + vendored corpora + compliance/split harness ✅
- **M1** — CommonMark block parser + minimal HTML
- **M2** — CommonMark inlines → 100% (652/652)
- **M3** — GFM (tables, strikethrough, task lists, autolinks)
- **M4** — terminal renderer (themes, wrapping, highlight, live tables)
- **M5+** — CLI viewer, ratatui viewport, benchmarks

## License

MIT OR Apache-2.0
