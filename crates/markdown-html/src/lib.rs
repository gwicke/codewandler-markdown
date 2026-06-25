//! `markdown-html` — render a [`markdown_stream`] event stream to HTML.
//!
//! Primarily the oracle for compliance testing: the CommonMark/GFM corpora specify expected HTML,
//! so `parse → render_html` is how the parser is scored. Incremental-only — it consumes events and
//! never re-parses Markdown.

#![forbid(unsafe_code)]

use markdown_stream::{BlockKind, Event, Inline, Link};

/// Render a sequence of events to an HTML string.
pub fn render(events: &[Event]) -> String {
    let mut out = String::new();
    render_into(&mut out, events);
    out
}

/// Render events into an existing buffer.
///
/// Inline nesting comes through as explicit `EnterInline`/`ExitInline` pairs, so each emits exactly
/// one tag and HTML nesting is exact. Two inline kinds need buffering: inside a `Code` span text is
/// escaped raw (no inner tags occur), and inside an `Image` span the inner text is *accumulated*
/// into the `alt` attribute rather than rendered, then emitted as a single `<img …/>`.
pub fn render_into(out: &mut String, events: &[Event]) {
    // `list_stack`: one `bool` per open list — `true` = ordered (`<ol>`), `false` = bullet (`<ul>`).
    let mut list_stack: Vec<bool> = Vec::new();
    // `image_stack`: one frame per in-progress image, buffering the plain-text `alt` for `![…](…)`.
    let mut image_stack: Vec<ImageFrame> = Vec::new();
    // Inside a raw HTML block, `Text` is emitted verbatim (no escaping) — the block's lines *are*
    // the output.
    let mut in_html = false;

    for ev in events {
        // While inside an image, suppress all tag output and accumulate inner text into `alt`.
        if let Some(frame) = image_stack.last_mut() {
            match ev {
                Event::ExitInline {
                    inline: Inline::Image(_),
                } => {
                    let frame = image_stack.pop().expect("image frame");
                    emit_image(out, &frame);
                    continue;
                }
                Event::EnterInline {
                    inline: Inline::Image(link),
                    ..
                } => {
                    // Nested image: start a new alt-accumulation frame.
                    image_stack.push(ImageFrame::new(link.clone()));
                    continue;
                }
                Event::Text { text, .. } => {
                    frame.alt.push_str(text);
                    continue;
                }
                Event::SoftBreak => {
                    frame.alt.push('\n');
                    continue;
                }
                Event::LineBreak => {
                    frame.alt.push('\n');
                    continue;
                }
                // Other enter/exit inlines inside an image contribute no alt text (their text
                // children still flow through the `Text` arm above).
                Event::EnterInline { .. } | Event::ExitInline { .. } => continue,
                _ => continue,
            }
        }

        match ev {
            Event::EnterBlock { block, data, .. } => match block {
                BlockKind::Document => {}
                BlockKind::Paragraph => out.push_str("<p>"),
                BlockKind::Heading => {
                    let l = data.level.clamp(1, 6);
                    out.push_str(&format!("<h{l}>"));
                }
                BlockKind::BlockQuote => out.push_str("<blockquote>\n"),
                BlockKind::ThematicBreak => out.push_str("<hr />\n"),
                BlockKind::List => {
                    let ordered = data.list.as_ref().is_some_and(|l| l.ordered);
                    let start = data.list.as_ref().map(|l| l.start).unwrap_or(1);
                    if ordered {
                        if start == 1 {
                            out.push_str("<ol>\n");
                        } else {
                            out.push_str(&format!("<ol start=\"{start}\">\n"));
                        }
                    } else {
                        out.push_str("<ul>\n");
                    }
                    list_stack.push(ordered);
                }
                BlockKind::ListItem => out.push_str("<li>"),
                BlockKind::FencedCode => {
                    let lang = data.info.split_whitespace().next().unwrap_or("");
                    if lang.is_empty() {
                        out.push_str("<pre><code>");
                    } else {
                        out.push_str(&format!("<pre><code class=\"language-{}\">", escape(lang)));
                    }
                }
                BlockKind::IndentedCode => {
                    out.push_str("<pre><code>");
                }
                BlockKind::HtmlBlock => in_html = true,
                BlockKind::Table | BlockKind::TableRow | BlockKind::TableCell => {}
            },
            Event::ExitBlock { block, .. } => match block {
                BlockKind::Document => {}
                BlockKind::Paragraph => out.push_str("</p>\n"),
                BlockKind::Heading => {
                    // The matching level is recomputed by HTML's own `</hN>` from the last `<hN>`;
                    // we approximate by scanning back is overkill — re-emit from a small heuristic.
                    close_heading(out);
                }
                BlockKind::BlockQuote => out.push_str("</blockquote>\n"),
                BlockKind::List => {
                    let ordered = list_stack.pop().unwrap_or(false);
                    out.push_str(if ordered { "</ol>\n" } else { "</ul>\n" });
                }
                BlockKind::ListItem => out.push_str("</li>\n"),
                BlockKind::FencedCode | BlockKind::IndentedCode => {
                    out.push_str("</code></pre>\n");
                }
                BlockKind::HtmlBlock => in_html = false,
                _ => {}
            },
            Event::Text { text, style, .. } => {
                // Inside an HTML block, and for inline raw HTML (`style.raw_html`), the text is
                // verbatim HTML and is emitted unescaped. Everything else escapes raw: regular text,
                // block-level code (fenced/indented), and the text inside an inline `Code` span all
                // want HTML-escaped output with no inner markup, and emphasis/links arrive as their
                // own enter/exit events.
                if in_html || style.raw_html {
                    out.push_str(text);
                } else {
                    out.push_str(&escape(text));
                }
            }
            Event::EnterInline { inline, .. } => match inline {
                Inline::Emphasis => out.push_str("<em>"),
                Inline::Strong => out.push_str("<strong>"),
                Inline::Strikethrough => out.push_str("<del>"),
                Inline::Code => out.push_str("<code>"),
                Inline::Link(Link { href, title, .. }) => {
                    out.push_str(&format!("<a href=\"{}\"", escape_attr(href)));
                    if !title.is_empty() {
                        out.push_str(&format!(" title=\"{}\"", escape_attr(title)));
                    }
                    out.push('>');
                }
                Inline::Image(link) => image_stack.push(ImageFrame::new(link.clone())),
            },
            Event::ExitInline { inline } => match inline {
                Inline::Emphasis => out.push_str("</em>"),
                Inline::Strong => out.push_str("</strong>"),
                Inline::Strikethrough => out.push_str("</del>"),
                Inline::Code => out.push_str("</code>"),
                Inline::Link(_) => out.push_str("</a>"),
                // An image's `ExitInline` is handled by the suppression branch above; reaching here
                // would mean an unbalanced stream, so do nothing.
                Inline::Image(_) => {}
            },
            Event::SoftBreak => out.push('\n'),
            Event::LineBreak => out.push_str("<br />\n"),
        }
    }
}

