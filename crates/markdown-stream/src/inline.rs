//! Inline parser: turns a run of text (a paragraph or heading's content) into styled `Text` events.
//!
//! Pragmatic cut covering the common constructs — code spans, strong/emphasis, strikethrough, links,
//! images, autolinks, backslash escapes, and hard/soft breaks. The full CommonMark delimiter run
//! algorithm (rule-of-three, exact precedence) is a later milestone; this renders real-world Markdown
//! faithfully for the common cases.

use crate::event::{Event, InlineStyle, Link, Span};

/// Parse the inline content of `text` (already joined with `\n` for multi-line paragraphs) under the
/// given base `style`, appending events to `out`.
pub fn parse(text: &str, style: &InlineStyle, out: &mut Vec<Event>) {
    let mut plain = String::new();
    emit(text, style, &mut plain, out);
    flush_plain(&mut plain, style, out);
}

fn flush_plain(plain: &mut String, style: &InlineStyle, out: &mut Vec<Event>) {
    if !plain.is_empty() {
        out.push(Event::Text {
            text: std::mem::take(plain),
            style: style.clone(),
            span: Span::default(),
        });
    }
}

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

fn emit(text: &str, style: &InlineStyle, plain: &mut String, out: &mut Vec<Event>) {
    let b = text.as_bytes();
    let mut i = 0;
    while i < b.len() {
        let c = b[i];
        match c {
            // Backslash escape.
            b'\\' if i + 1 < b.len() && is_ascii_punct(b[i + 1]) => {
                plain.push(b[i + 1] as char);
                i += 2;
            }
            // Hard line break: backslash before newline.
            b'\\' if i + 1 < b.len() && b[i + 1] == b'\n' => {
                flush_plain(plain, style, out);
                out.push(Event::LineBreak);
                i += 2;
            }
            // Soft / hard break at a newline.
            b'\n' => {
                let hard = plain.ends_with("  ");
                while plain.ends_with(' ') {
                    plain.pop();
                }
                flush_plain(plain, style, out);
                out.push(if hard {
                    Event::LineBreak
                } else {
                    Event::SoftBreak
                });
                i += 1;
                // skip leading spaces on the continuation line
                while i < b.len() && (b[i] == b' ' || b[i] == b'\t') {
                    i += 1;
                }
            }
            // Code span.
            b'`' => {
                let n = run_len(b, i, b'`');
                if let Some(close) = find_code_close(b, i + n, n) {
                    flush_plain(plain, style, out);
                    let mut st = style.clone();
                    st.code = true;
                    out.push(Event::Text {
                        text: code_span_text(&text[i + n..close]),
                        style: st,
                        span: Span::default(),
                    });
                    i = close + n;
                } else {
                    plain.push('`');
                    i += 1;
                }
            }
            // Strong / emphasis.
            b'*' | b'_' => {
                let n = run_len(b, i, c);
                let take = if n >= 2 { 2 } else { 1 };
                if let Some(close) = find_delim_close(b, i + n, c, take) {
                    flush_plain(plain, style, out);
                    let mut st = style.clone();
                    if take == 2 {
                        st.strong = true;
                    } else {
                        st.emphasis = true;
                    }
                    let inner = &text[i + take..close];
                    let mut inner_plain = String::new();
                    emit(inner, &st, &mut inner_plain, out);
                    flush_plain(&mut inner_plain, &st, out);
                    i = close + take;
                } else {
                    plain.push(c as char);
                    i += 1;
                }
            }
            // Strikethrough (GFM).
            b'~' if run_len(b, i, b'~') >= 2 => {
                if let Some(close) = find_delim_close(b, i + 2, b'~', 2) {
                    flush_plain(plain, style, out);
                    let mut st = style.clone();
                    st.strikethrough = true;
                    let inner = &text[i + 2..close];
                    let mut inner_plain = String::new();
                    emit(inner, &st, &mut inner_plain, out);
                    flush_plain(&mut inner_plain, &st, out);
                    i = close + 2;
                } else {
                    plain.push('~');
                    i += 1;
                }
            }
            // Image / link.
            b'!' if i + 1 < b.len() && b[i + 1] == b'[' => {
                if let Some(consumed) = try_link(text, i + 1, style, plain, out, true) {
                    i += 1 + consumed;
                } else {
                    plain.push('!');
                    i += 1;
                }
            }
            b'[' => {
                if let Some(consumed) = try_link(text, i, style, plain, out, false) {
                    i += consumed;
                } else {
                    plain.push('[');
                    i += 1;
                }
            }
            // Autolink <https://…>.
            b'<' => {
                if let Some((end, url)) = try_autolink(text, i) {
                    flush_plain(plain, style, out);
                    let mut st = style.clone();
                    st.link = Some(Link {
                        href: url.to_string(),
                        title: String::new(),
                        image: false,
                    });
                    out.push(Event::Text {
                        text: url.to_string(),
                        style: st,
                        span: Span::default(),
                    });
                    i = end;
                } else {
                    plain.push('<');
                    i += 1;
                }
            }
            _ => {
                // Copy one UTF-8 character.
                let ch_len = utf8_len(c);
                plain.push_str(&text[i..i + ch_len]);
                i += ch_len;
            }
        }
    }
}

fn utf8_len(b: u8) -> usize {
    match b {
        0x00..=0x7f => 1,
        0xc0..=0xdf => 2,
        0xe0..=0xef => 3,
        _ => 4,
    }
}

/// Find the closing run of exactly-or-more backticks of length `n` starting at `from`.
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

/// Find a closing delimiter run of `c` of length `take` (or more), not at the very start.
fn find_delim_close(b: &[u8], from: usize, c: u8, take: usize) -> Option<usize> {
    let mut i = from;
    while i < b.len() {
        if b[i] == c {
            let run = run_len(b, i, c);
            if run >= take {
                return Some(i);
            }
            i += run;
        } else if b[i] == b'`' {
            // skip over code spans so `*` inside code isn't matched
            let n = run_len(b, i, b'`');
            if let Some(close) = find_code_close(b, i + n, n) {
                i = close + n;
            } else {
                i += n;
            }
        } else {
            i += 1;
        }
    }
    None
}

/// Try to parse a link `[text](href "title")` (or image when `image`). `open` indexes the `[`.
/// Returns bytes consumed from `open`.
fn try_link(
    text: &str,
    open: usize,
    style: &InlineStyle,
    plain: &mut String,
    out: &mut Vec<Event>,
    image: bool,
) -> Option<usize> {
    let b = text.as_bytes();
    let close = matching_bracket(b, open)?;
    if close + 1 >= b.len() || b[close + 1] != b'(' {
        return None;
    }
    let paren_close = find_byte(b, close + 2, b')')?;
    let inside = &text[close + 2..paren_close];
    let (href, title) = split_dest_title(inside);
    flush_plain(plain, style, out);
    let mut st = style.clone();
    st.link = Some(Link {
        href: href.to_string(),
        title: title.to_string(),
        image,
    });
    let label = &text[open + 1..close];
    let mut inner_plain = String::new();
    emit(label, &st, &mut inner_plain, out);
    flush_plain(&mut inner_plain, &st, out);
    Some(paren_close + 1 - open)
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
