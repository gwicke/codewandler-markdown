//! `markdown-terminal` — render a [`markdown_stream`] event stream to styled terminal output.
//!
//! The primary product surface: turns the parser's events into ANSI, with themes, width-aware
//! wrapping, indented blockquotes/lists, and styled code. It is driven *incrementally* — feed it the
//! events from each `Parser::write` and completed blocks are rendered immediately — which is the use
//! case the library exists for (rendering an LLM's output as it streams).

#![forbid(unsafe_code)]

use std::io::{self, Write};

use markdown_stream::{Alignment, BlockKind, Event, InlineStyle};
use unicode_width::UnicodeWidthStr;

mod highlight;
mod live;
mod theme;
pub use live::LiveRenderer;
pub use theme::Theme;

/// Render a complete event stream to an ANSI string using the default theme and width 80.
pub fn render(events: &[Event]) -> String {
    render_with(events, &Theme::default(), 80)
}

/// Render a complete event stream with an explicit theme and wrap width.
pub fn render_with(events: &[Event], theme: &Theme, width: usize) -> String {
    let mut r = Renderer::new(theme.clone(), width);
    let mut out = Vec::new();
    r.feed(events, &mut out)
        .expect("string write is infallible");
    r.finish(&mut out).expect("string write is infallible");
    String::from_utf8(out).expect("renderer emits utf-8")
}

/// A stateful renderer that can be fed events incrementally and writes completed output to a sink.
///
/// This is the live renderer: hold one across many `Parser::write` calls and it emits each block as
/// it closes, never buffering the whole document.
pub struct Renderer {
    theme: Theme,
    width: usize,
    /// nesting prefixes (one per open blockquote / list level)
    prefixes: Vec<String>,
    list_stack: Vec<ListCtx>,
    /// accumulated styled segments for the current paragraph/heading
    segments: Vec<(String, InlineStyle)>,
    block: Vec<BlockKind>,
    in_code: bool,
    code_lang: String,
    table: Option<TableBuf>,
    /// blank line owed before the next block
    pending_gap: bool,
    wrote_any: bool,
}

struct ListCtx {
    ordered: bool,
    next: u64,
}

/// Buffers a table's rendered cells until it closes, so column widths can be computed.
struct TableBuf {
    aligns: Vec<Alignment>,
    rows: Vec<Vec<String>>,
    cur_row: Vec<String>,
}

impl Renderer {
    /// Create a live renderer with the given theme and wrap width.
    pub fn new(theme: Theme, width: usize) -> Self {
        Renderer {
            theme,
            width: width.max(20),
            prefixes: Vec::new(),
            list_stack: Vec::new(),
            segments: Vec::new(),
            block: Vec::new(),
            in_code: false,
            code_lang: String::new(),
            table: None,
            pending_gap: false,
            wrote_any: false,
        }
    }

    /// Feed a batch of events, writing any newly-completed output to `w`.
    pub fn feed<W: Write>(&mut self, events: &[Event], w: &mut W) -> io::Result<()> {
        for ev in events {
            self.event(ev, w)?;
        }
        Ok(())
    }

    /// Finish: flush a trailing newline if anything was written.
    pub fn finish<W: Write>(&mut self, w: &mut W) -> io::Result<()> {
        let _ = w;
        Ok(())
    }

    fn indent(&self) -> String {
        self.prefixes.concat()
    }

    fn gap<W: Write>(&mut self, w: &mut W) -> io::Result<()> {
        if self.pending_gap && self.wrote_any {
            writeln!(w)?;
        }
        self.pending_gap = false;
        Ok(())
    }

