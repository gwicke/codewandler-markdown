//! A small, dependency-free syntax highlighter for fenced code blocks.
//!
//! A generic tokenizer that colors strings, line/block comments, numbers, and a set of keywords
//! common across mainstream languages — enough to make code readable without a heavy grammar
//! dependency. A `syntect`-backed highlighter can slot in behind this later for full fidelity.

use crate::theme::Theme;

/// Keywords recognized across several common languages (intentionally a union — a few cross-language
/// false positives are an acceptable trade for zero per-language grammar data).
const KEYWORDS: &[&str] = &[
    // rust
    "fn", "let", "mut", "const", "static", "struct", "enum", "impl", "trait", "pub", "use", "mod",
    "match", "if", "else", "for", "while", "loop", "return", "break", "continue", "async", "await",
    "move", "ref", "where", "type", "as", "dyn", "crate", "super", "Self", "unsafe",
    // python
    "def", "class", "import", "from", "lambda", "with", "yield", "global", "pass", "raise", "try",
    "except", "finally", "elif", "not", "and", "or", "is", "in", "None", "True", "False",
    // js/ts
    "function", "var", "new", "this", "typeof", "instanceof", "null", "undefined", "export",
    "default", "extends", "case", "switch", "throw", "catch", "do", "void", "delete", "interface",
    // go
    "func", "package", "go", "defer", "chan", "map", "range", "select", "nil",
    // java/c-like
    "public", "private", "protected", "final", "abstract", "int", "long", "float", "double",
    "bool", "boolean", "char", "string", "true", "false",
];

/// Highlight a single line of code. Returns the line unchanged when the theme is no-color.
pub fn highlight_line(line: &str, _lang: &str, theme: &Theme) -> String {
    if theme.kw.is_empty() {
        return line.to_string();
    }
    let mut out = String::new();
    let b = line.as_bytes();
    let mut i = 0;
    while i < b.len() {
        let c = b[i];
        // Line comment: `//` or `#`.
        if (c == b'/' && b.get(i + 1) == Some(&b'/')) || c == b'#' {
            out.push_str(theme.comment);
            out.push_str(&line[i..]);
            out.push_str(theme.reset);
            break;
        }
        // String literal.
        if c == b'"' || c == b'\'' || c == b'`' {
            let start = i;
            i += 1;
            while i < b.len() {
                if b[i] == b'\\' {
                    i += 2;
                    continue;
                }
                if b[i] == c {
                    i += 1;
                    break;
                }
                i += 1;
            }
            let end = i.min(line.len());
            out.push_str(theme.str);
            out.push_str(&line[start..end]);
            out.push_str(theme.reset);
            continue;
        }
        // Number.
        if c.is_ascii_digit() {
            let start = i;
            while i < b.len() && (b[i].is_ascii_alphanumeric() || b[i] == b'.' || b[i] == b'_') {
                i += 1;
            }
            out.push_str(theme.num);
            out.push_str(&line[start..i]);
            out.push_str(theme.reset);
            continue;
        }
        // Identifier / keyword.
        if c.is_ascii_alphabetic() || c == b'_' {
            let start = i;
            while i < b.len() && (b[i].is_ascii_alphanumeric() || b[i] == b'_') {
                i += 1;
            }
            let word = &line[start..i];
            if KEYWORDS.contains(&word) {
                out.push_str(theme.kw);
                out.push_str(word);
                out.push_str(theme.reset);
            } else {
                out.push_str(word);
            }
            continue;
        }
        // Anything else: copy one UTF-8 character verbatim.
        let len = utf8_len(c);
        out.push_str(&line[i..(i + len).min(line.len())]);
        i += len;
    }
    out
}

fn utf8_len(b: u8) -> usize {
    match b {
        0x00..=0x7f => 1,
        0xc0..=0xdf => 2,
        0xe0..=0xef => 3,
        _ => 4,
    }
}
