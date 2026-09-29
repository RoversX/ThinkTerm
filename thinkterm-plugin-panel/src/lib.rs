//! A plugin's panel in ThinkTerm's right sidebar.
//!
//! A plugin draws its panel by hand: it sends a [`Frame`], a list of
//! rectangles, text, lines, filled areas and the hits that make parts of it
//! answer a click, in the panel's own units. Every ThinkTerm client shows
//! it with the same [`Player`], painting its items with the client's own
//! renderer and fonts and in ThinkTerm's colours, so a panel looks the same
//! on the desktop and in a browser without the plugin knowing which it is.
//!
//! The player never waits for the plugin. Hovering and scrolling happen in
//! it at once; a click is sent to the plugin, which answers with a new
//! frame when it is ready. A long list is drawn a page of rows at a time,
//! as the player asks for them, so a plugin with ten thousand rows sends
//! the few dozen that show.
//!
//! A panel whose frame asks for it (`Frame::extend`) gets an extended view
//! as well: a wide area beside the sidebar, which the client opens as a
//! second view of the panel and shows with a player of its own.
//!
//! What passes to and fro goes over the plugin channel and the plugin
//! protocol as JSON (docs/thinkterm/plugins.md): [`Env`] and [`Input`] to
//! the plugin, [`Frame`] and [`Rows`] from it. A panel beside a terminal on
//! another machine ([`Env::remote`]) has its plugin [`Ask`] the client for
//! what it wants done there, which the client answers ([`Answer`]).

pub mod player;
pub mod scene;
pub mod wire;

/// The Lucide icons a panel can be shown by in the sidebar's selector;
/// every client has these. A manifest naming another gets the first.
pub const ICONS: [&str; 12] = [
    "puzzle",
    "activity",
    "bug",
    "calendar",
    "chart-candlestick",
    "chart-line",
    "cpu",
    "database",
    "gauge",
    "git-branch",
    "git-compare",
    "list-todo",
];

pub use player::{Draw, Drawn, Op, Painting, Player};
pub use scene::{
    Align, Area, Bounds, Color, Cursor, Font, Frame, Hit, Item, Jump, Line, List, Rect, Scroll,
    Size, Text, Token,
};
pub use wire::{
    Answer, Ask, Button, Bytes, Click, CloseButton, Entry, EntryKind, Env, Input, Mods,
    MonoMetrics, Remote, Rows, RowsWanted, TextMetrics,
};
