//! What a plugin draws: a list of items, painted in order, in the panel's
//! own units -- logical pixels from its top left corner, the ones
//! [`Env`](crate::Env) gives its size in.
//!
//! There are no widgets. A button is a rounded rectangle, its label and a
//! hit over both; the player draws what it is given and nothing else, so a
//! panel looks the same wherever it is shown. Colours are ThinkTerm's own,
//! by name, so a panel follows the theme without being told of it. The one
//! exception is a [`Field`]: text is edited where it is typed, without
//! waiting on the plugin, so the client draws the field itself.
//!
//! An item that does not read -- a coordinate a plugin wrote as `null`, a
//! kind a newer ThinkTerm knows and this one does not -- is left out, and
//! the rest of its list stays.

use serde::de::{Deserializer, Visitor};
use serde::{Deserialize, Serialize, Serializer};
use serde_json::value::RawValue;
use std::fmt;

/// The whole panel. Each frame replaces the last.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Frame {
    #[serde(default, deserialize_with = "items")]
    pub items: Vec<Item>,
    /// A panel's frame asks for its extended view with this: a wide area
    /// beside the sidebar, as wide as the user makes it, which the plugin
    /// draws as a view of its own. It stays while the panel's frames ask for
    /// it, and goes when one does not. An extended view's own frame asks
    /// nothing.
    #[serde(default, skip_serializing_if = "is_default")]
    pub extend: bool,
    /// The keys the panel takes while it has the keyboard, named as a
    /// browser names them (`KeyboardEvent.key`): "ArrowDown", "Enter", "j".
    /// Only these are sent, and only without Ctrl, Alt or Command, which
    /// are the app's; Escape and Tab never are (see [`Input::Key`]).
    ///
    /// [`Input::Key`]: crate::wire::Input::Key
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub keys: Vec<String>,
    /// Lets go of the keyboard, once for each new seq: what the user types
    /// next goes nowhere until they press Escape, or somewhere -- never to
    /// the terminal.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub release: Option<u32>,
}

/// A list of items, keeping the ones that read. JSON only: an item is read
/// from its own text.
pub(crate) fn items<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Vec<Item>, D::Error> {
    let raw: Vec<Box<RawValue>> = Vec::deserialize(deserializer)?;
    Ok(raw
        .iter()
        .filter_map(|item| serde_json::from_str(item.get()).ok())
        .collect())
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Item {
    Rect(Rect),
    Text(Text),
    Line(Line),
    Area(Area),
    Hit(Hit),
    Scroll(Scroll),
    List(List),
    Field(Field),
}

/// A filled rectangle, with rounded corners and a one-unit border when
/// asked for.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Rect {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fill: Option<Color>,
    #[serde(default, skip_serializing_if = "is_zero")]
    pub radius: f32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub border: Option<Color>,
}

/// One line of text in a box: centred in its height, placed across its
/// width as `align` says, and cut to fit -- with an ellipsis in the
/// interface font, at the box's edge in the monospaced one, whose columns a
/// plugin lines up itself.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Text {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
    pub text: String,
    #[serde(default, skip_serializing_if = "is_default")]
    pub color: Color,
    #[serde(default, skip_serializing_if = "is_default")]
    pub font: Font,
    /// The interface font's size; the monospaced font has the one.
    #[serde(default, skip_serializing_if = "is_default")]
    pub size: Size,
    #[serde(default, skip_serializing_if = "is_default")]
    pub bold: bool,
    #[serde(default, skip_serializing_if = "is_default")]
    pub align: Align,
}

/// A line through `points`, given as x, y, x, y and so on.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Line {
    pub points: Vec<f32>,
    #[serde(default = "one")]
    pub width: f32,
    pub color: Color,
}

/// What lies between a line through `points` -- x, y pairs, left to right
/// -- and the level `base`: the fill under a chart. With `fade` it thins
/// out toward `base`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Area {
    pub points: Vec<f32>,
    pub base: f32,
    pub color: Color,
    #[serde(default, skip_serializing_if = "is_default")]
    pub fade: bool,
}

