//! Inline parser: turns a run of text (a paragraph or heading's content) into a stream of inline
//! events — styled `Text`, explicit `EnterInline`/`ExitInline` spans, and hard/soft breaks.
//!
//! This implements the CommonMark *delimiter run* algorithm for emphasis and strong emphasis (plus
//! GFM strikethrough), so nesting like `*a **b** c*` resolves correctly. The pipeline is three
//! passes, matching the spec's structure:
//!
//! 1. [`scan`] — left to right, turn the text into a flat list of [`Token`]s. Non-delimiter
//!    constructs (code spans, links, images, autolinks, escapes, breaks) are resolved here and
//!    become opaque tokens; runs of `*`/`_`/`~` become [`Delim`] tokens carrying their flanking
//!    flags.
//! 2. [`process_emphasis`] — the reference algorithm: walk closers left→right, match each to the
//!    nearest eligible opener (honouring the *rule of three*), and wrap the spanned tokens in an
//!    emphasis/strong/strikethrough node. Leftover delimiters become literal text.
//! 3. [`flatten`] — walk the resolved token tree and emit events, stamping each `Text` with the
//!    cumulative [`InlineStyle`] of the enclosing spans so the flat (terminal) renderer is unchanged.

use crate::entity::decode_entity;
use crate::event::{Event, Inline, InlineStyle, Link, LinkDef, Span};
use crate::linkref;
use std::collections::HashMap;

/// A resolved reference map: normalised label → definition. Threaded through scanning so reference
/// links/images (`[text][label]`, `[label]`, …) can be resolved against the definitions seen so far.
type Refs = HashMap<String, LinkDef>;

/// Parse the inline content of `text` (already joined with `\n` for multi-line paragraphs) under the
/// given base `style`, resolving reference links against `refs`, appending events to `out`. `gfm`
/// enables the GFM extended (bare) autolink syntax.
pub fn parse(text: &str, style: &InlineStyle, refs: &Refs, gfm: bool, out: &mut Vec<Event>) {
    let mut tokens = scan(text, refs, gfm, &mut None);
    process_emphasis(&mut tokens, 0);
    flatten(&tokens, style, out);
}

/// Like [`parse`], but also reports the normalised labels of any reference link/image whose label is
/// **not yet defined** in `refs`. The block parser uses this to detect *forward references* — a
/// `[label]`-shaped construct that would resolve if `label` were defined later in the document — so
/// it can hold the block and re-parse it once all definitions are known. The collected labels are
/// exactly those that, were they to appear in `refs`, would change the output, so the block parser
/// can release a held block the moment none of its labels remain undefinable.
pub fn parse_collect_unresolved(
    text: &str,
    style: &InlineStyle,
    refs: &Refs,
    gfm: bool,
    out: &mut Vec<Event>,
    unresolved: &mut Vec<String>,
) {
    let mut sink = Some(std::mem::take(unresolved));
    let mut tokens = scan(text, refs, gfm, &mut sink);
    process_emphasis(&mut tokens, 0);
    flatten(&tokens, style, out);
    *unresolved = sink.unwrap_or_default();
}

// ---------------------------------------------------------------------------------------------
// Token model
// ---------------------------------------------------------------------------------------------

/// One scanned inline token. Delimiter runs stay live until [`process_emphasis`] resolves them;
/// everything else is opaque.
#[derive(Debug, Clone)]
enum Token {
    /// Literal text (already un-escaped; never contains `\n`).
    Text(String),
    /// A code span: its (already collapsed/trimmed) contents.
    Code(String),
    /// A soft line break (a bare `\n`).
    SoftBreak,
    /// A hard line break (two trailing spaces or a backslash before `\n`).
    HardBreak,
    /// A link or image with its already-parsed inner tokens.
    Link { link: Link, inner: Vec<Token> },
    /// An autolink — a link whose visible text is `text` and whose destination is `href` (the two
    /// differ for an email autolink, where the href gains a `mailto:` prefix, and whenever the
    /// destination needs percent-encoding the text does not).
    Autolink { text: String, href: String },
    /// Inline raw HTML (an open/closing tag, comment, PI, declaration, or CDATA): emitted verbatim,
    /// *unescaped*, by the HTML renderer.
    RawHtml(String),
    /// A run of emphasis delimiters (`*`/`_`/`~`), still unresolved.
    Delim(Delim),
    /// An emphasis / strong / strikethrough node produced by [`process_emphasis`], wrapping the
    /// tokens that fell between its opening and closing delimiters.
    Node { kind: NodeKind, inner: Vec<Token> },
    /// A delimiter run that was matched and consumed: it leaves behind any *unconsumed* characters
    /// as literal text. Empty `text` is dropped during flatten.
    Consumed(String),
}

/// What an emphasis [`Token::Node`] represents.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum NodeKind {
    Emphasis,
    Strong,
    Strikethrough,
}

/// A live delimiter run awaiting matching.
#[derive(Debug, Clone)]
struct Delim {
    /// The delimiter character (`*`, `_`, or `~`).
    ch: u8,
    /// Number of delimiter characters still available to consume.
    len: usize,
    /// Original run length (used for the rule-of-three length classes).
    orig_len: usize,
    can_open: bool,
    can_close: bool,
}

// ---------------------------------------------------------------------------------------------
// Pass 1: scan
// ---------------------------------------------------------------------------------------------

