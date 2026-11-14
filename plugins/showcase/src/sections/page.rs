//! This page: what ThinkTerm keeps of the page -- its id, what it is opened
//! with, its tab's name and icon -- where it was opened from, what the
//! view's environment says, and buttons that type in that terminal and
//! read what it shows, with `thinkterm cli` run on a thread of the
//! plugin's own, and one that opens another page from this one.

use crate::chrome;
use crate::ui::{self, Kind, Look, Rgn, GAP, PAD};
use crate::Section;
use thinkterm_plugin_sdk::panel::{feature, Item, Pointer, Rect, Scroll, Size, Text, Token};
use thinkterm_plugin_sdk::View;

/// What the greeting types, without pressing Return: the user runs it, or
/// does not.
pub const GREETING: &str = "echo \"Hello from the Showcase plugin\"";
/// The most lines of the terminal kept to show, and of each line.
const SCREEN_LINES: usize = 12;
const LINE_CHARS: usize = 200;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Command {
    Greet,
    Read,
}

/// What came of a command run in the terminal.
#[derive(Debug, Clone, PartialEq)]
pub struct Ran {
    pub what: Command,
    /// The lines read, for a read; why it failed.
    pub result: Result<Vec<String>, String>,
}

#[derive(Debug, Default)]
pub struct State {
    /// A command on its way.
    busy: Option<Command>,
    status: Option<(String, Token)>,
    screen: Vec<String>,
}

impl State {
    /// What a command run for this view came back with.
    pub fn heard(&mut self, ran: Ran) {
        self.busy = None;
        self.status = Some(match (ran.what, ran.result) {
            (Command::Greet, Ok(_)) => (
                "Typed into the terminal. Return is yours to press.".to_string(),
                Token::Positive,
            ),
            (Command::Read, Ok(lines)) => {
                let plural = if lines.len() == 1 { "" } else { "s" };
                let said = format!("Read {} line{plural} off the screen", lines.len());
                self.screen = lines;
                (said, Token::Positive)
            }
            (_, Err(why)) => (why, Token::Negative),
        });
    }
}

/// A click on `id`, past "page:": the command to run, if any.
pub fn click(state: &mut State, id: &str) -> Option<Command> {
    let command = match id {
        "greet" => Command::Greet,
        "read" => Command::Read,
        _ => return None,
    };
    if state.busy.is_some() {
        return None;
    }
    state.busy = Some(command);
    Some(command)
}

/// Runs `what` with `cli`, `thinkterm cli` aimed at the terminal. Waits
/// for it: run it on a thread of the plugin's own.
pub fn run(mut cli: std::process::Command, what: Command) -> Ran {
    let args: &[&str] = match what {
        Command::Greet => &["send-text", "--no-paste", GREETING],
        Command::Read => &["get-text", "--trim"],
    };
    let result = match cli.args(args).output() {
        Err(err) => Err(format!("cannot run thinkterm cli: {err}")),
        Ok(out) if !out.status.success() => {
            let why = String::from_utf8_lossy(&out.stderr).trim().to_string();
            Err(ui::cut(&why, LINE_CHARS))
        }
        Ok(out) => {
            let text = String::from_utf8_lossy(&out.stdout);
            let lines: Vec<&str> = text
                .lines()
                .filter(|line| !line.trim().is_empty())
                .collect();
            let skip = lines.len().saturating_sub(SCREEN_LINES);
            Ok(lines[skip..]
                .iter()
                .map(|line| ui::cut(line, LINE_CHARS))
                .collect())
        }
    };
    Ran { what, result }
}

