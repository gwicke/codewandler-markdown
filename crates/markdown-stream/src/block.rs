//! The streaming block parser: turns input lines into block-level events, deferring inline parsing
//! until a paragraph/heading closes.
//!
//! CommonMark block structure with a proper **container stack**: a document holds a stack of open
//! containers (block quotes, lists, list items) each anchored to a continuation *column*, plus at
//! most one open *leaf* (paragraph, fenced/indented code, HTML block, table). Each input line is
//! matched against the open containers by leading indentation (CommonMark "continuation" rules):
//! a block quote continues with `>`, a list item's content continues at the column past its marker.
//! Containers the line no longer belongs to are closed; new containers the line opens are pushed.
//!
//! Lists carry a tight/loose flag that is only knowable once the list closes (a blank line between
//! items makes the list loose). To keep `EnterBlock(List{tight})` correct we **buffer a list's
//! events** into the [`Container::List`] frame and replay them — correctly `<p>`-wrapped or not —
//! when the list closes. The buffer is bounded by the list's own size, preserving bounded-memory
//! streaming, and the looseness decision is a pure function of the (chunk-independent) line sequence,
//! so split-equivalence still holds.

use crate::event::*;
use crate::inline;
use crate::linkref;
use crate::parser::Parser;
use std::collections::HashMap;

/// Expanded width of a tab stop, per CommonMark (tabs count to the next multiple of 4).
const TAB: usize = 4;

#[derive(Default)]
pub struct StreamParser {
    buf: Vec<u8>,
    started: bool,
    flushed: bool,
    /// The open container stack (block quotes / lists / list items), outermost first. The document
    /// itself is implicit (tracked by `started`).
    containers: Vec<Container>,
    /// The single open leaf block, if any (a paragraph, code block, …).
    leaf: Leaf,
    /// Was the previous processed line blank? Used for loose-list detection (a blank line between
    /// two items, or before a second block in an item, makes the enclosing list loose).
    last_blank: bool,
    /// Link reference definitions seen so far, keyed by normalised label. Populated in line order as
    /// paragraphs are scanned; references resolve against the definitions visible at close time.
    refs: HashMap<String, LinkDef>,
}

/// One open container in the stack.
enum Container {
    /// A block quote. Its `>` marker is consumed during the match phase.
    BlockQuote,
    /// A list. Events for the whole list are buffered here until it closes, so the `tight` flag can
    /// be back-patched once looseness is known.
    List(ListFrame),
    /// A single list item. `indent` is the column at which the item's content begins (the marker's
    /// own indent plus its width plus the spaces after it): a continuation line must be indented at
    /// least this far to stay in the item.
    Item { indent: usize },
}

/// A buffered list: its metadata plus the events emitted while it is open, so the final `tight`
/// flag (only known at close) can be applied to all item content retroactively.
struct ListFrame {
    ordered: bool,
    marker: char,
    start: u64,
    /// `true` once any blank line is found that should make the list loose.
    loose: bool,
    /// Buffered events for the list body (everything between `EnterBlock(List)` and
    /// `ExitBlock(List)`, exclusive). Item boundaries are marked so `<p>` wrappers can be inserted.
    events: Vec<BufEvent>,
    /// A blank line has been seen since the last block was added to this list, and no block has been
    /// added since. If another block is then added (a sibling item, or a second block in the current
    /// item), the list is loose. A trailing blank never commits, so it does not make the list loose.
    pending_blank: bool,
}

/// An event buffered inside a list frame. Plain events pass through; `ItemStart`/`ItemEnd` mark item
/// boundaries and `BlockSep` records where a `<p>` wrapper is needed in loose mode.
enum BufEvent {
    /// A raw event to replay verbatim.
    Raw(Event),
    /// Start of a list item's content (after `EnterBlock(ListItem)`).
    ItemStart,
    /// End of a list item's content (before `ExitBlock(ListItem)`).
    ItemEnd,
    /// A run of paragraph inline content (the text events between `<p>`…`</p>`), buffered so that in
    /// a tight list the wrapper is dropped and in a loose list it is kept.
    Para(Vec<Event>),
}

#[derive(Default)]
enum Leaf {
    #[default]
    None,
    Paragraph(String),
    /// An indented code block. Lines are accumulated (already de-indented by 4 columns) and emitted
    /// verbatim at close, with trailing blank lines trimmed.
    Indented(Vec<String>),
    Fenced {
        ch: u8,
        len: usize,
        /// The indentation (in columns) of the opening fence; up to this much leading whitespace is
        /// stripped from each content line.
        indent: usize,
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

/// A list marker parsed from a line: bullet/ordered, its char, start number, and the byte offset of
/// the first content character after the marker (and the spaces following it).
struct Marker {
    ordered: bool,
    marker: char,
    start: u64,
    /// Byte offset (within the de-indented content) just past the marker char and its separator.
    after: usize,
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
        self.close_containers_to(0, &mut out);
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
            self.emit(out, Event::enter(BlockKind::Document));
            self.started = true;
        }
    }

    /// Process one logical line. This is the CommonMark block algorithm in three phases:
    ///   1. **Match** the line against open containers, consuming each container's continuation
    ///      marker and tracking the surviving content offset/column.
    ///   2. Decide whether unmatched containers close (they do, unless a lazy paragraph continuation
    ///      keeps a paragraph alive).
    ///   3. **Parse** new containers and the leaf from the remaining content.
    fn process_line(&mut self, raw: &str, out: &mut Vec<Event>) {
        // Phase 1: walk the container stack, consuming continuation markers.
        let mut cur = Cursor::new(raw);
        let mut matched = 0usize; // number of containers whose continuation matched
        for c in &self.containers {
            match c {
                Container::BlockQuote => {
                    let save = cur.clone();
                    if cur.indent() <= 3 && cur.peek_nonspace() == Some(b'>') {
                        cur.advance_to_nonspace();
                        cur.bump(); // consume '>'
                        if cur.peek() == Some(b' ') {
                            cur.bump();
                        } else if cur.peek() == Some(b'\t') {
                            cur.consume_tab_as_space();
                        }
                        matched += 1;
                    } else {
                        cur = save;
                        break;
                    }
                }
                Container::List(_) => {
                    // A list as such has no continuation marker; its item does.
                    matched += 1;
                }
                Container::Item { indent } => {
                    if cur.is_blank() {
                        // A blank line "matches" any item (it may continue the item with later,
                        // sufficiently-indented content). Stop consuming further markers.
                        matched += 1;
                        // Keep matching outer list frames is moot; break out.
                        break;
                    }
                    if cur.indent() >= *indent {
                        cur.consume_cols(*indent);
                        matched += 1;
                    } else {
                        break;
                    }
                }
            }
        }

        let all_matched = matched == self.containers.len();
        let blank = cur.is_blank();

        // Phase 2 + 3: dispatch. Fenced/HTML/table leaves swallow lines specially.
        self.dispatch(raw, cur, matched, all_matched, blank, out);
        self.last_blank = blank;
    }