    fn event<W: Write>(&mut self, ev: &Event, w: &mut W) -> io::Result<()> {
        match ev {
            Event::EnterBlock { block, data, .. } => {
                match block {
                    BlockKind::Document => {}
                    BlockKind::BlockQuote => {
                        self.gap(w)?;
                        self.prefixes
                            .push(format!("{}│ {}", self.theme.muted, self.theme.reset));
                    }
                    BlockKind::List => {
                        self.gap(w)?;
                        self.list_stack.push(ListCtx {
                            ordered: data.list.as_ref().is_some_and(|l| l.ordered),
                            next: data.list.as_ref().map(|l| l.start).unwrap_or(1),
                        });
                    }
                    BlockKind::ListItem => {
                        let marker = match self.list_stack.last_mut() {
                            Some(l) if l.ordered => {
                                let n = l.next;
                                l.next += 1;
                                format!("{n}. ")
                            }
                            _ => "• ".to_string(),
                        };
                        self.prefixes.push(marker);
                    }
                    BlockKind::FencedCode | BlockKind::IndentedCode => {
                        self.gap(w)?;
                        self.in_code = true;
                        self.code_lang = data
                            .info
                            .split_whitespace()
                            .next()
                            .unwrap_or("")
                            .to_string();
                    }
                    BlockKind::Table => {
                        self.gap(w)?;
                        self.table = Some(TableBuf {
                            aligns: data.alignment.clone(),
                            rows: Vec::new(),
                            cur_row: Vec::new(),
                        });
                    }
                    BlockKind::TableRow => {
                        if let Some(t) = &mut self.table {
                            t.cur_row.clear();
                        }
                    }
                    BlockKind::TableCell => self.segments.clear(),
                    _ => {}
                }
                self.block.push(*block);
            }
            Event::ExitBlock { block, .. } => {
                self.block.pop();
                match block {
                    BlockKind::Paragraph => {
                        self.flush_segments(w, None)?;
                        self.pending_gap = true;
                    }
                    BlockKind::Heading => {
                        let style = format!("{}{}", self.theme.heading, self.theme.bold);
                        self.flush_segments(w, Some(&style))?;
                        self.pending_gap = true;
                    }
                    BlockKind::BlockQuote => {
                        self.prefixes.pop();
                        self.pending_gap = true;
                    }
                    BlockKind::List => {
                        self.list_stack.pop();
                        self.pending_gap = true;
                    }
                    BlockKind::ListItem => {
                        // tight items: flush their inline text as a one-liner
                        if !self.segments.is_empty() {
                            self.flush_segments(w, None)?;
                        }
                        self.prefixes.pop();
                    }
                    BlockKind::ThematicBreak => {
                        self.gap(w)?;
                        let rule = "─".repeat(self.width.min(60));
                        writeln!(
                            w,
                            "{}{}{}{}",
                            self.indent(),
                            self.theme.muted,
                            rule,
                            self.theme.reset
                        )?;
                        self.wrote_any = true;
                        self.pending_gap = true;
                    }
                    BlockKind::FencedCode | BlockKind::IndentedCode => {
                        self.in_code = false;
                        self.pending_gap = true;
                    }
                    BlockKind::TableCell => {
                        let s = self.inline_string();
                        if let Some(t) = &mut self.table {
                            t.cur_row.push(s);
                        }
                    }
                    BlockKind::TableRow => {
                        if let Some(t) = &mut self.table {
                            let row = std::mem::take(&mut t.cur_row);
                            t.rows.push(row);
                        }
                    }
                    BlockKind::Table => self.render_table(w)?,
                    _ => {}
                }
            }
            Event::Text { text, style, .. } => {
                if self.in_code {
                    self.write_code_line(w, text)?;
                } else {
                    self.segments.push((text.clone(), style.clone()));
                }
            }
            Event::SoftBreak => {
                if !self.in_code {
                    self.segments
                        .push((" ".to_string(), InlineStyle::default()));
                }
            }
            Event::LineBreak => {
                if !self.in_code {
                    self.segments
                        .push(("\n".to_string(), InlineStyle::default()));
                }
            }
        }
        Ok(())
    }

    fn write_code_line<W: Write>(&mut self, w: &mut W, text: &str) -> io::Result<()> {
        self.gap(w)?;
        // `text` already carries its trailing newline (one Text event per code line).
        for line in text.split_inclusive('\n') {
            let nl = line.ends_with('\n');
            let body = line.strip_suffix('\n').unwrap_or(line);
            let rendered = if self.code_lang.is_empty() {
                // No language → uniform code color rather than guessing tokens.
                format!("{}{}{}", self.theme.code, body, self.theme.reset)
            } else {
                highlight::highlight_line(body, &self.code_lang, &self.theme)
            };
            write!(w, "{}  {}", self.indent(), rendered)?;
            if nl {
                writeln!(w)?;
            }
        }
        self.wrote_any = true;
        Ok(())
    }

    /// Render the current inline segments to a single styled line (used for a table cell).
    fn inline_string(&mut self) -> String {
        let segs = std::mem::take(&mut self.segments);
        let mut s = String::new();
        for (text, style) in &segs {
            let t = text.replace('\n', " ");
            s.push_str(&self.styled(&t, style, None));
        }
        s
    }

