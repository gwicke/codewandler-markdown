//! A live renderer for streaming sources: re-render the accumulated Markdown in place as text
//! arrives, so an LLM's output is formatted *and* updates token-by-token.
//!
//! On a TTY it redraws by moving the cursor up over the previous render and re-emitting; when the
//! output isn't a terminal it simply accumulates and renders once at [`LiveRenderer::finish`] (so
//! `… | cat` stays clean, with no cursor escapes).

use std::io::{self, Write};

use markdown_stream::parse;

use crate::{render_with, Theme};

/// Renders a streaming Markdown message, updating the terminal in place.
pub struct LiveRenderer {
    theme: Theme,
    width: usize,
    live: bool,
    source: String,
    prev_lines: usize,
}

impl LiveRenderer {
    /// Create a live renderer. `live` should be true when the output is an interactive terminal
    /// (enables the in-place cursor redraw); false accumulates and renders once at `finish`.
    pub fn new(theme: Theme, width: usize, live: bool) -> Self {
        LiveRenderer {
            theme,
            width,
            live,
            source: String::new(),
            prev_lines: 0,
        }
    }

    /// Whether any text has been pushed since the last `finish`.
    pub fn is_active(&self) -> bool {
        !self.source.is_empty()
    }

    /// Append a chunk of the streaming message and (on a TTY) redraw it in place.
    pub fn push<W: Write>(&mut self, delta: &str, w: &mut W) -> io::Result<()> {
        if delta.is_empty() {
            return Ok(());
        }
        self.source.push_str(delta);
        if self.live {
            self.redraw(w)?;
        }
        Ok(())
    }

    fn redraw<W: Write>(&mut self, w: &mut W) -> io::Result<()> {
        let rendered = render_with(&parse(&self.source), &self.theme, self.width);
        if self.prev_lines > 0 {
            write!(w, "\x1b[{}A", self.prev_lines)?; // cursor up over the previous render
        }
        write!(w, "\r\x1b[J{rendered}")?; // column 0, clear to end of screen, redraw
        self.prev_lines = rendered.matches('\n').count();
        w.flush()
    }

    /// Commit the current message (it stays on screen) and reset for the next one. When not live,
    /// this is where the accumulated message is rendered (once).
    pub fn finish<W: Write>(&mut self, w: &mut W) -> io::Result<()> {
        if self.source.is_empty() {
            return Ok(());
        }
        if !self.live {
            let rendered = render_with(&parse(&self.source), &self.theme, self.width);
            write!(w, "{rendered}")?;
            w.flush()?;
        }
        // On a TTY the final state was already drawn by the last `push`; just reset.
        self.source.clear();
        self.prev_lines = 0;
        Ok(())
    }
}