    /// The continuation of `process_line` after the container-match phase: handle the open leaf's
    /// special swallowing, blank lines, lazy continuation, new containers, and new leaves.
    fn dispatch(
        &mut self,
        raw: &str,
        mut cur: Cursor,
        matched: usize,
        all_matched: bool,
        blank: bool,
        out: &mut Vec<Event>,
    ) {
        // --- Open fenced code block: literal lines until the closing fence. ---
        if let Leaf::Fenced { ch, len, indent } = self.leaf {
            if all_matched {
                let t = cur.rest_str();
                let tt = t.trim_start();
                if is_closing_fence(tt, ch, len) {
                    self.close_leaf(out);
                } else {
                    // Strip up to `indent` columns of leading whitespace from the content line.
                    let stripped = strip_cols(&t, indent);
                    self.emit(out, Event::text(format!("{stripped}\n")));
                }
                return;
            }
            // The fence's container was interrupted: close the leaf and re-handle the line below.
            self.close_leaf(out);
            self.close_containers_to(matched, out);
        }

        // --- Open HTML block: emit verbatim until the end condition. ---
        if let Leaf::Html { end } = self.leaf {
            if all_matched {
                let content = cur.rest_str();
                match end {
                    HtmlEnd::Marker(marker) => {
                        self.emit(out, Event::text(format!("{content}\n")));
                        if contains_ci(&content, marker) {
                            self.close_leaf(out);
                        }
                        return;
                    }
                    HtmlEnd::Blank => {
                        if content.trim().is_empty() {
                            self.close_leaf(out);
                            // fall through: blank line handled below
                        } else {
                            self.emit(out, Event::text(format!("{content}\n")));
                            return;
                        }
                    }
                }
                if !matches!(self.leaf, Leaf::None) {
                    return;
                }
            } else {
                self.close_leaf(out);
                self.close_containers_to(matched, out);
            }
        }

        // --- Open table: a pipe row continues it; otherwise it closes. ---
        if let Leaf::Table { aligns } = &self.leaf {
            if all_matched && !blank {
                let content = cur.rest_str();
                if content.contains('|') {
                    let aligns = aligns.clone();
                    self.emit_row(split_row(&content), &aligns, out);
                    return;
                }
            }
            self.close_leaf(out);
            if !all_matched {
                self.close_containers_to(matched, out);
            }
        }

        // --- Open indented code block: 4-space continuation, or blank lines (kept). ---
        if let Leaf::Indented(_) = &self.leaf {
            if all_matched && (blank || cur.indent() >= TAB) {
                if blank {
                    if let Leaf::Indented(lines) = &mut self.leaf {
                        lines.push(String::new());
                    }
                    return;
                }
                cur.consume_cols(TAB);
                let line = cur.rest_str_with_partial_tab();
                if let Leaf::Indented(lines) = &mut self.leaf {
                    lines.push(line);
                }
                return;
            }
            self.close_leaf(out);
            if !all_matched {
                self.close_containers_to(matched, out);
            }
        }

        // --- Blank line handling. ---
        if blank {
            // A blank line closes any open paragraph.
            if matches!(self.leaf, Leaf::Paragraph(_)) {
                self.close_leaf(out);
            }
            // Record a pending blank on the innermost open list: if content later resumes in the
            // same item or a sibling item appears (i.e. a new block is added to the list), the list
            // becomes loose. A trailing blank with no following content never commits, so it does
            // not make the list loose.
            self.note_blank_in_item();
            return;
        }

        // Lazy paragraph continuation: a paragraph survives even when outer containers didn't match,
        // *provided* the un-matched remainder is ordinary paragraph text (not a new block start).
        let lazy = !all_matched
            && matches!(self.leaf, Leaf::Paragraph(_))
            && self.can_lazily_continue(&cur);
        if !lazy && !all_matched {
            self.close_leaf(out);
            self.close_containers_to(matched, out);
        }

        // Now open any *new* containers (block quotes / list items) the line introduces, looping so
        // a line like `> - x` or `- - x` opens several at once.
        if !lazy {
            self.open_new_containers(&mut cur, out);
        }

        // If, after matching and opening, the innermost container is a *bare* list (its items all
        // closed and no new item opened), a non-item block is landing at the list's level — so the
        // list ends. E.g. `- a\n\n<!-- -->` closes the list before the HTML block.
        if !lazy && matches!(self.containers.last(), Some(Container::List(_))) {
            let keep = self.containers.len() - 1;
            self.close_containers_to(keep, out);
        }

        // Finally, the remaining content forms (or continues) a leaf.
        self.parse_leaf(&cur, raw, lazy, out);
    }

