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

## Releasing

Releases are CI-driven — never `cargo publish` locally.

1. Bump `[workspace.package] version` **and** the `version =` fields on the internal deps in the root
   `Cargo.toml` (the release gate fails if the tag and workspace version disagree).
2. Run the green gate; commit `Release X.Y.Z` (title + bullet body).
3. `git tag -a vX.Y.Z -m "Release X.Y.Z"`, then push `main` + the tag.
4. The `release` workflow (`.github/workflows/release.yml`) does the rest: gate →
   `cargo publish --workspace` → GitHub release with generated notes. The crates.io token is the
   org-level Actions secret `CARGO_REGISTRY_TOKEN` (org secrets don't show in `gh secret list -R`).

Publishing facts:

- Library crates publish under the `codewandler-` prefix (`markdown` is taken on crates.io);
  `[lib] name` keeps the short imports (`use markdown::…`, `use markdown_stream::…`), so renames
  never touch source.
- `mdview` is `publish = false` — that name is taken on crates.io too.
- `Cargo.lock` is gitignored (intentional): dependency upgrades surface only as manifest
  requirement changes.
- v0.2.0 was tagged but never published (local token lacked the `publish-new` scope); the crates.io
  history starts at 0.2.1.