/// A buffered image span: its target plus the plain text accumulated for the `alt` attribute.
struct ImageFrame {
    link: Link,
    alt: String,
}

impl ImageFrame {
    fn new(link: Link) -> Self {
        ImageFrame {
            link,
            alt: String::new(),
        }
    }
}

/// Emit a completed `<img …/>` from a buffered frame. `alt` is the plain-text rendering of the
/// image's inner content (CommonMark renders image descriptions as plain text).
fn emit_image(out: &mut String, frame: &ImageFrame) {
    out.push_str(&format!("<img src=\"{}\"", escape_attr(&frame.link.href)));
    out.push_str(&format!(" alt=\"{}\"", escape_attr(&frame.alt)));
    if !frame.link.title.is_empty() {
        out.push_str(&format!(" title=\"{}\"", escape_attr(&frame.link.title)));
    }
    out.push_str(" />");
}

/// Track the open heading level via a tiny side-channel: the last `<hN>` written. Rather than thread
/// state, we look back at the buffer's most recent unmatched `<hN>`.
fn close_heading(out: &mut String) {
    // Find the level of the most recent "<hN>" we opened.
    if let Some(pos) = out.rfind("<h") {
        if let Some(d) = out[pos + 2..].bytes().next() {
            if d.is_ascii_digit() {
                out.push_str(&format!("</h{}>\n", d as char));
                return;
            }
        }
    }
    out.push_str("</h1>\n");
}

fn escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            _ => out.push(c),
        }
    }
    out
}

fn escape_attr(s: &str) -> String {
    escape(s)
}
