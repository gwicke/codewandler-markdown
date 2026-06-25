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

use crate::event::{Event, Inline, InlineStyle, Link, Span};

/// Parse the inline content of `text` (already joined with `\n` for multi-line paragraphs) under the
/// given base `style`, appending events to `out`.
pub fn parse(text: &str, style: &InlineStyle, out: &mut Vec<Event>) {
    let mut tokens = scan(text);
    process_emphasis(&mut tokens, 0);
    flatten(&tokens, style, out);
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
    /// An autolink — rendered as a link whose text equals its destination.
    Autolink { href: String },
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
fn scan(text: &str) -> Vec<Token> {
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
            // Code span (longest-match backticks).
            b'`' => {
                let n = run_len(b, i, b'`');
                if let Some(close) = find_code_close(b, i + n, n) {
                    flush!();
                    tokens.push(Token::Code(code_span_text(&text[i + n..close])));
                    i = close + n;
                } else {
                    buf.push('`');
                    i += 1;
                }
            }
            // Image `![alt](dest)`.
            b'!' if i + 1 < b.len() && b[i + 1] == b'[' => {
                if let Some((tok, consumed)) = try_link(text, i + 1, true) {
                    flush!();
                    tokens.push(tok);
                    i += 1 + consumed;
                } else {
                    buf.push('!');
                    i += 1;
                }
            }
            // Link `[text](dest)`.
            b'[' => {
                if let Some((tok, consumed)) = try_link(text, i, false) {
                    flush!();
                    tokens.push(tok);
                    i += consumed;
                } else {
                    buf.push('[');
                    i += 1;
                }
            }
            // Autolink `<scheme:…>` / `<email>`.
            b'<' => {
                if let Some((end, url)) = try_autolink(text, i) {
                    flush!();
                    tokens.push(Token::Autolink {
                        href: url.to_string(),
                    });
                    i = end;
                } else {
                    buf.push('<');
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
            Token::Autolink { href } => {
                let mut st = base.clone();
                st.link = Some(Link {
                    href: href.clone(),
                    title: String::new(),
                    image: false,
                });
                let link = Link {
                    href: href.clone(),
                    title: String::new(),
                    image: false,
                };
                out.push(Event::EnterInline {
                    inline: Inline::Link(link.clone()),
                    span: Span::default(),
                });
                out.push(Event::Text {
                    text: href.clone(),
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

/// Try to parse a link `[text](dest "title")` (or image when `image`). `open` indexes the `[`.
/// Returns the resulting token and the bytes consumed from `open`.
fn try_link(text: &str, open: usize, image: bool) -> Option<(Token, usize)> {
    let b = text.as_bytes();
    let close = matching_bracket(b, open)?;
    if close + 1 >= b.len() || b[close + 1] != b'(' {
        return None;
    }
    let paren_close = find_byte(b, close + 2, b')')?;
    let inside = &text[close + 2..paren_close];
    let (href, title) = split_dest_title(inside);
    let label = &text[open + 1..close];
    let mut inner = scan(label);
    process_emphasis(&mut inner, 0);
    let link = Link {
        href: href.to_string(),
        title: title.to_string(),
        image,
    };
    Some((Token::Link { link, inner }, paren_close + 1 - open))
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

fn split_dest_title(inside: &str) -> (&str, &str) {
    let inside = inside.trim();
    if let Some(q) = inside.find([' ', '\t']) {
        let dest = &inside[..q];
        let title = inside[q..].trim().trim_matches(['"', '\'']);
        (dest, title)
    } else {
        (inside, "")
    }
}

fn try_autolink(text: &str, lt: usize) -> Option<(usize, &str)> {
    let b = text.as_bytes();
    let gt = find_byte(b, lt + 1, b'>')?;
    let inner = &text[lt + 1..gt];
    let has_ws = inner.contains(char::is_whitespace);
    let url_like =
        (inner.starts_with("http://") || inner.starts_with("https://") || inner.contains("://"))
            && !has_ws;
    let email_like = inner.contains('@') && !has_ws && !inner.contains('/');
    if url_like || email_like {
        Some((gt + 1, inner))
    } else {
        None
    }
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