/// Scan `text` left to right into a flat token list, resolving every non-emphasis construct.
/// `refs` resolves reference links/images encountered along the way; `gfm` enables the extended
/// (bare) autolink syntax. When `unresolved` is `Some`, the normalised labels of reference
/// links/images whose label is undefined are recorded into it (for forward-reference detection).
fn scan(text: &str, refs: &Refs, gfm: bool, unresolved: &mut Option<Vec<String>>) -> Vec<Token> {
    let b = text.as_bytes();
    let mut tokens: Vec<Token> = Vec::new();
    let mut buf = String::new();
    let mut i = 0;

    // Push any pending literal text as a single token.
    macro_rules! flush {
        () => {
            if !buf.is_empty() {
                tokens.push(Token::Text(std::mem::take(&mut buf)));
            }
        };
    }

    while i < b.len() {
        let c = b[i];

        // GFM extended autolink (`www.…`, `http(s)://…`, `ftp://…`, bare email). Attempted only in
        // GFM mode, at a valid left boundary, and only *consumes* on a successful match — otherwise
        // we fall through so the byte is handled normally (e.g. `_` stays an emphasis delimiter).
        if gfm && is_gfm_autolink_start(b, i) && gfm_autolink_boundary_ok(text, i) {
            if let Some((tok, end)) = try_gfm_autolink(text, i) {
                flush!();
                tokens.push(tok);
                i = end;
                continue;
            }
        }

        match c {
            // Backslash escape of an ASCII punctuation char → the literal char.
            b'\\' if i + 1 < b.len() && is_ascii_punct(b[i + 1]) => {
                buf.push(b[i + 1] as char);
                i += 2;
            }
            // Hard line break: backslash before newline.
            b'\\' if i + 1 < b.len() && b[i + 1] == b'\n' => {
                flush!();
                tokens.push(Token::HardBreak);
                i += 2;
                i = skip_line_lead(b, i);
            }
            // Soft / hard break at a newline.
            b'\n' => {
                let hard = buf.ends_with("  ");
                while buf.ends_with(' ') {
                    buf.pop();
                }
                flush!();
                tokens.push(if hard {
                    Token::HardBreak
                } else {
                    Token::SoftBreak
                });
                i += 1;
                i = skip_line_lead(b, i);
            }
            // Code span: an opening run of `n` backticks pairs with the next run of *exactly* `n`.
            // When there is no such closing run, the whole opening run is literal and scanning resumes
            // past it (it is not re-tried as a shorter opener) — so e.g. ```` ```foo`` ```` is all text.
            b'`' => {
                let n = run_len(b, i, b'`');
                if let Some(close) = find_code_close(b, i + n, n) {
                    flush!();
                    tokens.push(Token::Code(code_span_text(&text[i + n..close])));
                    i = close + n;
                } else {
                    for _ in 0..n {
                        buf.push('`');
                    }
                    i += n;
                }
            }
            // Image `![alt](dest)` / `![alt][ref]` / `![ref]`.
            b'!' if i + 1 < b.len() && b[i + 1] == b'[' => {
                if let Some((tok, consumed)) = try_link(text, i + 1, true, refs, gfm, unresolved) {
                    flush!();
                    tokens.push(tok);
                    i += 1 + consumed;
                } else {
                    buf.push('!');
                    i += 1;
                }
            }
            // Link `[text](dest)` / `[text][ref]` / `[ref]`.
            b'[' => {
                if let Some((tok, consumed)) = try_link(text, i, false, refs, gfm, unresolved) {
                    flush!();
                    tokens.push(tok);
                    i += consumed;
                } else {
                    buf.push('[');
                    i += 1;
                }
            }
            // Autolink `<scheme:…>` / `<email>`; on failure, inline raw HTML `<tag …>` / `</tag>` /
            // comment / PI / declaration / CDATA.
            b'<' => {
                if let Some((end, tok)) = try_autolink(text, i) {
                    flush!();
                    tokens.push(tok);
                    i = end;
                } else if let Some(end) = try_raw_html(b, i) {
                    flush!();
                    tokens.push(Token::RawHtml(text[i..end].to_string()));
                    i = end;
                } else {
                    buf.push('<');
                    i += 1;
                }
            }
            // Character reference (`&name;`, `&#DDD;`, `&#xHHH;`): decode to its literal character(s)
            // and append them to the text buffer. Because the result lands in `buf` (not re-scanned),
            // a decoded delimiter character (e.g. `&#42;` → `*`) stays literal and never starts a new
            // construct, matching the spec.
            b'&' => {
                if let Some((decoded, len)) = decode_entity(&text[i..]) {
                    buf.push_str(&decoded);
                    i += len;
                } else {
                    buf.push('&');
                    i += 1;
                }
            }
            // Emphasis / strong / strikethrough delimiter run.
            b'*' | b'_' | b'~' => {
                let n = run_len(b, i, c);
                // GFM: only runs of 1 or 2 tildes are strikethrough delimiters; 3+ are literal.
                if c == b'~' && n >= 3 {
                    for _ in 0..n {
                        buf.push('~');
                    }
                    i += n;
                    continue;
                }
                let before = char_before(text, i);
                let after = char_after(text, i + n);
                let (can_open, can_close) = flanking(c, before, after);
                flush!();
                tokens.push(Token::Delim(Delim {
                    ch: c,
                    len: n,
                    orig_len: n,
                    can_open,
                    can_close,
                }));
                i += n;
            }
            _ => {
                let ch_len = utf8_len(c);
                buf.push_str(&text[i..i + ch_len]);
                i += ch_len;
            }
        }
    }
    flush!();
    tokens
}

/// Skip leading spaces/tabs on a continuation line (we re-flow paragraphs, so leading indentation
/// after a break is collapsed exactly as the old scanner did).
fn skip_line_lead(b: &[u8], mut i: usize) -> usize {
    while i < b.len() && (b[i] == b' ' || b[i] == b'\t') {
        i += 1;
    }
    i
}

// ---------------------------------------------------------------------------------------------
// Flanking rules
// ---------------------------------------------------------------------------------------------

/// Compute `(can_open, can_close)` for a delimiter run of char `c` given the character immediately
/// before and after the run (start/end of text treated as whitespace per the spec).
fn flanking(c: u8, before: Option<char>, after: Option<char>) -> (bool, bool) {
    let before_ws = before.is_none_or(is_unicode_whitespace);
    let after_ws = after.is_none_or(is_unicode_whitespace);
    let before_punct = before.is_some_and(is_punct);
    let after_punct = after.is_some_and(is_punct);

    // Left-flanking: not followed by whitespace, and (not followed by punctuation OR (followed by
    // punctuation AND preceded by whitespace or punctuation)).
    let left = !after_ws && (!after_punct || before_ws || before_punct);
    // Right-flanking: not preceded by whitespace, and (not preceded by punctuation OR (preceded by
    // punctuation AND followed by whitespace or punctuation)).
    let right = !before_ws && (!before_punct || after_ws || after_punct);

    match c {
        b'_' => {
            let can_open = left && (!right || before_punct);
            let can_close = right && (!left || after_punct);
            (can_open, can_close)
        }
        // `*` and `~`.
        _ => (left, right),
    }
}

// ---------------------------------------------------------------------------------------------
// Pass 2: process_emphasis (CommonMark reference algorithm)
// ---------------------------------------------------------------------------------------------

