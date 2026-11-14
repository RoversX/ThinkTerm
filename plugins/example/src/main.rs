//! An example ThinkTerm plugin: two calls and a panel, in one small
//! program. To make one of your own, copy this directory, change the id in
//! plugin.toml and the name in Cargo.toml, and change what the calls do.
//! docs/thinkterm/plugins.md says how to install it.
//!
//! - `{"op": "decode", "text": "…"}` answers with what the text decodes
//!   to, or `null`:
//!   `thinkterm plugin call text-tools '{"op":"decode","text":"aGk="}'`.
//! - `{"op": "uuid"}` answers with a new UUID.
//! - Two actions (`[[action]]` in plugin.toml), which the command palette
//!   lists and a key can be bound to: one types a new UUID in the terminal
//!   the user is at (`At`), one opens the page.
//! - The panel decodes what is typed in it, or the lowest line that decodes
//!   on the screen of the terminal beside it (`View::cli`), and shows what
//!   that decodes to in as many lines as it has room for (`Text::wrap`).
//! - The same is a page (`[page]` in plugin.toml): a tab of its own, or a
//!   pane beside a terminal, opened from the new tab button's menu, with
//!   `thinkterm cli open-page text-tools`, or by the panel's own button
//!   (`Frame::open_page`), which hands the page what is typed in the panel
//!   to start with (`View::page`). What is typed in the page it keeps as
//!   what it opens with (`Frame::change_page`), so it comes back with it:
//!   after it was hidden a while, after a restart, on another device.

use anyhow::{bail, Context as _};
use base64::alphabet;
use base64::engine::general_purpose::{GeneralPurpose, GeneralPurposeConfig};
use base64::engine::DecodePaddingMode;
use base64::Engine as _;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use thinkterm_plugin_sdk::panel::{
    feature, Align, At, Click, FieldText, Frame, Hit, Input, PageChange, Rect, Size, Text, Token,
};
use thinkterm_plugin_sdk::{Cx, Emitter, Plugin, View};

const PAD: f32 = 12.0;
/// The panel's field, its button that reads the terminal, and the one
/// that opens the page.
const TYPED: &str = "typed";
const READ: &str = "read";
const OPEN: &str = "open";

struct TextTools {
    /// What each panel decodes: typed in its field, or put there from the
    /// terminal beside it.
    typed: HashMap<u64, FieldText>,
    /// Lines read off a panel's terminal, on their way to its field, with
    /// the turn of the panel's they answer.
    read: Arc<Mutex<Vec<(u64, u64, String)>>>,
    /// Each panel's turn: it moves on with each read asked for and each
    /// thing typed, and a line read for an earlier one is dropped -- what
    /// the user typed since, or a later read, stands.
    turns: HashMap<u64, u64>,
    /// Draws the panels again once a line is read; none in the tests.
    emitter: Option<Emitter>,
    /// The page each panel last asked for, by its turn among all of them,
    /// and what to open it with: said in every frame of the panel from
    /// then on, and opened once.
    pages: HashMap<u64, (u32, Value)>,
    asked: u32,
    /// What each page's field held when its page was last changed to open
    /// with it, by that change's turn among all of them: 0 for what it was
    /// opened with, which needs no change.
    kept: HashMap<u64, (u32, String)>,
    changes: u32,
}

impl Plugin for TextTools {
    fn call(&mut self, body: Value, _cx: &mut Cx) -> anyhow::Result<Value> {
        match body["op"].as_str() {
            Some("decode") => {
                let text = body["text"]
                    .as_str()
                    .context("\"decode\" needs a \"text\"")?;
                Ok(match decode(text.trim()) {
                    Some(decoded) => json!({"kind": decoded.kind, "text": decoded.text}),
                    None => Value::Null,
                })
            }
            Some("uuid") => Ok(json!(uuid::Uuid::new_v4().to_string())),
            // An action (plugin.toml): ThinkTerm adds where the user asked
            // for it, and the UUID is typed in the terminal there.
            Some("type-uuid") => {
                let terminal = At::of(&body)
                    .and_then(|at| at.terminal)
                    .context("\"type-uuid\" is asked for at a terminal")?;
                let uuid = uuid::Uuid::new_v4().to_string();
                let typed = thinkterm_plugin_sdk::cli(&terminal)
                    .args(["send-text", "--no-paste", &uuid])
                    .output()
                    .context("running thinkterm cli")?;
                if !typed.status.success() {
                    bail!("{}", String::from_utf8_lossy(&typed.stderr).trim());
                }
                Ok(json!(uuid))
            }
            _ => bail!("unknown op; this plugin knows \"decode\", \"uuid\" and \"type-uuid\""),
        }
    }

