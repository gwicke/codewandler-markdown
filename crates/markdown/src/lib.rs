//! `markdown` — the streaming Markdown facade.
//!
//! A thin top-level API over the incremental parser ([`markdown_stream`]) and the renderers
//! ([`markdown_html`], [`markdown_terminal`]). The library's hallmark is **streaming**: parse and
//! render incrementally, immediately, with memory bounded by unresolved state rather than document
//! size.
//!
//! ```
//! let ansi = markdown::render_string("# Hello\n\nsome **bold** text");
//! let html = markdown::html_string("# Hello");
//! ```

#![forbid(unsafe_code)]

pub use markdown_stream::{self as stream, Event, Inline, InlineStyle, Link, Parser, StreamParser};
pub use markdown_terminal::Theme;

/// Parse `input` and render it to ANSI terminal output.
pub fn render_string(input: &str) -> String {
    markdown_terminal::render(&stream::parse(input))
}

/// Parse `input` and render it to HTML (CommonMark).
pub fn html_string(input: &str) -> String {
    markdown_html::render(&stream::parse(input))
}

/// Parse `input` with GFM extensions and render it to HTML, applying the GFM disallowed-raw-HTML tag
/// filter. Use this for GitHub-Flavored Markdown; [`html_string`] stays pure CommonMark.
pub fn html_string_gfm(input: &str) -> String {
    markdown_html::render_gfm(&stream::parse_gfm(input))
}

/// Parse `input` into the raw event stream.
pub fn parse(input: &str) -> Vec<Event> {
    stream::parse(input)
}