    /// Open block-quote and list-item containers introduced by the current line, advancing `cur`
    /// past each marker. Loops to handle several markers on one line (`> - x`, `- - x`).
    fn open_new_containers(&mut self, cur: &mut Cursor, out: &mut Vec<Event>) {
        loop {
            // ≥4 columns of leading space is an indented-code amount, not a container marker — it
            // belongs to the leaf. Stop opening containers.
            let indent = cur.indent();
            if indent >= TAB {
                break;
            }

            // Block quote?
            if indent <= 3 && cur.peek_nonspace() == Some(b'>') {
                self.ensure_doc(out);
                self.close_leaf(out);
                // A block quote opening directly inside a list item is a (second) block of that item,
                // so a preceding blank line makes the enclosing list loose.
                if matches!(self.containers.last(), Some(Container::Item { .. })) {
                    self.commit_pending_blank();
                }
                self.emit(out, Event::enter(BlockKind::BlockQuote));
                self.containers.push(Container::BlockQuote);
                cur.advance_to_nonspace();
                cur.bump();
                if cur.peek() == Some(b' ') {
                    cur.bump();
                } else if cur.peek() == Some(b'\t') {
                    cur.consume_tab_as_space();
                }
                continue;
            }

            // List item? A thematic break takes precedence over a bullet marker, so `* * *` and
            // `- - -` are horizontal rules, not one-item lists. (The break is parsed by `parse_leaf`.)
            if indent <= 3 && !is_thematic_break(&cur.rest_after_indent()) {
                if let Some(m) = self.parse_marker(cur) {
                    self.start_list_item(cur, m, out);
                    continue;
                }
            }
            break;
        }
    }

    /// Parse a list marker at the cursor (which sits at ≤3 spaces of indent), validating that it can
    /// start/continue a list here (an ordered marker may only interrupt a paragraph if it starts at
    /// `1`). Returns the marker, leaving `cur` unchanged on failure.
    fn parse_marker(&self, cur: &Cursor) -> Option<Marker> {
        let rest = cur.rest_after_indent();
        let m = list_marker(&rest)?;
        // The interrupt restrictions only apply when a marker would interrupt a *running-text*
        // paragraph (not one already inside a list): a bullet or `1.` may interrupt, but an empty
        // marker (`-` then EOL) or an ordered marker that doesn't start at 1 may not. Inside a list,
        // these markers freely begin sibling items (e.g. `- foo\n-\n- bar`, `2. x` after `1) y`).
        if matches!(self.leaf, Leaf::Paragraph(_)) && !self.in_any_list() {
            let empty = rest[m.after..].trim().is_empty();
            if empty || (m.ordered && m.start != 1) {
                return None;
            }
        }
        Some(m)
    }

    /// Open a list (if needed) and a new item for marker `m`, advancing `cur` past the marker and the
    /// spaces that establish the item's content column.
    fn start_list_item(&mut self, cur: &mut Cursor, m: Marker, out: &mut Vec<Event>) {
        self.ensure_doc(out);
        self.close_leaf(out);

        // The item's content indent is stored *relative to the parent container's content column*
        // (the column the cursor sits at on entry, after outer block-quote/item markers were
        // consumed), because container matching consumes that many additional columns.
        let base_col = cur.col();
        // Advance past the leading indent and the marker characters.
        cur.advance_to_nonspace();
        cur.consume_bytes(m.after);
        let marker_width = cur.col() - base_col; // leading indent + marker chars

        // Spaces after the marker determine the content indent. 1–4 spaces → that many; ≥5 spaces or
        // a tab means only one space counts and the rest is part of an indented code block; an empty
        // marker (then EOL) gives one space of padding.
        let spaces = cur.count_spaces();
        let content_indent;
        if cur.is_blank_from_here() {
            // Empty item: marker immediately followed by end of line.
            content_indent = marker_width + 1;
        } else if (1..=TAB).contains(&spaces) {
            content_indent = marker_width + spaces;
            cur.consume_cols_max(spaces);
        } else {
            // ≥5 spaces (or tab): only one space is the marker padding; the remainder is code.
            content_indent = marker_width + 1;
            cur.consume_cols_max(1);
        }

        // Should this marker extend the current list or start a new one? At this point the deeper
        // items the line did not match have already been closed, so the top container is the list
        // this marker is a sibling of (if any). It extends iff that top container is a List of the
        // *same* kind (ordered-ness + marker char); a *different* marker char at the same level
        // begins a new sibling list, so the old one is closed first.
        let same_list = matches!(self.containers.last(), Some(Container::List(l))
            if l.ordered == m.ordered && l.marker == m.marker);
        let diff_list = matches!(self.containers.last(), Some(Container::List(_))) && !same_list;

        if diff_list {
            // Close the sibling list of the other kind (e.g. `-` items followed by a `+` item).
            let keep = self.containers.len() - 1;
            self.close_containers_to(keep, out);
        }

        if same_list {
            // Continuing the same list: a blank line before this sibling item makes the list loose.
            self.commit_pending_blank();
        } else {
            // Starting a fresh list. If it is nested directly inside a list item, it is a second
            // block of that item, so a blank line preceding it makes the *enclosing* list loose
            // (e.g. `1.  foo\n\n    - bar`).
            if matches!(self.containers.last(), Some(Container::Item { .. })) {
                self.commit_pending_blank();
            }
            let frame = ListFrame {
                ordered: m.ordered,
                marker: m.marker,
                start: m.start,
                loose: false,
                events: Vec::new(),
                pending_blank: false,
            };
            self.containers.push(Container::List(frame));
        }

        self.emit(out, Event::enter(BlockKind::ListItem));
        self.mark_item_start();
        self.containers.push(Container::Item {
            indent: content_indent,
        });
    }