/// Resolve the delimiter runs inside `tokens` into emphasis/strong/strikethrough nodes, in place.
/// `stack_bottom` is the index below which closers may not look for openers (the spec's
/// `stack_bottom`); the top-level call passes `0`. Link/image inner content is processed separately
/// during scan, so each call here operates on a single, independent delimiter stack.
///
/// This follows the reference algorithm: walk closers left→right; for each, scan back to the nearest
/// eligible opener of the same character (respecting the rule of three); on a match, wrap the
/// spanned tokens in a [`NodeKind`] node and consume the delimiters. We scan to `stack_bottom` each
/// time rather than threading the `openers_bottom` optimisation — that table only avoids rescanning
/// known-dead prefixes and never changes *which* delimiters pair up, and paragraph runs are short.
fn process_emphasis(tokens: &mut Vec<Token>, stack_bottom: usize) {
    let mut closer = stack_bottom;
    while closer < tokens.len() {
        // Only delimiters that can close are candidates.
        let (ch, closer_can_open, closer_len, closer_orig) = match &tokens[closer] {
            Token::Delim(d) if d.can_close => (d.ch, d.can_open, d.len, d.orig_len),
            _ => {
                closer += 1;
                continue;
            }
        };

        // Scan back for the nearest matching opener at index ≥ `stack_bottom`.
        let mut opener = closer;
        let mut found = false;
        while opener > stack_bottom {
            opener -= 1;
            if let Token::Delim(d) = &tokens[opener] {
                if d.ch == ch
                    && d.can_open
                    && rule_of_three(d.orig_len, d.can_close, closer_orig, closer_can_open)
                {
                    found = true;
                    break;
                }
            }
        }

        if !found {
            // No opener for this closer. A closer that also cannot open is now dead → literal text.
            if !closer_can_open {
                make_literal(&mut tokens[closer]);
            }
            closer += 1;
            continue;
        }

        // Strong if both runs still have ≥2 chars, else emphasis (1 char each).
        let use_strong = delim_len(&tokens[opener]) >= 2 && closer_len >= 2;
        let take = if use_strong { 2 } else { 1 };
        let kind = if ch == b'~' {
            NodeKind::Strikethrough
        } else if use_strong {
            NodeKind::Strong
        } else {
            NodeKind::Emphasis
        };

        // Pull the wrapped tokens out (any unmatched delimiters between opener and closer are
        // dropped as live delimiters — they become literal text via their leftover form below).
        let mut inner: Vec<Token> = tokens.drain(opener + 1..closer).collect();
        // Surviving live delimiters strictly inside the span can never match now → make literal.
        for t in &mut inner {
            if matches!(t, Token::Delim(_)) {
                make_literal(t);
            }
        }

        // Consume `take` chars from each delimiter (closer shifted to opener+1 after the drain).
        consume_delim(&mut tokens[opener], take);
        consume_delim(&mut tokens[opener + 1], take);

        // Insert the node between opener and closer: [opener][node][closer].
        tokens.insert(opener + 1, Token::Node { kind, inner });
        let closer_idx = opener + 2;

        // If the closer was fully consumed, advance past it; otherwise retry it (it may still match
        // an earlier opener). The loop pointer must land on (or just after) the closer's new index.
        if matches!(&tokens[closer_idx], Token::Consumed(s) if s.is_empty()) {
            closer = closer_idx + 1;
        } else {
            closer = closer_idx;
        }
    }
}

/// The *rule of three*: when the opener `can_close` or the closer `can_open`, a match whose summed
/// original lengths is a multiple of 3 is disallowed unless *both* lengths are individually
/// multiples of 3.
fn rule_of_three(
    opener_len: usize,
    opener_can_close: bool,
    closer_len: usize,
    closer_can_open: bool,
) -> bool {
    if opener_can_close || closer_can_open {
        let sum = opener_len + closer_len;
        if sum.is_multiple_of(3) && !(opener_len.is_multiple_of(3) && closer_len.is_multiple_of(3))
        {
            return false;
        }
    }
    true
}

/// Current remaining length of a delimiter token (0 for non-delimiters).
fn delim_len(t: &Token) -> usize {
    match t {
        Token::Delim(d) => d.len,
        _ => 0,
    }
}

/// Consume `take` characters from a delimiter token. When it reaches 0 it becomes an (empty)
/// `Consumed` placeholder; any leftover characters stay as a live `Delim` with reduced length.
fn consume_delim(t: &mut Token, take: usize) {
    if let Token::Delim(d) = t {
        d.len -= take;
        if d.len == 0 {
            *t = Token::Consumed(String::new());
        }
    }
}

/// Turn a still-live delimiter into literal text (its remaining characters), used when it can never
/// match anything.
fn make_literal(t: &mut Token) {
    if let Token::Delim(d) = t {
        let s: String = std::iter::repeat_n(d.ch as char, d.len).collect();
        *t = Token::Consumed(s);
    }
}

// ---------------------------------------------------------------------------------------------
// Pass 3: flatten to events
// ---------------------------------------------------------------------------------------------

