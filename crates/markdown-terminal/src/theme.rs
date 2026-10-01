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
    /// When `true`, the renderer brackets **every rendered row** in the faint attribute
    /// (`\x1b[2m` before the row's content, `\x1b[0m` after it), so a faded document is made of
    /// self-contained rows.
    ///
    /// This is what lets a caller treat faded output as ordinary content: no escape is left open
    /// across rows, so a block cannot bleed its dimming into the text after it, and the stream
    /// ends in a clean terminal state rather than a pending SGR nobody will close. Set by
    /// [`Theme::dimmed`]; every other theme leaves rows untouched.
    pub faint_rows: bool,
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
            faint_rows: false,
        }
    }
}

impl Theme {
    /// A theme for secondary/faded content (e.g. streamed "thinking" /
    /// reasoning): styled spans are faded, `reset` re-applies the faint
    /// attribute (`\x1b[2m`) instead of a bare `\x1b[0m` so inline formatting
    /// does not wipe the dimming *within* a row, and `faint_rows` makes the
    /// renderer bracket each row in `\x1b[2m` … `\x1b[0m` around that.
    ///
    /// The two halves are what let a caller stop managing the attribute: a row
    /// re-opens the faint whatever preceded it, so plain unstyled text is dim
    /// without the caller emitting an initial `\x1b[2m`, and the row that ends
    /// the stream closes it. Callers just write the bytes.
    pub fn dimmed() -> Self {
        Theme {
            heading: "\x1b[1;36m",
            code: "\x1b[38;5;180m",
            link: "\x1b[4;34m",
            muted: "\x1b[2m",
            bold: "\x1b[1m",
            italic: "\x1b[3m",
            strike: "\x1b[9m",
            reset: "\x1b[0m\x1b[2m",
            kw: "\x1b[35m",
            str: "\x1b[32m",
            comment: "\x1b[90m",
            num: "\x1b[33m",
            clickable_links: false,
            faint_rows: true,
        }
    }

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
            faint_rows: false,
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