    /// Build (or continue) the leaf from the cursor's remaining content. `lazy` marks a lazy
    /// paragraph continuation (no new containers were opened).
    fn parse_leaf(&mut self, cur: &Cursor, raw: &str, lazy: bool, out: &mut Vec<Event>) {
        let _ = raw;
        let content = cur.rest_str();
        let trimmed = content.trim_start();
        let indent = cur.indent();

        if lazy {
            // Pure paragraph continuation.
            if let Leaf::Paragraph(p) = &mut self.leaf {
                p.push('\n');
                p.push_str(content.trim_end());
            }
            return;
        }

        // A new block is about to be added inside a list item. If no leaf is currently open, this is a
        // *second* block within the item (the first having closed, e.g. across a blank line); commit
        // any pending blank so the enclosing list becomes loose.
        if matches!(self.leaf, Leaf::None)
            && matches!(self.containers.last(), Some(Container::Item { .. }))
        {
            self.commit_pending_blank();
        }

        // A ≥4-column-indented line while a paragraph is open can only continue it (indented code
        // cannot interrupt a paragraph), so append and stop — no block re-parsing.
        if indent >= TAB {
            if let Leaf::Paragraph(p) = &mut self.leaf {
                p.push('\n');
                p.push_str(content.trim_end());
                return;
            }
            // Otherwise it is an indented code block.
            self.ensure_doc(out);
            let mut c = cur.clone();
            c.consume_cols(TAB);
            let line = c.rest_str_with_partial_tab();
            self.leaf = Leaf::Indented(vec![line]);
            return;
        }

        // From here, work with the de-indented content (≤3 spaces stripped).
        let in_paragraph = matches!(self.leaf, Leaf::Paragraph(_));

        // HTML block start.
        if let Some(end) = html_block_start(trimmed, in_paragraph) {
            self.ensure_doc(out);
            self.close_leaf(out);
            self.emit(out, Event::enter(BlockKind::HtmlBlock));
            self.emit(out, Event::text(format!("{trimmed}\n")));
            match end {
                HtmlEnd::Marker(marker) if contains_ci(trimmed, marker) => {
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
            self.emit(
                out,
                Event::EnterBlock {
                    block: BlockKind::FencedCode,
                    data,
                    span: Span::default(),
                },
            );
            self.leaf = Leaf::Fenced { ch, len, indent };
            return;
        }

        // ATX heading.
        if let Some((level, htext)) = atx_heading(trimmed) {
            self.ensure_doc(out);
            self.close_leaf(out);
            let data = BlockData {
                level,
                ..Default::default()
            };
            self.emit(
                out,
                Event::EnterBlock {
                    block: BlockKind::Heading,
                    data,
                    span: Span::default(),
                },
            );
            self.parse_inline(htext, out);
            self.emit(out, Event::exit(BlockKind::Heading));
            return;
        }

        // Setext heading underline: a `=`/`-` run directly under a paragraph turns it into a heading.
        if let Leaf::Paragraph(_) = &self.leaf {
            if let Some(level) = setext_underline(trimmed) {
                if let Leaf::Paragraph(text) = std::mem::take(&mut self.leaf) {
                    let body = self.consume_refdefs(&text);
                    if body.is_empty() {
                        // The paragraph was only refdefs; the underline becomes its own thing —
                        // restore and fall through to thematic-break / paragraph handling.
                        self.leaf = Leaf::None;
                    } else {
                        let data = BlockData {
                            level,
                            ..Default::default()
                        };
                        self.emit(
                            out,
                            Event::EnterBlock {
                                block: BlockKind::Heading,
                                data,
                                span: Span::default(),
                            },
                        );
                        self.parse_inline(body.trim_end(), out);
                        self.emit(out, Event::exit(BlockKind::Heading));
                        return;
                    }
                }
            }
        }

        // Thematic break (checked after setext so `---` under a paragraph is a setext h2).
        if is_thematic_break(trimmed) {
            self.ensure_doc(out);
            self.close_leaf(out);
            self.emit(out, Event::enter(BlockKind::ThematicBreak));
            self.emit(out, Event::exit(BlockKind::ThematicBreak));
            return;
        }

        // Default: paragraph text (new or continuation).
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
                // Blank remaining content (e.g. an empty list marker line `-   `) opens no
                // paragraph; the item simply waits for content on a following line.
                if trimmed.is_empty() {
                    return;
                }
                self.ensure_doc(out);
                self.leaf = Leaf::Paragraph(trimmed.trim_end().to_string());
            }
        }
    }

    /// Can the current line lazily continue an open paragraph? It can unless its remaining content
    /// would start a new block (list/quote/heading/fence/thematic break/html). This is a conservative
    /// check matching the cases the spec forbids from lazy continuation.
    fn can_lazily_continue(&self, cur: &Cursor) -> bool {
        let rest = cur.rest_after_indent();
        let trimmed = rest.trim_start();
        if trimmed.is_empty() {
            return false;
        }
        if cur.indent() >= TAB {
            // Indented enough to be code — but code can't interrupt a paragraph, so it *is* lazy text.
            return true;
        }
        if is_thematic_break(trimmed) {
            return false;
        }
        if atx_heading(trimmed).is_some() {
            return false;
        }
        if fence_start(trimmed).is_some() {
            return false;
        }
        if cur.indent() <= 3 && cur.peek_nonspace() == Some(b'>') {
            return false;
        }
        if html_block_start(trimmed, true).is_some() {
            return false;
        }
        // A list marker interrupts a paragraph only if non-empty and (for ordered) starts at 1 —
        // *unless* a list is already open, in which case any marker (even an empty one, or an ordered
        // marker not starting at 1) starts a sibling item (e.g. `- foo\n-\n- bar`, `2. bar\n3) baz`).
        if let Some(m) = list_marker(trimmed) {
            if self.in_any_list() {
                return false;
            }
            let empty = trimmed[m.after..].trim().is_empty();
            if !(empty || (m.ordered && m.start != 1)) {
                return false;
            }
        }
        true
    }

    // --- list buffering helpers ---------------------------------------------------------------

    /// Emit an event, routing it into the innermost open list's buffer if one is open, else straight
    /// to `out`.
    fn emit(&mut self, out: &mut Vec<Event>, ev: Event) {
        if let Some(frame) = self.innermost_list_mut() {
            frame.events.push(BufEvent::Raw(ev));
        } else {
            out.push(ev);
        }
    }

    /// Emit a buffered run of paragraph inline content (so the `<p>` wrapper can be toggled by
    /// looseness at close time).
    fn emit_para(&mut self, out: &mut Vec<Event>, para: Vec<Event>) {
        if let Some(frame) = self.innermost_list_mut() {
            frame.events.push(BufEvent::Para(para));
        } else {
            // Not in a list: paragraphs are always wrapped.
            out.push(Event::enter(BlockKind::Paragraph));
            out.extend(para);
            out.push(Event::exit(BlockKind::Paragraph));
        }
    }