    fn draw(&mut self, view: &View, frame: &mut Frame) {
        let turn = self.turns.get(&view.id).copied().unwrap_or_default();
        // A page opened with a text starts with it in its field.
        let opened_with = match self.typed.contains_key(&view.id) {
            true => None,
            false => view.page.as_ref().and_then(|page| page.args["text"].as_str()),
        };
        let typed = self.typed.entry(view.id).or_default();
        if let Some(text) = opened_with {
            typed.set(text);
        }
        let mut read = self
            .read
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        read.retain(|(of, answers, line)| {
            if *of == view.id && *answers == turn {
                typed.set(line.as_str());
            }
            *of != view.id
        });
        drop(read);
        draw(typed, view, frame);
        if let Some((seq, args)) = self.pages.get(&view.id) {
            frame.open_page(*seq, args.clone());
        }
        // A page opens with what was last typed in it.
        if view.is_page() {
            let text = typed.text().trim();
            match self.kept.get(&view.id) {
                None => {
                    self.kept.insert(view.id, (0, text.to_string()));
                }
                Some((_, kept)) if kept != text => {
                    self.changes += 1;
                    self.kept.insert(view.id, (self.changes, text.to_string()));
                }
                Some(_) => {}
            }
            if let Some((seq, text)) = self.kept.get(&view.id).filter(|(seq, _)| *seq > 0) {
                let args = match text.as_str() {
                    "" => json!({}),
                    text => json!({ "text": text }),
                };
                frame.change_page(PageChange {
                    seq: *seq,
                    args: Some(args),
                    ..Default::default()
                });
            }
        }
    }

    fn input(&mut self, view: &View, input: Input, _cx: &mut Cx) {
        match input {
            Input::Text(text) | Input::Submit(text) if text.id == TYPED => {
                *self.turns.entry(view.id).or_default() += 1;
                self.typed.entry(view.id).or_default().heard(&text);
            }
            Input::Click(Click { id, .. }) if id == READ => {
                let Some(cli) = view.cli() else {
                    return;
                };
                let turn = self.turns.entry(view.id).or_default();
                *turn += 1;
                // Off the plugin's thread: drawing does not wait for it.
                let (read, emitter, of, turn) =
                    (Arc::clone(&self.read), self.emitter.clone(), view.id, *turn);
                std::thread::spawn(move || {
                    if let Some(line) = lowest_decodable_line(cli) {
                        read.lock()
                            .unwrap_or_else(|poisoned| poisoned.into_inner())
                            .push((of, turn, line));
                        if let Some(emitter) = emitter {
                            emitter.redraw();
                        }
                    }
                });
            }
            Input::Click(Click { id, .. }) if id == OPEN => {
                let text = self.typed.get(&view.id).map_or("", |typed| typed.text());
                let args = match text.trim() {
                    "" => Value::Null,
                    text => json!({ "text": text }),
                };
                self.asked += 1;
                self.pages.insert(view.id, (self.asked, args));
            }
            _ => {}
        }
    }

    fn closed(&mut self, view: &View) {
        self.typed.remove(&view.id);
        self.turns.remove(&view.id);
        self.pages.remove(&view.id);
        self.kept.remove(&view.id);
        // A line read for it may be on its way still: it is dropped when it
        // comes. One that came already goes now.
        self.read
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .retain(|(of, ..)| *of != view.id);
    }
}