/// Walk the resolved token tree and emit events, stamping each `Text` with the cumulative
/// [`InlineStyle`] of the spans currently open so the flat renderer behaves identically to before.
fn flatten(tokens: &[Token], base: &InlineStyle, out: &mut Vec<Event>) {
    for t in tokens {
        match t {
            Token::Text(s) => push_text(s, base, out),
            Token::Consumed(s) => {
                if !s.is_empty() {
                    push_text(s, base, out);
                }
            }
            Token::Code(s) => {
                let mut st = base.clone();
                st.code = true;
                out.push(Event::EnterInline {
                    inline: Inline::Code,
                    span: Span::default(),
                });
                out.push(Event::Text {
                    text: s.clone(),
                    style: st,
                    span: Span::default(),
                });
                out.push(Event::ExitInline {
                    inline: Inline::Code,
                });
            }
            Token::SoftBreak => out.push(Event::SoftBreak),
            Token::HardBreak => out.push(Event::LineBreak),
            Token::Node { kind, inner } => {
                let mut st = base.clone();
                let inline = match kind {
                    NodeKind::Emphasis => {
                        st.emphasis = true;
                        Inline::Emphasis
                    }
                    NodeKind::Strong => {
                        st.strong = true;
                        Inline::Strong
                    }
                    NodeKind::Strikethrough => {
                        st.strikethrough = true;
                        Inline::Strikethrough
                    }
                };
                out.push(Event::EnterInline {
                    inline: inline.clone(),
                    span: Span::default(),
                });
                flatten(inner, &st, out);
                out.push(Event::ExitInline { inline });
            }
            Token::Link { link, inner } => {
                let mut st = base.clone();
                st.link = Some(link.clone());
                let inline = if link.image {
                    Inline::Image(link.clone())
                } else {
                    Inline::Link(link.clone())
                };
                out.push(Event::EnterInline {
                    inline: inline.clone(),
                    span: Span::default(),
                });
                flatten(inner, &st, out);
                out.push(Event::ExitInline { inline });
            }
            Token::RawHtml(s) => {
                let mut st = base.clone();
                st.raw_html = true;
                out.push(Event::Text {
                    text: s.clone(),
                    style: st,
                    span: Span::default(),
                });
            }
            Token::Autolink { text, href } => {
                let link = Link {
                    href: href.clone(),
                    title: String::new(),
                    image: false,
                };
                let mut st = base.clone();
                st.link = Some(link.clone());
                out.push(Event::EnterInline {
                    inline: Inline::Link(link.clone()),
                    span: Span::default(),
                });
                out.push(Event::Text {
                    text: text.clone(),
                    style: st,
                    span: Span::default(),
                });
                out.push(Event::ExitInline {
                    inline: Inline::Link(link),
                });
            }
            // A live (unmatched) delimiter that survived processing becomes literal text.
            Token::Delim(d) => {
                let s: String = std::iter::repeat_n(d.ch as char, d.len).collect();
                push_text(&s, base, out);
            }
        }
    }
}

/// Emit a `Text` event, merging into the previous one when styles match so adjacent literal runs
/// collapse (keeps the event stream compact and matches the old output shape).
fn push_text(s: &str, style: &InlineStyle, out: &mut Vec<Event>) {
    if s.is_empty() {
        return;
    }
    if let Some(Event::Text {
        text, style: prev, ..
    }) = out.last_mut()
    {
        if prev == style {
            text.push_str(s);
            return;
        }
    }
    out.push(Event::Text {
        text: s.to_string(),
        style: style.clone(),
        span: Span::default(),
    });
}

// ---------------------------------------------------------------------------------------------
// Shared scanning helpers (largely lifted from the previous pragmatic parser)
// ---------------------------------------------------------------------------------------------

fn is_ascii_punct(c: u8) -> bool {
    c.is_ascii_punctuation()
}

fn run_len(b: &[u8], i: usize, c: u8) -> usize {
    let mut n = 0;
    while i + n < b.len() && b[i + n] == c {
        n += 1;
    }
    n
}

fn utf8_len(b: u8) -> usize {
    match b {
        0x00..=0x7f => 1,
        0xc0..=0xdf => 2,
        0xe0..=0xef => 3,
        _ => 4,
    }
}

/// The char immediately before byte offset `i` in `text` (`None` at the start).
fn char_before(text: &str, i: usize) -> Option<char> {
    text[..i].chars().next_back()
}

/// The char at byte offset `i` in `text` (`None` at the end).
fn char_after(text: &str, i: usize) -> Option<char> {
    text[i..].chars().next()
}

/// CommonMark whitespace: Unicode whitespace (covers space, tab, newline, and the Unicode set).
fn is_unicode_whitespace(c: char) -> bool {
    c.is_whitespace()
}

/// CommonMark "punctuation": an ASCII punctuation char, or a Unicode punctuation/symbol char.
fn is_punct(c: char) -> bool {
    if c.is_ascii() {
        return c.is_ascii_punctuation();
    }
    is_unicode_punct(c)
}

/// Find the closing run of exactly-`n` backticks starting at `from`.
fn find_code_close(b: &[u8], from: usize, n: usize) -> Option<usize> {
    let mut i = from;
    while i < b.len() {
        if b[i] == b'`' {
            let run = run_len(b, i, b'`');
            if run == n {
                return Some(i);
            }
            i += run;
        } else {
            i += 1;
        }
    }
    None
}

/// CommonMark code-span text: a single leading+trailing space is stripped if the content isn't all
/// spaces, and interior line breaks collapse to spaces.
fn code_span_text(s: &str) -> String {
    let collapsed: String = s.chars().map(|c| if c == '\n' { ' ' } else { c }).collect();
    if collapsed.len() >= 2
        && collapsed.starts_with(' ')
        && collapsed.ends_with(' ')
        && !collapsed.trim().is_empty()
    {
        collapsed[1..collapsed.len() - 1].to_string()
    } else {
        collapsed
    }
}

/// Try to parse a link or image starting at the `[` indexed by `open` (`image` set for `![…]`).
/// Returns the resulting token and the bytes consumed from `open`. Handles all four forms:
///   * inline `[text](dest "title")`,
///   * full reference `[text][label]`,
///   * collapsed reference `[label][]`, and
///   * shortcut reference `[label]`.
///
/// Inline syntax takes precedence: a `(` immediately after the `]` is tried as an inline
/// destination first, and only on failure do we fall back to reference resolution.
fn try_link(
    text: &str,
    open: usize,
    image: bool,
    refs: &Refs,
    gfm: bool,
    unresolved: &mut Option<Vec<String>>,
) -> Option<(Token, usize)> {
    let b = text.as_bytes();
    let close = matching_bracket(b, open)?;
    let label_raw = &text[open + 1..close];

    // Scan the bracketed content once; reused as the link/image inner tokens. A *link* (not an
    // image) may not contain another link — the inner link binds tighter — so if it does we reject
    // the outer link and let scanning fall through to the literal `[` and re-match the inner link.
    let inner = scan_inner(label_raw, refs, gfm, unresolved);
    let nested_link = contains_link(&inner);
    let make = |link: Link, end: usize| {
        if !image && nested_link {
            None
        } else {
            Some((
                Token::Link {
                    link,
                    inner: inner.clone(),
                },
                end - open,
            ))
        }
    };

    // 1. Inline `[text](dest "title")` — only when a `(` immediately follows the `]`.
    if b.get(close + 1) == Some(&b'(') {
        if let Some((link, end)) = parse_inline_target(text, close + 2, image) {
            return make(link, end);
        }
    }

    // 2. Full reference `[text][label]`.
    if b.get(close + 1) == Some(&b'[') {
        if let Some(ref_close) = find_byte(b, close + 2, b']') {
            let ref_label = &text[close + 2..ref_close];
            if ref_label.is_empty() {
                // Collapsed `[label][]`: the text *is* the label.
                if let Some(link) = resolve_ref(label_raw, image, refs) {
                    return make(link, ref_close + 1);
                }
                note_unresolved(label_raw, unresolved);
            } else if let Some(link) = resolve_ref(ref_label, image, refs) {
                return make(link, ref_close + 1);
            } else {
                note_unresolved(ref_label, unresolved);
            }
            // A full/collapsed reference whose label is undefined is not a link.
            return None;
        }
    }

    // 3. Shortcut reference `[label]` — the text itself is the label, and nothing (no `(`/`[`)
    //    follows. The label may not contain a `]` (guaranteed by `matching_bracket`).
    if let Some(link) = resolve_ref(label_raw, image, refs) {
        return make(link, close + 1);
    }
    // An undefined shortcut reference would resolve if its label were defined later, so record it.
    note_unresolved(label_raw, unresolved);

    None
}