    fn mark_item_start(&mut self) {
        if let Some(frame) = self.innermost_list_mut() {
            frame.events.push(BufEvent::ItemStart);
        }
    }

    fn mark_item_end(&mut self) {
        if let Some(frame) = self.innermost_list_mut() {
            frame.events.push(BufEvent::ItemEnd);
        }
    }

    /// Record that a blank line occurred while inside a list. Walking outward from the innermost
    /// container, mark each open list with a pending blank — but stop at the first block quote: a
    /// blank line inside a nested block quote belongs to that quote and must not make a list *outside*
    /// the quote loose (the quote "absorbs" the blank). If a later block lands in one of the marked
    /// lists, that list is loose.
    fn note_blank_in_item(&mut self) {
        for c in self.containers.iter_mut().rev() {
            match c {
                Container::List(l) => l.pending_blank = true,
                Container::BlockQuote => break,
                Container::Item { .. } => {}
            }
        }
    }

    /// Commit a pending blank: a new block is being added to the innermost open list, so if a blank
    /// preceded it that list becomes loose. The blank is then "consumed" — its flag is cleared on
    /// *every* open list, so a blank that separates blocks of an inner list does not also make an
    /// enclosing list loose (the enclosing list is only loose if a blank directly precedes one of its
    /// own added blocks).
    fn commit_pending_blank(&mut self) {
        let mut committed = false;
        for c in self.containers.iter_mut().rev() {
            if let Container::List(l) = c {
                if !committed {
                    if l.pending_blank {
                        l.loose = true;
                    }
                    committed = true;
                }
                l.pending_blank = false;
            }
        }
    }

    /// The innermost open list frame (mutable), or `None` if no list is open.
    fn innermost_list_mut(&mut self) -> Option<&mut ListFrame> {
        for c in self.containers.iter_mut().rev() {
            if let Container::List(l) = c {
                return Some(l);
            }
        }
        None
    }

    /// Whether any list is currently open (so a closing item's content gets buffered).
    fn in_any_list(&self) -> bool {
        self.containers
            .iter()
            .any(|c| matches!(c, Container::List(_)))
    }

    /// Whether a closing paragraph is a *direct* child of a list item (the innermost open container
    /// is an `Item`). Only such paragraphs participate in tight/loose `<p>`-stripping; a paragraph
    /// nested under, say, a block quote inside the item is always wrapped.
    fn para_is_direct_list_child(&self) -> bool {
        matches!(self.containers.last(), Some(Container::Item { .. }))
    }

    // --- container closing --------------------------------------------------------------------

    /// Close (and emit) all containers above index `keep`, deepest first.
    fn close_containers_to(&mut self, keep: usize, out: &mut Vec<Event>) {
        while self.containers.len() > keep {
            self.close_leaf(out);
            match self.containers.pop().unwrap() {
                Container::BlockQuote => {
                    self.emit(out, Event::exit(BlockKind::BlockQuote));
                }
                Container::Item { .. } => {
                    self.mark_item_end();
                    self.emit(out, Event::exit(BlockKind::ListItem));
                }
                Container::List(frame) => {
                    self.flush_list(frame, out);
                }
            }
        }
    }

    /// Replay a finished list's buffered events to `out` (or to the enclosing list's buffer if this
    /// list was nested), applying the resolved `tight`/`loose` decision to wrap (or not) each item's
    /// paragraph content in `<p>`.
    fn flush_list(&mut self, frame: ListFrame, out: &mut Vec<Event>) {
        let tight = !frame.loose;
        let data = BlockData {
            list: Some(ListData {
                ordered: frame.ordered,
                start: frame.start,
                tight,
                marker: frame.marker,
            }),
            ..Default::default()
        };

        // Assemble the body. Each item's child events are first collected flat (wrapped paragraphs
        // expanded), then run through `wrap_item_children`, which applies the CommonMark `<li>`
        // newline rules: a `\n` precedes every top-level *block* child of the item, and a `\n`
        // precedes `</li>` iff the item's last child is a block. Inline children (tight, unwrapped
        // paragraph text) get no separators, so a tight inline-only item stays `<li>x</li>`.
        let mut body: Vec<Event> = Vec::new();
        let mut item: Vec<Event> = Vec::new();
        let mut in_item = false;

        for be in frame.events {
            match be {
                BufEvent::ItemStart => {
                    in_item = true;
                    item.clear();
                }
                BufEvent::ItemEnd => {
                    let wrapped = wrap_item_children(std::mem::take(&mut item));
                    body.extend(wrapped);
                    in_item = false;
                }
                BufEvent::Para(inner) => {
                    let target = if in_item { &mut item } else { &mut body };
                    if tight {
                        target.extend(inner);
                    } else {
                        target.push(Event::enter(BlockKind::Paragraph));
                        target.extend(inner);
                        target.push(Event::exit(BlockKind::Paragraph));
                    }
                }
                BufEvent::Raw(ev) => {
                    if in_item {
                        item.push(ev);
                    } else {
                        body.push(ev);
                    }
                }
            }
        }

        // Route the assembled list either to the enclosing list buffer or straight to output.
        if let Some(parent) = self.innermost_list_mut() {
            parent.events.push(BufEvent::Raw(Event::EnterBlock {
                block: BlockKind::List,
                data,
                span: Span::default(),
            }));
            for ev in body {
                parent.events.push(BufEvent::Raw(ev));
            }
            parent
                .events
                .push(BufEvent::Raw(Event::exit(BlockKind::List)));
        } else {
            out.push(Event::EnterBlock {
                block: BlockKind::List,
                data,
                span: Span::default(),
            });
            out.extend(body);
            out.push(Event::exit(BlockKind::List));
        }
    }

    // --- leaf closing -------------------------------------------------------------------------