/// Where a click is the plugin's to hear, as a click on `id`. While the
/// pointer is over it the player tints it with `hover`, without asking,
/// its corners rounded by `radius`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Hit {
    pub id: String,
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hover: Option<Color>,
    #[serde(default, skip_serializing_if = "is_zero")]
    pub radius: f32,
    #[serde(default, skip_serializing_if = "is_default")]
    pub cursor: Cursor,
}

/// A region whose `items` scroll: they are in its own units, `height`
/// tall, and cut to the region. The player scrolls it without asking.
/// Scroll areas and lists do not go inside one.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Scroll {
    pub id: String,
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
    pub height: f32,
    #[serde(default, deserialize_with = "items")]
    pub items: Vec<Item>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub top: Option<Jump>,
}

/// `count` rows, each `row` tall, which the plugin draws as they come into
/// view: the player asks for them a page at a time as it scrolls, keeps
/// the ones near the view and lets the rest go. A new `key` says the rows
/// are other ones: the old are dropped, and the list starts at its top. A
/// new `version` says the same rows look different now -- one was picked,
/// a price moved: they are asked for again where they show, and the old
/// ones stay on show until the new ones come.
///
/// Rows `width` wide, wider than the list, scroll sideways too, all but
/// what starts within `fixed` of their left edge: line numbers stay while
/// the lines move.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct List {
    pub id: String,
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
    pub count: u32,
    pub row: f32,
    pub key: String,
    #[serde(default, skip_serializing_if = "is_default")]
    pub version: u32,
    #[serde(default, skip_serializing_if = "is_zero")]
    pub width: f32,
    #[serde(default, skip_serializing_if = "is_zero")]
    pub fixed: f32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub top: Option<Jump>,
}

/// Scrolls a region to `to` -- a row of a list, a height in a scroll area
/// -- once for each new `seq`, so a plugin that sends it in every frame
/// does not keep pulling the region back. With `through`, it scrolls only
/// as far as it takes to show from `to` to there: a row picked with the
/// keys is kept in view.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Jump {
    pub to: f32,
    pub seq: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub through: Option<f32>,
}

