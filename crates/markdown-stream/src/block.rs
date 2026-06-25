//! The streaming block parser: turns input lines into block-level events, deferring inline parsing
//! until a paragraph/heading closes.
//!
//! Pragmatic CommonMark subset built to grow: document, ATX headings, thematic breaks, fenced code,
//! blockquotes (single level), bullet/ordered lists, paragraphs (with lazy continuation), and blank
//! lines. Input is consumed line by line so the event stream is independent of chunking.

use crate::event::*;
use crate::inline;
use crate::linkref;
use crate::parser::Parser;
use std::collections::HashMap;

#[derive(Default)]
pub struct StreamParser {
    buf: Vec<u8>,
    started: bool,
    flushed: bool,
    in_quote: bool,
    list: Option<ListState>,
    leaf: Leaf,
    /// Link reference definitions seen so far, keyed by normalised label. Populated in line order as
    /// paragraphs are scanned; references resolve against the definitions visible at close time.
    refs: HashMap<String, LinkDef>,
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
    /// A raw HTML block (one of the seven CommonMark start conditions). Content is emitted verbatim,
    /// line by line, until `end` is satisfied.
    Html {
        end: HtmlEnd,
    },
}

/// The end condition for an open HTML block, per the seven CommonMark start conditions. The string
/// variants close on the *first line containing* the marker (inclusive); `Blank` closes on the first
/// blank line (which is not part of the block).
#[derive(Clone, Copy)]
enum HtmlEnd {
    /// Conditions 1–5: close on the first line that contains this (case-insensitive) marker.
    Marker(&'static str),
    /// Conditions 6–7: close on the first blank line.
    Blank,
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

        // Inside an HTML block: emit lines verbatim until the end condition fires. The marker
        // conditions (1–5) include the closing line in the block; the blank-line conditions (6–7)
        // close *before* a blank line, which is then handled normally (so it can, e.g., end a list).
        if let Leaf::Html { end } = self.leaf {
            match end {
                HtmlEnd::Marker(marker) => {
                    out.push(Event::text(format!("{content}\n")));
                    if contains_ci(&content, marker) {
                        self.close_leaf(out);
                    }
                    return;
                }
                HtmlEnd::Blank => {
                    if content.trim().is_empty() {
                        self.close_leaf(out);
                        // Fall through so the blank line ends a lazy blockquote like any other.
                    } else {
                        out.push(Event::text(format!("{content}\n")));
                        return;
                    }
                }
            }
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

        // HTML block start (one of the seven CommonMark conditions). Conditions 1–6 may interrupt an
        // open paragraph; condition 7 may not (it only starts when no paragraph is open), so we tell
        // the checker whether a paragraph is currently open. The block's own first line is emitted
        // verbatim here; subsequent lines flow through the in-HTML branch above.
        let in_paragraph = matches!(self.leaf, Leaf::Paragraph(_));
        if let Some(end) = html_block_start(trimmed, in_paragraph) {
            self.ensure_doc(out);
            self.close_leaf(out);
            self.close_list(out);
            out.push(Event::enter(BlockKind::HtmlBlock));
            out.push(Event::text(format!("{content}\n")));
            match end {
                // A marker condition whose closing marker is already on the start line closes here.
                HtmlEnd::Marker(marker) if contains_ci(&content, marker) => {
                    self.close_leaf(out);
                }
                _ => self.leaf = Leaf::Html { end },
            }
            return;
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
            inline::parse(htext, &InlineStyle::default(), &self.refs, out);
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
                // Consume any leading link reference definitions into `self.refs` (no output); only
                // the remaining lines form the paragraph. Definitions are registered *before* the
                // paragraph's own inline content is parsed, so a backward reference in trailing text
                // of the same block resolves.
                let body = self.consume_refdefs(&text);
                if body.is_empty() {
                    return;
                }
                let in_tight_item = self.list.as_ref().is_some_and(|l| l.item_open);
                if !in_tight_item {
                    out.push(Event::enter(BlockKind::Paragraph));
                }
                inline::parse(&body, &InlineStyle::default(), &self.refs, out);
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
            Leaf::Html { .. } => {
                out.push(Event::exit(BlockKind::HtmlBlock));
            }
        }
    }

    /// Strip leading link reference definitions from a buffered paragraph, registering each into
    /// `self.refs` (first definition of a label wins). Returns the remaining paragraph text (the
    /// lines after the last consumed definition), trimmed of the leading newline.
    ///
    /// A definition may span multiple buffered lines (the title may sit on a continuation line), so
    /// this works on the whole buffer rather than line-by-line. Parsing stops at the first position
    /// that does not begin a valid definition; everything from there on is paragraph text.
    fn consume_refdefs(&mut self, text: &str) -> String {
        let b = text.as_bytes();
        let mut pos = 0;
        loop {
            // A definition must start at a block-start position with ≤3 leading spaces.
            let line_start = pos;
            let mut p = pos;
            let mut spaces = 0;
            while p < b.len() && b[p] == b' ' {
                spaces += 1;
                p += 1;
            }
            if spaces > 3 {
                break;
            }
            match parse_refdef(b, p) {
                Some((label, def, next)) => {
                    if let Some(norm) = linkref::normalize_label(&label) {
                        self.refs.entry(norm).or_insert(def);
                        pos = next;
                    } else {
                        break;
                    }
                }
                None => {
                    pos = line_start;
                    break;
                }
            }
        }
        // `pos` now sits at a line boundary (each consumed definition ended past its newline), so the
        // remaining text is exactly the paragraph body.
        text[pos..].to_string()
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
            inline::parse(cell.trim(), &InlineStyle::default(), &self.refs, out);
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

/// Try to parse a single link reference definition `[label]: dest "title"` starting at byte `i` in
/// `b`. Returns `(raw_label, definition, next)` where `next` is the offset just past the definition,
/// or `None` if no valid definition starts here.
///
/// The destination may sit on the line after the label, and the title on the line after the
/// destination; the title is optional and, if a title-like token fails to parse, the definition ends
/// at the destination (the title line becomes ordinary paragraph text). A definition must be
/// followed by end-of-input or a line break — trailing non-whitespace on the destination's line
/// (with no valid title) makes the whole thing not a definition.
fn parse_refdef(b: &[u8], i: usize) -> Option<(String, LinkDef, usize)> {
    if b.get(i) != Some(&b'[') {
        return None;
    }
    // Label: up to the matching `]`, honouring backslash escapes; may not contain an unescaped `]`
    // and may not be empty, and is limited to 999 characters by the spec (corpus stays well under).
    let mut j = i + 1;
    let mut label = String::new();
    loop {
        match b.get(j) {
            Some(b'\\') if b.get(j + 1).is_some_and(|c| c.is_ascii_punctuation()) => {
                label.push('\\');
                label.push(b[j + 1] as char);
                j += 2;
            }
            Some(b']') => break,
            Some(b'[') => return None, // an unescaped `[` inside the label is invalid
            Some(&c) if c < 0x80 => {
                label.push(c as char);
                j += 1;
            }
            Some(_) => {
                // Multi-byte UTF-8 char.
                let s = String::from_utf8_lossy(&b[j..]);
                let ch = s.chars().next()?;
                label.push(ch);
                j += ch.len_utf8();
            }
            None => return None,
        }
    }
    // Require `]:`.
    if b.get(j) != Some(&b']') || b.get(j + 1) != Some(&b':') {
        return None;
    }
    j += 2;

    // Whitespace before the destination, spanning at most one line break.
    j = skip_inline_ws_to_one_newline(b, j)?;

    // Destination (required).
    let (raw_dest, after_dest) = linkref::parse_destination(b, j)?;
    j = after_dest;

    // Whitespace between destination and an optional title. A title is only valid if separated from
    // the destination by whitespace; if the destination is followed immediately by other content the
    // definition is invalid.
    let (title_ws, ws_newlines) = scan_ws(b, j);
    let after_ws = title_ws;

    // Attempt a title only if whitespace followed the destination and the title sits on this or the
    // next line (≤1 line break between dest and title).
    let dest_line_end = line_end(b, j);
    let mut def_title = String::new();
    let end;

    if after_ws > j && ws_newlines <= 1 {
        if let Some((raw_title, after_title)) = linkref::parse_title(b, after_ws) {
            // After the title only whitespace may remain on the line.
            let rest = skip_spaces(b, after_title);
            if rest >= b.len() || b[rest] == b'\n' {
                def_title = linkref::normalize_title(&raw_title);
                // Consume the line-ending newline so the next definition starts at a line boundary.
                end = if rest < b.len() { rest + 1 } else { rest };
            } else {
                // A title that is not alone on its line is invalid → the definition stops at the
                // destination, *iff* the destination itself ends a line cleanly.
                end = dest_line_end?;
            }
        } else {
            end = dest_line_end?;
        }
    } else {
        // No title: the destination must be alone on its line.
        end = dest_line_end?;
    }

    Some((
        label,
        LinkDef {
            dest: linkref::normalize_dest(&raw_dest),
            title: def_title,
        },
        end,
    ))
}

/// Skip spaces/tabs and at most one line break, returning the offset, or `None` if a *second* line
/// break is hit (a blank line ends the definition's whitespace run).
fn skip_inline_ws_to_one_newline(b: &[u8], mut i: usize) -> Option<usize> {
    let mut newlines = 0;
    while i < b.len() {
        match b[i] {
            b' ' | b'\t' | b'\r' => i += 1,
            b'\n' => {
                newlines += 1;
                if newlines > 1 {
                    return None;
                }
                i += 1;
            }
            _ => break,
        }
    }
    Some(i)
}

/// Skip spaces/tabs (not newlines) starting at `i`.
fn skip_spaces(b: &[u8], mut i: usize) -> usize {
    while i < b.len() && matches!(b[i], b' ' | b'\t' | b'\r') {
        i += 1;
    }
    i
}

/// Scan whitespace starting at `i`, returning `(end_offset, newline_count)`.
fn scan_ws(b: &[u8], mut i: usize) -> (usize, usize) {
    let mut nl = 0;
    while i < b.len() {
        match b[i] {
            b' ' | b'\t' | b'\r' => i += 1,
            b'\n' => {
                nl += 1;
                i += 1;
            }
            _ => break,
        }
    }
    (i, nl)
}

/// The offset just past the end of the current line (after the `\n`) starting from `i`, but only if
/// everything between `i` and the line end is whitespace. Returns `None` otherwise.
fn line_end(b: &[u8], i: usize) -> Option<usize> {
    let mut j = i;
    while j < b.len() {
        match b[j] {
            b' ' | b'\t' | b'\r' => j += 1,
            b'\n' => return Some(j + 1),
            _ => return None,
        }
    }
    Some(j)
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

/// HTML block tag names for start condition 6 (a `<` or `</` followed by one of these, then
/// whitespace / `>` / `/>` / end of line). Lower-cased; matching is case-insensitive.
const HTML_BLOCK_TAGS: &[&str] = &[
    "address",
    "article",
    "aside",
    "base",
    "basefont",
    "blockquote",
    "body",
    "caption",
    "center",
    "col",
    "colgroup",
    "dd",
    "details",
    "dialog",
    "dir",
    "div",
    "dl",
    "dt",
    "fieldset",
    "figcaption",
    "figure",
    "footer",
    "form",
    "frame",
    "frameset",
    "h1",
    "h2",
    "h3",
    "h4",
    "h5",
    "h6",
    "head",
    "header",
    "hr",
    "html",
    "iframe",
    "legend",
    "li",
    "link",
    "main",
    "menu",
    "menuitem",
    "nav",
    "noframes",
    "ol",
    "optgroup",
    "option",
    "p",
    "param",
    "search",
    "section",
    "summary",
    "table",
    "tbody",
    "td",
    "tfoot",
    "th",
    "thead",
    "title",
    "tr",
    "track",
    "ul",
];

/// Does `haystack` contain `needle` (ASCII case-insensitive)? Used for the marker-based HTML block
/// end conditions (`</script>`, `-->`, `?>`, `>`, `]]>`).
fn contains_ci(haystack: &str, needle: &str) -> bool {
    if needle.is_empty() {
        return true;
    }
    let h = haystack.as_bytes();
    let n = needle.as_bytes();
    if h.len() < n.len() {
        return false;
    }
    (0..=h.len() - n.len()).any(|i| {
        h[i..i + n.len()]
            .iter()
            .zip(n)
            .all(|(a, b)| a.eq_ignore_ascii_case(b))
    })
}

/// Check whether `line` (already stripped of ≤3 leading spaces via `trim_start`) begins an HTML
/// block. Returns the end condition for the opened block, or `None`. `in_paragraph` indicates an open
/// paragraph: conditions 1–6 may interrupt it, but condition 7 may not (so it is suppressed then).
///
/// Conditions follow the CommonMark spec ordering (1 is tried before 2, …): the first matching start
/// wins, which matters because, e.g. `<!--` (condition 2) must beat the generic `<!` (condition 4).
fn html_block_start(line: &str, in_paragraph: bool) -> Option<HtmlEnd> {
    let b = line.as_bytes();
    if b.first() != Some(&b'<') {
        return None;
    }

    // Condition 1: <script | <pre | <style | <textarea, then whitespace / `>` / EOL.
    for (tag, close) in [
        ("script", "</script>"),
        ("pre", "</pre>"),
        ("style", "</style>"),
        ("textarea", "</textarea>"),
    ] {
        if starts_tag_ci(line, tag) {
            let after = &line[1 + tag.len()..];
            if after.is_empty() || after.starts_with([' ', '\t', '>']) {
                return Some(HtmlEnd::Marker(close));
            }
        }
    }

    // Condition 2: <!-- … -->.
    if line.starts_with("<!--") {
        return Some(HtmlEnd::Marker("-->"));
    }

    // Condition 3: <? … ?>.
    if line.starts_with("<?") {
        return Some(HtmlEnd::Marker("?>"));
    }

    // Condition 5: <![CDATA[ … ]]>. Checked before condition 4 since both begin `<!`.
    if line.starts_with("<![CDATA[") {
        return Some(HtmlEnd::Marker("]]>"));
    }

    // Condition 4: <! followed by an ASCII letter, ending at the next `>`.
    if b.get(1) == Some(&b'!') && b.get(2).is_some_and(|c| c.is_ascii_alphabetic()) {
        return Some(HtmlEnd::Marker(">"));
    }

    // Condition 6: <tag | </tag for a known block tag, then whitespace / `>` / `/>` / EOL.
    let (rest, _closing) = match b.get(1) {
        Some(b'/') => (&line[2..], true),
        _ => (&line[1..], false),
    };
    for tag in HTML_BLOCK_TAGS {
        if starts_word_ci(rest, tag) {
            let after = &rest[tag.len()..];
            if after.is_empty() || after.starts_with([' ', '\t', '>']) || after.starts_with("/>") {
                return Some(HtmlEnd::Blank);
            }
        }
    }

    // Condition 7: a complete open or closing tag (any name except the condition-1 tags), with only
    // whitespace after it to end of line. This one may not interrupt a paragraph.
    if !in_paragraph {
        if let Some(after) = complete_tag(line) {
            if after.trim().is_empty() {
                return Some(HtmlEnd::Blank);
            }
        }
    }

    None
}

/// Does `line` start with `<tag` (ASCII case-insensitive) for condition 1?
fn starts_tag_ci(line: &str, tag: &str) -> bool {
    let b = line.as_bytes();
    b.first() == Some(&b'<') && starts_word_ci(&line[1..], tag)
}

/// Does `s` begin with `word` (ASCII case-insensitive)?
fn starts_word_ci(s: &str, word: &str) -> bool {
    let b = s.as_bytes();
    let w = word.as_bytes();
    b.len() >= w.len()
        && b[..w.len()]
            .iter()
            .zip(w)
            .all(|(a, c)| a.eq_ignore_ascii_case(c))
}

/// Parse a complete HTML open or closing tag at the start of `line` for start condition 7, returning
/// the slice *after* the tag, or `None`. The tag name may be any except script/style/pre/textarea
/// (those are handled by condition 1). This is the block-level cousin of inline raw HTML and reuses
/// the same open/close-tag grammar.
fn complete_tag(line: &str) -> Option<&str> {
    let b = line.as_bytes();
    let end = if b.get(1) == Some(&b'/') {
        crate::inline::scan_closing_tag(b, 0)?
    } else {
        let (e, name) = crate::inline::scan_open_tag(b, 0)?;
        // Condition 1 owns these names.
        let lname = name.to_ascii_lowercase();
        if matches!(lname.as_str(), "script" | "style" | "pre" | "textarea") {
            return None;
        }
        e
    };
    Some(&line[end..])
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
