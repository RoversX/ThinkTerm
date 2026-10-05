//! What passes between a panel and its plugin besides the frames: what the
//! plugin is told about the panel, what the user does in it, the rows of
//! its lists, and what it asks ThinkTerm to do on another machine.

use crate::scene::Item;
use base64::Engine;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use serde_json::value::RawValue;

/// What a plugin is told of a panel it draws: its size, how large
/// ThinkTerm's text is there, whether it is dark, the language ThinkTerm
/// speaks, and where the terminal beside it is. Sent when the panel opens,
/// and again whenever any of it changes.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Env {
    pub width: f32,
    pub height: f32,
    /// Device pixels to a unit, for lines meant to fall on whole pixels.
    pub scale: f32,
    pub dark: bool,
    pub small: TextMetrics,
    pub body: TextMetrics,
    pub title: TextMetrics,
    pub mono: MonoMetrics,
    /// ThinkTerm's language, as a tag: "en-US", "zh-CN". Empty from a
    /// ThinkTerm that does not say.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub locale: String,
    /// The directory of the terminal beside the panel -- the pane in focus
    /// in its window -- when that terminal runs on the machine the plugin
    /// does; none otherwise.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    /// Where that terminal runs when it is another machine, reached over
    /// SSH: what the plugin wants of its files and programs there, it asks
    /// ThinkTerm for ([`Ask`]).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub remote: Option<Remote>,
    /// Whether the panel would get its extended view were its frame to ask
    /// for one: the client shows them, and has the room. So a plugin knows
    /// whether to fit what it would put there into the panel itself. Never
    /// so for an extended view.
    #[serde(default, skip_serializing_if = "is_false")]
    pub can_extend: bool,
    /// In an extended view, the close button ThinkTerm draws over its top
    /// left corner: a plugin leaves that square empty, and lines its first
    /// row up with it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub close: Option<CloseButton>,
    /// What the client does besides drawing and sending clicks
    /// ([`feature`]): a plugin draws for what it has. One that does not
    /// say `fields` leaves fields out, and one that does not say `keys`
    /// never gives the panel the keyboard but through a field.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub features: Vec<String>,
}

/// The names of what a client can do, in [`Env::features`].
pub mod feature {
    /// It shows fields, and sends what is typed in them.
    pub const FIELDS: &str = "fields";
    /// It gives a panel the keyboard when the user asks, and sends the keys
    /// the panel takes.
    pub const KEYS: &str = "keys";
}

impl Env {
    /// Whether the client says it does `feature`.
    pub fn has(&self, feature: &str) -> bool {
        self.features.iter().any(|has| has == feature)
    }
}

/// The other machine the terminal beside a panel runs on.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Remote {
    /// The machine, by the name ThinkTerm shows the user.
    pub host: String,
    /// The machine, and the account ThinkTerm reaches it as, by a word to
    /// compare rather than show: two accounts on one host share a `host`,
    /// not this. What the plugin asks of it names it, and is done only
    /// while the terminal beside the panel is still there.
    pub machine: String,
    /// The terminal's directory there.
    pub cwd: String,
}

/// What a plugin asks ThinkTerm to do on the machine the terminal beside
/// one of its panels runs on, when that is another ([`Env::remote`]), over
/// ThinkTerm's own connection to it. Answered with an [`Answer`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum Ask {
    /// Runs `args` -- a program and its arguments -- in the directory
    /// `cwd`, keeping at most `limit` bytes of what it prints.
    Run {
        args: Vec<String>,
        cwd: String,
        limit: u64,
    },
    /// At most `limit` bytes of the file at `path`.
    Read { path: String, limit: u64 },
    /// What is at `path`, a link not followed.
    Stat { path: String },
}

