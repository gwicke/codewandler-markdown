//! `markdown-ratatui` — render a [`markdown_stream`] event stream to `ratatui::text::Text`.
//!
//! A sibling of `markdown-terminal`: it walks the same parser events and does the same width-aware
//! wrapping, list/blockquote indentation, and inline styling — but emits `ratatui` `Line`/`Span`
//! directly (styled with `ratatui::style::Style`) instead of ANSI. That lets a TUI render Markdown
//! natively, with no ANSI round-trip. Output is pre-wrapped to the given width with list hanging
//! indents baked in, so render it WITHOUT a wrapping `Paragraph` (or keep wrap only as a safety net;
//! never `trim`, it would eat the hanging indents).

#![forbid(unsafe_code)]

use markdown_stream::{Alignment, BlockKind, Event, InlineStyle};
use ratatui::style::Style;
use ratatui::text::{Line, Span, Text};
use unicode_width::UnicodeWidthStr;

mod theme;
pub use theme::Theme;

/// Render a complete event stream with the default theme and width 80.
pub fn render(events: &[Event]) -> Text<'static> {
    render_with(events, &Theme::default(), 80)
}

/// Render a complete event stream with an explicit theme and wrap width.
pub fn render_with(events: &[Event], theme: &Theme, width: usize) -> Text<'static> {
    let mut r = Renderer::new(theme.clone(), width);
    r.feed(events);
    Text::from(r.finish())
}

struct Renderer {
    theme: Theme,
    width: usize,
    /// nesting prefixes (one per open blockquote / list level)
    prefixes: Vec<Prefix>,
    list_stack: Vec<ListCtx>,
    /// accumulated styled segments for the current paragraph/heading/table cell
    segments: Vec<(String, InlineStyle)>,
    in_code: bool,
    table: Option<TableBuf>,
    /// blank line owed before the next block
    pending_gap: bool,
    wrote_any: bool,
    /// spans of the physical line currently being built
    cur: Vec<Span<'static>>,
    lines: Vec<Line<'static>>,
}

struct ListCtx {
    ordered: bool,
    next: u64,
}

/// A nesting prefix: `first` (text + style) is printed on the first line a level appears on (a list
/// marker like `1. `), `cont` on continuation/wrapped lines (blanks of equal width for list markers;
/// the `│ ` bar repeats for blockquotes). `emitted` flips after `first` is used once.
struct Prefix {
    first: (String, Style),
    cont: (String, Style),
    emitted: bool,
}

impl Prefix {
    fn repeating(text: String, style: Style) -> Self {
        Prefix {
            first: (text.clone(), style),
            cont: (text, style),
            emitted: false,
        }
    }

    fn marker(marker: String) -> Self {
        let pad = " ".repeat(UnicodeWidthStr::width(marker.as_str()));
        Prefix {
            first: (marker, Style::default()),
            cont: (pad, Style::default()),
            emitted: false,
        }
    }
}

