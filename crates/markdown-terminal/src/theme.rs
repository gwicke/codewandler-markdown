//! Terminal themes: named roles mapped to raw ANSI SGR sequences. (Expanded in M4.)

/// A terminal theme — ANSI escape sequences for each rendered role.
#[derive(Debug, Clone)]
pub struct Theme {
    pub heading: &'static str,
    pub code: &'static str,
    pub link: &'static str,
    pub muted: &'static str,
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
            reset: "",
        }
    }
}