/// What came of an [`Ask`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "result", rename_all = "snake_case")]
pub enum Answer {
    /// The program ran and ended with `status`, `None` when it was killed.
    /// `out` is what it printed, `cut` when it printed more than that.
    Ran {
        status: Option<i32>,
        out: Bytes,
        #[serde(default)]
        cut: bool,
    },
    /// The file's bytes, `cut` when it has more.
    Read {
        bytes: Bytes,
        #[serde(default)]
        cut: bool,
    },
    /// What is there, if anything is.
    Stat { entry: Option<Entry> },
    /// It could not be done, and why. With `connect`, ThinkTerm is not
    /// connected to the machine: the panel shows the user a button that
    /// connects it, and asking again after that may work.
    Failed {
        why: String,
        #[serde(default, skip_serializing_if = "is_false")]
        connect: bool,
    },
}

impl Answer {
    pub fn failed(why: impl Into<String>) -> Self {
        Self::Failed {
            why: why.into(),
            connect: false,
        }
    }
}

/// A file, a directory, a link or something else, as [`Ask::Stat`] finds it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Entry {
    pub kind: EntryKind,
    pub len: u64,
    /// When it last changed, in seconds since 1970.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub modified: Option<u64>,
    /// Where a link points.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EntryKind {
    File,
    Dir,
    Link,
    Other,
}

/// Bytes, carried in JSON as base64.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Bytes(pub Vec<u8>);

impl Serialize for Bytes {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&base64::engine::general_purpose::STANDARD.encode(&self.0))
    }
}

impl<'de> Deserialize<'de> for Bytes {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let text = std::borrow::Cow::<str>::deserialize(deserializer)?;
        base64::engine::general_purpose::STANDARD
            .decode(text.as_bytes())
            .map(Bytes)
            .map_err(serde::de::Error::custom)
    }
}

/// Where a close button sits, in a view's units: its top left corner, and
/// its side.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct CloseButton {
    pub x: f32,
    pub y: f32,
    pub size: f32,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct TextMetrics {
    /// The font's size.
    pub size: f32,
    /// How tall a line of it is.
    pub line: f32,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct MonoMetrics {
    pub size: f32,
    pub line: f32,
    /// How wide a column is: a character most scripts write in one, twice
    /// that for one they write in two.
    pub advance: f32,
}

/// Something the user did in the panel. Hovering and scrolling are not
/// among them: the player does both without asking. More may come: a
/// plugin lets be what it does not know.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum Input {
    /// A press on a hit, at `x`, `y` within it.
    Click(Click),
    /// The user closed an extended view with its close button. It goes
    /// whatever the plugin does, and its panel gets no other until its
    /// frames have stopped asking for one: this is for the plugin to stop.
    Close,
    /// What a field holds changed: typed, pasted or cut. Not sent for a
    /// secret field.
    Text(Typed),
    /// The user pressed Return in a field, Command-Return in one of lines:
    /// what it holds.
    Submit(Typed),
    /// The panel has the keyboard: in its field `id`, or in none of them.
    Focus(Focus),
    /// The keyboard went from the panel: where the user put it, or nowhere
    /// until they do.
    Blur,
    /// One of the keys the panel takes ([`Frame::keys`]), pressed while it
    /// has the keyboard. Escape gives the keyboard back, and Tab moves it
    /// between the panel's fields: neither is sent. Nor is a key pressed
    /// with Ctrl, Alt or Command, which are the app's, or what the field
    /// with the keyboard uses itself.
    ///
    /// [`Frame::keys`]: crate::scene::Frame::keys
    Key(Key),
}

/// A field, by its id, what it holds, and the `seq` of the text it was
/// typed over: one the plugin has put other text in since was typed over
/// what is gone ([`FieldText::heard`]).
///
/// [`FieldText::heard`]: crate::scene::FieldText::heard
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Typed {
    pub id: String,
    pub text: String,
    #[serde(default, skip_serializing_if = "is_zero")]
    pub seq: u32,
}

/// Where in a panel with the keyboard it is: in the field `id`, or none.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Focus {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
}

