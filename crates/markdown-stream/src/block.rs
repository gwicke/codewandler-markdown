//! The streaming block parser: turns input lines into block-level events, deferring inline parsing
//! until a paragraph/heading closes.
//!
//! Pragmatic CommonMark subset built to grow: document, ATX headings, thematic breaks, fenced code,
//! blockquotes (single level), bullet/ordered lists, paragraphs (with lazy continuation), and blank
//! lines. Input is consumed line by line so the event stream is independent of chunking.

use crate::event::*;
use crate::inline;
use crate::parser::Parser;

#[derive(Default)]
pub struct StreamParser {
    buf: Vec<u8>,
    started: bool,
    flushed: bool,
    in_quote: bool,
    list: Option<ListState>,
    leaf: Leaf,
}

struct ListState {
    ordered: bool,
    marker: char,
    item_open: bool,
}

#[derive(Default)]
enum Leaf {
    #[default]
    None,
    Paragraph(String),
    Fenced {
        ch: u8,
        len: usize,
    },
    Table {
        aligns: Vec<Alignment>,
    },
}

impl StreamParser {
    pub fn new() -> Self {
        Self::default()
    }
}

impl Parser for StreamParser {
    fn write(&mut self, chunk: &[u8]) -> Vec<Event> {
        let mut out = Vec::new();
        self.buf.extend_from_slice(chunk);
        while let Some(nl) = self.buf.iter().position(|&b| b == b'\n') {
            let mut line: Vec<u8> = self.buf.drain(..=nl).collect();
            line.pop(); // drop '\n'
            if line.last() == Some(&b'\r') {
                line.pop();
            }
            let s = String::from_utf8_lossy(&line).into_owned();
            self.process_line(&s, &mut out);
        }
        out
    }

    fn flush(&mut self) -> Vec<Event> {
        let mut out = Vec::new();
        if self.flushed {
            return out;
        }
        if !self.buf.is_empty() {
            let line = std::mem::take(&mut self.buf);
            let s = String::from_utf8_lossy(&line).into_owned();
            self.process_line(&s, &mut out);
        }
        self.close_leaf(&mut out);
        self.close_list(&mut out);
        self.close_quote(&mut out);
        if self.started {
            out.push(Event::exit(BlockKind::Document));
        }
        self.flushed = true;
        out
    }

    fn reset(&mut self) {
        *self = Self::default();
    }
}

impl StreamParser {
    fn ensure_doc(&mut self, out: &mut Vec<Event>) {
        if !self.started {
            out.push(Event::enter(BlockKind::Document));
            self.started = true;
        }
    }

    fn process_line(&mut self, raw: &str, out: &mut Vec<Event>) {
        // Strip a single blockquote marker, tracking quote open/close.
        let (quoted, content) = strip_quote(raw);

        if quoted {
            self.ensure_doc(out);
            if !self.in_quote {
                self.close_leaf(out);
                self.close_list(out);
                out.push(Event::enter(BlockKind::BlockQuote));
                self.in_quote = true;
            }
        }

        let trimmed = content.trim_start();

        // Inside a fenced code block: everything is literal until the closing fence.
        if let Leaf::Fenced { ch, len, .. } = &self.leaf {
            if is_closing_fence(trimmed, *ch, *len) {
                self.close_leaf(out);
            } else {
                out.push(Event::text(format!("{content}\n")));
            }
            return;
        }

        // Inside a GFM table: a pipe row is a body row; anything else closes the table and is then
        // handled normally.
        if let Leaf::Table { aligns } = &self.leaf {
            if !content.trim().is_empty() && content.contains('|') {
                let aligns = aligns.clone();
                self.emit_row(split_row(&content), &aligns, out);
                return;
            }
            self.close_leaf(out);
        }

        // Blank line: closes a paragraph; ends a lazy blockquote.
        if content.trim().is_empty() {
            self.close_leaf(out);
            if self.in_quote && !quoted {
                self.close_quote(out);
            }
            return;
        }

        // A non-quoted line while a paragraph is open continues it lazily (paragraph
        // continuation); otherwise a non-quoted line ends an open blockquote.
        if self.in_quote && !quoted && !matches!(self.leaf, Leaf::Paragraph(_)) {
            self.close_list(out);
            self.close_quote(out);
        }

        // Fenced code start.
        if let Some((ch, len, info)) = fence_start(trimmed) {
            self.ensure_doc(out);
            self.close_leaf(out);
            let data = BlockData {
                info,
                ..Default::default()
            };
            out.push(Event::EnterBlock {
                block: BlockKind::FencedCode,
                data,
                span: Span::default(),
            });
            self.leaf = Leaf::Fenced { ch, len };
            return;
        }

        // ATX heading.
        if let Some((level, htext)) = atx_heading(trimmed) {
            self.ensure_doc(out);
            self.close_leaf(out);
            self.close_list(out);
            let data = BlockData {
                level,
                ..Default::default()
            };
            out.push(Event::EnterBlock {
                block: BlockKind::Heading,
                data,
                span: Span::default(),
            });
            inline::parse(htext, &InlineStyle::default(), out);
            out.push(Event::exit(BlockKind::Heading));
            return;
        }

        // Thematic break.
        if is_thematic_break(trimmed) {
            self.ensure_doc(out);
            self.close_leaf(out);
            self.close_list(out);
            out.push(Event::enter(BlockKind::ThematicBreak));
            out.push(Event::exit(BlockKind::ThematicBreak));
            return;
        }

        // List item.
        if let Some((ordered, marker, start, rest)) = list_marker(trimmed) {
            self.ensure_doc(out);
            self.close_leaf(out);
            self.start_or_continue_list(ordered, marker, start, out);
            // The item's first line becomes a paragraph (loose rendering for now).
            self.leaf = Leaf::Paragraph(rest.to_string());
            return;
        }

        // Default: paragraph text (new or lazy continuation).
        match &mut self.leaf {
            Leaf::Paragraph(p) => {
                // A single-line paragraph containing `|` followed by a delimiter row starts a table.
                if !p.contains('\n') && p.contains('|') {
                    if let Some(aligns) = parse_delim_row(trimmed) {
                        let headers = split_row(p);
                        if headers.len() == aligns.len() {
                            let header = std::mem::take(p);
                            self.leaf = Leaf::None;
                            self.start_table(&header, aligns, out);
                            return;
                        }
                    }
                }
                p.push('\n');
                p.push_str(content.trim_end());
            }
            _ => {
                self.ensure_doc(out);
                if self.list.is_none() {
                    self.close_list(out);
                }
                self.leaf = Leaf::Paragraph(content.trim_end().to_string());
            }
        }
    }

