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

/// A buffered table cell: its content as styled runs, unstyled and unwrapped, so it can be wrapped
/// into whatever column width the table ends up with.
type Cell = Vec<(String, InlineStyle)>;

struct TableBuf {
    aligns: Vec<Alignment>,
    rows: Vec<Vec<Cell>>,
    cur_row: Vec<Cell>,
}

/// Narrowest a column may be squeezed to before the grid stops being readable and the table falls
/// back to the stacked layout. A column that is *naturally* this narrow is not squeezed, so it does
/// not trigger the fallback.
const MIN_COL: usize = 8;

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

        for (li, line) in wrap_runs(&segments, avail, false).iter().enumerate() {
            if li > 0 {
                self.cur.extend(cont_spans.clone());
            }
            for (text, style) in line {
                let st = self.style_for(style, base);
                self.cur.push(Span::styled(text.clone(), st));
            }
            self.newline();
        }
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

    /// Render a buffered table: fit the columns to the width, then wrap every cell into its column.
    ///
    /// Mirrors `markdown-terminal` exactly — same water-fill, same stacked fallback — so the two
    /// renderers agree line for line on a given width.
    fn render_table(&mut self) {
        let Some(t) = self.table.take() else {
            return;
        };
        let ncol = t
            .aligns
            .len()
            .max(t.rows.iter().map(Vec::len).max().unwrap_or(0));
        if ncol == 0 {
            return;
        }
        let mut naturals = vec![0usize; ncol];
        for row in &t.rows {
            for (i, cell) in row.iter().enumerate() {
                naturals[i] = naturals[i].max(runs_width(cell));
            }
        }
        let (_, indent_w) = self.indent_cont_spans();
        let avail = self.width.saturating_sub(indent_w);
        // Fixed chrome: `"│ "` opens the row, `" │"` closes each column, `" "` separates them.
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

        self.gap();
        if unsalvageable {
            self.render_table_stacked(&t, ncol, avail);
        } else {
            self.render_table_grid(&t, &widths);
        }
        self.pending_gap = true;
    }

    /// Draw the table as a grid of box-drawing borders, wrapping every cell into its column.
    fn render_table_grid(&mut self, t: &TableBuf, widths: &[usize]) {
        let ncol = widths.len();
        let muted = self.theme.muted;
        let empty: Cell = Vec::new();
        for (ri, row) in t.rows.iter().enumerate() {
            let cells: Vec<Vec<Vec<Span<'static>>>> = (0..ncol)
                .map(|i| {
                    let cell = row.get(i).unwrap_or(&empty);
                    // `hard` splits a token wider than the column, so a long flag name or URL can
                    // never overrun the border.
                    self.wrap_cell(cell, widths[i], true)
                })
                .collect();
            let height = cells.iter().map(Vec::len).max().unwrap_or(1);
            let (ind, _) = self.indent_cont_spans();
            for li in 0..height {
                self.cur.extend(ind.clone());
                self.cur.push(Span::styled("│ ".to_string(), muted));
                for (i, width) in widths.iter().enumerate() {
                    let line: Vec<&Span<'static>> = cells[i]
                        .get(li)
                        .map(|l| l.iter().collect())
                        .unwrap_or_default();
                    let vis: usize = line.iter().map(|s| s.width()).sum();
                    let pad = width.saturating_sub(vis);
                    match t.aligns.get(i).copied().unwrap_or(Alignment::None) {
                        Alignment::Right => {
                            self.cur.push(Span::raw(" ".repeat(pad)));
                            self.cur.extend(line.into_iter().cloned());
                        }
                        Alignment::Center => {
                            let l = pad / 2;
                            self.cur.push(Span::raw(" ".repeat(l)));
                            self.cur.extend(line.into_iter().cloned());
                            self.cur.push(Span::raw(" ".repeat(pad - l)));
                        }
                        _ => {
                            self.cur.extend(line.into_iter().cloned());
                            self.cur.push(Span::raw(" ".repeat(pad)));
                        }
                    }
                    self.cur.push(Span::styled(" │".to_string(), muted));
                    if i + 1 < ncol {
                        self.cur.push(Span::raw(" "));
                    }
                }
                self.newline();
            }
            // The rule belongs under the *last* line of the header, which may have wrapped.
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
    }

    /// Draw the table as a stack of `label: value` records — the fallback for a width too narrow to
    /// hold a grid. The header row supplies the labels, empty cells are skipped, and records are
    /// separated by a blank line. There is no column chrome here, so the whole width is available.
    fn render_table_stacked(&mut self, t: &TableBuf, ncol: usize, avail: usize) {
        let Some((header, rest)) = t.rows.split_first() else {
            return;
        };
        let bold = self.theme.bold;
        let empty: Cell = Vec::new();
        let mut first_record = true;
        for row in rest {
            let mut wrote_in_record = false;
            for i in 0..ncol {
                let cell = row.get(i).unwrap_or(&empty);
                if runs_text(cell).trim().is_empty() {
                    continue;
                }
                if !wrote_in_record && !first_record {
                    self.newline();
                }
                first_record = false;
                wrote_in_record = true;
                let label = header.get(i).map(|c| runs_text(c)).unwrap_or_default();
                // Continuation lines hang under the value; a long label may claim at most half.
                let hang = (UnicodeWidthStr::width(label.as_str()) + 2).min(avail / 2);
                let (ind, _) = self.indent_cont_spans();
                let lines = self.wrap_cell(cell, avail.saturating_sub(hang), false);
                for (li, line) in lines.iter().enumerate() {
                    self.cur.extend(ind.clone());
                    if li == 0 {
                        if !label.is_empty() {
                            self.cur.push(Span::styled(format!("{label}: "), bold));
                        }
                    } else {
                        self.cur.push(Span::raw(" ".repeat(hang)));
                    }
                    self.cur.extend(line.iter().cloned());
                    self.newline();
                }
            }
        }
    }

    /// Wrap a cell's runs into styled lines of at most `width` visible columns.
    fn wrap_cell(&self, cell: &Cell, width: usize, hard: bool) -> Vec<Vec<Span<'static>>> {
        // A cell comes from a single source line, so a newline in it is a separator, not a break.
        let cell: Cell = cell
            .iter()
            .map(|(text, style)| (text.replace('\n', " "), style.clone()))
            .collect();
        wrap_runs(&cell, width, hard)
            .into_iter()
            .map(|line| {
                line.into_iter()
                    .map(|(text, style)| Span::styled(text, self.style_for(&style, None)))
                    .collect()
            })
            .collect()
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

/// Visible width of a cell's unstyled runs.
fn runs_width(runs: &[(String, InlineStyle)]) -> usize {
    runs.iter()
        .map(|(text, _)| UnicodeWidthStr::width(text.as_str()))
        .sum()
}

/// A cell's plain text, runs concatenated — used for stacked-layout labels.
fn runs_text(runs: &[(String, InlineStyle)]) -> String {
    runs.iter().map(|(text, _)| text.as_str()).collect()
}

/// The widest unbreakable token in a cell — the narrowest a column can be and still hold this cell
/// without a hard split.
fn runs_widest_token(runs: &[(String, InlineStyle)]) -> usize {
    runs.iter()
        .flat_map(|(text, _)| atoms(text))
        .filter_map(|a| match a {
            Atom::Word(word) => Some(UnicodeWidthStr::width(word)),
            _ => None,
        })
        .max()
        .unwrap_or(0)
}

/// Fit `naturals` into `budget` columns: natural widths if the table fits, otherwise water-fill
/// (the widest columns give ground first) and hand the remainder to the last, most flexible column.
///
/// The same policy as `markdown-terminal`, so both renderers lay a table out identically. The
/// water-fill is a binary search for the largest level `L` with `sum(min(nat_i, L)) <= budget`; that
/// sum is monotonic in `L`, so this is exact and costs `O(ncol * log budget)`.
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

/// Split styled runs into physical lines of at most `width` visible columns. Shared by paragraph
/// wrapping and table cells so both reflow words identically.
///
/// `hard` splits a token wider than a whole line at char boundaries by visible width — needed for
/// table cells, where a long `--flag` or URL would otherwise overrun the column border. A hard break
/// before any word is dropped (loose list items emit a phantom `\n` ahead of their paragraph).
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
        *line_vis += UnicodeWidthStr::width(text);
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
                    let wv = UnicodeWidthStr::width(word);
                    let sep = usize::from(line_vis > 0 && pending_space);
                    if line_vis > 0 && line_vis + sep + wv > width {
                        lines.push(Vec::new());
                        line_vis = 0;
                    } else if sep == 1 {
                        push(&mut lines, &mut line_vis, " ", &InlineStyle::default());
                    }
                    if hard && wv > width {
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

    // --- tables -------------------------------------------------------------

    /// A 4-column table whose last cell is long enough to need wrapping.
    const WIDE: &str = "| Option | Type | Default | Description |\n\
                        |---|---|---|---|\n\
                        | `--width` | integer | `80` | Target line width in columns used when wrapping prose and rendered table cells to the terminal. |\n";

    fn widths(src: &str, width: usize) -> Vec<usize> {
        lines(src, width)
            .iter()
            .map(|l| unicode_width::UnicodeWidthStr::width(l.as_str()))
            .collect()
    }

    #[test]
    fn table_that_fits_keeps_natural_widths() {
        let ls = lines(
            "| L | C | R |\n|:--|:-:|--:|\n| left | mid | right |\n| x | yy | zzzz |\n",
            80,
        );
        assert_eq!(
            ls,
            vec![
                "│ L    │  C  │     R │",
                "├──────┼─────┼───────┤",
                "│ left │ mid │ right │",
                "│ x    │ yy  │  zzzz │",
            ]
        );
    }

    #[test]
    fn every_table_line_fits_the_width() {
        for width in [80usize, 60, 40, 30, 24] {
            let ls = lines(WIDE, width);
            for (i, l) in ls.iter().enumerate() {
                let w = unicode_width::UnicodeWidthStr::width(l.as_str());
                assert!(w <= width, "line {i} is {w} wide at width {width}: {l:?}");
            }
        }
    }

    #[test]
    fn wide_table_wraps_inside_its_column() {
        let ls = lines(WIDE, 80);
        assert!(ls[0].starts_with("│ Option  │ Type    │ Default │ Description"));
        let w = widths(WIDE, 80);
        assert!(w.iter().all(|&x| x == w[0]), "ragged rows: {w:?}");
    }

    #[test]
    fn narrow_width_falls_back_to_stacked() {
        let out = lines(WIDE, 40).join("\n");
        assert!(!out.contains('│'), "no grid expected:\n{out}");
        assert!(!out.contains('─'), "no rule expected:\n{out}");
        assert!(out.contains("Option: --width"), "{out}");
        assert!(out.contains("Type: integer"), "{out}");
    }

    #[test]
    fn fit_columns_matches_the_terminal_renderer_policy() {
        use super::fit_columns;
        assert_eq!(fit_columns(&[4, 4], 20), vec![4, 4]);
        assert_eq!(fit_columns(&[10, 7, 7, 95], 66), vec![10, 7, 7, 42]);
        assert_eq!(fit_columns(&[10, 10], 15), vec![7, 8]);
        let w = fit_columns(&[100, 100, 100], 10);
        assert!(w.iter().sum::<usize>() <= 10, "{w:?}");
    }
}