    fn close_leaf(&mut self, out: &mut Vec<Event>) {
        match std::mem::take(&mut self.leaf) {
            Leaf::None => {}
            Leaf::Paragraph(text) => {
                let body = self.consume_refdefs(&text);
                if body.is_empty() {
                    return;
                }
                let mut inner = Vec::new();
                inline::parse(&body, &InlineStyle::default(), &self.refs, &mut inner);
                if self.para_is_direct_list_child() {
                    // A direct child of a list item: buffer as a `Para` run so the list's looseness
                    // can decide on the `<p>` wrapper later (tight → no wrapper).
                    self.emit_para(out, inner);
                } else if self.in_any_list() {
                    // Inside a list but nested under another block (e.g. a blockquote in the item):
                    // always wrapped, but routed through the list buffer to preserve order.
                    self.emit(out, Event::enter(BlockKind::Paragraph));
                    for ev in inner {
                        self.emit(out, ev);
                    }
                    self.emit(out, Event::exit(BlockKind::Paragraph));
                } else {
                    out.push(Event::enter(BlockKind::Paragraph));
                    out.extend(inner);
                    out.push(Event::exit(BlockKind::Paragraph));
                }
            }
            Leaf::Indented(mut lines) => {
                // Trim trailing blank lines.
                while lines.last().map(|l| l.trim().is_empty()) == Some(true) {
                    lines.pop();
                }
                if lines.is_empty() {
                    return;
                }
                self.emit(out, Event::enter(BlockKind::IndentedCode));
                let mut text = lines.join("\n");
                text.push('\n');
                self.emit(out, Event::text(text));
                self.emit(out, Event::exit(BlockKind::IndentedCode));
            }
            Leaf::Fenced { .. } => {
                self.emit(out, Event::exit(BlockKind::FencedCode));
            }
            Leaf::Table { .. } => {
                self.emit(out, Event::exit(BlockKind::Table));
            }
            Leaf::Html { .. } => {
                self.emit(out, Event::exit(BlockKind::HtmlBlock));
            }
        }
    }

    /// Parse inline content directly to `out` *or* the innermost list buffer.
    fn parse_inline(&mut self, text: &str, out: &mut Vec<Event>) {
        if self.in_any_list() {
            let mut inner = Vec::new();
            inline::parse(text, &InlineStyle::default(), &self.refs, &mut inner);
            for ev in inner {
                self.emit(out, ev);
            }
        } else {
            inline::parse(text, &InlineStyle::default(), &self.refs, out);
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
        text[pos..].to_string()
    }

    fn start_table(&mut self, header: &str, aligns: Vec<Alignment>, out: &mut Vec<Event>) {
        let data = BlockData {
            alignment: aligns.clone(),
            ..Default::default()
        };
        self.emit(
            out,
            Event::EnterBlock {
                block: BlockKind::Table,
                data,
                span: Span::default(),
            },
        );
        self.emit_row(split_row(header), &aligns, out);
        self.leaf = Leaf::Table { aligns };
    }

    fn emit_row(&mut self, mut cells: Vec<String>, aligns: &[Alignment], out: &mut Vec<Event>) {
        cells.resize(aligns.len(), String::new());
        self.emit(out, Event::enter(BlockKind::TableRow));
        for cell in cells {
            self.emit(out, Event::enter(BlockKind::TableCell));
            self.parse_inline(cell.trim(), out);
            self.emit(out, Event::exit(BlockKind::TableCell));
        }
        self.emit(out, Event::exit(BlockKind::TableRow));
    }
}

// ---------------------------------------------------------------------------
// Cursor: a position within the current line tracking byte offset and virtual column (tabs → 4).
// ---------------------------------------------------------------------------

/// A scanning cursor over one input line that tracks both a byte offset and a virtual *column*
/// (with tabs expanded to the next multiple of [`TAB`]). Container matching is column-based, so the
/// cursor lets a tab be partially consumed (e.g. a 4-wide tab where only 2 columns are needed).
#[derive(Clone)]
struct Cursor {
    bytes: Vec<u8>,
    /// Current byte offset.
    pos: usize,
    /// Current virtual column.
    column: usize,
    /// Columns of an in-progress tab already "consumed" (when a tab straddles a needed boundary).
    partial_tab: usize,
}

impl Cursor {
    fn new(line: &str) -> Self {
        Cursor {
            bytes: line.as_bytes().to_vec(),
            pos: 0,
            column: 0,
            partial_tab: 0,
        }
    }

    fn col(&self) -> usize {
        self.column
    }

    fn peek(&self) -> Option<u8> {
        self.bytes.get(self.pos).copied()
    }

    /// The next non-space/tab byte (without advancing).
    fn peek_nonspace(&self) -> Option<u8> {
        let mut i = self.pos;
        while i < self.bytes.len() && matches!(self.bytes[i], b' ' | b'\t') {
            i += 1;
        }
        self.bytes.get(i).copied()
    }

    /// Columns of leading whitespace from the current position to the next non-space. The current
    /// column already reflects any partially-consumed tab, so `TAB - (col % TAB)` yields a tab's
    /// *remaining* width directly.
    fn indent(&self) -> usize {
        let mut col = self.column;
        let mut i = self.pos;
        let start = col;
        while i < self.bytes.len() {
            match self.bytes[i] {
                b' ' => {
                    col += 1;
                    i += 1;
                }
                b'\t' => {
                    col += TAB - (col % TAB);
                    i += 1;
                }
                _ => break,
            }
        }
        col - start
    }

    /// Is the rest of the line blank (only whitespace)?
    fn is_blank(&self) -> bool {
        self.bytes[self.pos..]
            .iter()
            .all(|&b| matches!(b, b' ' | b'\t'))
    }

    fn is_blank_from_here(&self) -> bool {
        self.is_blank()
    }

    /// Advance one byte, updating the column. A tab advances to the next tab stop; if some of its
    /// columns were already consumed (`partial_tab`), `self.column` already reflects them, so the
    /// remaining width is simply `TAB - (column % TAB)`.
    fn bump(&mut self) {
        if let Some(b) = self.peek() {
            match b {
                b'\t' => {
                    self.column += TAB - (self.column % TAB);
                    self.partial_tab = 0;
                }
                _ => self.column += 1,
            }
            self.pos += 1;
        }
    }

    /// Advance past `n` raw bytes (used for ASCII marker characters).
    fn consume_bytes(&mut self, n: usize) {
        for _ in 0..n {
            self.bump();
        }
    }