/// A buffered table cell: its rendered spans plus their visible width (for column sizing).
type Cell = (Vec<Span<'static>>, usize);

struct TableBuf {
    aligns: Vec<Alignment>,
    rows: Vec<Vec<Cell>>,
    cur_row: Vec<Cell>,
}

impl Renderer {
    fn new(theme: Theme, width: usize) -> Self {
        Renderer {
            theme,
            width: width.max(20),
            prefixes: Vec::new(),
            list_stack: Vec::new(),
            segments: Vec::new(),
            in_code: false,
            table: None,
            pending_gap: false,
            wrote_any: false,
            cur: Vec::new(),
            lines: Vec::new(),
        }
    }

    fn feed(&mut self, events: &[Event]) {
        for ev in events {
            self.event(ev);
        }
    }

    fn finish(mut self) -> Vec<Line<'static>> {
        if !self.cur.is_empty() {
            self.newline();
        }
        self.lines
    }

    /// Push the in-progress spans as a finished line.
    fn newline(&mut self) {
        let spans = std::mem::take(&mut self.cur);
        self.lines.push(Line::from(spans));
        self.wrote_any = true;
    }

    /// Emit an owed blank line before a block.
    fn gap(&mut self) {
        if self.pending_gap && self.wrote_any {
            self.lines.push(Line::default());
        }
        self.pending_gap = false;
    }

    /// Prefix spans for the first line of a block (consumes each level's marker once), plus width.
    fn indent_first_spans(&mut self) -> (Vec<Span<'static>>, usize) {
        let mut spans = Vec::new();
        let mut w = 0;
        for p in &mut self.prefixes {
            let seg = if p.emitted {
                &p.cont
            } else {
                p.emitted = true;
                &p.first
            };
            w += UnicodeWidthStr::width(seg.0.as_str());
            spans.push(Span::styled(seg.0.clone(), seg.1));
        }
        (spans, w)
    }

    /// Prefix spans for continuation/wrapped lines (markers become blanks), plus width.
    fn indent_cont_spans(&self) -> (Vec<Span<'static>>, usize) {
        let mut spans = Vec::new();
        let mut w = 0;
        for p in &self.prefixes {
            w += UnicodeWidthStr::width(p.cont.0.as_str());
            spans.push(Span::styled(p.cont.0.clone(), p.cont.1));
        }
        (spans, w)
    }

    /// Compose a `Style` from an inline style and an optional block base (e.g. heading).
    fn style_for(&self, inline: &InlineStyle, base: Option<Style>) -> Style {
        let mut s = base.unwrap_or_default();
        if inline.strong {
            s = s.patch(self.theme.bold);
        }
        if inline.emphasis {
            s = s.patch(self.theme.italic);
        }
        if inline.strikethrough {
            s = s.patch(self.theme.strike);
        }
        if inline.code {
            s = s.patch(self.theme.code);
        }
        if inline.link.is_some() {
            s = s.patch(self.theme.link);
        }
        s
    }

    fn event(&mut self, ev: &Event) {
        match ev {
            Event::EnterBlock { block, data, .. } => match block {
                BlockKind::BlockQuote => {
                    self.gap();
                    let muted = self.theme.muted;
                    self.prefixes
                        .push(Prefix::repeating("│ ".to_string(), muted));
                }
                BlockKind::List => {
                    self.gap();
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
                    self.prefixes.push(Prefix::marker(marker));
                }
                BlockKind::FencedCode | BlockKind::IndentedCode => {
                    self.gap();
                    self.in_code = true;
                }
                BlockKind::Table => {
                    self.gap();
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
            },
            Event::ExitBlock { block, .. } => match block {
                BlockKind::Paragraph => {
                    self.flush_segments(None);
                    self.pending_gap = true;
                }
                BlockKind::Heading => {
                    let base = self.theme.heading;
                    self.flush_segments(Some(base));
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
                    if !self.segments.is_empty() {
                        self.flush_segments(None);
                    }
                    self.prefixes.pop();
                }
                BlockKind::ThematicBreak => {
                    self.gap();
                    self.thematic_break();
                    self.pending_gap = true;
                }
                BlockKind::FencedCode | BlockKind::IndentedCode => {
                    self.in_code = false;
                    self.pending_gap = true;
                }
                BlockKind::TableCell => {
                    let cell = self.take_cell();
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
                BlockKind::Table => self.render_table(),
                _ => {}
            },
            Event::Text { text, style, .. } => {
                if self.in_code {
                    self.write_code_line(text);
                } else {
                    self.segments.push((text.clone(), style.clone()));
                }
            }
            // Inline nesting is already baked into each Text event's `InlineStyle`.
            Event::EnterInline { .. } | Event::ExitInline { .. } => {}
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
    }

    /// Render the accumulated inline segments as wrapped, styled, indented lines.
    fn flush_segments(&mut self, base: Option<Style>) {
        if self.segments.is_empty() {
            return;
        }
        self.gap();
        let segments = std::mem::take(&mut self.segments);
        let (cont_spans, cont_w) = self.indent_cont_spans();
        let avail = self.width.saturating_sub(cont_w).max(20);
        let (first_spans, _) = self.indent_first_spans();
        self.cur.extend(first_spans);

        let mut line_vis = 0usize;
        let mut pending_space = false;
        let mut started = false;
        for (raw, style) in &segments {
            for atom in atoms(raw) {
                match atom {
                    Atom::Space => pending_space = true,
                    Atom::Hard => {
                        // Drop a hard break before any word: loose list items emit a phantom "\n"
                        // ahead of their paragraph, which would print a bare-marker line.
                        if !started {
                            continue;
                        }
                        self.newline();
                        self.cur.extend(cont_spans.clone());
                        line_vis = 0;
                        pending_space = false;
                    }
                    Atom::Word(word) => {
                        let wv = UnicodeWidthStr::width(word);
                        let sep = usize::from(line_vis > 0 && pending_space);
                        if line_vis > 0 && line_vis + sep + wv > avail {
                            self.newline();
                            self.cur.extend(cont_spans.clone());
                            line_vis = 0;
                        } else if sep == 1 {
                            self.cur.push(Span::raw(" "));
                            line_vis += 1;
                        }
                        let st = self.style_for(style, base);
                        self.cur.push(Span::styled(word.to_string(), st));
                        line_vis += wv;
                        pending_space = false;
                        started = true;
                    }
                }
            }
        }
        self.newline();
    }

    /// Render one fenced/indented code line (uniform code color; no syntax highlighting in v1).
    fn write_code_line(&mut self, text: &str) {
        self.gap();
        for piece in text.split_inclusive('\n') {
            let nl = piece.ends_with('\n');
            let body = piece.strip_suffix('\n').unwrap_or(piece);
            let (ind, _) = self.indent_cont_spans();
            self.cur.extend(ind);
            self.cur.push(Span::raw("  "));
            self.cur
                .push(Span::styled(body.to_string(), self.theme.code));
            if nl {
                self.newline();
            }
        }
    }

    fn thematic_break(&mut self) {
        let (ind, _) = self.indent_cont_spans();
        self.cur.extend(ind);
        let rule = "─".repeat(self.width.min(60));
        self.cur.push(Span::styled(rule, self.theme.muted));
        self.newline();
    }

    /// Build a table cell's spans + visible width from the accumulated segments.
    fn take_cell(&mut self) -> Cell {
        let segs = std::mem::take(&mut self.segments);
        let mut spans = Vec::new();
        let mut w = 0;
        for (text, style) in &segs {
            let t = text.replace('\n', " ");
            w += UnicodeWidthStr::width(t.as_str());
            let st = self.style_for(style, None);
            spans.push(Span::styled(t, st));
        }
        (spans, w)
    }

    /// Render a buffered table: column widths, box-drawing borders, aligned cells.
    fn render_table(&mut self) {
        let Some(t) = self.table.take() else {
            return;
        };
        self.gap();
        let ncol = t
            .aligns
            .len()
            .max(t.rows.iter().map(Vec::len).max().unwrap_or(0));
        let mut widths = vec![0usize; ncol];
        for row in &t.rows {
            for (i, cell) in row.iter().enumerate() {
                widths[i] = widths[i].max(cell.1);
            }
        }
        let muted = self.theme.muted;
        let empty: Cell = (Vec::new(), 0);
        for (ri, row) in t.rows.iter().enumerate() {
            let (ind, _) = self.indent_cont_spans();
            self.cur.extend(ind);
            self.cur.push(Span::styled("│ ".to_string(), muted));
            for (i, width) in widths.iter().enumerate() {
                let cell = row.get(i).unwrap_or(&empty);
                let pad = width.saturating_sub(cell.1);
                match t.aligns.get(i).copied().unwrap_or(Alignment::None) {
                    Alignment::Right => {
                        self.cur.push(Span::raw(" ".repeat(pad)));
                        self.cur.extend(cell.0.clone());
                    }
                    Alignment::Center => {
                        let l = pad / 2;
                        self.cur.push(Span::raw(" ".repeat(l)));
                        self.cur.extend(cell.0.clone());
                        self.cur.push(Span::raw(" ".repeat(pad - l)));
                    }
                    _ => {
                        self.cur.extend(cell.0.clone());
                        self.cur.push(Span::raw(" ".repeat(pad)));
                    }
                }
                self.cur.push(Span::styled(" │ ".to_string(), muted));
            }
            self.newline();
            if ri == 0 {
                let (ind2, _) = self.indent_cont_spans();
                self.cur.extend(ind2);
                self.cur.push(Span::styled("├".to_string(), muted));
                for (i, width) in widths.iter().enumerate() {
                    self.cur.push(Span::styled("─".repeat(width + 2), muted));
                    let joint = if i + 1 < ncol { "┼" } else { "┤" };
                    self.cur.push(Span::styled(joint.to_string(), muted));
                }
                self.newline();
            }
        }
        self.pending_gap = true;
    }
}

/// A wrapping atom: a word, a space between words, or a hard line break.
enum Atom<'a> {
    Word(&'a str),
    Space,
    Hard,
}

/// Split a string into wrap atoms — words, spaces, and hard breaks — preserving exactly where
/// spaces did and didn't exist (adjacent styled runs must not gain a space).
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

#[cfg(test)]
mod tests {
    use super::{render_with, Theme};
    use markdown_stream::parse;

    /// Render to plain per-line strings (joined span text), ignoring style.
    fn lines(src: &str, width: usize) -> Vec<String> {
        let text = render_with(&parse(src), &Theme::no_color(), width);
        text.lines
            .iter()
            .map(|l| {
                l.spans
                    .iter()
                    .map(|s| s.content.as_ref())
                    .collect::<String>()
            })
            .collect()
    }

    #[test]
    fn wrapped_list_item_shows_marker_once_then_aligns() {
        let ls = lines("- alpha beta gamma delta epsilon zeta eta theta iota\n", 24);
        let body: Vec<&String> = ls.iter().filter(|l| !l.trim().is_empty()).collect();
        assert!(body.len() > 1, "input should wrap: {ls:?}");
        assert!(
            body[0].starts_with("• "),
            "marker on first line: {:?}",
            body[0]
        );
        for l in &body[1..] {
            assert!(!l.starts_with("• "), "marker repeated on wrap: {l:?}");
            assert!(
                l.starts_with("  ") && !l.trim_start().is_empty(),
                "continuation should be space-aligned: {l:?}"
            );
        }
    }

    #[test]
    fn loose_list_item_has_no_bare_marker_line() {
        let src = "1. first item that is quite long and certainly wraps\n\n\
                   2. second item that is also long enough to wrap as well\n";
        let ls = lines(src, 24);
        for l in &ls {
            let t = l.trim_end();
            assert!(t != "1." && t != "2.", "bare marker line in {ls:?}");
        }
        assert_eq!(
            ls.iter().filter(|l| l.starts_with("1. ")).count(),
            1,
            "{ls:?}"
        );
        assert_eq!(
            ls.iter().filter(|l| l.starts_with("2. ")).count(),
            1,
            "{ls:?}"
        );
    }

    #[test]
    fn heading_span_carries_style() {
        let text = render_with(&parse("# Title\n"), &Theme::default(), 40);
        let span = &text.lines[0].spans[0];
        assert!(span.content.contains("Title"));
        assert!(span
            .style
            .add_modifier
            .contains(ratatui::style::Modifier::BOLD));
    }
}