    fn start_or_continue_list(
        &mut self,
        ordered: bool,
        marker: char,
        start: u64,
        out: &mut Vec<Event>,
    ) {
        let same = matches!(&self.list, Some(l) if l.ordered == ordered && l.marker == marker);
        if !same {
            self.close_list(out);
            let data = BlockData {
                list: Some(ListData {
                    ordered,
                    start,
                    tight: true,
                    marker,
                }),
                ..Default::default()
            };
            out.push(Event::EnterBlock {
                block: BlockKind::List,
                data,
                span: Span::default(),
            });
            self.list = Some(ListState {
                ordered,
                marker,
                item_open: false,
            });
        }
        if let Some(l) = &mut self.list {
            if l.item_open {
                out.push(Event::exit(BlockKind::ListItem));
            }
            out.push(Event::enter(BlockKind::ListItem));
            l.item_open = true;
        }
    }

    fn close_leaf(&mut self, out: &mut Vec<Event>) {
        match std::mem::take(&mut self.leaf) {
            Leaf::None => {}
            Leaf::Paragraph(text) => {
                let in_tight_item = self.list.as_ref().is_some_and(|l| l.item_open);
                if !in_tight_item {
                    out.push(Event::enter(BlockKind::Paragraph));
                }
                inline::parse(&text, &InlineStyle::default(), out);
                if !in_tight_item {
                    out.push(Event::exit(BlockKind::Paragraph));
                }
            }
            Leaf::Fenced { .. } => {
                out.push(Event::exit(BlockKind::FencedCode));
            }
            Leaf::Table { .. } => {
                out.push(Event::exit(BlockKind::Table));
            }
        }
    }

    fn start_table(&mut self, header: &str, aligns: Vec<Alignment>, out: &mut Vec<Event>) {
        let data = BlockData {
            alignment: aligns.clone(),
            ..Default::default()
        };
        out.push(Event::EnterBlock {
            block: BlockKind::Table,
            data,
            span: Span::default(),
        });
        self.emit_row(split_row(header), &aligns, out);
        self.leaf = Leaf::Table { aligns };
    }

    fn emit_row(&mut self, mut cells: Vec<String>, aligns: &[Alignment], out: &mut Vec<Event>) {
        cells.resize(aligns.len(), String::new()); // pad short rows / drop extra cells (GFM)
        out.push(Event::enter(BlockKind::TableRow));
        for cell in cells {
            out.push(Event::enter(BlockKind::TableCell));
            inline::parse(cell.trim(), &InlineStyle::default(), out);
            out.push(Event::exit(BlockKind::TableCell));
        }
        out.push(Event::exit(BlockKind::TableRow));
    }

    fn close_list(&mut self, out: &mut Vec<Event>) {
        self.close_leaf(out);
        if let Some(mut l) = self.list.take() {
            if l.item_open {
                out.push(Event::exit(BlockKind::ListItem));
                l.item_open = false;
            }
            out.push(Event::exit(BlockKind::List));
        }
    }