/// Record a reference label that did not resolve (its normalised form), so the block parser can
/// detect that the enclosing block holds a forward reference. A label that cannot normalise (empty /
/// whitespace-only) can never resolve, so it is ignored.
fn note_unresolved(label: &str, unresolved: &mut Option<Vec<String>>) {
    if let Some(sink) = unresolved {
        if let Some(norm) = linkref::normalize_label(label) {
            sink.push(norm);
        }
    }
}

/// Parse and scan a link/image label's inner content (emphasis resolved).
fn scan_inner(
    label: &str,
    refs: &Refs,
    gfm: bool,
    unresolved: &mut Option<Vec<String>>,
) -> Vec<Token> {
    let mut inner = scan(label, refs, gfm, unresolved);
    process_emphasis(&mut inner, 0);
    inner
}

/// Does this token list contain a (non-image) link at any nesting depth? Used to enforce
/// CommonMark's "a link may not contain another link" rule.
fn contains_link(tokens: &[Token]) -> bool {
    tokens.iter().any(|t| match t {
        Token::Link { link, inner } => !link.image || contains_link(inner),
        Token::Node { inner, .. } => contains_link(inner),
        _ => false,
    })
}

/// Resolve a reference `label` against `refs`, returning a [`Link`] if defined.
fn resolve_ref(label: &str, image: bool, refs: &Refs) -> Option<Link> {
    let norm = linkref::normalize_label(label)?;
    let def = refs.get(&norm)?;
    Some(Link {
        href: def.dest.clone(),
        title: def.title.clone(),
        image,
    })
}

/// Parse an inline target `dest "title")` whose opening `(` has already been consumed; `start`
/// indexes the first byte after the `(`. Returns the [`Link`] and the offset just past the `)`.
fn parse_inline_target(text: &str, start: usize, image: bool) -> Option<(Link, usize)> {
    let b = text.as_bytes();
    let mut i = linkref::skip_ws(b, start);

    // Destination (optional → empty href).
    let (raw_dest, after_dest) = if b.get(i) == Some(&b')') {
        (String::new(), i)
    } else {
        linkref::parse_destination(b, i)?
    };
    i = after_dest;

    // Optional title, which must be separated from the destination by whitespace.
    let ws_end = linkref::skip_ws(b, i);
    let (raw_title, after_title) = if ws_end > i && b.get(ws_end).is_some_and(|&c| c != b')') {
        linkref::parse_title(b, ws_end)?
    } else {
        (String::new(), i)
    };
    i = linkref::skip_ws(b, after_title);

    // A closing `)` must follow.
    if b.get(i) != Some(&b')') {
        return None;
    }
    let link = Link {
        href: linkref::normalize_dest(&raw_dest),
        title: linkref::normalize_title(&raw_title),
        image,
    };
    Some((link, i + 1))
}

fn matching_bracket(b: &[u8], open: usize) -> Option<usize> {
    let mut depth = 0i32;
    let mut i = open;
    while i < b.len() {
        match b[i] {
            b'\\' => i += 2,
            b'[' => {
                depth += 1;
                i += 1;
            }
            b']' => {
                depth -= 1;
                if depth == 0 {
                    return Some(i);
                }
                i += 1;
            }
            b'`' => {
                let n = run_len(b, i, b'`');
                i = find_code_close(b, i + n, n).map(|c| c + n).unwrap_or(i + n);
            }
            _ => i += 1,
        }
    }
    None
}

fn find_byte(b: &[u8], from: usize, target: u8) -> Option<usize> {
    (from..b.len()).find(|&i| b[i] == target)
}

/// Try to match inline raw HTML starting at the `<` indexed by `lt`, returning the offset just past
/// the match. Tries, in CommonMark's order: open tag, closing tag, HTML comment, processing
/// instruction, declaration, and CDATA. The matched bytes are emitted verbatim (unescaped).
fn try_raw_html(b: &[u8], lt: usize) -> Option<usize> {
    if b.get(lt) != Some(&b'<') {
        return None;
    }
    if b.get(lt + 1) == Some(&b'/') {
        return scan_closing_tag(b, lt);
    }
    if let Some((end, _)) = scan_open_tag(b, lt) {
        return Some(end);
    }
    // The declaration-family constructs all begin `<!` or `<?`.
    match b.get(lt + 1) {
        Some(b'!') => scan_comment(b, lt)
            .or_else(|| scan_cdata(b, lt))
            .or_else(|| scan_declaration(b, lt)),
        Some(b'?') => scan_processing_instruction(b, lt),
        _ => None,
    }
}

/// Scan an HTML open tag `<name (attr)* /?>` starting at the `<` indexed by `lt`. Returns the offset
/// just past the `>` and the (raw) tag name. Shared with block start condition 7.
pub(crate) fn scan_open_tag(b: &[u8], lt: usize) -> Option<(usize, String)> {
    if b.get(lt) != Some(&b'<') {
        return None;
    }
    let mut i = lt + 1;
    // Tag name: ASCII letter, then letters/digits/`-`.
    let name_start = i;
    if !b.get(i).is_some_and(|c| c.is_ascii_alphabetic()) {
        return None;
    }
    i += 1;
    while b
        .get(i)
        .is_some_and(|&c| c.is_ascii_alphanumeric() || c == b'-')
    {
        i += 1;
    }
    let name = String::from_utf8_lossy(&b[name_start..i]).into_owned();

    // Zero or more attributes, each introduced by whitespace.
    loop {
        let ws_end = skip_html_ws(b, i);
        // An attribute must be preceded by whitespace; stop if none was consumed.
        if let Some(next) = scan_attribute(b, ws_end) {
            if ws_end == i {
                // No whitespace before the attribute → invalid (unless we're at the tag end).
                break;
            }
            i = next;
        } else {
            i = ws_end;
            break;
        }
    }

    let i = skip_html_ws(b, i);
    // Optional self-closing slash, then the required `>`.
    let i = if b.get(i) == Some(&b'/') { i + 1 } else { i };
    if b.get(i) == Some(&b'>') {
        Some((i + 1, name))
    } else {
        None
    }
}

