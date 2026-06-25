//! `markdown-html` — render a [`markdown_stream`] event stream to HTML.
//!
//! Primarily the oracle for compliance testing: the CommonMark/GFM corpora specify expected HTML,
//! so `parse → render_html` is how we score the parser. Incremental-only (consumes events, never
//! re-parses).

#![forbid(unsafe_code)]

use markdown_stream::Event;

/// Render a sequence of events to an HTML string. (Implementation grows alongside the parser.)
pub fn render(events: &[Event]) -> String {
    let mut out = String::new();
    render_into(&mut out, events);
    out
}

/// Render events into an existing buffer.
pub fn render_into(_out: &mut String, _events: &[Event]) {
    // Implemented in M1 (blocks) and M2 (inlines).
}
