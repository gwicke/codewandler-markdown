//! `mdview` — render Markdown to the terminal, streaming.
//!
//! Reads a file argument or stdin and renders it through the live terminal renderer. Output is
//! styled when stdout is a TTY (and `NO_COLOR` is unset) and plain otherwise, so it pipes cleanly.
//!
//! ```text
//! mdview README.md
//! some-llm --stream | mdview
//! ```

use std::io::{self, Read, Write};

use markdown_stream::{Parser, StreamParser};
use markdown_terminal::{Renderer, Theme};

fn main() {
    let arg = std::env::args().nth(1);
    match arg.as_deref() {
        Some("-h") | Some("--help") => {
            eprintln!("usage: mdview [FILE]   (reads stdin if FILE is omitted)");
            return;
        }
        _ => {}
    }

    let input = match arg {
        Some(path) => std::fs::read_to_string(&path).unwrap_or_else(|e| {
            eprintln!("mdview: {path}: {e}");
            std::process::exit(1);
        }),
        None => {
            let mut s = String::new();
            io::stdin().read_to_string(&mut s).expect("read stdin");
            s
        }
    };

    let mut parser = StreamParser::new();
    let mut renderer = Renderer::new(Theme::auto(), term_width());
    let stdout = io::stdout();
    let mut out = stdout.lock();

    // Feed in chunks so the streaming/live path is exercised even for file input.
    for chunk in input.as_bytes().chunks(256) {
        let events = parser.write(chunk);
        let _ = renderer.feed(&events, &mut out);
    }
    let events = parser.flush();
    let _ = renderer.feed(&events, &mut out);
    let _ = renderer.finish(&mut out);
    let _ = out.flush();
}

/// Best-effort terminal width from `$COLUMNS`, defaulting to 80.
fn term_width() -> usize {
    std::env::var("COLUMNS")
        .ok()
        .and_then(|c| c.parse::<usize>().ok())
        .filter(|&w| w >= 20)
        .unwrap_or(80)
}