pub fn draw(items: &mut Vec<Item>, look: &Look, r: Rgn, state: &State, view: &View) {
    let env = look.env;
    let top = chrome::header(items, look, r, Section::Page, 0.0);
    let w = r.w - PAD * 2.0;
    let columns = if w >= 700.0 { 2 } else { 1 };
    let col_w = (w - PAD * (columns - 1) as f32) / columns as f32;
    let mut inside = Vec::new();
    let mut heights = vec![0.0_f32; columns];

    let opened = opened_from(view);
    let kept = kept(view);
    let environment = environment(view);
    let parts: [(&str, &[Info]); 3] = [
        ("Opened from", &opened),
        ("What ThinkTerm keeps", &kept),
        ("This view's environment", &environment),
    ];
    let mut place = |items: &mut Vec<Item>, tall: &dyn Fn(&mut Vec<Item>, Rgn) -> f32| {
        let col = if columns == 2 && heights[1] < heights[0] {
            1
        } else {
            0
        };
        let x = PAD + col as f32 * (col_w + PAD);
        let y = heights[col];
        let mut body = Vec::new();
        let h = tall(
            &mut body,
            Rgn::new(x + PAD, y + PAD, col_w - PAD * 2.0, 0.0),
        ) + PAD * 2.0;
        ui::card(items, Rgn::new(x, y, col_w, h));
        items.append(&mut body);
        heights[col] = y + h + GAP * 2.0;
    };
    // The terminal's card first: it is what can be done here.
    place(&mut inside, &|items: &mut Vec<Item>, r: Rgn| {
        terminal(items, look, r, state, view)
    });
    for (title, rows) in parts {
        place(&mut inside, &|items: &mut Vec<Item>, r: Rgn| {
            let mut y = r.y + ui::card_title(items, look, r, title);
            for (label, value, mono) in rows {
                y += info(items, look, r.x, y, r.w, label, value, *mono);
            }
            y - GAP - r.y
        });
    }
    let height = heights.iter().copied().fold(0.0, f32::max);
    let view_h = (r.bottom() - top).max(0.0);
    items.push(Scroll::new("page", r.x, top, r.w, view_h, height, inside).into());
    let _ = env;
}

