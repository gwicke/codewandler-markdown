//! Shared link-syntax primitives: parsing a CommonMark *link destination*, *link title*, and
//! *link label*, plus the destination normalisation (escape/entity resolution + percent-encoding)
//! and label case-folding used when matching reference links.
//!
//! These primitives are used in two places:
//!   * inline `[text](dest "title")` / `![alt](dest "title")` parsing (in [`crate::inline`]), and
//!   * block-level link reference definitions `[label]: dest "title"` (in [`crate::block`]).
//!
//! Keeping them here keeps both sites byte-for-byte consistent with the spec corpus.

// ---------------------------------------------------------------------------------------------
// Link destination
// ---------------------------------------------------------------------------------------------

/// Parse a CommonMark link destination starting at byte `i` in `b`. Returns `(raw_dest, next)` where
/// `raw_dest` is the still-unescaped destination text and `next` is the byte offset just past it, or
/// `None` if no valid destination starts here.
///
/// Two forms:
///   * **Angle-bracketed** `<...>` — anything but unescaped `<`, `>`, or a newline; may be empty.
///   * **Bare** — a run of non-space, non-control characters in which parentheses must balance;
///     it stops at the first unbalanced `)`, at ASCII whitespace, or at a control char.
pub fn parse_destination(b: &[u8], i: usize) -> Option<(String, usize)> {
    if i < b.len() && b[i] == b'<' {
        // Angle-bracketed: scan to the closing '>', honouring backslash escapes; bail on a raw
        // newline or an unescaped '<'.
        let mut j = i + 1;
        let mut raw = String::new();
        while j < b.len() {
            match b[j] {
                b'>' => return Some((raw, j + 1)),
                b'\n' | b'<' => return None,
                b'\\' if j + 1 < b.len() && b[j + 1].is_ascii_punctuation() => {
                    raw.push('\\');
                    raw.push(b[j + 1] as char);
                    j += 2;
                }
                c => {
                    raw.push(c as char);
                    j += 1;
                }
            }
        }
        return None;
    }

    // Bare destination: balanced parentheses, stop at whitespace / control / unbalanced ')'.
    let start = i;
    let mut j = i;
    let mut depth: i32 = 0;
    while j < b.len() {
        let c = b[j];
        match c {
            b'\\' if j + 1 < b.len() && b[j + 1].is_ascii_punctuation() => {
                j += 2;
                continue;
            }
            b'(' => depth += 1,
            b')' => {
                if depth == 0 {
                    break;
                }
                depth -= 1;
            }
            // ASCII space/control characters terminate a bare destination.
            0x00..=0x20 | 0x7f => break,
            _ => {}
        }
        j += 1;
    }
    if j == start || depth != 0 {
        return None;
    }
    Some((String::from_utf8_lossy(&b[start..j]).into_owned(), j))
}

/// Parse a CommonMark link title starting at byte `i` in `b`. Returns `(raw_title, next)` with the
/// still-unescaped title text and the offset just past the closing delimiter, or `None`.
///
/// A title is delimited by `"`…`"`, `'`…`'`, or `(`…`)`; it may span lines and honours backslash
/// escapes. The parenthesised form may not contain an unescaped `(` or `)`.
pub fn parse_title(b: &[u8], i: usize) -> Option<(String, usize)> {
    let open = *b.get(i)?;
    let close = match open {
        b'"' => b'"',
        b'\'' => b'\'',
        b'(' => b')',
        _ => return None,
    };
    let mut j = i + 1;
    let mut raw = String::new();
    while j < b.len() {
        let c = b[j];
        if c == b'\\' && j + 1 < b.len() && b[j + 1].is_ascii_punctuation() {
            raw.push('\\');
            raw.push(b[j + 1] as char);
            j += 2;
            continue;
        }
        if c == close {
            return Some((raw, j + 1));
        }
        // A parenthesised title may not contain an unescaped '('.
        if open == b'(' && c == b'(' {
            return None;
        }
        raw.push(c as char);
        j += 1;
    }
    None
}

/// Skip ASCII whitespace (spaces, tabs, newlines) starting at `i`, returning the new offset.
pub fn skip_ws(b: &[u8], mut i: usize) -> usize {
    while i < b.len() && matches!(b[i], b' ' | b'\t' | b'\n' | b'\r') {
        i += 1;
    }
    i
}