/// A key, named as a browser names it, and whether Shift was held -- the
/// one modifier sent with one.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Key {
    pub key: String,
    #[serde(default)]
    pub mods: Mods,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Click {
    pub id: String,
    pub x: f32,
    pub y: f32,
    #[serde(default)]
    pub button: Button,
    /// 2 for the second press of a double click.
    #[serde(default = "one")]
    pub count: u32,
    #[serde(default)]
    pub mods: Mods,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Button {
    #[default]
    Left,
    Middle,
    Right,
}

/// The modifier keys held. `cmd` is Command on a Mac, and the Windows or
/// Super key elsewhere.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Mods {
    #[serde(default, skip_serializing_if = "is_false")]
    pub shift: bool,
    #[serde(default, skip_serializing_if = "is_false")]
    pub ctrl: bool,
    #[serde(default, skip_serializing_if = "is_false")]
    pub alt: bool,
    #[serde(default, skip_serializing_if = "is_false")]
    pub cmd: bool,
}

/// Rows `from..to` of the list `list`, as drawn with `key` and `version`.
/// `layout` counts what makes rows drawn before out of date -- the panel's
/// size and fonts changing, the plugin starting again: it is only echoed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RowsWanted {
    pub list: String,
    pub key: String,
    #[serde(default, skip_serializing_if = "is_zero")]
    pub version: u32,
    #[serde(default, skip_serializing_if = "is_zero")]
    pub layout: u32,
    pub from: u32,
    pub to: u32,
}

/// Rows answering a [`RowsWanted`], its `list`, `key`, `version`, `layout`
/// and `from` repeated: `rows[0]` is row `from`. A row's items are in its
/// own units, from the list's left edge and the row's top. Scroll areas and
/// lists do not go in a row; an item that does not read is left out.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Rows {
    pub list: String,
    pub key: String,
    #[serde(default, skip_serializing_if = "is_zero")]
    pub version: u32,
    #[serde(default, skip_serializing_if = "is_zero")]
    pub layout: u32,
    pub from: u32,
    #[serde(deserialize_with = "rows")]
    pub rows: Vec<Vec<Item>>,
}

/// Rows, each keeping the items that read; a row that is not a list is
/// empty.
fn rows<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Vec<Vec<Item>>, D::Error> {
    let raw: Vec<Box<RawValue>> = Vec::deserialize(deserializer)?;
    Ok(raw
        .iter()
        .map(|row| {
            let mut read = serde_json::Deserializer::from_str(row.get());
            crate::scene::items(&mut read).unwrap_or_default()
        })
        .collect())
}

fn one() -> u32 {
    1
}

fn is_false(value: &bool) -> bool {
    !value
}