/// A label, its value, and whether the value is in the terminal's font.
type Info = (&'static str, String, bool);

/// A label and its value under it; says how tall the two are.
#[allow(clippy::too_many_arguments)]
fn info(
    items: &mut Vec<Item>,
    look: &Look,
    x: f32,
    y: f32,
    w: f32,
    label: &str,
    value: &str,
    mono: bool,
) -> f32 {
    let env = look.env;
    ui::caption(items, look, x, y, w, label);
    let line = if mono { env.mono.line } else { env.body.line };
    let mut text = Text::new(x, y + env.small.line, w, line, value);
    if mono {
        text = text.mono();
    }
    items.push(text.into());
    env.small.line + line + GAP
}

fn opened_from(view: &View) -> Vec<Info> {
    let env = &view.env;
    let mut rows = vec![(
        "Directory",
        env.cwd
            .clone()
            .or_else(|| env.remote.as_ref().map(|remote| remote.cwd.clone()))
            .unwrap_or_else(|| "None: opened from no terminal".to_string()),
        true,
    )];
    match &env.terminal {
        Some(terminal) => {
            rows.push(("Terminal", format!("pane {}", terminal.pane), false));
            rows.push((
                "Reached through",
                terminal
                    .socket
                    .clone()
                    .unwrap_or_else(|| "the session server (--prefer-mux)".to_string()),
                true,
            ));
        }
        None => rows.push((
            "Terminal",
            "None. After a restart a page keeps its directory, not its terminal.".to_string(),
            false,
        )),
    }
    if let Some(remote) = &env.remote {
        rows.push(("Machine", remote.host.clone(), false));
    }
    rows
}

fn kept(view: &View) -> Vec<Info> {
    let Some(page) = &view.page else {
        return vec![("Page", "This is not a page".to_string(), false)];
    };
    let args = match &page.args {
        serde_json::Value::Null => "{}".to_string(),
        args => args.to_string(),
    };
    vec![
        ("Page id", page.id.clone(), true),
        ("Opened with", ui::cut(&args, 120), true),
        (
            "Tab",
            format!(
                "Showcase \u{00b7} {}, icon {}",
                Section::Page.title(),
                Section::Page.icon()
            ),
            false,
        ),
    ]
}

fn environment(view: &View) -> Vec<Info> {
    let env = &view.env;
    let features = match env.features.is_empty() {
        true => "none".to_string(),
        false => env.features.join(", "),
    };
    let wraps = if env.has(feature::WRAP) {
        ""
    } else {
        " (no wrapping)"
    };
    vec![
        (
            "Size",
            format!(
                "{:.0} \u{00d7} {:.0} at {}\u{00d7}",
                env.width, env.height, env.scale
            ),
            false,
        ),
        (
            "Theme, language, pointer",
            format!(
                "{} \u{00b7} {} \u{00b7} {}",
                if env.dark { "dark" } else { "light" },
                if env.locale.is_empty() {
                    "not said"
                } else {
                    &env.locale
                },
                if env.pointer == Pointer::Touch {
                    "finger"
                } else {
                    "mouse"
                }
            ),
            false,
        ),
        ("Client does", format!("{features}{wraps}"), false),
        (
            "Text, size / line",
            format!(
                "small {}/{} \u{00b7} body {}/{} \u{00b7} title {}/{}",
                env.small.size,
                env.small.line,
                env.body.size,
                env.body.line,
                env.title.size,
                env.title.line
            ),
            false,
        ),
        (
            "Mono",
            format!(
                "{}/{}, a column {:.2} wide",
                env.mono.size, env.mono.line, env.mono.advance
            ),
            false,
        ),
    ]
}

/// The terminal's card: type in it, read it, open another page.
fn terminal(items: &mut Vec<Item>, look: &Look, r: Rgn, state: &State, view: &View) -> f32 {
    let env = look.env;
    let mut y = r.y + ui::card_title(items, look, r, "The terminal it came from");
    let reach = view.env.terminal.is_some();
    let mut flow = ui::Flow::new(r.x, y, r.w);
    let h = look.control();
    let buttons = [
        (
            "page:greet",
            if state.busy == Some(Command::Greet) {
                "Typing\u{2026}"
            } else {
                "Type a greeting"
            },
            reach,
            Kind::Accent,
        ),
        (
            "page:read",
            if state.busy == Some(Command::Read) {
                "Reading\u{2026}"
            } else {
                "Read the screen"
            },
            reach,
            Kind::Plain,
        ),
        ("open:page", "Open another page", true, Kind::Plain),
    ];
    for (id, label, can, kind) in buttons {
        let bw = ui::button_w(look, label);
        let (bx, by) = flow.place(bw, h);
        let kind = if can && state.busy.is_none() {
            kind
        } else {
            Kind::Disabled
        };
        ui::button(items, look, id, bx, by, bw.min(r.w), label, kind);
    }
    y = flow.bottom() + GAP;
    let (said, color) = match (&state.status, reach) {
        (Some((said, color)), _) => (said.clone(), *color),
        (None, true) => (format!("Types {GREETING}"), Token::TextMuted),
        (None, false) => (
            "Opened from no terminal: there is nothing to type in.".to_string(),
            Token::TextMuted,
        ),
    };
    y += ui::paragraph(items, look, r.x, y, r.w, &said, Size::Small, color, 2) + GAP;
    if !state.screen.is_empty() {
        let line = env.mono.line;
        let box_h = line * state.screen.len() as f32 + 16.0;
        items.push(
            Rect::new(r.x, y, r.w, box_h)
                .fill(Token::Bg)
                .border(Token::Border)
                .radius(6.0)
                .into(),
        );
        for (n, said) in state.screen.iter().enumerate() {
            items.push(
                Text::new(
                    r.x + 10.0,
                    y + 8.0 + n as f32 * line,
                    r.w - 20.0,
                    line,
                    said.clone(),
                )
                .mono()
                .color(Token::TextMuted)
                .into(),
            );
        }
        y += box_h + GAP;
    }
    y - GAP - r.y
}