// ---------------------------------------------------------------------------------------------
// Destination normalisation (for HTML href output)
// ---------------------------------------------------------------------------------------------

/// Normalise a raw link destination for use as an `href`/`src`: resolve backslash escapes and
/// entity references to their character value, then percent-encode the bytes CommonMark reserves
/// (everything outside an "unreserved or already-percent-encoded" set is left, control/space/non-
/// ASCII and a few delimiters are encoded). Matches the reference renderer's output.
pub fn normalize_dest(raw: &str) -> String {
    let decoded = unescape_string(raw);
    percent_encode_uri(&decoded)
}

/// Resolve a raw title's backslash escapes and entity references to plain text (no percent-encoding;
/// titles are emitted as escaped attribute text by the HTML renderer).
pub fn normalize_title(raw: &str) -> String {
    unescape_string(raw)
}

/// Resolve every backslash escape (`\<punct>` → `<punct>`) and HTML entity / numeric reference in
/// `s` to its literal value, leaving all other bytes untouched.
pub fn unescape_string(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = String::with_capacity(s.len());
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'\\' if i + 1 < b.len() && b[i + 1].is_ascii_punctuation() => {
                out.push(b[i + 1] as char);
                i += 2;
            }
            b'&' => {
                if let Some((ch, len)) = crate::entity::decode_entity(&s[i..]) {
                    out.push_str(&ch);
                    i += len;
                } else {
                    out.push('&');
                    i += 1;
                }
            }
            c if c < 0x80 => {
                out.push(c as char);
                i += 1;
            }
            _ => {
                // Multi-byte UTF-8: copy the whole char.
                let ch = s[i..].chars().next().unwrap();
                out.push(ch);
                i += ch.len_utf8();
            }
        }
    }
    out
}

/// Percent-encode a URI the way the CommonMark reference renderer does: keep ASCII alphanumerics and
/// a fixed set of "safe" punctuation, leave existing valid `%XX` escapes intact, and `%`-encode
/// every other byte (including all non-ASCII, which is first UTF-8 encoded).
fn percent_encode_uri(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = String::with_capacity(s.len());
    let mut i = 0;
    while i < b.len() {
        let c = b[i];
        if c == b'%'
            && i + 2 < b.len()
            && b[i + 1].is_ascii_hexdigit()
            && b[i + 2].is_ascii_hexdigit()
        {
            // Preserve an existing percent-escape verbatim.
            out.push('%');
            out.push(b[i + 1] as char);
            out.push(b[i + 2] as char);
            i += 3;
        } else if is_uri_safe(c) {
            out.push(c as char);
            i += 1;
        } else {
            out.push_str(&format!("%{c:02X}"));
            i += 1;
        }
    }
    out
}

/// The byte set the reference renderer leaves un-encoded in a URI.
fn is_uri_safe(c: u8) -> bool {
    c.is_ascii_alphanumeric()
        || matches!(
            c,
            b'-' | b'_'
                | b'.'
                | b'~'
                | b'!'
                | b'#'
                | b'$'
                | b'&'
                | b'\''
                | b'('
                | b')'
                | b'*'
                | b'+'
                | b','
                | b'/'
                | b':'
                | b';'
                | b'='
                | b'?'
                | b'@'
        )
}

// ---------------------------------------------------------------------------------------------
// Label normalisation (reference matching)
// ---------------------------------------------------------------------------------------------

/// Normalise a link label for reference matching: strip surrounding whitespace, collapse internal
/// runs of whitespace to a single space, and case-fold (Unicode simple fold approximated by
/// lowercasing). Returns `None` for an empty (or whitespace-only) label, which is invalid.
pub fn normalize_label(label: &str) -> Option<String> {
    let mut out = String::with_capacity(label.len());
    let mut last_ws = false;
    for ch in label.trim().chars() {
        if ch.is_whitespace() {
            if !last_ws {
                out.push(' ');
                last_ws = true;
            }
        } else {
            // Unicode case folding, approximated by the case mappings std exposes (`to_lowercase`).
            // This handles the corpus's ß→ss / Greek-final-sigma cases via uppercase-then-lowercase.
            for folded in ch.to_uppercase().flat_map(|u| u.to_lowercase()) {
                out.push(folded);
            }
            last_ws = false;
        }
    }
    let out = out.trim_end().to_string();
    if out.is_empty() {
        None
    } else {
        Some(out)
    }
}
