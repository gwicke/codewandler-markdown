//! The parser's output: a flat, append-only stream of [`Event`]s (SAX-like, no AST).
//!
//! Faithful to the Go `stream/event.go` model: blocks are delimited by `EnterBlock`/`ExitBlock`
//! pairs, inline content arrives as styled `Text` plus `SoftBreak`/`LineBreak`. A renderer consumes
//! this stream directly and never re-parses Markdown.

/// A byte range in the source, with the 1-based line/column of its start.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Span {
    pub start: usize,
    pub end: usize,
    pub line: u32,
    pub column: u32,
}

/// The kind of block an `EnterBlock`/`ExitBlock` event refers to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BlockKind {
    Document,
    Paragraph,
    Heading,
    BlockQuote,
    List,
    ListItem,
    FencedCode,
    IndentedCode,
    ThematicBreak,
    HtmlBlock,
    Table,
    TableRow,
    TableCell,
}

/// Column alignment for GFM table columns.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Alignment {
    None,
    Left,
    Center,
    Right,
}

/// List metadata carried on an `EnterBlock(List)` event.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ListData {
    /// `true` for ordered (`1.`) lists, `false` for bullet (`-`/`*`/`+`) lists.
    pub ordered: bool,
    /// Starting number for ordered lists.
    pub start: u64,
    /// `true` if the list is tight (no blank lines between items → no `<p>` wrappers).
    pub tight: bool,
    /// The marker character: `-`/`*`/`+` for bullets, `.`/`)` for ordered.
    pub marker: char,
}

/// A link or image target carried on a styled `Text` event.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Link {
    pub href: String,
    pub title: String,
    /// `true` if this is an image (`![alt](src)`) rather than a link.
    pub image: bool,
}

/// Inline styling carried on a `Text` event. Multiple flags may apply at once
/// (e.g. bold + italic). `link` is set for text inside a link/image.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct InlineStyle {
    pub emphasis: bool, // italic
    pub strong: bool,   // bold
    pub code: bool,     // inline code span
    pub strikethrough: bool,
    pub link: Option<Link>,
}

impl InlineStyle {
    /// `true` when no styling applies (plain text).
    pub fn is_plain(&self) -> bool {
        !self.emphasis && !self.strong && !self.code && !self.strikethrough && self.link.is_none()
    }
}

/// Variant-specific payload for an `EnterBlock` event. Defaults are empty/zero for blocks that
/// carry no extra data (paragraphs, blockquotes, …).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct BlockData {
    /// Heading level (1–6) for `Heading`.
    pub level: u8,
    /// Fenced-code info string (the language) for `FencedCode`.
    pub info: String,
    /// List metadata for `List`.
    pub list: Option<ListData>,
    /// Per-column alignment for `Table`.
    pub alignment: Vec<Alignment>,
}

/// One parser event. The stream is a depth-first walk: every `EnterBlock` is eventually balanced by
/// an `ExitBlock` of the same kind.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
    EnterBlock {
        block: BlockKind,
        data: BlockData,
        span: Span,
    },
    ExitBlock {
        block: BlockKind,
        span: Span,
    },
    /// A run of inline text with its accumulated styling.
    Text {
        text: String,
        style: InlineStyle,
        span: Span,
    },
    /// A newline within a paragraph (rendered as a space / `\n` in HTML).
    SoftBreak,
    /// A hard line break (two trailing spaces or a backslash).
    LineBreak,
}

impl Event {
    /// Convenience constructor for a plain (unstyled) text event.
    pub fn text(s: impl Into<String>) -> Event {
        Event::Text {
            text: s.into(),
            style: InlineStyle::default(),
            span: Span::default(),
        }
    }

    /// Convenience constructor for an `EnterBlock` with default data.
    pub fn enter(block: BlockKind) -> Event {
        Event::EnterBlock {
            block,
            data: BlockData::default(),
            span: Span::default(),
        }
    }

    /// Convenience constructor for an `ExitBlock`.
    pub fn exit(block: BlockKind) -> Event {
        Event::ExitBlock {
            block,
            span: Span::default(),
        }
    }
}
