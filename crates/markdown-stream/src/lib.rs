//! `markdown-stream` — an incremental, streaming CommonMark/GFM parser.
//!
//! Pure `std`, no runtime dependencies. Feed bytes to a [`StreamParser`] with [`Parser::write`] and
//! consume the flat [`Event`] stream it emits; call [`Parser::flush`] at end of input. The event
//! stream is **independent of how the input is chunked** (split-equivalence) — this is what makes
//! the parser usable on a live token stream from an LLM.
//!
//! The parser is *append-only and chunk-safe*; renderers (`markdown-html`, `markdown-terminal`)
//! consume the events and never re-parse Markdown.

#![forbid(unsafe_code)]

mod block;
mod entity;
mod event;
mod inline;
mod linkref;
mod parser;

pub use block::StreamParser;
pub use event::{
    Alignment, BlockData, BlockKind, Event, Inline, InlineStyle, Link, ListData, Span,
};
pub use parser::Parser;

/// Parse a complete document to a `Vec<Event>` (convenience over the streaming API).
pub fn parse(input: &str) -> Vec<Event> {
    let mut p = StreamParser::new();
    let mut events = p.write(input.as_bytes());
    events.extend(p.flush());
    events
}
