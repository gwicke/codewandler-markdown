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
    prefixes: Vec<Prefix>,
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

impl Renderer {
    /// Get the current wrap width.
    pub fn width(&self) -> usize {
        self.width
    }

    /// Update the wrap width. Takes effect on the next `feed` call.
    pub fn set_width(&mut self, width: usize) {
        self.width = width.max(20);
    }
}

struct ListCtx {
    ordered: bool,
    next: u64,
}

/// A nesting prefix. `first` is printed on the first line the level appears on (a list marker like
/// `1. `); `cont` is printed on every continuation/wrapped line — for a list marker that's blanks of
/// equal width so wrapped text aligns under the item, while a blockquote keeps its `│ ` bar on both.
struct Prefix {
    first: String,
    cont: String,
    /// set once `first` has been emitted; thereafter every line (even a leading one) uses `cont`,
    /// so a second paragraph or a nested list inside one item doesn't re-print the marker.
    emitted: bool,
}

impl Prefix {
    /// A prefix that repeats identically on every line (a blockquote bar).
    fn repeating(s: String) -> Self {
        Prefix {
            cont: s.clone(),
            first: s,
            emitted: false,
        }
    }

    /// A hanging list marker: `marker` on the first line, blanks of the same visible width after.
    fn marker(marker: String) -> Self {
        let cont = " ".repeat(visible_width(&marker));
        Prefix {
            first: marker,
            cont,
            emitted: false,
        }
    }
}

/// A cell's content as the parser delivered it: styled runs, unstyled, not yet wrapped. Keeping
/// runs rather than a pre-rendered ANSI string is what lets a cell be wrapped *before* styling, so
/// inline attributes and OSC 8 hyperlinks survive a line break intact.
type Cell = Vec<(String, InlineStyle)>;

/// Buffers a table's cells until it closes, so column widths can be computed from the whole table
/// (see `render_table`) — a grid cannot be laid out from a prefix of its own rows.
struct TableBuf {
    aligns: Vec<Alignment>,
    rows: Vec<Vec<Cell>>,
    cur_row: Vec<Cell>,
}

