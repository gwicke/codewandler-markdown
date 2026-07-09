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

Published on crates.io under the `codewandler-` prefix; the import names stay short
(`use markdown::…`, `use markdown_stream::…`).

| Crate | crates.io | What |
|---|---|---|
| `markdown-stream` | `codewandler-markdown-stream` | the incremental parser + event model (pure `std`) |
| `markdown-html` | `codewandler-markdown-html` | events → HTML (the compliance oracle) |
| `markdown-terminal` | `codewandler-markdown-terminal` | events → styled terminal output (ANSI) + the live renderer |
| `markdown-ratatui` | `codewandler-markdown-ratatui` | events → `ratatui` `Text` for TUIs |
| `markdown` | `codewandler-markdown` | the top-level facade (`render_string`, `html_string`, `parse`) |
| `mdview` | — | the CLI (workspace-only; the name is taken on crates.io) |

```sh
cargo add codewandler-markdown   # imported in code as `markdown`
```

## Status

The streaming parser, the live terminal renderer, and the `mdview` CLI are working end to end:

- **Parser** — streaming blocks (headings, fenced code, blockquotes, bullet/ordered lists, thematic
  breaks, GFM tables) + inlines (emphasis, strong, code spans, links, images, strikethrough,
  autolinks, escapes, hard/soft breaks); chunk-safe (**split-equivalent**). CommonMark **219/652**,
  GFM 222/672 and climbing.
- **Terminal renderer** — themed ANSI, width-aware word wrapping, indented lists/blockquotes, GFM
  tables with per-column alignment + box-drawing borders, and dependency-free syntax highlighting for
  fenced code. TTY / `NO_COLOR` aware (plain when piped).
- **`mdview`** — `mdview FILE` or `… | mdview`: render Markdown to the terminal, streaming.

```sh
printf '# Hi\n\n- **bold** item\n\n```rust\nfn main() {}\n```\n' | cargo run -q -p mdview
```

### Next

- Full CommonMark/GFM compliance (the corpora ratchet upward), nested lists, setext headings,
  reference links, indented code.
- HTML table rendering; the live in-place redraw of streaming tables; a ratatui viewport; criterion
  benchmarks vs `pulldown-cmark`/`comrak`/`termimad`.

## License

MIT OR Apache-2.0
