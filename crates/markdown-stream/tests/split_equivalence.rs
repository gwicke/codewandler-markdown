//! Split-equivalence: parsing an input as a single chunk must produce exactly the same event stream
//! as parsing it split at *any* byte boundary. This is the library's defining invariant — it holds
//! at every milestone, including the empty-stream scaffold.

use markdown_stream::{parse, Event, Parser, StreamParser};

fn parse_split(input: &str, at: usize) -> Vec<Event> {
    let b = input.as_bytes();
    let mut p = StreamParser::new();
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
