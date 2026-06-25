//! `markdown-html` — render a [`markdown_stream`] event stream to HTML.
//!
//! Primarily the oracle for compliance testing: the CommonMark/GFM corpora specify expected HTML,
//! so `parse → render_html` is how the parser is scored. Incremental-only — it consumes events and
//! never re-parses Markdown.

#![forbid(unsafe_code)]

use markdown_stream::{BlockKind, Event, InlineStyle, Link};

/// Render a sequence of events to an HTML string.
pub fn render(events: &[Event]) -> String {
    let mut out = String::new();
    render_into(&mut out, events);
    out
}

/// Render events into an existing buffer.
pub fn render_into(out: &mut String, events: &[Event]) {
    let mut in_code = false;
    let mut list_stack: Vec<bool> = Vec::new(); // true = ordered

    for ev in events {
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
                    in_code = true;
                    let lang = data.info.split_whitespace().next().unwrap_or("");
                    if lang.is_empty() {
                        out.push_str("<pre><code>");
                    } else {
                        out.push_str(&format!("<pre><code class=\"language-{}\">", escape(lang)));
                    }
                }
                BlockKind::IndentedCode => {
                    in_code = true;
                    out.push_str("<pre><code>");
                }
                BlockKind::HtmlBlock => {}
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
                    in_code = false;
                    out.push_str("</code></pre>\n");
                }
                _ => {}
            },
            Event::Text { text, style, .. } => {
                if in_code {
                    out.push_str(&escape(text));
                } else {
                    push_styled(out, text, style);
                }
            }
            Event::SoftBreak => out.push('\n'),
            Event::LineBreak => out.push_str("<br />\n"),
        }
    }
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

fn push_styled(out: &mut String, text: &str, style: &InlineStyle) {
    if style.code {
        out.push_str("<code>");
        out.push_str(&escape(text));
        out.push_str("</code>");
        return;
    }
    let (open, close) = tags(style);
    out.push_str(&open);
    out.push_str(&escape(text));
    out.push_str(&close);
}

fn tags(style: &InlineStyle) -> (String, String) {
    let mut open = String::new();
    let mut close = String::new();
    if let Some(Link { href, title, image }) = &style.link {
        if *image {
            // images are emitted as <img>; handled simply
            return (
                format!("<img src=\"{}\" alt=\"", escape_attr(href)),
                "\" />".to_string(),
            );
        }
        let t = if title.is_empty() {
            String::new()
        } else {
            format!(" title=\"{}\"", escape_attr(title))
        };
        open.push_str(&format!("<a href=\"{}\"{}>", escape_attr(href), t));
        close.insert_str(0, "</a>");
    }
    if style.strikethrough {
        open.push_str("<del>");
        close.insert_str(0, "</del>");
    }
    if style.emphasis {
        open.push_str("<em>");
        close.insert_str(0, "</em>");
    }
    if style.strong {
        open.push_str("<strong>");
        close.insert_str(0, "</strong>");
    }
    (open, close)
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