/// Scan an HTML closing tag `</name>` starting at the `<` indexed by `lt`. Returns the offset just
/// past the `>`. Shared with block start condition 7.
pub(crate) fn scan_closing_tag(b: &[u8], lt: usize) -> Option<usize> {
    if b.get(lt) != Some(&b'<') || b.get(lt + 1) != Some(&b'/') {
        return None;
    }
    let mut i = lt + 2;
    if !b.get(i).is_some_and(|c| c.is_ascii_alphabetic()) {
        return None;
    }
    i += 1;
    while b
        .get(i)
        .is_some_and(|&c| c.is_ascii_alphanumeric() || c == b'-')
    {
        i += 1;
    }
    let i = skip_html_ws(b, i);
    if b.get(i) == Some(&b'>') {
        Some(i + 1)
    } else {
        None
    }
}

/// Scan one HTML attribute starting at `i` (the position after the introducing whitespace): a name
/// `[A-Za-z_:][A-Za-z0-9_.:-]*`, optionally followed by `= value`. Returns the offset just past it.
fn scan_attribute(b: &[u8], i: usize) -> Option<usize> {
    // Attribute name.
    if !b
        .get(i)
        .is_some_and(|&c| c.is_ascii_alphabetic() || c == b'_' || c == b':')
    {
        return None;
    }
    let mut j = i + 1;
    while b
        .get(j)
        .is_some_and(|&c| c.is_ascii_alphanumeric() || matches!(c, b'_' | b'.' | b':' | b'-'))
    {
        j += 1;
    }
    // Optional `= value`.
    let eq = skip_html_ws(b, j);
    if b.get(eq) != Some(&b'=') {
        return Some(j);
    }
    let val = skip_html_ws(b, eq + 1);
    let after = scan_attr_value(b, val)?;
    Some(after)
}

/// Scan an attribute value at `i`: double-quoted, single-quoted, or unquoted. Returns the offset
/// just past it.
fn scan_attr_value(b: &[u8], i: usize) -> Option<usize> {
    match b.get(i) {
        Some(&q @ (b'"' | b'\'')) => {
            let mut j = i + 1;
            while let Some(&c) = b.get(j) {
                if c == q {
                    return Some(j + 1);
                }
                j += 1;
            }
            None
        }
        Some(_) => {
            // Unquoted: one or more chars that are not whitespace or one of `"'=<>` and backtick.
            let mut j = i;
            while b.get(j).is_some_and(|&c| {
                !matches!(
                    c,
                    b' ' | b'\t' | b'\r' | b'\n' | b'"' | b'\'' | b'=' | b'<' | b'>' | b'`'
                )
            }) {
                j += 1;
            }
            if j > i {
                Some(j)
            } else {
                None
            }
        }
        None => None,
    }
}

/// Scan an HTML comment starting at the `<` indexed by `lt`: `<!-->`, `<!--->`, or `<!--` text `-->`
/// where text does not start with `>` or `->`, and does not end with `-`. Returns the offset past it.
fn scan_comment(b: &[u8], lt: usize) -> Option<usize> {
    if !b[lt..].starts_with(b"<!--") {
        return None;
    }
    let body = lt + 4;
    // Special-cased empty comments.
    if b[body..].starts_with(b">") {
        return Some(body + 1); // <!-->
    }
    if b[body..].starts_with(b"->") {
        return Some(body + 2); // <!--->
    }
    // Find the first `-->`; the text in between must not end with `-`.
    let mut i = body;
    while i + 3 <= b.len() {
        if &b[i..i + 3] == b"-->" {
            // Text is b[body..i]; it must not end with `-` (i.e. avoid `--->` collapsing the close).
            if i > body && b[i - 1] == b'-' {
                return None;
            }
            return Some(i + 3);
        }
        i += 1;
    }
    None
}

/// Scan an HTML processing instruction `<? … ?>` starting at the `<` indexed by `lt`.
fn scan_processing_instruction(b: &[u8], lt: usize) -> Option<usize> {
    if !b[lt..].starts_with(b"<?") {
        return None;
    }
    let mut i = lt + 2;
    while i + 2 <= b.len() {
        if &b[i..i + 2] == b"?>" {
            return Some(i + 2);
        }
        i += 1;
    }
    None
}

/// Scan an HTML declaration `<!NAME … >` starting at the `<` indexed by `lt`: `<!` then one or more
/// ASCII letters, then any chars, then `>`.
fn scan_declaration(b: &[u8], lt: usize) -> Option<usize> {
    if !b[lt..].starts_with(b"<!") {
        return None;
    }
    let mut i = lt + 2;
    if !b.get(i).is_some_and(|c| c.is_ascii_alphabetic()) {
        return None;
    }
    while b.get(i).is_some_and(|c| c.is_ascii_alphabetic()) {
        i += 1;
    }
    while let Some(&c) = b.get(i) {
        if c == b'>' {
            return Some(i + 1);
        }
        i += 1;
    }
    None
}

/// Scan a CDATA section `<![CDATA[ … ]]>` starting at the `<` indexed by `lt`.
fn scan_cdata(b: &[u8], lt: usize) -> Option<usize> {
    if !b[lt..].starts_with(b"<![CDATA[") {
        return None;
    }
    let mut i = lt + 9;
    while i + 3 <= b.len() {
        if &b[i..i + 3] == b"]]>" {
            return Some(i + 3);
        }
        i += 1;
    }
    None
}

/// Skip HTML whitespace (space, tab, CR, LF) starting at `i`.
fn skip_html_ws(b: &[u8], mut i: usize) -> usize {
    while matches!(b.get(i), Some(b' ' | b'\t' | b'\r' | b'\n')) {
        i += 1;
    }
    i
}

