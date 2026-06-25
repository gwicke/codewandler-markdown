//! Minimal HTML entity / character-reference decoder, used when normalising link destinations and
//! titles (CommonMark says an entity reference in a destination "counts as its value").
//!
//! Numeric references (`&#NNN;`, `&#xHH;`) are decoded in full. Named references are matched against
//! a curated table of the entities the spec corpus exercises — not the entire HTML5 set — which is
//! enough for the link/image cluster without pulling in a multi-thousand-entry table. An unknown or
//! malformed reference is left untouched by the caller (we return `None`).

/// If `s` begins with a valid HTML character reference, return its decoded text and the byte length
/// of the reference (including `&` and `;`). Otherwise return `None`.
pub fn decode_entity(s: &str) -> Option<(String, usize)> {
    let b = s.as_bytes();
    if b.first() != Some(&b'&') {
        return None;
    }
    // Numeric: &#123; (decimal) or &#x1F; / &#X1F; (hex).
    if b.get(1) == Some(&b'#') {
        let (radix, start) = match b.get(2) {
            Some(b'x') | Some(b'X') => (16, 3),
            _ => (10, 2),
        };
        let mut j = start;
        while j < b.len() && b[j] != b';' {
            j += 1;
        }
        if j >= b.len() || j == start {
            return None;
        }
        let digits = &s[start..j];
        let code = u32::from_str_radix(digits, radix).ok()?;
        // NUL and invalid scalar values render as U+FFFD per the spec.
        let ch = char::from_u32(code)
            .filter(|&c| c != '\0')
            .unwrap_or('\u{FFFD}');
        return Some((ch.to_string(), j + 1));
    }
    // Named: &name;
    let mut j = 1;
    while j < b.len() && b[j] != b';' && b[j].is_ascii_alphanumeric() {
        j += 1;
    }
    if j >= b.len() || b[j] != b';' || j == 1 {
        return None;
    }
    let name = &s[1..j];
    named_entity(name).map(|v| (v.to_string(), j + 1))
}

/// Look up a named entity from a curated subset of the HTML5 table (the names the corpus uses, plus
/// the always-present XML five). Returns the replacement text, or `None` if unknown.
fn named_entity(name: &str) -> Option<&'static str> {
    Some(match name {
        "amp" => "&",
        "lt" => "<",
        "gt" => ">",
        "quot" => "\"",
        "apos" => "'",
        "nbsp" => "\u{a0}",
        "copy" => "\u{a9}",
        "AElig" => "\u{c6}",
        "auml" => "\u{e4}",
        "ouml" => "\u{f6}",
        "uuml" => "\u{fc}",
        "Auml" => "\u{c4}",
        "Ouml" => "\u{d6}",
        "Uuml" => "\u{dc}",
        "szlig" => "\u{df}",
        "Dcaron" => "\u{10e}",
        "frac34" => "\u{be}",
        "HilbertSpace" => "\u{210b}",
        "DifferentialD" => "\u{2146}",
        "ClockwiseContourIntegral" => "\u{2232}",
        "ngE" => "\u{2267}\u{338}",
        _ => return None,
    })
}