/// The panel: a field to type in, where the client has fields; a button
/// that reads the terminal beside it, where there is one; and what the
/// text decodes to.
fn draw(typed: &FieldText, view: &View, frame: &mut Frame) {
    let env = &view.env;
    let width = env.width - PAD * 2.0;
    let mut y = PAD;
    if env.has(feature::FIELDS) {
        let tall = env.body.line + 12.0;
        frame.push(
            typed
                .field(TYPED, PAD, y, width, tall)
                .placeholder("Text to decode")
                .clear(),
        );
        y += tall + 8.0;
    }
    if env.terminal.is_some() {
        let tall = env.body.line + 10.0;
        frame.push(
            Rect::new(PAD, y, width, tall)
                .fill(Token::BgRaised)
                .radius(6.0),
        );
        frame.push(
            Hit::new(READ, PAD, y, width, tall)
                .hover(Token::BgHover)
                .radius(6.0),
        );
        let label = Text::new(PAD, y, width, tall, "Decode from the terminal");
        frame.push(label.align(Align::Center));
        y += tall + 12.0;
    }
    // The panel is narrow: what is in it can be had as a page.
    if !view.is_page() {
        let tall = env.body.line + 10.0;
        frame.push(
            Hit::new(OPEN, PAD, y, width, tall)
                .hover(Token::BgHover)
                .radius(6.0),
        );
        let label = Text::new(PAD, y, width, tall, "Open as a page");
        frame.push(label.align(Align::Center).color(Token::TextMuted));
        y += tall + 12.0;
    }
    let text = typed.text().trim();
    if text.is_empty() {
        return;
    }
    let Some(decoded) = decode(text) else {
        let none = Text::new(PAD, y, width, env.body.line, "Nothing here decodes");
        frame.push(none.color(Token::TextMuted));
        return;
    };
    let kind = Text::new(PAD, y, width, env.small.line, decoded.kind);
    frame.push(kind.size(Size::Small).color(Token::TextMuted));
    y += env.small.line + 4.0;
    // As many lines as the panel has room for, where the client breaks
    // them with the font it shows them in; one line where it cannot.
    if env.has(feature::WRAP) {
        let rows = ((env.height - y - PAD) / env.body.line).floor().max(1.0);
        frame.push(Text::new(PAD, y, width, rows * env.body.line, decoded.text).wrap());
    } else {
        frame.push(Text::new(PAD, y, width, env.body.line, decoded.text));
    }
}

/// The lowest line on the screen of the terminal `cli` reaches that
/// decodes: what a command just printed, under the line it was typed on.
fn lowest_decodable_line(mut cli: std::process::Command) -> Option<String> {
    let out = cli.args(["get-text", "--trim", "--unwrap"]).output().ok()?;
    if !out.status.success() {
        eprintln!("get-text: {}", String::from_utf8_lossy(&out.stderr).trim());
        return None;
    }
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .rev()
        .map(str::trim)
        .find(|line| decode(line).is_some())
        .map(str::to_string)
}

fn main() -> std::io::Result<()> {
    thinkterm_plugin_sdk::run_with(|emitter| TextTools {
        typed: HashMap::new(),
        read: Arc::default(),
        turns: HashMap::new(),
        emitter: Some(emitter),
        pages: HashMap::new(),
        asked: 0,
        kept: HashMap::new(),
        changes: 0,
    })
}

#[derive(Debug, PartialEq)]
struct Decoded {
    kind: &'static str,
    text: String,
}

/// What `text` decodes to, trying a JWT, a Unix time, URL encoding and
/// Base64 in that order.
fn decode(text: &str) -> Option<Decoded> {
    if text.is_empty() {
        return None;
    }
    jwt(text)
        .or_else(|| unix_time(text))
        .or_else(|| percent(text))
        .or_else(|| base64_text(text))
}

/// Base64 in either alphabet, padded or not.
fn base64_bytes(text: &str) -> Option<Vec<u8>> {
    let config =
        GeneralPurposeConfig::new().with_decode_padding_mode(DecodePaddingMode::Indifferent);
    GeneralPurpose::new(&alphabet::STANDARD, config)
        .decode(text)
        .or_else(|_| GeneralPurpose::new(&alphabet::URL_SAFE, config).decode(text))
        .ok()
}

/// A JSON Web Token: its header and payload, the signature left out.
fn jwt(text: &str) -> Option<Decoded> {
    let mut parts = text.split('.');
    let (header, payload, _signature) = (parts.next()?, parts.next()?, parts.next()?);
    if parts.next().is_some() {
        return None;
    }
    let object = |part: &str| {
        serde_json::from_slice::<Value>(&base64_bytes(part)?)
            .ok()
            .filter(Value::is_object)
    };
    let pretty = |value: &Value| serde_json::to_string_pretty(value).unwrap_or_default();
    Some(Decoded {
        kind: "JWT",
        text: format!(
            "{}\n{}",
            pretty(&object(header)?),
            pretty(&object(payload)?)
        ),
    })
}

/// Seconds (ten digits) or milliseconds (thirteen) since 1970, as a date.
fn unix_time(text: &str) -> Option<Decoded> {
    if !text.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    let seconds: i64 = match text.len() {
        10 => text.parse().ok()?,
        13 => text.parse::<i64>().ok()? / 1000,
        _ => return None,
    };
    let (days, time) = (seconds.div_euclid(86_400), seconds.rem_euclid(86_400));
    let (year, month, day) = civil_from_days(days);
    Some(Decoded {
        kind: "Unix time",
        text: format!(
            "{year:04}-{month:02}-{day:02} {:02}:{:02}:{:02} UTC",
            time / 3600,
            time / 60 % 60,
            time % 60
        ),
    })
}

