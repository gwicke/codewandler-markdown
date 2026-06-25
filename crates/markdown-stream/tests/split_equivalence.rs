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
