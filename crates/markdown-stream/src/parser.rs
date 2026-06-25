//! The streaming parser contract.

use crate::event::Event;

/// An incremental, chunk-safe Markdown parser.
///
/// Feed bytes with [`write`](Parser::write) and drain the returned events; call
/// [`flush`](Parser::flush) at end of input to close any open blocks. The emitted event stream is
/// **independent of how the input is split** across `write` calls (split-equivalence) — the single
/// most important invariant of this library.
pub trait Parser {
    /// Feed a chunk of input. Returns any events that became complete as a result. A chunk may end
    /// mid-line; the parser buffers the remainder until the next `write`/`flush`.
    fn write(&mut self, chunk: &[u8]) -> Vec<Event>;

    /// Signal end of input: process any buffered partial line and emit closing events for every
    /// still-open block (ending with `ExitBlock(Document)`).
    fn flush(&mut self) -> Vec<Event>;

    /// Clear all state so the parser can be reused for a new document.
    fn reset(&mut self);
}