/// The date `days` after 1970-01-01, by Howard Hinnant's `civil_from_days`.
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let day_of_era = z.rem_euclid(146_097);
    let year_of_era =
        (day_of_era - day_of_era / 1460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let shifted_month = (5 * day_of_year + 2) / 153;
    let day = (day_of_year - (153 * shifted_month + 2) / 5 + 1) as u32;
    let month = if shifted_month < 10 {
        shifted_month + 3
    } else {
        shifted_month - 9
    } as u32;
    (year_of_era + era * 400 + i64::from(month <= 2), month, day)
}

/// Text with `%XX` escapes in it, unescaped.
fn percent(text: &str) -> Option<Decoded> {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut escaped = false;
    let mut at = 0;
    while at < bytes.len() {
        let hex = bytes
            .get(at + 1..at + 3)
            .filter(|digits| bytes[at] == b'%' && digits.iter().all(u8::is_ascii_hexdigit));
        match hex {
            Some(digits) => {
                let digits = std::str::from_utf8(digits).ok()?;
                out.push(u8::from_str_radix(digits, 16).ok()?);
                escaped = true;
                at += 3;
            }
            None => {
                out.push(bytes[at]);
                at += 1;
            }
        }
    }
    if !escaped {
        return None;
    }
    Some(Decoded {
        kind: "URL encoding",
        text: String::from_utf8(out).ok()?,
    })
}