    /// Skip leading spaces/tabs to the first non-whitespace byte.
    fn advance_to_nonspace(&mut self) {
        while matches!(self.peek(), Some(b' ') | Some(b'\t')) {
            self.bump();
        }
    }

    /// Consume exactly `cols` columns of leading whitespace (splitting a tab if necessary). When a tab
    /// straddles the target column it is consumed partially: the byte is left in place but `column`
    /// (and `partial_tab`) advance, so a later read still sees the tab's remaining columns.
    fn consume_cols(&mut self, cols: usize) {
        let target = self.column + cols;
        while self.column < target {
            match self.peek() {
                Some(b' ') => self.bump(),
                Some(b'\t') => {
                    // Remaining width of the (possibly already partially consumed) tab.
                    let width = TAB - (self.column % TAB);
                    if self.column + width <= target {
                        self.bump();
                    } else {
                        let take = target - self.column;
                        self.partial_tab += take;
                        self.column = target;
                    }
                }
                _ => break,
            }
        }
    }

    /// Consume at most `cols` columns of leading whitespace.
    fn consume_cols_max(&mut self, cols: usize) {
        self.consume_cols(cols);
    }

    /// Count columns of available leading whitespace (alias of [`indent`]).
    fn count_spaces(&self) -> usize {
        self.indent()
    }

    /// A `&str` view of the rest of the line from the byte cursor.
    fn rest_str(&self) -> String {
        String::from_utf8_lossy(&self.bytes[self.pos..]).into_owned()
    }

    /// The rest of the line after skipping the leading indent (≤ whatever spaces are present).
    fn rest_after_indent(&self) -> String {
        let mut i = self.pos;
        while i < self.bytes.len() && matches!(self.bytes[i], b' ' | b'\t') {
            i += 1;
        }
        String::from_utf8_lossy(&self.bytes[i..]).into_owned()
    }

    /// Rest of the line, but if the cursor sits mid-tab (a tab whose leading columns were already
    /// consumed for column alignment), emit the tab's remaining columns as spaces before the rest.
    /// Used for code blocks, where the exact remaining indentation must be preserved verbatim.
    fn rest_str_with_partial_tab(&self) -> String {
        if self.partial_tab > 0 && self.peek() == Some(b'\t') {
            let remaining = TAB - (self.column % TAB);
            let mut s = " ".repeat(remaining);
            s.push_str(&String::from_utf8_lossy(&self.bytes[self.pos + 1..]));
            return s;
        }
        self.rest_str()
    }

