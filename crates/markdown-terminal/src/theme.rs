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
    // syntax-highlighting roles for fenced code
    pub kw: &'static str,
    pub str: &'static str,
    pub comment: &'static str,
    pub num: &'static str,
    /// When `true`, inline links are wrapped in OSC 8 hyperlink sequences
    /// (`\x1b]8;;<href>\x1b\\ ... \x1b]8;;\x1b\\`) so they are clickable in
    /// terminals that support it (iTerm2, GNOME Terminal, VS Code, Alacritty,
    /// WezTerm, Kitty, …). Off by default — callers opt in (e.g. only on a real
    /// TTY) so logs and pipes stay free of escape noise.
    pub clickable_links: bool,
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
            kw: "\x1b[35m",      // magenta
            str: "\x1b[32m",     // green
            comment: "\x1b[90m", // bright black
            num: "\x1b[33m",     // yellow
            clickable_links: false,
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
            kw: "",
            str: "",
            comment: "",
            num: "",
            clickable_links: false,
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