/// A box the user types in, which the client draws -- the box, the text,
/// the caret and what is selected -- in its own font and colours. A click
/// in it gives it the keyboard. What it holds goes to the plugin as it
/// changes, and again when the user presses Return ([`Input::Text`],
/// [`Input::Submit`]).
///
/// What it holds is the client's while the field shows: `value` is taken
/// when it first comes, and again for each new `seq`, so the frames that
/// follow do not undo what is typed meanwhile -- a plugin empties it after
/// a submit by sending `""` with the next seq ([`FieldText`] keeps count).
/// `focus` moves the keyboard to it once for each new seq, but only while
/// its panel has the keyboard already: a plugin never takes it from the
/// terminal. A field does not go in a scroll area or a list's rows, and of
/// two with one id the first is kept. One on one line is drawn rounded at
/// its ends, as the sidebar's search is, with an icon before its text when
/// it names one.
///
/// [`Input::Text`]: crate::wire::Input::Text
/// [`Input::Submit`]: crate::wire::Input::Submit
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Field {
    pub id: String,
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
    #[serde(default, skip_serializing_if = "is_default")]
    pub kind: FieldKind,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub value: String,
    #[serde(default, skip_serializing_if = "is_default")]
    pub seq: u32,
    /// Shown, faint, while it is empty.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub placeholder: String,
    #[serde(default, skip_serializing_if = "is_default")]
    pub font: Font,
    /// The most characters it holds; 0 for as many as any field holds
    /// ([`FIELD_LIMIT`](crate::player::FIELD_LIMIT)).
    #[serde(default, skip_serializing_if = "is_default")]
    pub max: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub focus: Option<u32>,
    /// Shown before its text, on one line: one of
    /// [`FIELD_ICONS`](crate::FIELD_ICONS) -- `search`, say -- by name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub icon: Option<String>,
    /// A button at its end, on one line, that empties it while it holds
    /// something. The client draws it and empties the field itself, which
    /// the plugin hears as text like any other.
    #[serde(default, skip_serializing_if = "is_default")]
    pub clear: bool,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FieldKind {
    /// One line: Return submits it.
    #[default]
    Line,
    /// Lines: Return starts a new one, and Command-Return -- Ctrl+Return
    /// off a Mac -- submits them.
    Lines,
    /// One line shown as dots. The plugin is sent what it holds only when
    /// it is submitted, and nothing can be copied or cut out of it.
    Secret,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Font {
    /// The font the sidebar is written in.
    #[default]
    Ui,
    /// The terminal's font.
    Mono,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Size {
    Small,
    #[default]
    Body,
    Title,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Align {
    #[default]
    Left,
    Center,
    Right,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Cursor {
    /// The hand: something to press.
    #[default]
    Pointer,
    /// The arrow: a region that answers clicks without looking like it.
    Arrow,
    /// The I-beam: text is typed there. A field has it.
    Text,
}

/// A colour: one of ThinkTerm's by name, which follow its theme, or a fixed
/// one written `#rrggbb` or `#rrggbbaa`. A name this ThinkTerm does not
/// know draws as [`Token::Text`], so a plugin written for a newer one still
/// shows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Color {
    Token(Token),
    Rgba([u8; 4]),
}

impl Default for Color {
    fn default() -> Self {
        Self::Token(Token::Text)
    }
}

impl From<Token> for Color {
    fn from(token: Token) -> Self {
        Self::Token(token)
    }
}

/// ThinkTerm's colours, as the sidebar uses them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Token {
    Text,
    TextMuted,
    TextFaint,
    /// The panel's own ground.
    Bg,
    /// A card or a field on the ground.
    BgRaised,
    BgHover,
    BgSelected,
    Border,
    Accent,
    /// Text on `Accent`.
    OnAccent,
    /// Added, up, fine.
    Positive,
    /// Removed, down, wrong.
    Negative,
    Warning,
    /// Grounds for a line or a row: under text in `Text`.
    PositiveBg,
    NegativeBg,
    /// Stronger, for a word within such a line.
    PositiveBgStrong,
    NegativeBgStrong,
}

impl Token {
    pub const ALL: [Self; 17] = [
        Self::Text,
        Self::TextMuted,
        Self::TextFaint,
        Self::Bg,
        Self::BgRaised,
        Self::BgHover,
        Self::BgSelected,
        Self::Border,
        Self::Accent,
        Self::OnAccent,
        Self::Positive,
        Self::Negative,
        Self::Warning,
        Self::PositiveBg,
        Self::NegativeBg,
        Self::PositiveBgStrong,
        Self::NegativeBgStrong,
    ];

    pub fn name(self) -> &'static str {
        match self {
            Self::Text => "text",
            Self::TextMuted => "text-muted",
            Self::TextFaint => "text-faint",
            Self::Bg => "bg",
            Self::BgRaised => "bg-raised",
            Self::BgHover => "bg-hover",
            Self::BgSelected => "bg-selected",
            Self::Border => "border",
            Self::Accent => "accent",
            Self::OnAccent => "on-accent",
            Self::Positive => "positive",
            Self::Negative => "negative",
            Self::Warning => "warning",
            Self::PositiveBg => "positive-bg",
            Self::NegativeBg => "negative-bg",
            Self::PositiveBgStrong => "positive-bg-strong",
            Self::NegativeBgStrong => "negative-bg-strong",
        }
    }

    pub fn from_name(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|token| token.name() == name)
    }
}

impl Color {
    /// `#rrggbb`, `#rrggbbaa` or a token's name.
    pub fn parse(text: &str) -> Self {
        let parsed = match text.strip_prefix('#') {
            Some(hex) => parse_hex(hex).map(Self::Rgba),
            None => Token::from_name(text).map(Self::Token),
        };
        parsed.unwrap_or_default()
    }
}

fn parse_hex(hex: &str) -> Option<[u8; 4]> {
    if !(hex.len() == 6 || hex.len() == 8) || !hex.is_ascii() {
        return None;
    }
    let byte = |at: usize| u8::from_str_radix(hex.get(at..at + 2)?, 16).ok();
    let alpha = if hex.len() == 8 { byte(6)? } else { 255 };
    Some([byte(0)?, byte(2)?, byte(4)?, alpha])
}

impl fmt::Display for Color {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Token(token) => f.write_str(token.name()),
            Self::Rgba([r, g, b, 255]) => write!(f, "#{r:02x}{g:02x}{b:02x}"),
            Self::Rgba([r, g, b, a]) => write!(f, "#{r:02x}{g:02x}{b:02x}{a:02x}"),
        }
    }
}