    /// Consume a single tab as if it were one space (for blockquote `> \t` padding).
    fn consume_tab_as_space(&mut self) {
        if self.peek() == Some(b'\t') {
            let width = TAB - (self.column % TAB);
            if width <= 1 {
                self.bump();
            } else {
                self.partial_tab += 1;
                self.column += 1;
            }
        }
    }
}

// ---------------------------------------------------------------------------
// line classifiers
// ---------------------------------------------------------------------------

/// Strip up to `cols` columns of leading whitespace from `s`, returning the remainder. Tabs expand
/// to [`TAB`]-column stops; a tab straddling the boundary leaves its remaining columns as spaces.
fn strip_cols(s: &str, cols: usize) -> String {
    let b = s.as_bytes();
    let mut col = 0;
    let mut i = 0;
    while i < b.len() && col < cols {
        match b[i] {
            b' ' => {
                col += 1;
                i += 1;
            }
            b'\t' => {
                let width = TAB - (col % TAB);
                if col + width <= cols {
                    col += width;
                    i += 1;
                } else {
                    // partial tab: keep the leftover as spaces
                    let leftover = (col + width) - cols;
                    let mut out = " ".repeat(leftover);
                    out.push_str(&String::from_utf8_lossy(&b[i + 1..]));
                    return out;
                }
            }
            _ => break,
        }
    }
    String::from_utf8_lossy(&b[i..]).into_owned()
}

/// Block kinds that, as a list item's child, force the CommonMark `<li>` newline layout (a `\n`
/// before the child and, if it is the item's last child, before `</li>`).
fn is_block_enter(ev: &Event) -> bool {
    matches!(
        ev,
        Event::EnterBlock {
            block: BlockKind::List
                | BlockKind::BlockQuote
                | BlockKind::FencedCode
                | BlockKind::IndentedCode
                | BlockKind::Heading
                | BlockKind::ThematicBreak
                | BlockKind::HtmlBlock
                | BlockKind::Table
                | BlockKind::Paragraph,
            ..
        }
    )
}

/// Insert the CommonMark `<li>` separator newlines into a list item's flat child-event stream.
///
/// A `\n` is emitted before a *top-level* block child (one whose `EnterBlock` sits at item depth 0)
/// **only when the previous top-level child was inline** (or this is the first child). Block children
/// already end their own output with a newline (the HTML renderer emits `</p>\n`, `</ul>\n`, …), so a
/// separator between two consecutive blocks would double it; the separator is only needed after the
/// `<li>` itself (leading block child) or after an inline run (`<li>a\n<ul>…`). Inline content (tight,
/// unwrapped paragraph text) needs no separators — a tight inline-only item stays `<li>text</li>`.
fn wrap_item_children(events: Vec<Event>) -> Vec<Event> {
    let mut out = Vec::with_capacity(events.len() + 2);
    let mut depth = 0i32;
    // `prev_block`: was the most recent top-level child a block? Starts `false` so a leading block
    // child gets its `\n` (the `<li>\n…` layout).
    let mut prev_block = false;
    for ev in events {
        match &ev {
            Event::EnterBlock { .. } => {
                if depth == 0 {
                    if is_block_enter(&ev) {
                        if !prev_block {
                            out.push(Event::text("\n"));
                        }
                        prev_block = true;
                    } else {
                        prev_block = false;
                    }
                }
                depth += 1;
                out.push(ev);
            }
            Event::ExitBlock { .. } => {
                depth -= 1;
                out.push(ev);
            }
            _ => {
                if depth == 0 {
                    prev_block = false;
                }
                out.push(ev);
            }
        }
    }
    out
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
    let text = rest.trim().trim_end_matches('#');
    // A closing run of `#` must be preceded by a space (or be the whole tail).
    let text = if text.len() < rest.trim().len() {
        text.trim_end()
    } else {
        text
    };
    Some((hashes as u8, text.trim()))
}

/// A setext underline: a line of only `=` (level 1) or only `-` (level 2), ≤3 leading spaces.
fn setext_underline(line: &str) -> Option<u8> {
    let t = line.trim_end();
    if t.is_empty() {
        return None;
    }
    if t.bytes().all(|b| b == b'=') {
        Some(1)
    } else if t.bytes().all(|b| b == b'-') {
        Some(2)
    } else {
        None
    }
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
    if ch == b'`' && info.contains('`') {
        return None;
    }
    Some((ch, len, info))
}

fn is_closing_fence(line: &str, ch: u8, open_len: usize) -> bool {
    let len = line.bytes().take_while(|&c| c == ch).count();
    len >= open_len && line[len..].trim().is_empty()
}

/// HTML block tag names for start condition 6.
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

fn html_block_start(line: &str, in_paragraph: bool) -> Option<HtmlEnd> {
    let b = line.as_bytes();
    if b.first() != Some(&b'<') {
        return None;
    }

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

    if line.starts_with("<!--") {
        return Some(HtmlEnd::Marker("-->"));
    }
    if line.starts_with("<?") {
        return Some(HtmlEnd::Marker("?>"));
    }
    if line.starts_with("<![CDATA[") {
        return Some(HtmlEnd::Marker("]]>"));
    }
    if b.get(1) == Some(&b'!') && b.get(2).is_some_and(|c| c.is_ascii_alphabetic()) {
        return Some(HtmlEnd::Marker(">"));
    }

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

    if !in_paragraph {
        if let Some(after) = complete_tag(line) {
            if after.trim().is_empty() {
                return Some(HtmlEnd::Blank);
            }
        }
    }

    None
}

fn starts_tag_ci(line: &str, tag: &str) -> bool {
    let b = line.as_bytes();
    b.first() == Some(&b'<') && starts_word_ci(&line[1..], tag)
}

fn starts_word_ci(s: &str, word: &str) -> bool {
    let b = s.as_bytes();
    let w = word.as_bytes();
    b.len() >= w.len()
        && b[..w.len()]
            .iter()
            .zip(w)
            .all(|(a, c)| a.eq_ignore_ascii_case(c))
}

fn complete_tag(line: &str) -> Option<&str> {
    let b = line.as_bytes();
    let end = if b.get(1) == Some(&b'/') {
        crate::inline::scan_closing_tag(b, 0)?
    } else {
        let (e, name) = crate::inline::scan_open_tag(b, 0)?;
        let lname = name.to_ascii_lowercase();
        if matches!(lname.as_str(), "script" | "style" | "pre" | "textarea") {
            return None;
        }
        e
    };
    Some(&line[end..])
}

/// Parse a list marker at the start of `line` (already de-indented). Returns the marker kind, char,
/// start number, and the byte offset just past the marker+separator (before the spaces that follow).
fn list_marker(line: &str) -> Option<Marker> {
    let b = line.as_bytes();
    // Bullet: -, *, + followed by a space/tab or end of line.
    if let Some(&c) = b.first() {
        if c == b'-' || c == b'*' || c == b'+' {
            match b.get(1) {
                Some(b' ') | Some(b'\t') | None => {
                    return Some(Marker {
                        ordered: false,
                        marker: c as char,
                        start: 1,
                        after: 1,
                    });
                }
                _ => {}
            }
        }
    }
    // Ordered: 1–9 digits, then '.' or ')', then a space/tab or EOL.
    let digits = line.bytes().take_while(|c| c.is_ascii_digit()).count();
    if (1..=9).contains(&digits) {
        let sep = b.get(digits).copied();
        if sep == Some(b'.') || sep == Some(b')') {
            match b.get(digits + 1) {
                Some(b' ') | Some(b'\t') | None => {
                    let start: u64 = line[..digits].parse().unwrap_or(1);
                    return Some(Marker {
                        ordered: true,
                        marker: sep.unwrap() as char,
                        start,
                        after: digits + 1,
                    });
                }
                _ => {}
            }
        }
    }
    None
}

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

// ---------------------------------------------------------------------------
// link reference definitions (unchanged from M2b)
// ---------------------------------------------------------------------------

/// Try to parse a single link reference definition `[label]: dest "title"` starting at byte `i`.
fn parse_refdef(b: &[u8], i: usize) -> Option<(String, LinkDef, usize)> {
    if b.get(i) != Some(&b'[') {
        return None;
    }
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
            Some(b'[') => return None,
            Some(&c) if c < 0x80 => {
                label.push(c as char);
                j += 1;
            }
            Some(_) => {
                let s = String::from_utf8_lossy(&b[j..]);
                let ch = s.chars().next()?;
                label.push(ch);
                j += ch.len_utf8();
            }
            None => return None,
        }
    }
    if b.get(j) != Some(&b']') || b.get(j + 1) != Some(&b':') {
        return None;
    }
    j += 2;

    j = skip_inline_ws_to_one_newline(b, j)?;

    let (raw_dest, after_dest) = linkref::parse_destination(b, j)?;
    j = after_dest;

    let (title_ws, ws_newlines) = scan_ws(b, j);
    let after_ws = title_ws;

    let dest_line_end = line_end(b, j);
    let mut def_title = String::new();
    let end;

    if after_ws > j && ws_newlines <= 1 {
        if let Some((raw_title, after_title)) = linkref::parse_title(b, after_ws) {
            let rest = skip_spaces(b, after_title);
            if rest >= b.len() || b[rest] == b'\n' {
                def_title = linkref::normalize_title(&raw_title);
                end = if rest < b.len() { rest + 1 } else { rest };
            } else {
                end = dest_line_end?;
            }
        } else {
            end = dest_line_end?;
        }
    } else {
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

fn skip_spaces(b: &[u8], mut i: usize) -> usize {
    while i < b.len() && matches!(b[i], b' ' | b'\t' | b'\r') {
        i += 1;
    }
    i
}

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
