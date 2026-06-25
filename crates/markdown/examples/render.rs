//! Stream stdin through the parser into the live terminal renderer.
//!
//! `echo "# Hi\n\nsome **bold**" | cargo run --example render`

use std::io::{self, Read};

use markdown::stream::{Parser, StreamParser};
use markdown_terminal::{Renderer, Theme};

fn main() {
    let mut input = String::new();
    io::stdin().read_to_string(&mut input).expect("read stdin");

    let mut parser = StreamParser::new();
    let mut renderer = Renderer::new(Theme::default(), 80);
    let mut out = io::stdout().lock();

    // Feed the input in small chunks to exercise the streaming path.
    for chunk in input.as_bytes().chunks(7) {
        let events = parser.write(chunk);
        renderer.feed(&events, &mut out).expect("write");
    }
    let events = parser.flush();
    renderer.feed(&events, &mut out).expect("write");
    renderer.finish(&mut out).expect("write");
}