impl Serialize for Color {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for Color {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct Named;
        impl Visitor<'_> for Named {
            type Value = Color;
            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("a colour's name, or #rrggbb")
            }
            fn visit_str<E: serde::de::Error>(self, text: &str) -> Result<Color, E> {
                Ok(Color::parse(text))
            }
        }
        deserializer.deserialize_str(Named)
    }
}

impl Frame {
    pub fn push(&mut self, item: impl Into<Item>) {
        self.items.push(item.into());
    }

    /// Asks for the panel's extended view, or stops asking.
    pub fn extend(&mut self, extend: bool) {
        self.extend = extend;
    }

    /// The keys the panel takes while it has the keyboard.
    pub fn keys(&mut self, keys: &[&str]) {
        self.keys = keys.iter().map(|key| key.to_string()).collect();
    }

    /// Lets go of the keyboard: once for each new `seq`.
    pub fn release(&mut self, seq: u32) {
        self.release = Some(seq);
    }
}

impl Rect {
    pub fn new(x: f32, y: f32, w: f32, h: f32) -> Self {
        Self {
            x,
            y,
            w,
            h,
            fill: None,
            radius: 0.0,
            border: None,
        }
    }

    pub fn fill(mut self, color: impl Into<Color>) -> Self {
        self.fill = Some(color.into());
        self
    }

    pub fn radius(mut self, radius: f32) -> Self {
        self.radius = radius;
        self
    }

    pub fn border(mut self, color: impl Into<Color>) -> Self {
        self.border = Some(color.into());
        self
    }
}

impl Text {
    pub fn new(x: f32, y: f32, w: f32, h: f32, text: impl Into<String>) -> Self {
        Self {
            x,
            y,
            w,
            h,
            text: text.into(),
            color: Color::default(),
            font: Font::Ui,
            size: Size::Body,
            bold: false,
            align: Align::Left,
        }
    }

    pub fn color(mut self, color: impl Into<Color>) -> Self {
        self.color = color.into();
        self
    }

    pub fn mono(mut self) -> Self {
        self.font = Font::Mono;
        self
    }

    pub fn size(mut self, size: Size) -> Self {
        self.size = size;
        self
    }

    pub fn bold(mut self) -> Self {
        self.bold = true;
        self
    }

    pub fn align(mut self, align: Align) -> Self {
        self.align = align;
        self
    }
}

impl Line {
    pub fn new(points: Vec<f32>, color: impl Into<Color>) -> Self {
        Self {
            points,
            width: 1.0,
            color: color.into(),
        }
    }

    pub fn width(mut self, width: f32) -> Self {
        self.width = width;
        self
    }
}

impl Area {
    pub fn new(points: Vec<f32>, base: f32, color: impl Into<Color>) -> Self {
        Self {
            points,
            base,
            color: color.into(),
            fade: false,
        }
    }

    pub fn fade(mut self) -> Self {
        self.fade = true;
        self
    }
}

impl Hit {
    pub fn new(id: impl Into<String>, x: f32, y: f32, w: f32, h: f32) -> Self {
        Self {
            id: id.into(),
            x,
            y,
            w,
            h,
            hover: None,
            radius: 0.0,
            cursor: Cursor::Pointer,
        }
    }

    pub fn hover(mut self, color: impl Into<Color>) -> Self {
        self.hover = Some(color.into());
        self
    }

    /// The corners of the hover tint: those of what it lights up.
    pub fn radius(mut self, radius: f32) -> Self {
        self.radius = radius;
        self
    }

    pub fn cursor(mut self, cursor: Cursor) -> Self {
        self.cursor = cursor;
        self
    }
}