/// Try to parse a CommonMark autolink at the `<` indexed by `lt`. Returns the offset just past the
/// closing `>` and the resulting [`Token::Autolink`]. Two forms:
///
///   * **URI** `<scheme:rest>` — `scheme` is an ASCII letter then 1–31 of letter/digit/`+`/`.`/`-`
///     (2–32 chars total), and `rest` contains no whitespace, no `<`, and no control characters. The
///     destination is the inner text percent-encoded (backslashes are *not* escapes here).
///   * **email** `<addr>` — `addr` matches the HTML5 email production; the destination is
///     `mailto:addr`.
fn try_autolink(text: &str, lt: usize) -> Option<(usize, Token)> {
    let b = text.as_bytes();
    let gt = find_byte(b, lt + 1, b'>')?;
    let inner = &text[lt + 1..gt];
    if is_uri_autolink(inner) {
        Some((
            gt + 1,
            Token::Autolink {
                text: inner.to_string(),
                href: linkref::normalize_autolink(inner),
            },
        ))
    } else if is_email_autolink(inner) {
        Some((
            gt + 1,
            Token::Autolink {
                text: inner.to_string(),
                href: format!("mailto:{inner}"),
            },
        ))
    } else {
        None
    }
}

// ---------------------------------------------------------------------------------------------
// GFM extended autolinks (`www.`, `http(s)://`, `ftp://`, bare email)
// ---------------------------------------------------------------------------------------------

/// A GFM extended autolink may only begin at a valid left boundary: the start of the text, or
/// immediately after whitespace or one of `*`, `_`, `~`, `(`.
fn gfm_autolink_boundary_ok(text: &str, i: usize) -> bool {
    match char_before(text, i) {
        None => true,
        Some(c) => c.is_whitespace() || matches!(c, '*' | '_' | '~' | '('),
    }
}

/// A quick first-byte gate: could a GFM extended autolink start at `i`? (`w` for `www`, `h`/`f` for
/// the URL schemes, or an email local-part character.)
fn is_gfm_autolink_start(b: &[u8], i: usize) -> bool {
    matches!(b[i], b'w' | b'W' | b'h' | b'H' | b'f' | b'F') || is_email_local_char(b[i])
}

/// Try to parse a GFM extended autolink at `i`. Returns the link token and the offset just past it.
fn try_gfm_autolink(text: &str, i: usize) -> Option<(Token, usize)> {
    // URL autolinks take precedence over the bare-email form (a scheme `http://a@b` is a URL).
    if let Some((end, scheme_len)) = gfm_url_extent(text, i) {
        // `www.` autolinks get an implicit `http://` scheme on the href; explicit schemes keep theirs.
        let raw = &text[i..end];
        let href = if scheme_len == 0 {
            format!("http://{}", linkref::normalize_autolink(raw))
        } else {
            linkref::normalize_autolink(raw)
        };
        return Some((
            Token::Autolink {
                text: raw.to_string(),
                href,
            },
            end,
        ));
    }
    // Bare email autolink.
    if let Some(end) = gfm_email_extent(text, i) {
        let raw = &text[i..end];
        return Some((
            Token::Autolink {
                text: raw.to_string(),
                href: format!("mailto:{raw}"),
            },
            end,
        ));
    }
    None
}

/// Find the extent of a GFM URL autolink starting at `i`, returning `(end, scheme_len)` where
/// `scheme_len` is 0 for a `www.` autolink (no explicit scheme) or the byte length of the matched
/// `http://` / `https://` / `ftp://` scheme. Applies the GFM domain rule and trailing-punctuation /
/// paren-balancing / entity trimming.
fn gfm_url_extent(text: &str, i: usize) -> Option<(usize, usize)> {
    let rest = &text[i..];
    let lower = rest.to_ascii_lowercase();
    let scheme_len = if lower.starts_with("http://") {
        7
    } else if lower.starts_with("https://") {
        8
    } else if lower.starts_with("ftp://") {
        6
    } else if lower.starts_with("www.") {
        0
    } else {
        return None;
    };

    let domain_start = scheme_len;
    let b = rest.as_bytes();
    // Scan the whole URL up to whitespace or `<` (the autolink terminators), then trim the GFM
    // trailing characters (punctuation, unbalanced `)`, entity suffix) down to just after the scheme.
    let mut k = domain_start;
    while k < b.len() && !b[k].is_ascii_whitespace() && b[k] != b'<' {
        k += 1;
    }
    let end = trim_gfm_url_tail(rest, domain_start, k);
    if end <= domain_start {
        return None;
    }

    // The domain is everything from the scheme to the first `/` (or the end), and must be valid.
    let after = &rest[domain_start..end];
    let domain = after.split('/').next().unwrap_or(after);
    if !is_valid_gfm_domain(domain) {
        return None;
    }
    Some((i + end, scheme_len))
}

/// Trim a GFM URL autolink's trailing characters: trailing `?!.,:*_~`, unbalanced `)`, and an
/// entity-like `&…;` suffix are not part of the link. `path_start` is where the path begins (so the
/// domain is never trimmed below it); `end` is the provisional end. Returns the trimmed end offset.
fn trim_gfm_url_tail(rest: &str, path_start: usize, mut end: usize) -> usize {
    let b = rest.as_bytes();
    let min = path_start;
    loop {
        let before = end;
        // Trailing punctuation that is never part of the link.
        while end > min
            && matches!(
                b[end - 1],
                b'?' | b'!' | b'.' | b',' | b':' | b'*' | b'_' | b'~'
            )
        {
            end -= 1;
        }
        // Unbalanced trailing `)`: while there are more `)` than `(` in the link, drop a `)`.
        while end > min && b[end - 1] == b')' {
            let opens = rest[..end].bytes().filter(|&c| c == b'(').count();
            let closes = rest[..end].bytes().filter(|&c| c == b')').count();
            if closes > opens {
                end -= 1;
            } else {
                break;
            }
        }
        // A trailing entity reference `&name;` (or `&#...;`) is excluded from the link.
        if end > min && b[end - 1] == b';' {
            if let Some(amp) = rest[..end].rfind('&') {
                if amp >= min {
                    let candidate = &rest[amp..end];
                    if candidate[1..candidate.len() - 1]
                        .bytes()
                        .all(|c| c.is_ascii_alphanumeric() || c == b'#')
                        && candidate.len() > 2
                    {
                        end = amp;
                    }
                }
            }
        }
        if end == before {
            break;
        }
    }
    end
}

