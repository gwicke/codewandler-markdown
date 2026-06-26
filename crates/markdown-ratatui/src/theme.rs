//! Terminal themes for the ratatui renderer: named roles mapped to `ratatui::style::Style`. Mirrors
//! the ANSI defaults of `markdown-terminal::Theme` so the two renderers look the same.

use ratatui::style::{Color, Modifier, Style};

/// A theme — the `Style` for each rendered role. `Style::default()` (an empty style) disables a role.
#[derive(Debug, Clone)]
pub struct Theme {
    pub heading: Style,
    pub code: Style,
    pub link: Style,
    pub muted: Style,
    pub bold: Style,
    pub italic: Style,
    pub strike: Style,
    // syntax-highlighting roles for fenced code (reserved; v1 renders code uniformly)
    pub kw: Style,
    pub str: Style,
    pub comment: Style,
    pub num: Style,
}

impl Default for Theme {
    /// A dark-terminal default matching `markdown-terminal::Theme::default()`.
    fn default() -> Self {
        Theme {
            heading: Style::new().fg(Color::Cyan).add_modifier(Modifier::BOLD), // 1;36
            code: Style::new().fg(Color::Indexed(180)),                         // 38;5;180
            link: Style::new()
                .fg(Color::Blue)
                .add_modifier(Modifier::UNDERLINED), // 4;34
            muted: Style::new().add_modifier(Modifier::DIM),                    // 2
            bold: Style::new().add_modifier(Modifier::BOLD),                    // 1
            italic: Style::new().add_modifier(Modifier::ITALIC),                // 3
            strike: Style::new().add_modifier(Modifier::CROSSED_OUT),           // 9
            kw: Style::new().fg(Color::Magenta),                                // 35
            str: Style::new().fg(Color::Green),                                 // 32
            comment: Style::new().fg(Color::DarkGray),                          // 90 (bright black)
            num: Style::new().fg(Color::Yellow),                                // 33
        }
    }
}

impl Theme {
    /// A theme that applies no styling (every role is the empty `Style`).
    pub fn no_color() -> Self {
        let s = Style::new();
        Theme {
            heading: s,
            code: s,
            link: s,
            muted: s,
            bold: s,
            italic: s,
            strike: s,
            kw: s,
            str: s,
            comment: s,
            num: s,
        }
    }
}