fn is_zero(value: &u32) -> bool {
    *value == 0
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{json, to_value};

    #[test]
    fn input_is_json_named_by_kind() {
        let click = Input::Click(Click {
            id: "file:2".into(),
            x: 3.0,
            y: 4.5,
            button: Button::Left,
            count: 1,
            mods: Mods {
                shift: true,
                ..Mods::default()
            },
        });
        let value = to_value(&click).unwrap();
        assert_eq!(
            value,
            json!({"click": {"id": "file:2", "x": 3.0, "y": 4.5, "button": "left", "count": 1, "mods": {"shift": true}}})
        );
        let bare: Input =
            serde_json::from_value(json!({"click": {"id": "a", "x": 0, "y": 0}})).unwrap();
        let Input::Click(bare) = bare else {
            panic!("{bare:?}")
        };
        assert_eq!(
            (bare.count, bare.button, bare.mods),
            (1, Button::Left, Mods::default())
        );
        assert_eq!(to_value(Input::Close).unwrap(), json!("close"));
        assert_eq!(
            serde_json::from_value::<Input>(json!("close")).unwrap(),
            Input::Close
        );

        let typed = Typed {
            id: "add".into(),
            text: "TSM".into(),
            seq: 0,
        };
        assert_eq!(
            to_value(Input::Submit(typed.clone())).unwrap(),
            json!({"submit": {"id": "add", "text": "TSM"}})
        );
        let over = Typed {
            seq: 2,
            ..typed.clone()
        };
        assert_eq!(
            to_value(Input::Text(over)).unwrap(),
            json!({"text": {"id": "add", "text": "TSM", "seq": 2}}),
            "typed over the text the plugin put there the second time"
        );
        assert_eq!(
            to_value(Input::Text(typed)).unwrap(),
            json!({"text": {"id": "add", "text": "TSM"}})
        );
        assert_eq!(
            to_value(Input::Focus(Focus::default())).unwrap(),
            json!({"focus": {}}),
            "the panel, in none of its fields"
        );
        assert_eq!(to_value(Input::Blur).unwrap(), json!("blur"));
        let key: Input = serde_json::from_value(json!({"key": {"key": "ArrowDown"}})).unwrap();
        assert_eq!(
            key,
            Input::Key(Key {
                key: "ArrowDown".into(),
                mods: Mods::default()
            })
        );
    }

    #[test]
    fn an_env_says_what_its_client_does() {
        let env: Env = serde_json::from_value(json!({
            "width": 300, "height": 500, "scale": 2, "dark": true,
            "small": {"size": 11, "line": 15},
            "body": {"size": 13, "line": 18},
            "title": {"size": 15, "line": 20},
            "mono": {"size": 12, "line": 17, "advance": 7},
            "features": ["fields", "someday"]
        }))
        .unwrap();
        assert!(env.has(feature::FIELDS));
        assert!(!env.has(feature::KEYS));
        let older = Env {
            features: Vec::new(),
            ..env
        };
        assert!(
            to_value(&older).unwrap().get("features").is_none(),
            "a client that does none of it says nothing"
        );
    }

    #[test]
    fn asks_and_answers_are_json_named_by_what_they_are() {
        let run = Ask::Run {
            args: vec!["git".into(), "status".into()],
            cwd: "/home/user/repo".into(),
            limit: 1024,
        };
        assert_eq!(
            to_value(&run).unwrap(),
            json!({"op": "run", "args": ["git", "status"], "cwd": "/home/user/repo", "limit": 1024})
        );
        let ran = Answer::Ran {
            status: Some(0),
            out: Bytes(b"a\0\xff".to_vec()),
            cut: false,
        };
        let sent = to_value(&ran).unwrap();
        assert_eq!(
            sent,
            json!({"result": "ran", "status": 0, "out": "YQD/", "cut": false})
        );
        assert_eq!(
            serde_json::from_value::<Answer>(sent).unwrap(),
            ran,
            "bytes round trip"
        );
        assert_eq!(
            to_value(Answer::failed("no")).unwrap(),
            json!({"result": "failed", "why": "no"})
        );
        let stat: Answer = serde_json::from_value(json!({
            "result": "stat",
            "entry": {"kind": "link", "len": 7, "target": "elsewhere"}
        }))
        .unwrap();
        let Answer::Stat { entry: Some(entry) } = stat else {
            panic!("{stat:?}")
        };
        assert_eq!((entry.kind, entry.modified), (EntryKind::Link, None));
        assert!(serde_json::from_value::<Answer>(
            json!({"result": "ran", "status": 0, "out": "not base64!"})
        )
        .is_err());
    }

    #[test]
    fn a_row_keeps_what_reads_of_it_and_its_place() {
        let rows: Rows = serde_json::from_value(json!({
            "list": "l", "key": "k", "layout": 3, "from": 10,
            "rows": [
                [{"rect": {"x": 0, "y": 0, "w": null, "h": 1}}, {"hit": {"id": "a", "x": 0, "y": 0, "w": 1, "h": 1}}],
                "not a row",
                []
            ]
        }))
        .unwrap();
        assert_eq!((rows.version, rows.layout, rows.from), (0, 3, 10));
        let lengths: Vec<usize> = rows.rows.iter().map(Vec::len).collect();
        assert_eq!(lengths, [1, 0, 0], "each row stays where it was");
        let sent = to_value(&rows).unwrap();
        assert!(sent.get("version").is_none(), "0 is left out: {sent}");
        assert_eq!(sent["layout"], 3);
    }
}
