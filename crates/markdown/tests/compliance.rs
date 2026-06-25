//! CommonMark/GFM compliance harness — the project's acceptance criteria.
//!
//! Each spec case provides Markdown input and the canonical expected HTML. We score `parse → HTML`
//! against it. The `WANT_*` baselines fail the build on any regression; raise them as the parser and
//! HTML renderer land (the discipline inherited from the Go original).

// The baselines are a ratchet that starts at 0 and only rises; `pass >= 0` is "absurd" only while the
// floor is still 0.
#![allow(clippy::absurd_extreme_comparisons)]

use serde::Deserialize;

#[derive(Deserialize)]
struct Case {
    markdown: String,
    html: String,
    #[serde(default)]
    #[allow(dead_code)]
    example: u32,
    #[serde(default)]
    #[allow(dead_code)]
    section: String,
    #[serde(default)]
    #[allow(dead_code)]
    extension: String,
}

fn score(corpus_json: &str) -> (usize, usize) {
    let cases: Vec<Case> = serde_json::from_str(corpus_json).expect("valid corpus JSON");
    let total = cases.len();
    let pass = cases
        .iter()
        .filter(|c| markdown::html_string(&c.markdown) == c.html)
        .count();
    (pass, total)
}

const COMMONMARK: &str = include_str!("../../../corpus/commonmark-0.31.2.json");
const GFM: &str = include_str!("../../../corpus/gfm-0.29.json");

// Baselines — raised as compliance improves; a drop below these fails the build.
const WANT_COMMONMARK: usize = 372;
const WANT_GFM: usize = 361;

#[test]
fn commonmark_compliance() {
    let (pass, total) = score(COMMONMARK);
    println!("CommonMark 0.31.2: {pass}/{total}");
    assert!(
        pass >= WANT_COMMONMARK,
        "CommonMark compliance regressed: {pass} < {WANT_COMMONMARK}"
    );
}

#[test]
fn gfm_compliance() {
    let (pass, total) = score(GFM);
    println!("GFM 0.29: {pass}/{total}");
    assert!(
        pass >= WANT_GFM,
        "GFM compliance regressed: {pass} < {WANT_GFM}"
    );
}
