//! Terminal themes: named roles mapped to raw ANSI SGR sequences. Every escape the renderer emits
//! comes from a theme field, so [`Theme::no_color`] yields completely plain output.

/// A terminal theme — the ANSI escape sequences for each rendered role. An empty string disables a
/// role (so `no_color` emits no escapes at all).
#[derive(Debug, Clone)]
pub struct Theme {
    pub heading: &'static str,
    pub code: &'static str,
    pub link: &'static str,
    pub muted: &'static str,
    pub bold: &'static str,
    pub italic: &'static str,
    pub strike: &'static str,
    pub reset: &'static str,
}

impl Default for Theme {
    /// A sensible dark-terminal default.
    fn default() -> Self {
        Theme {
            heading: "\x1b[1;36m", // bold cyan
            code: "\x1b[38;5;180m",
            link: "\x1b[4;34m", // underline blue
            muted: "\x1b[2m",
            bold: "\x1b[1m",
            italic: "\x1b[3m",
            strike: "\x1b[9m",
            reset: "\x1b[0m",
        }
    }
}

impl Theme {
    /// A theme that emits no escape sequences (for non-TTY / `--no-color`).
    pub fn no_color() -> Self {
        Theme {
            heading: "",
            code: "",
            link: "",
            muted: "",
            bold: "",
            italic: "",
            strike: "",
            reset: "",
        }
    }

    /// Pick a theme based on the environment: the styled default when stdout is a terminal and
    /// `NO_COLOR` is unset, otherwise [`Theme::no_color`] (so `… | cat` stays clean).
    pub fn auto() -> Self {
        use std::io::IsTerminal;
        if std::io::stdout().is_terminal() && std::env::var_os("NO_COLOR").is_none() {
            Theme::default()
        } else {
            Theme::no_color()
        }
    }
}
