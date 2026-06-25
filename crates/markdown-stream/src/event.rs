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

/// A resolved link reference definition (`[label]: dest "title"`). The destination is already
/// normalized (escapes/entities resolved, percent-encoded) and the title un-escaped, so resolving a
/// reference link is just a map lookup. Parser-owned; not part of the public event stream.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LinkDef {
    pub dest: String,
    pub title: String,
}

/// The kind of inline span an `EnterInline`/`ExitInline` event opens or closes.
///
/// Unlike the cumulative [`InlineStyle`] flags carried on `Text` (which a flat renderer reads),
/// these events make nesting *explicit*: an HTML renderer emits exactly one tag per enter/exit, so
/// `*a **b** c*` nests as `<em>a <strong>b</strong> c</em>` rather than three independent runs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Inline {
    Emphasis,
    Strong,
    Strikethrough,
    Code,
    Link(Link),
    Image(Link),
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
    /// `true` for inline raw HTML (`<tag …>`, comments, …): the text is verbatim HTML and an HTML
    /// renderer must emit it *unescaped*. The terminal renderer ignores this flag (the literal tag
    /// text is shown as-is), so it has no effect on terminal output.
    pub raw_html: bool,
}

impl InlineStyle {
    /// `true` when no styling applies (plain text).
    pub fn is_plain(&self) -> bool {
        !self.emphasis
            && !self.strong
            && !self.code
            && !self.strikethrough
            && self.link.is_none()
            && !self.raw_html
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
    /// Open an inline span (emphasis, strong, link, …). Balanced by a matching `ExitInline`.
    EnterInline {
        inline: Inline,
        span: Span,
    },
    /// Close the most recently opened inline span of the matching `inline`.
    ExitInline {
        inline: Inline,
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
