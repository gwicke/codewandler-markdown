//! Forward-reference resolution: a reference link/image (`[label]`, `[text][label]`, `[label][]`,
//! and `![…]` forms) must resolve against a link reference definition that appears *later* in the
//! document, not only against definitions seen so far. The parser holds a block once it contains an
//! unresolved reference (and, to keep document order, every block after it) until the definition
//! lands or end-of-input is reached, then re-resolves it. A label that is never defined stays
//! literal text, per CommonMark.

use markdown::{html_string, html_string_gfm};

/// Each case renders identically under the CommonMark and GFM HTML paths (these inputs use no
/// GFM-only syntax), so we assert both at once.
fn check(input: &str, expected: &str) {
    assert_eq!(html_string(input), expected, "CommonMark: {input:?}");
    assert_eq!(html_string_gfm(input), expected, "GFM: {input:?}");
}

#[test]
fn shortcut_forward_reference() {
    check(
        "[foo]\n\n[foo]: /url\n",
        "<p><a href=\"/url\">foo</a></p>\n",
    );
}

#[test]
fn collapsed_forward_reference() {
    check(
        "[foo][]\n\n[foo]: /url1\n",
        "<p><a href=\"/url1\">foo</a></p>\n",
    );
}

#[test]
fn full_forward_reference() {
    check(
        "[foo][bar]\n\n[bar]: /url\n",
        "<p><a href=\"/url\">foo</a></p>\n",
    );
}

#[test]
fn forward_image_reference() {
    check(
        "![foo]\n\n[foo]: /url\n",
        "<p><img src=\"/url\" alt=\"foo\" /></p>\n",
    );
}

#[test]
fn undefined_reference_stays_literal() {
    check(
        "[never]\n\nplain para\n",
        "<p>[never]</p>\n<p>plain para</p>\n",
    );
}

#[test]
fn case_insensitive_forward_reference() {
    // Reference matching is case-insensitive, so `[FOO]` resolves against a later `[foo]: …`.
    check(
        "[FOO]\n\n[foo]: /url\n",
        "<p><a href=\"/url\">FOO</a></p>\n",
    );
}

#[test]
fn forward_reference_in_heading() {
    check(
        "# [foo]\n\n[foo]: /url\n",
        "<h1><a href=\"/url\">foo</a></h1>\n",
    );
}

#[test]
fn forward_reference_in_block_quote() {
    check(
        "> [foo]\n\n[foo]: /url\n",
        "<blockquote>\n<p><a href=\"/url\">foo</a></p>\n</blockquote>\n",
    );
}

#[test]
fn forward_reference_in_loose_list() {
    check(
        "- [foo]\n\n- bar\n\n[foo]: /url\n",
        "<ul>\n<li>\n<p><a href=\"/url\">foo</a></p>\n</li>\n<li>\n<p>bar</p>\n</li>\n</ul>\n",
    );
}

#[test]
fn document_order_preserved_across_held_blocks() {
    // The held paragraph (`[foo]`) and the paragraphs around it must emit in source order.
    check(
        "first para\n\n[foo]\n\nlast para\n\n[foo]: /url\n",
        "<p>first para</p>\n<p><a href=\"/url\">foo</a></p>\n<p>last para</p>\n",
    );
}

#[test]
fn mixed_forward_and_backward_references() {
    // `[a]` is already defined (backward) when used; `[b]` is a forward reference. Both resolve.
    check(
        "[a]: /a\n\n[a] then [b]\n\n[b]: /b\n",
        "<p><a href=\"/a\">a</a> then <a href=\"/b\">b</a></p>\n",
    );
}
