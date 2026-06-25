//! Split-equivalence: parsing an input as a single chunk must produce exactly the same event stream
//! as parsing it split at *any* byte boundary. This is the library's defining invariant — it holds
//! at every milestone, including the empty-stream scaffold.

use markdown_stream::{parse, parse_gfm, Event, Parser, StreamParser};

fn parse_split(input: &str, at: usize) -> Vec<Event> {
    let b = input.as_bytes();
    let mut p = StreamParser::new();
    let mut ev = p.write(&b[..at]);
    ev.extend(p.write(&b[at..]));
    ev.extend(p.flush());
    ev
}

fn parse_split_gfm(input: &str, at: usize) -> Vec<Event> {
    let b = input.as_bytes();
    let mut p = StreamParser::new_gfm();
    let mut ev = p.write(&b[..at]);
    ev.extend(p.write(&b[at..]));
    ev.extend(p.flush());
    ev
}

#[test]
fn split_equivalence_on_samples() {
    let samples = [
        "# Heading\n\npara *one*\n",
        "- a\n- b\n\n> quote\n",
        "```\ncode\n```\n",
        "text with `code` and **bold**\n",
        "1. first\n2. second\n",
        // HTML block (condition 6, blank-line terminated) followed by a paragraph.
        "<div>\nraw *html*\n</div>\n\nafter\n",
        // HTML block (condition 1, marker-terminated by `</pre>`) with an interior blank line.
        "<pre>\nx\n\ny\n</pre>\nokay\n",
        // Inline raw HTML inside a paragraph.
        "a <span class=\"x\"> b </span> c\n",
        // Nested lists: the looseness/tightness decision and the buffered list events must be
        // identical no matter where the chunk boundary falls.
        "- foo\n  - bar\n    - baz\n",
        // A loose list (blank line between items) — looseness is only known once the list closes,
        // so the buffered `EnterBlock(List{tight})` flag must still be chunk-independent.
        "- a\n\n- b\n\n- c\n",
        // Mixed: a loose outer item containing a tight nested list plus a trailing paragraph.
        "* foo\n  * bar\n\n  baz\n",
        // A list item whose content is a code block and a blockquote (multiple child blocks).
        "1.  foo\n\n    ```\n    bar\n    ```\n\n    > quux\n",
        // Character references in general text: named, decimal, and hex, including a decoded
        // delimiter (`&#42;` → `*`) that must stay literal regardless of the chunk boundary.
        "&copy; &amp; &#42;foo&#42; &#x41; &nbsp;end\n",
        // Hard line break (two trailing spaces) — trailing-space accumulation must be chunk-safe.
        "foo  \nbar\n",
        // Forward reference: `[foo]` is used in a paragraph that closes *before* its definition. The
        // parser must hold the paragraph (and everything after) until the def lands, re-resolving it
        // at flush — and the held/released event stream must be identical no matter where the chunk
        // boundary falls (including inside the held span and across the resolving definition line).
        "[foo] and [bar]\n\nmiddle para\n\n[foo]: /a\n[bar]: /b\n",
        // A forward reference whose label is *never* defined stays literal — still chunk-independent.
        "see [missing] here\n\nplain tail\n",
        // Forward reference inside a (loose) list item: the deferred run must survive list buffering.
        "- [foo]\n\n- bar\n\n[foo]: /url\n",
    ];
    for s in samples {
        let whole = parse(s);
        for at in 0..=s.len() {
            if !s.is_char_boundary(at) {
                continue;
            }
            assert_eq!(parse_split(s, at), whole, "split at byte {at} of {s:?}");
        }
    }
}

/// The GFM-extension path (extended autolinks, task lists) must be equally chunk-independent.
#[test]
fn split_equivalence_gfm_samples() {
    let samples = [
        // Extended autolinks: bare `www.`, a scheme URL with trailing punctuation, and a bare email.
        "Visit www.example.com/a.b. and http://x.org), mail foo@bar.example.com here\n",
        // Task-list items, tight and nested.
        "- [ ] todo\n- [x] done\n  - [ ] sub\n",
        // A checkbox marker that is *not* the first content (so it stays literal).
        "- text\n\n  [ ] not a task\n",
        // Forward reference under the GFM path: held and re-resolved at flush, chunk-independently.
        "[foo] then www.example.com\n\nmid\n\n[foo]: /url\n",
    ];
    for s in samples {
        let whole = parse_gfm(s);
        for at in 0..=s.len() {
            if !s.is_char_boundary(at) {
                continue;
            }
            assert_eq!(
                parse_split_gfm(s, at),
                whole,
                "gfm split at byte {at} of {s:?}"
            );
        }
    }
}