impl Scroll {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        id: impl Into<String>,
        x: f32,
        y: f32,
        w: f32,
        h: f32,
        height: f32,
        items: Vec<Item>,
    ) -> Self {
        Self {
            id: id.into(),
            x,
            y,
            w,
            h,
            height,
            items,
            top: None,
        }
    }

    pub fn top(mut self, jump: Jump) -> Self {
        self.top = Some(jump);
        self
    }
}

impl List {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        id: impl Into<String>,
        x: f32,
        y: f32,
        w: f32,
        h: f32,
        count: u32,
        row: f32,
        key: impl Into<String>,
    ) -> Self {
        Self {
            id: id.into(),
            x,
            y,
            w,
            h,
            count,
            row,
            key: key.into(),
            version: 0,
            width: 0.0,
            fixed: 0.0,
            top: None,
        }
    }

    pub fn version(mut self, version: u32) -> Self {
        self.version = version;
        self
    }

    /// Rows `width` wide, scrolling sideways but for what starts within
    /// `fixed` of their left edge.
    pub fn wide(mut self, width: f32, fixed: f32) -> Self {
        self.width = width;
        self.fixed = fixed;
        self
    }

    pub fn top(mut self, jump: Jump) -> Self {
        self.top = Some(jump);
        self
    }
}

impl Field {
    pub fn new(id: impl Into<String>, x: f32, y: f32, w: f32, h: f32) -> Self {
        Self {
            id: id.into(),
            x,
            y,
            w,
            h,
            kind: FieldKind::Line,
            value: String::new(),
            seq: 0,
            placeholder: String::new(),
            font: Font::Ui,
            max: 0,
            focus: None,
            icon: None,
            clear: false,
        }
    }

    /// Lines rather than one.
    pub fn lines(mut self) -> Self {
        self.kind = FieldKind::Lines;
        self
    }

    /// Shown as dots, and sent only when submitted.
    pub fn secret(mut self) -> Self {
        self.kind = FieldKind::Secret;
        self
    }

    /// What it holds, taken once for each new `seq`.
    pub fn value(mut self, value: impl Into<String>, seq: u32) -> Self {
        self.value = value.into();
        self.seq = seq;
        self
    }

    pub fn placeholder(mut self, placeholder: impl Into<String>) -> Self {
        self.placeholder = placeholder.into();
        self
    }

    /// In the terminal's font.
    pub fn mono(mut self) -> Self {
        self.font = Font::Mono;
        self
    }

    pub fn max(mut self, max: u32) -> Self {
        self.max = max;
        self
    }

    /// Moves the keyboard here, once for each new `seq`, while the panel
    /// has it.
    pub fn focus(mut self, seq: u32) -> Self {
        self.focus = Some(seq);
        self
    }

    /// The icon before its text, on one line: one of
    /// [`FIELD_ICONS`](crate::FIELD_ICONS).
    pub fn icon(mut self, icon: impl Into<String>) -> Self {
        self.icon = Some(icon.into());
        self
    }

    /// A button at its end, on one line, that empties it.
    pub fn clear(mut self) -> Self {
        self.clear = true;
        self
    }
}

/// A field's text as its plugin keeps it: what the field last said it
/// holds, and how many times the plugin put text in it, which
/// [`field`](Self::field) sends as its `seq`. So what the plugin puts there
/// is taken once, and frames drawn while the user types do not undo it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FieldText {
    text: String,
    seq: u32,
}

impl FieldText {
    pub fn text(&self) -> &str {
        &self.text
    }

    /// What the field said it holds -- an `Input::Text`'s or an
    /// `Input::Submit`'s -- unless it was typed over text this has put
    /// there since: the field takes that text, and says what it holds
    /// again. True when it was taken.
    pub fn heard(&mut self, typed: &crate::wire::Typed) -> bool {
        if typed.seq != self.seq {
            return false;
        }
        self.text = typed.text.clone();
        true
    }

    /// Puts `text` in the field, over what was typed.
    pub fn set(&mut self, text: impl Into<String>) {
        self.text = text.into();
        self.seq = self.seq.wrapping_add(1);
    }

    pub fn clear(&mut self) {
        self.set(String::new());
    }

    /// The field holding it, at its place.
    pub fn field(&self, id: impl Into<String>, x: f32, y: f32, w: f32, h: f32) -> Field {
        Field::new(id, x, y, w, h).value(self.text.clone(), self.seq)
    }
}

