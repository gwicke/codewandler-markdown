//! `markdown-terminal` — render a [`markdown_stream`] event stream to styled terminal output.
//!
//! The primary product surface: turns the parser's events into ANSI, with themes, width-aware
//! wrapping, syntax-highlighted code, and a **live renderer** that updates the terminal as a
//! document streams in (the use case the library exists for: rendering LLM output token-by-token).

#![forbid(unsafe_code)]

use markdown_stream::Event;

mod theme;
pub use theme::Theme;

/// Render a complete event stream to an ANSI string using the default theme.
pub fn render(events: &[Event]) -> String {
    render_with(events, &Theme::default())
}

/// Render a complete event stream with an explicit theme. (Implementation grows in M4.)
pub fn render_with(_events: &[Event], _theme: &Theme) -> String {
    String::new()
}