/// Narrowest a column may be squeezed to before the grid stops being readable and the table falls
/// back to the stacked layout. A column that is *naturally* this narrow (a one-character index) is
/// not squeezed, so it does not trigger the fallback.
const MIN_COL: usize = 8;

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

    /// Indent for continuation/wrapped lines: every level's continuation form (list markers become
    /// blanks, blockquote bars stay).
    fn indent(&self) -> String {
        self.prefixes.iter().map(|p| p.cont.as_str()).collect()
    }

    /// Indent for the first physical line of a block: emit each level's marker the first time the
    /// level appears, then fall back to its continuation form. This is what keeps a list marker on
    /// the item's first line only, rather than repeating it down every wrapped line.
    fn indent_first(&mut self) -> String {
        let mut s = String::new();
        for p in &mut self.prefixes {
            if p.emitted {
                s.push_str(&p.cont);
            } else {
                s.push_str(&p.first);
                p.emitted = true;
            }
        }
        s
    }

    fn gap<W: Write>(&mut self, w: &mut W) -> io::Result<()> {
        if self.pending_gap && self.wrote_any {
            writeln!(w)?;
        }
        self.pending_gap = false;
        Ok(())
    }

    /// Remove trailing whitespace (spaces, tabs, newlines) from accumulated segments.
    /// Used when a nested list interrupts a paragraph to avoid rendering an extra
    /// continuation line for the trailing newline.
    fn trim_trailing_whitespace(&mut self) {
        while let Some((text, _)) = self.segments.last_mut() {
            let trimmed = text.trim_end_matches(|c: char| c.is_whitespace());
            if trimmed.len() == text.len() {
                break;
            }
            if trimmed.is_empty() {
                self.segments.pop();
            } else {
                *text = trimmed.to_string();
            }
        }
    }

    fn event<W: Write>(&mut self, ev: &Event, w: &mut W) -> io::Result<()> {
        match ev {
            Event::EnterBlock { block, data, .. } => {
                match block {
                    BlockKind::Document => {}
                    BlockKind::BlockQuote => {
                        self.gap(w)?;
                        self.prefixes.push(Prefix::repeating(format!(
                            "{}│ {}",
                            self.theme.muted, self.theme.reset
                        )));
                    }
                    BlockKind::List => {
                        // Flush any pending inline content from a parent list item's paragraph
                        // before starting a nested list. The parser buffers paragraph events for
                        // direct list-item children until the list closes, but nested lists are
                        // emitted immediately, so we must flush proactively.
                        if !self.segments.is_empty() {
                            // The parser may include a trailing newline in the paragraph text when
                            // a nested list interrupts it. Trim trailing whitespace to avoid
                            // rendering an extra continuation line.
                            self.trim_trailing_whitespace();
                            self.flush_segments(w, None)?;
                            // flush_segments sets pending_gap = true for the next block, but a
                            // nested list is a child block, not a sibling — suppress the extra gap.
                            self.pending_gap = false;
                        }
                        self.gap(w)?;
                        self.list_stack.push(ListCtx {
                            ordered: data.list.as_ref().is_some_and(|l| l.ordered),
                            next: data.list.as_ref().map(|l| l.start).unwrap_or(1),
                        });
                    }
                    BlockKind::ListItem => {
                        // Flush any pending inline content from the previous block in this list item
                        // (or a parent list item) before starting a new item.
                        if !self.segments.is_empty() {
                            self.flush_segments(w, None)?;
                        }
                        let marker = match self.list_stack.last_mut() {
                            Some(l) if l.ordered => {
                                let n = l.next;
                                l.next += 1;
                                format!("{n}. ")
                            }
                            _ => "• ".to_string(),
                        };
                        self.prefixes.push(Prefix::marker(marker));
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
                        // Keep the runs unstyled: the table wraps each cell before styling.
                        let cell = std::mem::take(&mut self.segments);
                        if let Some(t) = &mut self.table {
                            t.cur_row.push(cell);
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
            // Inline nesting is tracked by the parser; the terminal renderer reads cumulative
            // styling off each `Text` event, so the enter/exit markers are no-ops here.
            Event::EnterInline { .. } => {}
            Event::ExitInline { .. } => {}
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

    /// Render a buffered table: fit the columns to the terminal width, draw box-drawing borders,
    /// and wrap each cell into its column.
    ///
    /// A grid is only kept while it stays readable. Columns are given their natural widths when the
    /// table fits; otherwise they are water-filled down to the available budget (the widest columns
    /// give up space first) and any leftover goes to the last, most flexible column. Cells are then
    /// word-wrapped, so a row grows taller instead of the table growing wider. If the squeeze leaves
    /// a column narrower than [`MIN_COL`] — or too narrow even for its own header label — the grid
    /// is abandoned for the stacked layout, where every cell is a `label: value` line that uses the
    /// full width.
    fn render_table<W: Write>(&mut self, w: &mut W) -> io::Result<()> {
        let Some(t) = self.table.take() else {
            return Ok(());
        };
        let ncol = t
            .aligns
            .len()
            .max(t.rows.iter().map(Vec::len).max().unwrap_or(0));
        if ncol == 0 {
            return Ok(());
        }
        // Natural width of each column: the widest cell it holds, unstyled.
        let mut naturals = vec![0usize; ncol];
        for row in &t.rows {
            for (i, cell) in row.iter().enumerate() {
                naturals[i] = naturals[i].max(runs_width(cell));
            }
        }
        let indent = self.indent();
        let avail = self.width.saturating_sub(visible_width(&indent));
        // Fixed chrome: `"│ "` opens the row, `" │"` closes each column, `" "` separates them — so a
        // row and its rule are both `1 + 3 * ncol` wider than the columns they hold.
        let widths = fit_columns(&naturals, avail.saturating_sub(1 + 3 * ncol));
        // A grid stays salvageable while every column still fits its *header*: a header label like
        // `Description` is the tightest, most atomic content a column must hold, so a column too
        // narrow for it would split `Description` into `Descripti`/`on`. A column that is merely
        // narrow but not squeezed (a one-character index) is not a problem.
        let header_tok: Vec<usize> = (0..ncol)
            .map(|i| {
                t.rows
                    .first()
                    .and_then(|r| r.get(i))
                    .map(|c| runs_widest_token(c))
                    .unwrap_or(0)
            })
            .collect();
        let unsalvageable = widths
            .iter()
            .zip(&naturals)
            .zip(&header_tok)
            .any(|((w, n), h)| *w < *n && (*w < MIN_COL || *w < *h));

        self.gap(w)?;
        if unsalvageable {
            self.render_table_stacked(w, &t, ncol, &indent, avail)?;
        } else {
            self.render_table_grid(w, &t, &widths, &indent)?;
        }
        self.wrote_any = true;
        self.pending_gap = true;
        Ok(())
    }

    /// Draw the table as a grid of box-drawing borders, wrapping every cell into its column.
    fn render_table_grid<W: Write>(
        &mut self,
        w: &mut W,
        t: &TableBuf,
        widths: &[usize],
        indent: &str,
    ) -> io::Result<()> {
        let ncol = widths.len();
        let m = self.theme.muted;
        let r = self.theme.reset;
        let empty: Cell = Vec::new();
        for (ri, row) in t.rows.iter().enumerate() {
            // Wrap first, then emit: the row is as tall as its tallest cell.
            let cells: Vec<Vec<String>> = (0..ncol)
                .map(|i| {
                    let cell = row.get(i).unwrap_or(&empty);
                    // `hard` splits a token wider than the column, so a long flag name or URL can
                    // never overrun the border.
                    self.wrap_cell(cell, widths[i], true)
                })
                .collect();
            let height = cells.iter().map(Vec::len).max().unwrap_or(1);
            for li in 0..height {
                write!(w, "{indent}{m}│{r} ")?;
                for (i, width) in widths.iter().enumerate() {
                    let line = cells[i].get(li).map(String::as_str).unwrap_or("");
                    let pad = width.saturating_sub(visible_width(line));
                    match t.aligns.get(i).copied().unwrap_or(Alignment::None) {
                        Alignment::Right => write!(w, "{}{line}", " ".repeat(pad))?,
                        Alignment::Center => {
                            let l = pad / 2;
                            write!(w, "{}{line}{}", " ".repeat(l), " ".repeat(pad - l))?;
                        }
                        _ => write!(w, "{line}{}", " ".repeat(pad))?,
                    }
                    write!(w, " {m}│{r}")?;
                    if i + 1 < ncol {
                        write!(w, " ")?;
                    }
                }
                writeln!(w)?;
            }
            // The rule belongs under the *last* line of the header, which may have wrapped.
            if ri == 0 {
                write!(w, "{indent}{m}├")?;
                for (i, width) in widths.iter().enumerate() {
                    write!(w, "{}", "─".repeat(width + 2))?;
                    write!(w, "{}", if i + 1 < ncol { "┼" } else { "┤" })?;
                }
                writeln!(w, "{r}")?;
            }
        }
        Ok(())
    }

    /// Draw the table as a stack of `label: value` records — the fallback for a terminal too narrow
    /// to hold a grid. The header row supplies the labels, a column with nothing in it is skipped
    /// rather than printed as a dangling `Label:`, and each record is separated by a blank line.
    ///
    /// There is no column chrome here, so the whole `avail` width is available to each value.
    fn render_table_stacked<W: Write>(
        &mut self,
        w: &mut W,
        t: &TableBuf,
        ncol: usize,
        indent: &str,
        avail: usize,
    ) -> io::Result<()> {
        let label_style = format!("{}{}", self.theme.bold, self.theme.reset);
        let Some((header, rest)) = t.rows.split_first() else {
            return Ok(());
        };
        let empty: Cell = Vec::new();
        let mut first_record = true;
        for row in rest {
            let mut wrote_in_record = false;
            for i in 0..ncol {
                let cell = row.get(i).unwrap_or(&empty);
                if runs_text(cell).trim().is_empty() {
                    // A sparse table should not print dangling `Label:` lines.
                    continue;
                }
                // Fields of one record run together; records are separated by a blank line.
                if !wrote_in_record && !first_record {
                    writeln!(w)?;
                }
                first_record = false;
                wrote_in_record = true;
                let label = header.get(i).map(|c| runs_text(c)).unwrap_or_default();
                // Continuation lines hang under the value, not under the label. A long label may
                // claim at most half the line, so the value always has room to be read.
                let hang = (visible_width(&label) + 2).min(avail / 2);
                let lead = if label.is_empty() {
                    "  ".to_string()
                } else {
                    format!(
                        "{}: ",
                        self.styled(&label, &InlineStyle::default(), Some(&label_style))
                    )
                };
                let lines = self.wrap_cell(cell, avail.saturating_sub(hang), false);
                for (li, line) in lines.iter().enumerate() {
                    if li == 0 {
                        writeln!(w, "{indent}{lead}{line}")?;
                    } else {
                        writeln!(w, "{indent}{}{line}", " ".repeat(hang))?;
                    }
                }
            }
        }
        Ok(())
    }

    /// Wrap a cell's runs into styled lines of at most `width` visible columns.
    fn wrap_cell(&self, cell: &Cell, width: usize, hard: bool) -> Vec<String> {
        // A cell comes from a single source line, so a newline in it is a separator, not a break.
        let cell: Cell = cell
            .iter()
            .map(|(text, style)| (text.replace('\n', " "), style.clone()))
            .collect();
        wrap_runs(&cell, width, hard)
            .iter()
            .map(|line| {
                line.iter()
                    .map(|(text, style)| self.styled(text, style, None))
                    .collect()
            })
            .collect()
    }

    /// Render the accumulated inline segments as a wrapped, styled, indented block.
    fn flush_segments<W: Write>(&mut self, w: &mut W, block_style: Option<&str>) -> io::Result<()> {
        if self.segments.is_empty() {
            return Ok(());
        }
        self.gap(w)?;
        let segments = std::mem::take(&mut self.segments);
        // `cont` indents wrapped/continuation lines (list markers become blanks); `first` carries
        // the marker and is consumed by `indent_first`, so it appears on the opening line only.
        let cont = self.indent();
        let avail = self.width.saturating_sub(visible_width(&cont)).max(20);
        let first = self.indent_first();

        write!(w, "{first}")?;
        for (li, line) in wrap_runs(&segments, avail, false).iter().enumerate() {
            if li > 0 {
                write!(w, "{cont}")?;
            }
            for (text, style) in line {
                write!(w, "{}", self.styled(text, style, block_style))?;
            }
            writeln!(w)?;
        }
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
        // OSC 8 hyperlink wrapper (clickable in supporting terminals). Off by
        // default and enabled via `Theme::clickable_links`. Images are skipped
        // — OSC 8 links target text, not image content — and an empty target is
        // never wrapped.
        let osc_open = if self.theme.clickable_links {
            style
                .link
                .as_ref()
                .filter(|l| !l.image && !l.href.is_empty())
                .map(|l| format!("\x1b]8;;{}\x1b\\", l.href))
        } else {
            None
        };
        let osc_close = "\x1b]8;;\x1b\\";
        if codes.is_empty() && osc_open.is_none() {
            text.to_string()
        } else if let Some(open) = osc_open {
            if codes.is_empty() {
                format!("{open}{text}{osc_close}")
            } else {
                format!("{open}{codes}{text}{}{osc_close}", self.theme.reset)
            }
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

/// Visible width of a string, ignoring ANSI escape sequences.
///
/// Skips whole escape sequences rather than guessing where one ends: a CSI runs to its final byte
/// in `@`..`~`, and an OSC (an OSC 8 hyperlink, whose payload is a URL) to BEL or ST. Guessing by
/// "up to the next letter" would count the URL as visible text.
fn visible_width(s: &str) -> usize {
    let mut w = 0;
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '\x1b' {
            w += UnicodeWidthStr::width(c.to_string().as_str());
            continue;
        }
        match chars.next() {
            // ESC [ … final byte
            Some('[') => {
                for e in chars.by_ref() {
                    if matches!(e, '\x40'..='\x7e') {
                        break;
                    }
                }
            }
            // ESC ] … BEL | ESC \  (OSC, including OSC 8 hyperlinks)
            Some(']') => {
                while let Some(e) = chars.next() {
                    if e == '\x07' {
                        break;
                    }
                    if e == '\x1b' {
                        chars.next(); // consume the ST backslash
                        break;
                    }
                }
            }
            // ESC P/X/^/_ … ESC \  (DCS, SOS, PM, APC)
            Some('P' | 'X' | '^' | '_') => {
                let mut prev = '\0';
                for e in chars.by_ref() {
                    if e == '\\' && prev == '\x1b' {
                        break;
                    }
                    prev = e;
                }
            }
            // Anything else (a two-byte escape) is consumed already.
            _ => {}
        }
    }
    w
}

/// Visible width of a cell's unstyled runs.
fn runs_width(runs: &[(String, InlineStyle)]) -> usize {
    runs.iter().map(|(text, _)| visible_width(text)).sum()
}

/// The widest unbreakable token in a cell — the narrowest a column can be and still hold this cell
/// without a hard split.
fn runs_widest_token(runs: &[(String, InlineStyle)]) -> usize {
    runs.iter()
        .flat_map(|(text, _)| atoms(text))
        .filter_map(|a| match a {
            Atom::Word(word) => Some(visible_width(word)),
            _ => None,
        })
        .max()
        .unwrap_or(0)
}

/// A cell's plain text, runs concatenated — used for stacked-layout labels, where styling the
/// value is the reader's business, not the label's.
fn runs_text(runs: &[(String, InlineStyle)]) -> String {
    runs.iter().map(|(text, _)| text.as_str()).collect()
}

/// Fit `naturals` into `budget` columns: each column keeps its natural width if the table fits,
/// otherwise the widest columns give up space first (water-filling) and whatever is left over goes
/// to the last, most flexible column.
///
/// Water-filling is a binary search for the largest level `L` with `sum(min(nat_i, L)) <= budget`;
/// that sum is monotonic in `L`, so it is exact and costs `O(ncol * log budget)` rather than a
/// decrement loop over the widest column. Handing the remainder to the last column matters: without
/// it a column frozen at its sampled width stays frozen and wraps far more than it needs to.
fn fit_columns(naturals: &[usize], budget: usize) -> Vec<usize> {
    let total: usize = naturals.iter().sum();
    if total <= budget {
        return naturals.to_vec();
    }
    let fits = |level: usize| naturals.iter().map(|n| (*n).min(level)).sum::<usize>() <= budget;
    let (mut lo, mut hi) = (0usize, budget);
    while lo < hi {
        let mid = lo + (hi - lo).div_ceil(2);
        if fits(mid) {
            lo = mid;
        } else {
            hi = mid - 1;
        }
    }
    let mut widths: Vec<usize> = naturals.iter().map(|n| (*n).min(lo)).collect();
    let slack = budget - widths.iter().sum::<usize>();
    if let Some(last) = widths.last_mut() {
        *last += slack;
    }
    widths
}

/// Split styled runs into physical lines of at most `width` visible columns, preserving the style of
/// every run. Shared by paragraph wrapping and table cells, so both reflow words identically.
///
/// `hard` splits a token that is wider than a whole line, at char boundaries by visible width —
/// needed for table cells, where a long `--flag` or URL would otherwise overrun the column border.
/// A hard break before any word is dropped (loose list items emit a phantom `\n` ahead of their
/// paragraph, which would otherwise print a bare-marker line).
fn wrap_runs(
    segments: &[(String, InlineStyle)],
    width: usize,
    hard: bool,
) -> Vec<Vec<(String, InlineStyle)>> {
    let width = width.max(1);
    let mut lines: Vec<Vec<(String, InlineStyle)>> = vec![Vec::new()];
    let mut line_vis = 0usize;
    let mut pending_space = false;
    let mut started = false;

    let push = |lines: &mut Vec<Vec<(String, InlineStyle)>>,
                line_vis: &mut usize,
                text: &str,
                style: &InlineStyle| {
        if let Some(line) = lines.last_mut() {
            line.push((text.to_string(), style.clone()));
        }
        *line_vis += visible_width(text);
    };

    for (raw, style) in segments {
        for atom in atoms(raw) {
            match atom {
                Atom::Space => pending_space = true,
                Atom::Hard => {
                    if !started {
                        continue;
                    }
                    lines.push(Vec::new());
                    line_vis = 0;
                    pending_space = false;
                }
                Atom::Word(word) => {
                    let wv = visible_width(word);
                    let sep = usize::from(line_vis > 0 && pending_space);
                    if line_vis > 0 && line_vis + sep + wv > width {
                        // wrap to a fresh line (the pending space is dropped at the break)
                        lines.push(Vec::new());
                        line_vis = 0;
                    } else if sep == 1 {
                        push(&mut lines, &mut line_vis, " ", &InlineStyle::default());
                    }
                    if hard && wv > width {
                        // A token that cannot fit on any line: break it by visible width.
                        let mut chunk = String::new();
                        let mut chunk_vis = 0usize;
                        for c in word.chars() {
                            let cw = UnicodeWidthStr::width(c.to_string().as_str());
                            if chunk_vis + cw > width {
                                push(&mut lines, &mut line_vis, &chunk, style);
                                lines.push(Vec::new());
                                line_vis = 0;
                                chunk.clear();
                                chunk_vis = 0;
                            }
                            chunk.push(c);
                            chunk_vis += cw;
                        }
                        push(&mut lines, &mut line_vis, &chunk, style);
                    } else {
                        push(&mut lines, &mut line_vis, word, style);
                    }
                    pending_space = false;
                    started = true;
                }
            }
        }
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::{render_with, Theme};
    use markdown_stream::parse;

    fn render(src: &str, width: usize) -> String {
        render_with(&parse(src), &Theme::no_color(), width)
    }

    #[test]
    fn link_is_clickable_when_theme_enables_it() {
        let theme = Theme {
            clickable_links: true,
            ..Theme::default()
        };
        let out = render_with(&parse("[text](https://example.com/a)"), &theme, 80);
        assert!(
            out.contains("\x1b]8;;https://example.com/a\x1b\\"),
            "OSC 8 open sequence must wrap the href"
        );
        assert!(
            out.contains("\x1b]8;;\x1b\\"),
            "OSC 8 close sequence must be emitted"
        );
        assert!(out.contains("text"), "link text must still be rendered");
    }

    #[test]
    fn link_is_not_clickable_by_default() {
        // Default theme leaves clickable_links disabled.
        let out = render_with(
            &parse("[text](https://example.com/a)"),
            &Theme::default(),
            80,
        );
        assert!(
            !out.contains("\x1b]8;;"),
            "default theme must not emit OSC 8 hyperlink sequences"
        );
    }

    #[test]
    fn image_alt_text_is_not_wrapped_as_link() {
        // Images (`![alt](src)`) must not be wrapped in an OSC 8 hyperlink.
        let theme = Theme {
            clickable_links: true,
            ..Theme::default()
        };
        let out = render_with(&parse("![alt](https://example.com/i.png)"), &theme, 80);
        assert!(
            !out.contains("\x1b]8;;"),
            "image alt text must not be wrapped as a clickable link"
        );
    }

    #[test]
    fn wrapped_list_item_shows_marker_once_then_aligns() {
        // A single bullet long enough to wrap several times.
        let out = render("- alpha beta gamma delta epsilon zeta eta theta iota\n", 24);
        let lines: Vec<&str> = out.lines().filter(|l| !l.is_empty()).collect();
        assert!(lines.len() > 1, "input should wrap across lines: {out:?}");
        assert!(
            lines[0].starts_with("• "),
            "marker on first line: {:?}",
            lines[0]
        );
        for l in &lines[1..] {
            assert!(
                !l.starts_with("• "),
                "marker must not repeat on a wrapped line: {l:?}"
            );
            assert!(
                l.starts_with("  ") && !l.trim_start().is_empty(),
                "continuation should be space-aligned under the marker: {l:?}"
            );
        }
        // exactly one marker for the whole item
        assert_eq!(out.matches("• ").count(), 1, "bullet count: {out:?}");
    }

    #[test]
    fn loose_list_item_has_no_bare_marker_line() {
        // Blank lines between items make the list "loose"; the parser emits a leading "\n" per
        // item, which previously rendered as a bare "1." line before the content.
        let src = "1. first item that is quite long and certainly wraps\n\n\
                   2. second item that is also long enough to wrap as well\n";
        let out = render(src, 24);
        for l in out.lines() {
            let t = l.trim_end();
            assert!(t != "1." && t != "2.", "bare marker line in:\n{out}");
        }
        assert_eq!(out.matches("1. ").count(), 1, "one '1. ' marker: {out:?}");
        assert_eq!(out.matches("2. ").count(), 1, "one '2. ' marker: {out:?}");
    }

    #[test]
    fn dimmed_theme_reapplies_faint_after_inline_style() {
        // Inline formatting must not clear the dimming: `Theme::dimmed`'s
        // `reset` re-applies `\x1b[2m` so faded content (e.g. streamed
        // thinking) stays dimmed through bold/code spans.
        let out = render_with(&parse("**bold** and `code`"), &Theme::dimmed(), 80);
        assert!(
            out.contains("\x1b[0m\x1b[2m"),
            "reset must re-apply faint (\\x1b[2m), not a bare \\x1b[0m: {out:?}"
        );
        assert!(
            out.contains("\x1b[2m"),
            "output must contain the faint attribute"
        );
    }

    #[test]
    fn nested_list_renders_correctly() {
        // Simple nested list: parent item with text, then nested list
        let input = "- top list\n  - nested item 1\n  - nested item 2\n";
        let out = render_with(&parse(input), &Theme::no_color(), 80);
        println!("Nested list output:\n{}", out);

        let lines: Vec<&str> = out.lines().filter(|l| !l.is_empty()).collect();
        assert_eq!(lines.len(), 3, "should have 3 lines: {:?}", lines);

        // First line: parent item marker + text
        assert!(
            lines[0].starts_with("• "),
            "line 1 should start with bullet: {:?}",
            lines[0]
        );
        assert!(
            !lines[0].starts_with("• •"),
            "line 1 should not have double bullet: {:?}",
            lines[0]
        );

        // Second line: nested item (indented + bullet)
        assert!(
            lines[1].starts_with("  • "),
            "line 2 should start with two spaces + bullet: {:?}",
            lines[1]
        );
        assert!(
            !lines[1].starts_with("• •"),
            "line 2 should not have double bullet: {:?}",
            lines[1]
        );

        // Third line: nested item (indented + bullet)
        assert!(
            lines[2].starts_with("  • "),
            "line 3 should start with two spaces + bullet: {:?}",
            lines[2]
        );
        assert!(
            !lines[2].starts_with("• •"),
            "line 3 should not have double bullet: {:?}",
            lines[2]
        );
    }

    #[test]
    fn three_level_nested_list() {
        // Three-level nested list from split_equivalence test
        let input = "- foo\n  - bar\n    - baz\n";
        let out = render_with(&parse(input), &Theme::no_color(), 80);
        println!("Three-level nested list output:\n{}", out);

        let lines: Vec<&str> = out.lines().filter(|l| !l.is_empty()).collect();
        assert_eq!(lines.len(), 3, "should have 3 lines: {:?}", lines);

        // First line: top level
        assert!(lines[0].starts_with("• "), "line 1: {:?}", lines[0]);
        assert!(
            !lines[0].starts_with("• •"),
            "line 1 double bullet: {:?}",
            lines[0]
        );

        // Second line: second level
        assert!(lines[1].starts_with("  • "), "line 2: {:?}", lines[1]);
        assert!(
            !lines[1].starts_with("• •"),
            "line 2 double bullet: {:?}",
            lines[1]
        );

        // Third line: third level
        assert!(lines[2].starts_with("    • "), "line 3: {:?}", lines[2]);
        assert!(
            !lines[2].starts_with("• •"),
            "line 3 double bullet: {:?}",
            lines[2]
        );
    }

    #[test]
    fn user_issue_nested_list_with_text() {
        // Test case similar to the user's reported issue:
        // parent item with text, followed by nested list items
        let input = "- Top of loop: cur_h = 40\n  - input_row = output.cursor_row()\n    After Frame 1, the cursor was positioned at visual_row\n  - visual_row = (row + layout.cursor_row).saturating_sub(delta)\n";
        let out = render_with(&parse(input), &Theme::no_color(), 80);
        println!("User issue test:\n{}", out);

        // Check for double bullets - the main bug being fixed
        for line in out.lines() {
            assert!(
                !line.contains("• •"),
                "found double bullet in line: {:?}",
                line
            );
        }

        let lines: Vec<&str> = out.lines().filter(|l| !l.is_empty()).collect();

        // First line: parent item with text
        assert!(lines[0].starts_with("• "), "line 1: {:?}", lines[0]);
        assert!(
            lines[0].contains("Top of loop"),
            "line 1 should contain text: {:?}",
            lines[0]
        );

        // Second line: nested item 1 (starts with bullet)
        let nested1_idx = lines
            .iter()
            .position(|l| l.starts_with("  • "))
            .expect("nested item 1 not found");
        assert!(
            lines[nested1_idx].contains("input_row"),
            "nested 1: {:?}",
            lines[nested1_idx]
        );

        // Fourth line: nested item 2
        let nested2_idx = lines
            .iter()
            .position(|l| l.starts_with("  • visual_row"))
            .expect("nested item 2 not found");
        assert!(
            lines[nested2_idx].contains("visual_row"),
            "nested 2: {:?}",
            lines[nested2_idx]
        );

        // Ensure nested2 comes after nested1
        assert!(
            nested2_idx > nested1_idx,
            "nested2 should come after nested1"
        );
    }

    #[test]
    fn loose_list_with_nested_list() {
        // Loose list (blank lines between items) with a nested list
        let input = "- item 1\n\n- item 2\n  - nested 1\n  - nested 2\n\n- item 3\n";
        let out = render_with(&parse(input), &Theme::no_color(), 80);
        println!("Loose list with nested:\n{}", out);

        // Check for double bullets
        for line in out.lines() {
            assert!(
                !line.contains("• •"),
                "found double bullet in line: {:?}",
                line
            );
        }

        // item 1 (loose -> wrapped in <p> -> rendered as paragraph with blank line after)
        // item 2 with nested list
        // item 3 (loose)
        let lines: Vec<&str> = out.lines().filter(|l| !l.is_empty()).collect();
        // Should have: item1, item2 text, nested1, nested2, item3
        assert!(
            lines.len() >= 5,
            "should have at least 5 non-empty lines: {:?}",
            lines
        );

        // Check structure
        assert!(lines[0].starts_with("• "), "line 1: {:?}", lines[0]);
        assert!(lines[0].contains("item 1"), "line 1: {:?}", lines[0]);

        // item 2 starts after a blank line
        let item2_idx = lines
            .iter()
            .position(|l| l.contains("item 2"))
            .expect("item 2 not found");
        assert!(
            lines[item2_idx].starts_with("• "),
            "item 2 line: {:?}",
            lines[item2_idx]
        );

        // nested items after item 2
        assert!(
            lines[item2_idx + 1].starts_with("  • "),
            "nested 1: {:?}",
            lines[item2_idx + 1]
        );
        assert!(
            lines[item2_idx + 2].starts_with("  • "),
            "nested 2: {:?}",
            lines[item2_idx + 2]
        );
    }

    // --- tables -------------------------------------------------------------

    /// A 4-column table whose last cell is long enough to need wrapping.
    const WIDE: &str = "| Option | Type | Default | Description |\n\
                        |---|---|---|---|\n\
                        | `--width` | integer | `80` | Target line width in columns used when wrapping prose and rendered table cells to the terminal. |\n";

    /// Every line's visible width, using the renderer's own ANSI-aware measurement.
    fn widths(out: &str) -> Vec<usize> {
        out.lines().map(super::visible_width).collect()
    }

    #[test]
    fn table_that_fits_keeps_natural_widths() {
        // Characterization: a table narrower than the terminal is laid out from its content, with
        // each line exactly as wide as the rule below the header.
        let out = render(
            "| L | C | R |\n|:--|:-:|--:|\n| left | mid | right |\n| x | yy | zzzz |\n",
            80,
        );
        assert_eq!(
            out,
            "│ L    │  C  │     R │\n\
             ├──────┼─────┼───────┤\n\
             │ left │ mid │ right │\n\
             │ x    │ yy  │  zzzz │\n"
        );
        // Rows and rules are the same width — no trailing space after the final border.
        let w = widths(&out);
        assert!(w.iter().all(|&x| x == w[0]), "ragged rows: {w:?}");
    }

    #[test]
    fn every_table_line_fits_the_width() {
        for width in [80usize, 60, 40, 30, 24] {
            let out = render(WIDE, width);
            for (i, w) in widths(&out).iter().enumerate() {
                assert!(*w <= width, "line {i} is {w} wide at width {width}:\n{out}");
            }
        }
    }

    #[test]
    fn wide_table_wraps_inside_its_column() {
        let out = render(WIDE, 80);
        let lines: Vec<&str> = out.lines().collect();
        // Columns keep their natural widths; only the prose column gives ground.
        assert!(lines[0].starts_with("│ Option  │ Type    │ Default │ Description"));
        // No cell text is lost, and the borders still line up.
        let text: String = out
            .lines()
            .map(|l| l.replace(['│', '├', '┼', '┤', '─'], ""))
            .collect();
        for word in ["--width", "integer", "Target", "terminal."] {
            assert!(text.contains(word), "{word} missing from:\n{out}");
        }
        let w = widths(&out);
        assert!(w.iter().all(|&x| x == w[0]), "ragged rows: {w:?}");
    }

    #[test]
    fn wrapped_header_keeps_rule_below_it() {
        let out = render(
            "| Column header that is quite long | b |\n|---|---|\n| x | y |\n",
            24,
        );
        let lines: Vec<&str> = out.lines().collect();
        assert!(lines[0].contains("Column header"));
        assert!(lines[1].contains("that is quite"), "{:?}", lines[1]);
        assert!(lines[2].contains("long"), "{:?}", lines[2]);
        // The rule comes after the header's *last* line, not its first.
        assert!(lines[3].starts_with('├'), "{:?}", lines[3]);
        assert!(lines[4].contains('x'), "{:?}", lines[4]);
    }

    #[test]
    fn alignment_applies_to_every_wrapped_line() {
        // A right-aligned narrow column next to a wrapped cell: every line is padded to the column.
        let out = render(
            "| a | R |\n|---|--:|\n| xxxxxxxxxxxxxxxx | 1 |\n| y | 2 |\n",
            40,
        );
        let rule = out.lines().find(|l| l.starts_with('├')).unwrap();
        for l in out.lines().filter(|l| l.starts_with('│')) {
            assert_eq!(
                super::visible_width(l),
                super::visible_width(rule),
                "row does not match the rule width: {l:?}"
            );
        }
    }

    #[test]
    fn token_longer_than_column_is_hard_split() {
        // A single unbreakable token must not overrun the border.
        let out = render(
            "| f | v |\n|---|---|\n| `--a-very-long-flag-name` | x |\n",
            20,
        );
        for (i, w) in widths(&out).iter().enumerate() {
            assert!(*w <= 20, "line {i} is {w} wide:\n{out}");
        }
        assert!(out.contains("--a-very-lon"), "{out}");
        assert!(out.contains("g-flag-name"), "{out}");
    }

    #[test]
    fn narrow_width_falls_back_to_stacked() {
        let out = render(WIDE, 40);
        assert!(!out.contains('│'), "no grid expected:\n{out}");
        assert!(!out.contains('─'), "no rule expected:\n{out}");
        // The header row supplies the labels.
        assert!(out.contains("Option: --width"), "{out}");
        assert!(out.contains("Type: integer"), "{out}");
        // A wrapped value hangs under the first line's value, not under the label.
        let lines: Vec<&str> = out.lines().collect();
        let desc = lines
            .iter()
            .position(|l| l.starts_with("Description:"))
            .unwrap();
        let cont = lines[desc + 1];
        assert!(
            cont.starts_with(&" ".repeat("Description: ".len())),
            "continuation must hang under the value: {cont:?}"
        );
    }

    #[test]
    fn header_that_cannot_fit_falls_back_to_stacked() {
        // At 44 the prose column lands at 9 — wide enough to pass MIN_COL, but too narrow for the
        // header label `Description`, which would hard-split into `Descripti`/`on`.
        let src = "| Option | Type | Default | Description |\n\
                   |---|---|---|---|\n\
                   | `--width` | integer | `80` | Target line width in columns used when wrapping prose and rendered table cells to the terminal. |\n";
        let out = render(src, 44);
        assert!(!out.contains('│'), "grid is unsalvageable here:\n{out}");
        assert!(out.contains("Description: Target line"), "{out}");
        for (i, w) in widths(&out).iter().enumerate() {
            assert!(*w <= 44, "line {i} is {w} wide:\n{out}");
        }
        // One column wider and the grid is fine again.
        let out = render(src, 60);
        assert!(out.contains('│'), "grid expected at 60:\n{out}");
        assert!(out.lines().any(|l| l.contains("Description")), "{out}");
    }

    #[test]
    fn stacked_layout_separates_records() {
        let out = render(
            "| Option | Type | Default | Description |\n\
             |---|---|---|---|\n\
             | `--width` | integer | `80` | Target line width in columns. |\n\
             | `--no-color` | flag | off | Disable ANSI styling entirely. |\n",
            40,
        );
        assert!(!out.contains('│'), "stacked expected:\n{out}");
        // Two records => exactly one blank line between them.
        assert_eq!(out.matches("\n\n").count(), 1, "{out}");
    }

    #[test]
    fn sparse_columns_do_not_trigger_the_stacked_layout() {
        // A one-character index column is not "squeezed", so the grid survives.
        let out = render(
            "| # | Name |\n|---|---|\n| 1 | a fairly long name value |\n| 2 | another long name value |\n",
            30,
        );
        assert!(out.contains('│'), "grid expected:\n{out}");
        for (i, w) in widths(&out).iter().enumerate() {
            assert!(*w <= 30, "line {i} is {w} wide:\n{out}");
        }
    }

    #[test]
    fn table_in_blockquote_reserves_the_indent() {
        let out = render(
            "> | a | Description |\n> |---|---|\n> | 1 | some fairly long description text here |\n",
            40,
        );
        for (i, w) in widths(&out).iter().enumerate() {
            assert!(*w <= 40, "line {i} is {w} wide:\n{out}");
        }
        // Every line carries the blockquote bar.
        assert!(out.lines().all(|l| l.starts_with("│ ")), "{out}");
    }

    #[test]
    fn osc8_link_does_not_inflate_column_width() {
        // A hyperlink's href is not visible text: it must not widen the column.
        let theme = Theme {
            clickable_links: true,
            ..Theme::default()
        };
        let src = "| a | b |\n|---|---|\n| [x](https://example.com/very/long/path) | y |\n";
        let plain = render_with(&parse(src), &Theme::no_color(), 80);
        let clicked = render_with(&parse(src), &theme, 80);
        assert_eq!(
            widths(&plain),
            widths(&clicked),
            "column widths differ with clickable links on"
        );
    }

    #[test]
    fn visible_width_ignores_escape_sequences() {
        assert_eq!(super::visible_width("hello"), 5);
        assert_eq!(super::visible_width("\x1b[4;34mhello\x1b[0m"), 5);
        // OSC 8: the payload is a URL and must not be counted.
        assert_eq!(
            super::visible_width(
                "\x1b]8;;https://example.com/very/long/path\x1b\\hello\x1b]8;;\x1b\\"
            ),
            5
        );
        // CJK is double-width.
        assert_eq!(super::visible_width("日本語"), 6);
    }

    #[test]
    fn fit_columns_waterfills_and_fills_the_last_column() {
        // Fits: naturals are returned untouched.
        assert_eq!(super::fit_columns(&[4, 4], 20), vec![4, 4]);
        // Too wide: the widest column gives ground first.
        assert_eq!(super::fit_columns(&[10, 7, 7, 95], 66), vec![10, 7, 7, 42]);
        // Slack left by the water-filling goes to the last column, so a column never stays
        // needlessly narrow (7+8 fits 15; 8+8 does not).
        assert_eq!(super::fit_columns(&[10, 10], 15), vec![7, 8]);
        // Never exceeds the budget, even when the columns cannot all fit.
        let w = super::fit_columns(&[100, 100, 100], 10);
        assert!(w.iter().sum::<usize>() <= 10, "{w:?}");
    }
}