macro_rules! into_item {
    ($($kind:ident),*) => {
        $(impl From<$kind> for Item {
            fn from(item: $kind) -> Self {
                Self::$kind(item)
            }
        })*
    };
}

into_item!(Rect, Text, Line, Area, Hit, Scroll, List, Field);

impl Color {
    pub const fn rgb(r: u8, g: u8, b: u8) -> Self {
        Self::Rgba([r, g, b, 255])
    }

    pub const fn rgba(r: u8, g: u8, b: u8, a: u8) -> Self {
        Self::Rgba([r, g, b, a])
    }
}

fn one() -> f32 {
    1.0
}

fn is_zero(value: &f32) -> bool {
    *value == 0.0
}

fn is_default<T: Default + PartialEq>(value: &T) -> bool {
    *value == T::default()
}

impl Item {
    /// The box it covers in its own units, for telling whether it shows:
    /// a line's with room for its width.
    pub fn bounds(&self) -> Bounds {
        match self {
            Self::Rect(rect) => Bounds::new(rect.x, rect.y, rect.w, rect.h),
            Self::Text(text) => Bounds::new(text.x, text.y, text.w, text.h),
            Self::Hit(hit) => Bounds::new(hit.x, hit.y, hit.w, hit.h),
            Self::Scroll(scroll) => Bounds::new(scroll.x, scroll.y, scroll.w, scroll.h),
            Self::List(list) => Bounds::new(list.x, list.y, list.w, list.h),
            Self::Field(field) => Bounds::new(field.x, field.y, field.w, field.h),
            Self::Line(line) => points_bounds(&line.points, None).grow(line.width / 2.0),
            Self::Area(area) => points_bounds(&area.points, Some(area.base)),
        }
    }
}

fn points_bounds(points: &[f32], base: Option<f32>) -> Bounds {
    let mut bounds = Bounds::EMPTY;
    for [x, y] in points.as_chunks::<2>().0 {
        bounds = bounds.include(*x, *y);
        if let Some(base) = base {
            bounds = bounds.include(*x, base);
        }
    }
    bounds
}

/// A box by its edges.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Bounds {
    pub left: f32,
    pub top: f32,
    pub right: f32,
    pub bottom: f32,
}

impl Bounds {
    /// Covers nothing, and grows from anything it includes.
    pub const EMPTY: Self = Self {
        left: f32::INFINITY,
        top: f32::INFINITY,
        right: f32::NEG_INFINITY,
        bottom: f32::NEG_INFINITY,
    };

    pub fn new(x: f32, y: f32, w: f32, h: f32) -> Self {
        Self {
            left: x,
            top: y,
            right: x + w.max(0.0),
            bottom: y + h.max(0.0),
        }
    }

    pub fn width(&self) -> f32 {
        (self.right - self.left).max(0.0)
    }

    pub fn height(&self) -> f32 {
        (self.bottom - self.top).max(0.0)
    }

    pub fn is_empty(&self) -> bool {
        !(self.right > self.left && self.bottom > self.top)
    }

    pub fn contains(&self, x: f32, y: f32) -> bool {
        x >= self.left && x < self.right && y >= self.top && y < self.bottom
    }

    /// Whether any of it lies within `other`. A line with no width still
    /// shows, so this is by edges rather than by area.
    pub fn touches(&self, other: &Bounds) -> bool {
        self.left <= other.right
            && self.right >= other.left
            && self.top <= other.bottom
            && self.bottom >= other.top
    }

    /// What the two have in common; empty when nothing.
    pub fn intersect(&self, other: &Bounds) -> Bounds {
        Bounds {
            left: self.left.max(other.left),
            top: self.top.max(other.top),
            right: self.right.min(other.right),
            bottom: self.bottom.min(other.bottom),
        }
    }

    pub fn offset(&self, dx: f32, dy: f32) -> Bounds {
        Bounds {
            left: self.left + dx,
            top: self.top + dy,
            right: self.right + dx,
            bottom: self.bottom + dy,
        }
    }