/// A GFM autolink domain: at least one dot, each label of letters/digits/`-`/`_`, and the *last two*
/// labels may not contain `_`. Empty labels (a trailing or doubled dot) are rejected.
fn is_valid_gfm_domain(domain: &str) -> bool {
    let labels: Vec<&str> = domain.trim_end_matches('.').split('.').collect();
    if labels.len() < 2 || labels.iter().any(|l| l.is_empty()) {
        return false;
    }
    if labels.iter().any(|l| {
        !l.bytes()
            .all(|c| c.is_ascii_alphanumeric() || c == b'-' || c == b'_')
    }) {
        return false;
    }
    // The last two labels must not contain underscores.
    let n = labels.len();
    !labels[n - 1].contains('_') && !labels[n - 2].contains('_')
}

/// Find the extent of a bare GFM email autolink starting at `i`, or `None`. The boundary already
/// guaranteed a valid left edge; the address runs to the first character outside the email charset.
fn gfm_email_extent(text: &str, i: usize) -> Option<usize> {
    let b = text.as_bytes();
    // Local part: one or more email chars (no `@`).
    let mut j = i;
    while j < b.len() && is_email_local_char(b[j]) {
        j += 1;
    }
    if j == i || b.get(j) != Some(&b'@') {
        return None;
    }
    j += 1;
    // Domain: labels of letters/digits/`-`/`_`/`.`. A trailing `.` is excluded from the autolink
    // (`a.b.` links `a.b`), but a trailing `-`/`_` makes the whole address invalid.
    let dom_start = j;
    while j < b.len() && (b[j].is_ascii_alphanumeric() || matches!(b[j], b'-' | b'_' | b'.')) {
        j += 1;
    }
    // Drop only a trailing `.`.
    while j > dom_start && b[j - 1] == b'.' {
        j -= 1;
    }
    let domain = &text[dom_start..j];
    if domain.is_empty() {
        return None;
    }
    // The GFM email domain: ≥2 dot-separated labels of letters/digits/`-`/`_`, none empty; the final
    // label may not end in `-` or `_`, and `_` is not allowed in the last two labels.
    let labels: Vec<&str> = domain.split('.').collect();
    if labels.len() < 2 || labels.iter().any(|l| l.is_empty()) {
        return None;
    }
    if labels.iter().any(|l| {
        !l.bytes()
            .all(|c| c.is_ascii_alphanumeric() || c == b'-' || c == b'_')
    }) {
        return None;
    }
    let n = labels.len();
    let last = labels[n - 1].as_bytes();
    if matches!(last[last.len() - 1], b'-' | b'_')
        || labels[n - 1].contains('_')
        || labels[n - 2].contains('_')
    {
        return None;
    }
    Some(j)
}

/// A character allowed in the local part of a (bare) GFM email autolink.
fn is_email_local_char(c: u8) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, b'.' | b'-' | b'_' | b'+')
}

/// Does `inner` (the text between `<` and `>`) form a CommonMark URI autolink?
fn is_uri_autolink(inner: &str) -> bool {
    let b = inner.as_bytes();
    // Scheme: a letter, then 1–31 of letter/digit/`+`/`.`/`-`, then a `:`.
    if b.first().is_none_or(|c| !c.is_ascii_alphabetic()) {
        return false;
    }
    let mut i = 1;
    while i < b.len() && (b[i].is_ascii_alphanumeric() || matches!(b[i], b'+' | b'.' | b'-')) {
        i += 1;
    }
    // Scheme length is `i` (chars before the colon); must be 2–32 and followed by `:`.
    if !(2..=32).contains(&i) || b.get(i) != Some(&b':') {
        return false;
    }
    // The remainder may not contain whitespace, `<`, or ASCII control characters.
    inner[i + 1..]
        .bytes()
        .all(|c| c > 0x20 && c != b'<' && c != 0x7f)
}

/// Does `inner` form a CommonMark email autolink? Implements the spec's email production directly.
fn is_email_autolink(inner: &str) -> bool {
    let Some((local, domain)) = inner.split_once('@') else {
        return false;
    };
    if local.is_empty()
        || !local.bytes().all(|c| {
            c.is_ascii_alphanumeric()
                || matches!(
                    c,
                    b'.' | b'!'
                        | b'#'
                        | b'$'
                        | b'%'
                        | b'&'
                        | b'\''
                        | b'*'
                        | b'+'
                        | b'/'
                        | b'='
                        | b'?'
                        | b'^'
                        | b'_'
                        | b'`'
                        | b'{'
                        | b'|'
                        | b'}'
                        | b'~'
                        | b'-'
                )
        })
    {
        return false;
    }
    // Domain: one or more `.`-separated labels of letter/digit/`-`, each not starting or ending with
    // `-` and at most 63 chars.
    if domain.is_empty() {
        return false;
    }
    domain.split('.').all(is_email_label)
}

/// One label of an email autolink's domain: 1–63 of letter/digit/`-`, not edged with `-`.
fn is_email_label(label: &str) -> bool {
    let b = label.as_bytes();
    if b.is_empty() || b.len() > 63 || b[0] == b'-' || b[b.len() - 1] == b'-' {
        return false;
    }
    b.iter().all(|&c| c.is_ascii_alphanumeric() || c == b'-')
}

/// A coarse but corpus-adequate test for non-ASCII Unicode punctuation/symbol characters used by the
/// flanking rules. Covers the ranges the spec corpus exercises (general punctuation, CJK punctuation,
/// fullwidth forms, the Latin-1 punctuation/symbol block) without pulling in a Unicode-tables crate.
fn is_unicode_punct(c: char) -> bool {
    matches!(c,
        '\u{00A1}'..='\u{00BF}'   // Latin-1 punctuation & symbols (¡ … ¿, incl. £ ¥)
        | '\u{2000}'..='\u{206F}' // General Punctuation
        | '\u{20A0}'..='\u{20CF}' // Currency Symbols (€ and friends — category Sc)
        | '\u{2E00}'..='\u{2E7F}' // Supplemental Punctuation
        | '\u{3000}'..='\u{303F}' // CJK Symbols and Punctuation
        | '\u{FF00}'..='\u{FF0F}' // Fullwidth ASCII punctuation (! … /)
        | '\u{FF1A}'..='\u{FF20}'
        | '\u{FF3B}'..='\u{FF40}'
        | '\u{FF5B}'..='\u{FF65}'
    )
}
