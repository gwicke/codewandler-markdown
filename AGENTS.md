# AGENTS.md — working contract for `codewandler/markdown` (Rust)

A faithful Rust port of the Go `codewandler/markdown-go`: an **incremental, streaming** CommonMark/GFM
parser with terminal + HTML renderers. The Go original is the reference for behavior and the source of
the compliance corpora.

## Product invariants (do not regress)

- **The parser is append-only and chunk-safe.** `Parser::write(chunk)` may be called with the input
  split at *any* byte boundary and must emit the *same* event stream as a single `write` of the whole
  input (split-equivalence). This is the reason the library exists.
- **Renderers never parse Markdown.** `markdown-html` and `markdown-terminal` consume the
  `markdown-stream` event stream only. Any new inline syntax goes through the parser (a future
  `InlineScanner` + an inline event), never as renderer-side string munging.
- **Memory is bounded by unresolved state, not document size.** Completed blocks are emitted and
  dropped; large partial buffers are released; scratch buffers are reused.
- **Terminal rendering is the first-class output path.** HTML exists primarily to score compliance.
- **Pure `std` parser.** `markdown-stream` has no runtime dependencies.

## Layout

| Crate | Role | Go origin |
|---|---|---|
| `markdown-stream` | the incremental parser + event model (pure std) | `stream/` |
| `markdown-html` | events → HTML (compliance oracle) | `html/` |
| `markdown-terminal` | events → ANSI; themes, width, highlight, live renderer | `terminal/` |
| `markdown` | facade: `render_string` / `html_string` / `parse` | `markdown.go` |
| `corpus/` | vendored CommonMark/GFM spec JSON (the acceptance criteria) | `internal/*tests/testdata` |

## Compliance is the spec

The acceptance criteria are the vendored corpora (`corpus/`): CommonMark 0.31.2 (652), GFM spec (672),
GFM extensions (30), GFM regression (26). The compliance test renders `parse → HTML` and compares to
the expected HTML; a `WANT_*` baseline constant fails the build on any regression. To raise compliance:
find a failing section, fix it, re-scan for newly-passing cases (fixes unlock others), then bump the
baseline.

## The green gate (run before every commit)

```bash
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all --check
```

Narrowly-scoped changes; don't revert others' work; ASCII by default. Mirror the Go original's behavior
when in doubt — it's cloned read-only alongside as the reference.