    fn close_quote(&mut self, out: &mut Vec<Event>) {
        if self.in_quote {
            self.close_leaf(out);
            self.close_list(out);
            out.push(Event::exit(BlockKind::BlockQuote));
            self.in_quote = false;
        }
    }
}

// ---------------------------------------------------------------------------
// line classifiers
// ---------------------------------------------------------------------------

fn strip_quote(line: &str) -> (bool, String) {
    let t = line.trim_start();
    if let Some(rest) = t.strip_prefix('>') {
        (true, rest.strip_prefix(' ').unwrap_or(rest).to_string())
    } else {
        (false, line.to_string())
    }
}

fn atx_heading(line: &str) -> Option<(u8, &str)> {
    let hashes = line.bytes().take_while(|&b| b == b'#').count();
    if hashes == 0 || hashes > 6 {
        return None;
    }
    let rest = &line[hashes..];
    if !rest.is_empty() && !rest.starts_with([' ', '\t']) {
        return None;
    }
    let text = rest.trim().trim_end_matches('#').trim_end();
    Some((hashes as u8, text))
}

fn is_thematic_break(line: &str) -> bool {
    let s: String = line.chars().filter(|c| !c.is_whitespace()).collect();
    s.len() >= 3
        && (s.bytes().all(|b| b == b'-')
            || s.bytes().all(|b| b == b'*')
            || s.bytes().all(|b| b == b'_'))
}

fn fence_start(line: &str) -> Option<(u8, usize, String)> {
    let b = line.as_bytes();
    let ch = *b.first()?;
    if ch != b'`' && ch != b'~' {
        return None;
    }
    let len = line.bytes().take_while(|&c| c == ch).count();
    if len < 3 {
        return None;
    }
    let info = line[len..].trim().to_string();
    // A ``` info string may not itself contain a backtick.
    if ch == b'`' && info.contains('`') {
        return None;
    }
    Some((ch, len, info))
}

fn is_closing_fence(line: &str, ch: u8, open_len: usize) -> bool {
    let len = line.bytes().take_while(|&c| c == ch).count();
    len >= open_len && line[len..].trim().is_empty()
}

/// Parse a list marker. Returns `(ordered, marker_char, start, rest_after_marker)`.
fn list_marker(line: &str) -> Option<(bool, char, u64, &str)> {
    let b = line.as_bytes();
    // Bullet: -, *, + followed by a space.
    if let Some(&c) = b.first() {
        if (c == b'-' || c == b'*' || c == b'+') && b.get(1) == Some(&b' ') {
            return Some((false, c as char, 1, line[2..].trim_start()));
        }
    }
    // Ordered: digits, then '.' or ')', then a space.
    let digits = line.bytes().take_while(|c| c.is_ascii_digit()).count();
    if (1..=9).contains(&digits) {
        let sep = b.get(digits).copied();
        if (sep == Some(b'.') || sep == Some(b')')) && b.get(digits + 1) == Some(&b' ') {
            let start: u64 = line[..digits].parse().unwrap_or(1);
            let marker = sep.unwrap() as char;
            return Some((true, marker, start, line[digits + 2..].trim_start()));
        }
    }
    None
}

/// Split a GFM table row into trimmed cell strings, honoring escaped pipes and the optional
/// leading/trailing `|`.
fn split_row(line: &str) -> Vec<String> {
    let mut s = line.trim();
    s = s.strip_prefix('|').unwrap_or(s);
    s = s.strip_suffix('|').unwrap_or(s);
    let mut cells = Vec::new();
    let mut cur = String::new();
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\\' => {
                if let Some(&n) = chars.peek() {
                    cur.push('\\');
                    cur.push(n);
                    chars.next();
                } else {
                    cur.push('\\');
                }
            }
            '|' => {
                cells.push(cur.trim().to_string());
                cur.clear();
            }
            _ => cur.push(c),
        }
    }
    cells.push(cur.trim().to_string());
    cells
}

/// Parse a table delimiter row (e.g. `| :--- | :--: | ---: |`) into per-column alignments. Returns
/// `None` if the line isn't a valid delimiter row.
fn parse_delim_row(line: &str) -> Option<Vec<Alignment>> {
    if !line.contains('|') && !line.contains('-') {
        return None;
    }
    let cells = split_row(line);
    if cells.is_empty() {
        return None;
    }
    let mut aligns = Vec::with_capacity(cells.len());
    for cell in &cells {
        let c = cell.trim();
        if c.is_empty() {
            return None;
        }
        let left = c.starts_with(':');
        let right = c.ends_with(':');
        let mid = &c[usize::from(left)..c.len() - usize::from(right)];
        if mid.is_empty() || !mid.bytes().all(|b| b == b'-') {
            return None;
        }
        aligns.push(match (left, right) {
            (true, true) => Alignment::Center,
            (true, false) => Alignment::Left,
            (false, true) => Alignment::Right,
            (false, false) => Alignment::None,
        });
    }
    Some(aligns)
}