    /// Render a buffered table: compute column widths, draw box-drawing borders, align cells.
    fn render_table<W: Write>(&mut self, w: &mut W) -> io::Result<()> {
        let Some(t) = self.table.take() else {
            return Ok(());
        };
        self.gap(w)?;
        let ncol = t
            .aligns
            .len()
            .max(t.rows.iter().map(Vec::len).max().unwrap_or(0));
        let mut widths = vec![0usize; ncol];
        for row in &t.rows {
            for (i, cell) in row.iter().enumerate() {
                widths[i] = widths[i].max(visible_width(cell));
            }
        }
        let indent = self.indent();
        let m = self.theme.muted;
        let r = self.theme.reset;
        for (ri, row) in t.rows.iter().enumerate() {
            write!(w, "{indent}{m}│{r} ")?;
            for (i, width) in widths.iter().enumerate() {
                let cell = row.get(i).map(String::as_str).unwrap_or("");
                let pad = width.saturating_sub(visible_width(cell));
                match t.aligns.get(i).copied().unwrap_or(Alignment::None) {
                    Alignment::Right => write!(w, "{}{cell}", " ".repeat(pad))?,
                    Alignment::Center => {
                        let l = pad / 2;
                        write!(w, "{}{cell}{}", " ".repeat(l), " ".repeat(pad - l))?;
                    }
                    _ => write!(w, "{cell}{}", " ".repeat(pad))?,
                }
                write!(w, " {m}│{r} ")?;
            }
            writeln!(w)?;
            if ri == 0 {
                write!(w, "{indent}{m}├")?;
                for (i, width) in widths.iter().enumerate() {
                    write!(w, "{}", "─".repeat(width + 2))?;
                    write!(w, "{}", if i + 1 < ncol { "┼" } else { "┤" })?;
                }
                writeln!(w, "{r}")?;
            }
        }
        self.wrote_any = true;
        self.pending_gap = true;
        Ok(())
    }

    /// Render the accumulated inline segments as a wrapped, styled, indented block.
    fn flush_segments<W: Write>(&mut self, w: &mut W, block_style: Option<&str>) -> io::Result<()> {
        if self.segments.is_empty() {
            return Ok(());
        }
        self.gap(w)?;
        let segments = std::mem::take(&mut self.segments);
        let indent = self.indent();
        let avail = self.width.saturating_sub(visible_width(&indent)).max(20);

        let mut line_vis = 0usize;
        let mut pending_space = false;
        write!(w, "{indent}")?;

        for (raw, style) in &segments {
            for atom in atoms(raw) {
                match atom {
                    Atom::Space => pending_space = true,
                    Atom::Hard => {
                        writeln!(w)?;
                        write!(w, "{indent}")?;
                        line_vis = 0;
                        pending_space = false;
                    }
                    Atom::Word(word) => {
                        let wv = visible_width(word);
                        let sep = usize::from(line_vis > 0 && pending_space);
                        if line_vis > 0 && line_vis + sep + wv > avail {
                            // wrap to a fresh line (the pending space is dropped at the break)
                            writeln!(w)?;
                            write!(w, "{indent}")?;
                            line_vis = 0;
                        } else if sep == 1 {
                            write!(w, " ")?;
                            line_vis += 1;
                        }
                        write!(w, "{}", self.styled(word, style, block_style))?;
                        line_vis += wv;
                        pending_space = false;
                    }
                }
            }
        }
        writeln!(w)?;
        self.wrote_any = true;
        Ok(())
    }

    fn styled(&self, text: &str, style: &InlineStyle, block_style: Option<&str>) -> String {
        let mut codes = String::new();
        if let Some(bs) = block_style {
            codes.push_str(bs);
        }
        if style.strong {
            codes.push_str(self.theme.bold);
        }
        if style.emphasis {
            codes.push_str(self.theme.italic);
        }
        if style.strikethrough {
            codes.push_str(self.theme.strike);
        }
        if style.code {
            codes.push_str(self.theme.code);
        }
        if style.link.is_some() {
            codes.push_str(self.theme.link);
        }
        if codes.is_empty() {
            text.to_string()
        } else {
            format!("{codes}{text}{}", self.theme.reset)
        }
    }
}

/// A wrapping atom: a word, a space between words, or a hard line break.
enum Atom<'a> {
    Word(&'a str),
    Space,
    Hard,
}

/// Split a string into wrap atoms — words, the spaces between them, and hard line breaks — so the
/// renderer can re-flow words while preserving exactly where spaces did and didn't exist (adjacent
/// styled runs like `**bold**,` must not gain a space).
fn atoms(s: &str) -> Vec<Atom<'_>> {
    let mut out = Vec::new();
    let b = s.as_bytes();
    let (mut start, mut i) = (0usize, 0usize);
    while i < b.len() {
        match b[i] {
            b'\n' => {
                if start < i {
                    out.push(Atom::Word(&s[start..i]));
                }
                out.push(Atom::Hard);
                i += 1;
                start = i;
            }
            b' ' | b'\t' => {
                if start < i {
                    out.push(Atom::Word(&s[start..i]));
                }
                out.push(Atom::Space);
                i += 1;
                start = i;
            }
            _ => i += 1,
        }
    }
    if start < s.len() {
        out.push(Atom::Word(&s[start..]));
    }
    out
}

/// Visible width of a string, ignoring ANSI SGR sequences.
fn visible_width(s: &str) -> usize {
    let mut w = 0;
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\x1b' {
            // skip an escape sequence up to and including the final letter
            for e in chars.by_ref() {
                if e.is_ascii_alphabetic() {
                    break;
                }
            }
        } else {
            w += UnicodeWidthStr::width(c.to_string().as_str());
        }
    }
    w
}