/// Base64 that decodes to readable text.
fn base64_text(text: &str) -> Option<Decoded> {
    let compact: String = text.split_ascii_whitespace().collect();
    if compact.len() < 4 {
        return None;
    }
    let decoded = String::from_utf8(base64_bytes(&compact)?).ok()?;
    if decoded
        .chars()
        .any(|c| c.is_control() && !matches!(c, '\n' | '\r' | '\t'))
    {
        return None;
    }
    Some(Decoded {
        kind: "Base64",
        text: decoded,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::engine::general_purpose::URL_SAFE_NO_PAD;

    fn decoded(text: &str) -> (&'static str, String) {
        let found = decode(text).unwrap_or_else(|| panic!("{text:?} did not decode"));
        (found.kind, found.text)
    }

    #[test]
    fn each_kind_decodes() {
        assert_eq!(
            decoded("aGVsbG8gd29ybGQ="),
            ("Base64", "hello world".into())
        );
        assert_eq!(decoded("aGVsbG8gd29ybGQ"), ("Base64", "hello world".into()));
        assert_eq!(decoded("a%20b%2Fc"), ("URL encoding", "a b/c".into()));
        assert_eq!(
            decoded("1700000000"),
            ("Unix time", "2023-11-14 22:13:20 UTC".into())
        );
        assert_eq!(
            decoded("1700000000123"),
            ("Unix time", "2023-11-14 22:13:20 UTC".into())
        );
        let token = format!(
            "{}.{}.signature",
            URL_SAFE_NO_PAD.encode(r#"{"alg":"HS256"}"#),
            URL_SAFE_NO_PAD.encode(r#"{"sub":"example"}"#)
        );
        let (kind, text) = decoded(&token);
        assert_eq!(kind, "JWT");
        assert!(text.contains("\"alg\": \"HS256\"") && text.contains("\"sub\": \"example\""));
    }

    #[test]
    fn what_is_not_encoded_is_not_decoded() {
        for text in ["", "test", "hello world", "12345", "100%", "%zz", "a.b.c"] {
            assert_eq!(decode(text), None, "{text:?}");
        }
    }

    fn tools() -> TextTools {
        TextTools {
            typed: HashMap::new(),
            read: Arc::default(),
            turns: HashMap::new(),
            emitter: None,
            pages: HashMap::new(),
            asked: 0,
            kept: HashMap::new(),
            changes: 0,
        }
    }

    #[test]
    fn the_panel_opens_the_page_with_what_is_typed_and_the_page_starts_with_it() {
        let env: thinkterm_plugin_sdk::panel::Env = serde_json::from_value(json!({
            "width": 300, "height": 500, "scale": 2, "dark": true,
            "small": {"size": 11, "line": 15},
            "body": {"size": 13, "line": 18},
            "title": {"size": 15, "line": 20},
            "mono": {"size": 12, "line": 17, "advance": 7},
            "features": ["fields"]
        }))
        .unwrap();
        let mut tools = tools();
        let panel = View::new(1, env.clone());
        let mut frame = Frame::default();
        tools.draw(&panel, &mut frame);
        assert!(frame.page.is_none(), "nothing asks for a page by itself");
        tools.typed.entry(1).or_default().set("aGk=");
        let press = Click {
            id: OPEN.into(),
            x: 1.0,
            y: 1.0,
            button: thinkterm_plugin_sdk::panel::Button::Left,
            count: 1,
            mods: Default::default(),
        };
        tools.input(&panel, Input::Click(press), &mut Cx::new());
        let mut frame = Frame::default();
        tools.draw(&panel, &mut frame);
        let asked = frame.page.expect("the press asks for the page");
        assert_eq!(asked.args, json!({"text": "aGk="}));

        // The page it opens: no button to open another, and its field
        // holds what it was opened with.
        let mut page = View::new(2, env);
        page.page = Some(thinkterm_plugin_sdk::Page {
            id: "0f3a9c2e5b7d1846".into(),
            args: asked.args,
        });
        let mut frame = Frame::default();
        tools.draw(&page, &mut frame);
        assert_eq!(tools.typed[&2].text(), "aGk=");
        let drawn = serde_json::to_string(&frame).unwrap();
        assert!(!drawn.contains("Open as a page"), "{drawn}");
        assert!(frame.page.is_none());
        assert!(frame.page_change.is_none(), "it opens with what it holds already");

        // Typed in, it is to open with that from then on.
        tools.typed.get_mut(&2).unwrap().set("MTcwMDAwMDAwMA==");
        let mut frame = Frame::default();
        tools.draw(&page, &mut frame);
        let change = frame.page_change.expect("what is typed is kept");
        assert_eq!(change.args, Some(json!({"text": "MTcwMDAwMDAwMA=="})));
        let mut again = Frame::default();
        tools.draw(&page, &mut again);
        assert_eq!(again.page_change.map(|again| again.seq), Some(change.seq), "said once");
        // Emptied, it opens with nothing.
        tools.typed.get_mut(&2).unwrap().set("");
        let mut frame = Frame::default();
        tools.draw(&page, &mut frame);
        assert_eq!(frame.page_change.and_then(|change| change.args), Some(json!({})));
    }

    #[test]
    fn a_line_read_for_an_earlier_turn_is_dropped() {
        let env = serde_json::from_value(json!({
            "width": 320.0, "height": 640.0, "scale": 2.0, "dark": true,
            "small": {"size": 11.0, "line": 15.0},
            "body": {"size": 13.0, "line": 17.0},
            "title": {"size": 15.0, "line": 20.0},
            "mono": {"size": 12.0, "line": 16.0, "advance": 7.2},
            "features": ["fields", "wrap"]
        }))
        .unwrap();
        let view = View::new(3, env);
        let mut plugin = tools();
        // A read asked for, its turn 1; then the user typed, turn 2.
        plugin.turns.insert(view.id, 2);
        plugin
            .read
            .lock()
            .unwrap()
            .push((view.id, 1, "aGk=".into()));
        plugin.draw(&view, &mut Frame::default());
        assert_eq!(plugin.typed[&view.id].text(), "");
        assert!(plugin.read.lock().unwrap().is_empty(), "let go of");
        // One read for the turn it is now is taken.
        plugin
            .read
            .lock()
            .unwrap()
            .push((view.id, 2, "aGk=".into()));
        plugin.draw(&view, &mut Frame::default());
        assert_eq!(plugin.typed[&view.id].text(), "aGk=");
    }

    #[test]
    fn the_calls() {
        let mut plugin = tools();
        let mut cx = Cx::new();
        assert_eq!(
            plugin
                .call(json!({"op": "decode", "text": " aGk= \n"}), &mut cx)
                .unwrap(),
            json!({"kind": "Base64", "text": "hi"})
        );
        assert_eq!(
            plugin
                .call(json!({"op": "decode", "text": ""}), &mut cx)
                .unwrap(),
            Value::Null
        );
        let made = plugin.call(json!({"op": "uuid"}), &mut cx).unwrap();
        let made = uuid::Uuid::parse_str(made.as_str().unwrap()).unwrap();
        assert_eq!(made.get_version_num(), 4);
        assert!(plugin.call(json!({"op": "nope"}), &mut cx).is_err());
    }
}
