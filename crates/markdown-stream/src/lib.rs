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

mod event;
mod parser;

pub use event::{
    Alignment, BlockData, BlockKind, Event, InlineStyle, Link, ListData, Span,
};
pub use parser::Parser;

/// The concrete streaming parser.
///
/// The implementation is built up across milestones (blocks → inlines → GFM); this scaffold
/// establishes the public contract and an honest empty stream until the block parser lands.
#[derive(Default)]
pub struct StreamParser {}

impl StreamParser {
    /// Create a new parser with default options.
    pub fn new() -> Self {
        Self::default()
    }
}

impl Parser for StreamParser {
    fn write(&mut self, _chunk: &[u8]) -> Vec<Event> {
        Vec::new()
    }

    fn flush(&mut self) -> Vec<Event> {
        Vec::new()
    }

    fn reset(&mut self) {
        *self = Self::default();
    }
}

/// Parse a complete document to a `Vec<Event>` (convenience over the streaming API).
pub fn parse(input: &str) -> Vec<Event> {
    let mut p = StreamParser::new();
    let mut events = p.write(input.as_bytes());
    events.extend(p.flush());
    events
}