    fn include(self, x: f32, y: f32) -> Bounds {
        Bounds {
            left: self.left.min(x),
            top: self.top.min(y),
            right: self.right.max(x),
            bottom: self.bottom.max(y),
        }
    }

    fn grow(self, by: f32) -> Bounds {
        Bounds {
            left: self.left - by,
            top: self.top - by,
            right: self.right + by,
            bottom: self.bottom + by,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{json, to_value};

    fn read(value: serde_json::Value) -> Frame {
        serde_json::from_str(&value.to_string()).unwrap()
    }

    #[test]
    fn an_item_that_does_not_read_is_left_out_and_the_rest_stay() {
        let frame = read(json!({"items": [
            {"rect": {"x": null, "y": 0, "w": 1, "h": 1}},
            {"sparkle": {"x": 0}},
            {"hit": {"id": "kept", "x": 0, "y": 0, "w": 1, "h": 1}},
            {"scroll": {"id": "s", "x": 0, "y": 0, "w": 9, "h": 9, "height": 20, "items": [
                "not an item",
                {"text": {"x": 0, "y": 0, "w": 9, "h": 9, "text": "inside"}}
            ]}}
        ]}));
        assert_eq!(frame.items.len(), 2, "{frame:?}");
        let Item::Scroll(scroll) = &frame.items[1] else {
            panic!("{frame:?}")
        };
        assert_eq!(scroll.items.len(), 1);
    }

    #[test]
    fn items_are_json_named_by_kind() {
        let frame = read(json!({"items": [
            {"rect": {"x": 0, "y": 0, "w": 10, "h": 4, "fill": "bg-hover", "radius": 3}},
            {"text": {"x": 1, "y": 0, "w": 8, "h": 4, "text": "hi", "font": "mono", "align": "right"}},
            {"line": {"points": [0, 1, 2, 3], "color": "#ff0000"}},
            {"hit": {"id": "a", "x": 0, "y": 0, "w": 10, "h": 4}},
            {"list": {"id": "l", "x": 0, "y": 4, "w": 10, "h": 20, "count": 100, "row": 2, "key": "k"}},
        ]}));
        let Item::Text(text) = &frame.items[1] else {
            panic!("{frame:?}")
        };
        assert_eq!(text.color, Color::Token(Token::Text), "text by default");
        assert_eq!(
            (text.font, text.size, text.align),
            (Font::Mono, Size::Body, Align::Right)
        );
        let Item::Line(line) = &frame.items[2] else {
            panic!("{frame:?}")
        };
        assert_eq!(line.width, 1.0);
        assert_eq!(line.color, Color::Rgba([255, 0, 0, 255]));
        let Item::Hit(hit) = &frame.items[3] else {
            panic!("{frame:?}")
        };
        assert_eq!(hit.cursor, Cursor::Pointer);

        // What is the default is left out.
        assert_eq!(
            to_value(&frame.items[1]).unwrap(),
            json!({"text": {"x": 1.0, "y": 0.0, "w": 8.0, "h": 4.0, "text": "hi", "font": "mono", "align": "right"}})
        );
        assert!(!frame.extend);
        assert!(to_value(&frame).unwrap().get("extend").is_none());
        let mut extending = read(json!({"items": [], "extend": true}));
        assert!(extending.extend);
        extending.extend(false);
        assert_eq!(to_value(&extending).unwrap(), json!({"items": []}));
    }

    #[test]
    fn a_field_and_the_keys_a_frame_takes_are_json_too() {
        let frame = read(json!({
            "items": [
                {"field": {"id": "add", "x": 8, "y": 4, "w": 200, "h": 26, "placeholder": "Symbol", "icon": "plus"}},
                {"field": {"id": "note", "x": 8, "y": 40, "w": 200, "h": 80, "kind": "lines", "value": "a\nb", "seq": 2, "focus": 1}}
            ],
            "keys": ["ArrowDown", "j"],
            "release": 3
        }));
        let Item::Field(add) = &frame.items[0] else {
            panic!("{frame:?}")
        };
        assert_eq!(
            (add.kind, add.seq, add.font, add.max, add.focus),
            (FieldKind::Line, 0, Font::Ui, 0, None)
        );
        assert_eq!(add.icon.as_deref(), Some("plus"));
        let Item::Field(note) = &frame.items[1] else {
            panic!("{frame:?}")
        };
        assert_eq!(
            (note.kind, note.value.as_str(), note.seq, note.focus),
            (FieldKind::Lines, "a\nb", 2, Some(1))
        );
        assert_eq!(frame.keys, ["ArrowDown", "j"]);
        assert_eq!(frame.release, Some(3));
        assert_eq!(
            to_value(Field::new("pass", 0.0, 0.0, 10.0, 10.0).secret()).unwrap(),
            json!({"id": "pass", "x": 0.0, "y": 0.0, "w": 10.0, "h": 10.0, "kind": "secret"}),
            "what is the default is left out"
        );
        let mut plain = Frame::default();
        plain.keys(&[]);
        assert_eq!(to_value(&plain).unwrap(), json!({"items": []}));
    }

    #[test]
    fn a_field_s_text_is_sent_again_only_when_the_plugin_puts_some_there() {
        let mut text = FieldText::default();
        let first = text.field("add", 0.0, 0.0, 10.0, 10.0);
        let typed = |text: &str, seq: u32| crate::wire::Typed {
            id: "add".into(),
            text: text.into(),
            seq,
        };
        assert!(text.heard(&typed("TS", first.seq)));
        let shown = text.field("add", 0.0, 0.0, 10.0, 10.0);
        assert_eq!((shown.value.as_str(), shown.seq), ("TS", first.seq));
        text.clear();
        let cleared = text.field("add", 0.0, 0.0, 10.0, 10.0);
        assert_eq!((cleared.value.as_str(), cleared.seq), ("", first.seq + 1));
        assert_eq!(text.text(), "");
        // Typed over the text before it was emptied: the field takes the
        // empty one, so the plugin keeps it too.
        assert!(!text.heard(&typed("TSx", first.seq)));
        assert_eq!(text.text(), "");
        assert!(text.heard(&typed("x", first.seq + 1)));
        assert_eq!(text.text(), "x");
    }

    #[test]
    fn colours_are_names_or_hex_and_the_unknown_is_text() {
        assert_eq!(Color::parse("negative-bg"), Color::Token(Token::NegativeBg));
        assert_eq!(Color::parse("#0a0b0c"), Color::Rgba([10, 11, 12, 255]));
        assert_eq!(Color::parse("#0a0b0c80"), Color::Rgba([10, 11, 12, 128]));
        for unknown in ["sparkle", "#12", "#gg0000", "", "#ééé"] {
            assert_eq!(
                Color::parse(unknown),
                Color::Token(Token::Text),
                "{unknown}"
            );
        }
        for color in [
            Color::Rgba([1, 2, 3, 255]),
            Color::Rgba([1, 2, 3, 4]),
            Token::Accent.into(),
        ] {
            assert_eq!(Color::parse(&color.to_string()), color);
        }
        for token in Token::ALL {
            assert_eq!(Token::from_name(token.name()), Some(token));
        }
    }

    #[test]
    fn bounds_cover_what_an_item_draws() {
        let line = Item::Line(Line {
            points: vec![10.0, 5.0, 20.0, 1.0],
            width: 2.0,
            color: Color::default(),
        });
        assert_eq!(
            line.bounds(),
            Bounds {
                left: 9.0,
                top: 0.0,
                right: 21.0,
                bottom: 6.0
            }
        );
        let area = Item::Area(Area {
            points: vec![0.0, 5.0, 4.0, 2.0],
            base: 10.0,
            color: Color::default(),
            fade: true,
        });
        assert_eq!(
            area.bounds(),
            Bounds {
                left: 0.0,
                top: 2.0,
                right: 4.0,
                bottom: 10.0
            }
        );
        let flat = Bounds::new(0.0, 3.0, 5.0, 0.0);
        assert!(flat.touches(&Bounds::new(0.0, 0.0, 10.0, 10.0)));
        assert!(Bounds::new(0.0, 0.0, 1.0, 1.0)
            .intersect(&Bounds::new(2.0, 2.0, 1.0, 1.0))
            .is_empty());
    }
}
