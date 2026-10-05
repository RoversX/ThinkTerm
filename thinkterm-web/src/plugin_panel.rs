//! A plugin's panel in the right panel, as the page shows it: the view the
//! plugin host on the server's machine serves the page, through the server
//! (`PluginFrame`), and the player that shows it (thinkterm-plugin-panel).
//! The page paints the player's painting on a canvas of its own and hands
//! the pointer and the wheel back here; everything else is decided here.
//!
//! What is to go to the host comes back from each change as frames, for
//! the app to send on the way to the host it keeps. Times are the
//! platform's monotonic milliseconds.
//!
//! A panel's extended view, beside the right panel, is a model of its own,
//! opened as the extended view of the panel's opening: the app opens it
//! while the panel's frames ask for one.
//!
//! The page puts a field of its own over each field the player paints,
//! which the browser edits; what it holds, and where the keyboard goes,
//! come back here for the player to decide what the plugin hears. A panel
//! has the keyboard through its fields only: the page offers no key for it.

use serde::Serialize;
use thinkterm_plugin_channel::wire::{PanelEvent, PanelRequest, Raw, ToHost};
use thinkterm_plugin_panel::{Button, Env, Frame, Mods, Painting, Player, Rows};

/// How long a panel the host closed, but may serve again, waits before it
/// is opened anew: the plugin is being started again.
pub const REOPEN_MS: f64 = 400.0;
/// The least time between two sizes told to the plugin: a panel being
/// resized changes its size each frame, and each is drawn anew.
pub const ENV_EVERY_MS: f64 = 100.0;

/// Frames for the host, in order.
pub type Sends = Vec<Vec<u8>>;

/// What the page shows of the panel.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct PanelView {
    /// `starting`, `open` or `stopped`.
    pub state: &'static str,
    /// What the panel says instead of a painting: starting, or why it
    /// stopped.
    pub message: Option<String>,
    /// What to paint; none before the plugin first drew.
    pub painting: Option<Painting>,
    /// Whether its fields take typing: its program drew it on this
    /// opening. What one starting again last drew takes none.
    pub live: bool,
}

#[derive(Debug, Clone, PartialEq)]
enum Status {
    /// Opened, and not drawn on this opening yet.
    Opening,
    Open,
    /// Closed by the host or with the way to it; to be opened again from
    /// this time on.
    Again(f64),
    Stopped(String),
}

pub struct PanelModel {
    plugin: String,
    /// The page's number for this opening of it.
    view: u64,
    /// For an extended view, the page's number for the opening of the
    /// panel it extends.
    extends: Option<u64>,
    player: Player,
    status: Status,
    revision: u64,
    /// When the plugin was last told the panel's size, and whether the
    /// player has one since that it is still to be told: rows asked for
    /// meanwhile, which are drawn for it, wait until it is.
    env_told: f64,
    env_due: bool,
    /// Why what was sent for it did not reach the host, until the host is
    /// heard from again, and how many openings in a row that happened to:
    /// each waits twice as long before the next.
    trouble: Option<String>,
    failures: u32,
    /// The user closed its extended view with the close button: it is not
    /// opened again until its frames have stopped asking for one.
    dismissed: bool,
}

impl PanelModel {
    /// Plugin `plugin`'s panel, the size `env` says, opened as the page's
    /// panel number `view` at `now` -- the extended view of panel
    /// `extends`, with one: with the frame that opens it.
    pub fn open(plugin: &str, view: u64, extends: Option<u64>, env: Env, now: f64) -> (Self, Sends) {
        let model = Self {
            plugin: plugin.to_string(),
            view,
            extends,
            player: Player::new(env),
            status: Status::Opening,
            revision: 0,
            env_told: now,
            env_due: false,
            trouble: None,
            failures: 0,
            dismissed: false,
        };
        let open = model.open_frame();
        (model, vec![open])
    }

    pub fn plugin(&self) -> &str {
        &self.plugin
    }

    pub fn view(&self) -> u64 {
        self.view
    }

    pub fn revision(&self) -> u64 {
        self.revision
    }

    pub fn extends(&self) -> Option<u64> {
        self.extends
    }

    /// Whether the plugin has drawn it on this opening.
    pub fn drawn(&self) -> bool {
        self.status == Status::Open
    }

    /// Whether its frames ask for its extended view, and it is to have one:
    /// not once it cannot be served.
    pub fn wants_extended(&self) -> bool {
        self.player.shown() && self.player.extend() && !self.dismissed && !matches!(self.status, Status::Stopped(_))
    }

    /// Its extended view was closed with the close button.
    pub fn dismiss(&mut self) {
        self.dismissed = true;
        self.changed();
    }

    /// Tells the plugin the user closed this extended view with its close
    /// button: the frame, for the view is closed at once.
    pub fn closed_by_user(&self) -> Vec<u8> {
        self.frame(PanelRequest::Input { input: Raw::new(&thinkterm_plugin_panel::Input::Close) })
    }

    /// Whether it is waiting to be opened again.
    pub fn waiting(&self) -> bool {
        matches!(self.status, Status::Again(_))
    }

    /// How long until it is to be opened again, if it waits to be.
    pub fn again_in(&self, now: f64) -> Option<f64> {
        match self.status {
            Status::Again(at) => Some((at - now).max(0.0)),
            _ => None,
        }
    }

    pub fn view_json(&self) -> PanelView {
        let stopped = |reason: &str| {
            let mut args = thinkterm_i18n::FluentArgs::new();
            args.set("reason", reason.to_string());
            ("stopped", Some(thinkterm_i18n::tr_args("right-plugin-stopped", &args)))
        };
        let (state, message) = match (&self.status, &self.trouble) {
            (Status::Open, _) => ("open", None),
            (Status::Stopped(reason), _) => stopped(reason),
            // Opened again and again while the host cannot be reached: why.
            (Status::Opening | Status::Again(_), Some(why)) => stopped(why),
            (Status::Opening | Status::Again(_), None) if self.player.shown() => ("open", None),
            (Status::Opening | Status::Again(_), None) => {
                ("starting", Some(thinkterm_i18n::tr("right-plugin-starting")))
            }
        };
        PanelView {
            state,
            message,
            painting: (state != "stopped" && self.player.shown()).then(|| self.player.painting()),
            live: self.status == Status::Open,
        }
    }

    /// The panel's size, fonts or theme changed on the page: the plugin is
    /// told by [`tell_env`](Self::tell_env).
    pub fn set_env(&mut self, env: Env) {
        if self.player.set_env(env) {
            self.env_due = true;
            self.changed();
        }
    }

    /// Tells the plugin a size it is still to be told, and asks for the
    /// rows drawn for it, once the last was told long enough ago: the
    /// frames, and how long until then when it is not yet. One waiting to
    /// be opened again is told with the opening.
    pub fn tell_env(&mut self, now: f64) -> (Sends, Option<f64>) {
        if !self.env_due || self.waiting() {
            return (Vec::new(), None);
        }
        let due = self.env_told + ENV_EVERY_MS;
        if now < due {
            return (Vec::new(), Some(due - now));
        }
        self.env_told = now;
        self.env_due = false;
        let env = Raw::new(self.player.env());
        let mut sends = vec![self.frame(PanelRequest::Env { env })];
        sends.extend(self.wanted());
        (sends, None)
    }

    /// What the host said about the panel, at `now`.
    pub fn heard(&mut self, event: PanelEvent, now: f64) -> Sends {
        let mut sends = Vec::new();
        // Heard, so the host is reached.
        self.trouble = None;
        self.failures = 0;
        match event {
            PanelEvent::Frame { frame } => {
                // Taken in either way, so the next one can come.
                sends.push(self.frame(PanelRequest::Shown));
                match frame.read::<Frame>() {
                    Ok(frame) => {
                        // Once its frames stop asking, the next that asks
                        // opens it.
                        if !frame.extend {
                            self.dismissed = false;
                        }
                        self.player.frame(frame);
                        self.status = Status::Open;
                    }
                    Err(err) => log::warn!("plugin panel {}: a frame that does not read: {err}", self.plugin),
                }
                // The keyboard may have moved with it, or gone with a field.
                sends.extend(self.told());
            }
            PanelEvent::Rows { rows } => match rows.read::<Rows>() {
                Ok(rows) => self.player.rows(rows),
                Err(err) => log::warn!("plugin panel {}: rows that do not read: {err}", self.plugin),
            },
            PanelEvent::Closed { reason, again } => {
                self.status = if again {
                    Status::Again(now + REOPEN_MS)
                } else {
                    log::info!("plugin panel {}: {reason}", self.plugin);
                    Status::Stopped(reason)
                };
                self.let_go_of_keyboard();
            }
            // The page never says its terminal runs on another machine: a
            // plugin asking of one anyway is told so.
            PanelEvent::Remote { id, .. } => {
                let answer = thinkterm_plugin_panel::Answer::failed(
                    "the terminal beside this panel runs where the plugin does",
                );
                let answer = Raw::new(&answer);
                sends.push(self.frame(PanelRequest::Answer { id, answer }));
            }
        }
        sends.extend(self.wanted());
        self.changed();
        sends
    }

    /// The way to the host went, with the panel: it is opened again once
    /// there is one, from `at` on.
    pub fn lost(&mut self, at: f64) {
        if !matches!(self.status, Status::Stopped(_)) {
            self.status = Status::Again(at);
            self.changed();
        }
        self.let_go_of_keyboard();
    }

    /// Something sent for it did not reach the host, for `why` -- one that
    /// cannot be started, say: it is to be opened again, in two seconds,
    /// then twice as long each time again to half a minute, and says why
    /// meanwhile. How long until then; `None` when it is not to be.
    pub fn unsent(&mut self, why: &str, now: f64) -> Option<f64> {
        match self.status {
            Status::Stopped(_) => return None,
            // What else was sent with it went the same way.
            Status::Again(at) => return Some((at - now).max(0.0)),
            Status::Opening | Status::Open => {}
        }
        self.failures = self.failures.saturating_add(1);
        let delay = 1000.0 * f64::from(1u32 << self.failures.min(5)).min(30.0);
        self.status = Status::Again(now + delay);
        self.trouble = Some(why.to_string());
        self.let_go_of_keyboard();
        self.changed();
        Some(delay)
    }

    /// Opens it again at `now`, under a new number -- an extended view as
    /// that of the panel's opening `extends`: what the last opening asked
    /// will not be answered.
    pub fn reopen(&mut self, view: u64, extends: Option<u64>, now: f64) -> Sends {
        self.view = view;
        self.extends = extends;
        self.status = Status::Opening;
        self.env_told = now;
        self.env_due = false;
        self.player.restarted();
        // What was asked of the last opening is asked again on this one,
        // once the plugin has drawn on it: asked before, a plugin that
        // makes what it keeps for a view as it draws it has no rows yet.
        self.changed();
        vec![self.open_frame()]
    }

    /// The frame that lets it go.
    pub fn close(&self) -> Vec<u8> {
        self.frame(PanelRequest::Close)
    }

    /// The pointer moved to `x`, `y`. True when the panel is to be painted
    /// again.
    pub fn pointer(&mut self, x: f32, y: f32) -> bool {
        let moved = self.player.pointer_moved(x, y);
        if moved {
            self.changed();
        }
        moved
    }

    pub fn leave(&mut self) -> bool {
        let moved = self.player.pointer_left();
        if moved {
            self.changed();
        }
        moved
    }

    /// A press: what the plugin is to hear of it.
    pub fn click(&mut self, x: f32, y: f32, button: Button, count: u32, mods: Mods) -> Sends {
        let Some(input) = self.player.click(x, y, button, count, mods) else {
            return Vec::new();
        };
        self.changed();
        vec![self.frame(PanelRequest::Input { input: Raw::new(&input) })]
    }

    /// Whether the panel has the keyboard: one of its fields is the page's
    /// field with it.
    pub fn has_keyboard(&self) -> bool {
        self.player.has_keyboard()
    }

    /// The user put the keyboard in field `id`, pressing in it or tabbing
    /// to it.
    pub fn focus_field(&mut self, id: &str) -> Sends {
        if self.status != Status::Open {
            return Vec::new();
        }
        self.player.focus_field(id);
        self.changed();
        self.told()
    }

    /// What field `id`, with the keyboard, holds now the user edited it:
    /// what the plugin is to hear, and what the field is to hold instead,
    /// when it takes less -- fewer characters, one line.
    pub fn edit_field(&mut self, id: &str, text: &str) -> (Sends, Option<String>) {
        if self.status != Status::Open {
            return (Vec::new(), None);
        }
        self.player.edit_field(id, text);
        self.changed();
        let held = self
            .player
            .field_text(id)
            .map(|(held, _)| held)
            .filter(|held| *held != text)
            .map(str::to_string);
        (self.told(), held)
    }

    /// The user submitted field `id`.
    pub fn submit_field(&mut self, id: &str) -> Sends {
        if self.status != Status::Open {
            return Vec::new();
        }
        self.player.submit(id);
        self.told()
    }

    /// The keyboard went from the panel: to the terminal, or elsewhere on
    /// the page.
    pub fn blur(&mut self) -> Sends {
        self.player.blur();
        self.changed();
        self.told()
    }

    /// A key the page's field with the keyboard does not use, named as the
    /// browser names it: whether it was the panel's, and what the plugin is
    /// to hear.
    pub fn key(&mut self, key: &str, mods: Mods) -> (bool, Sends) {
        if self.status != Status::Open {
            return (false, Vec::new());
        }
        let taken = self.player.key(key, mods);
        self.changed();
        (taken, self.told())
    }

    /// The panel is not served: the keyboard goes back, and the plugin,
    /// whose view is gone, is told nothing of it.
    fn let_go_of_keyboard(&mut self) {
        self.player.blur();
        self.player.told();
    }

    /// What the player decided the plugin is to hear, as frames.
    fn told(&mut self) -> Sends {
        self.player
            .told()
            .into_iter()
            .map(|input| self.frame(PanelRequest::Input { input: Raw::new(&input) }))
            .collect()
    }

    /// The wheel turned: what scrolled is painted again, and the rows it
    /// brings near are asked for.
    pub fn wheel(&mut self, x: f32, y: f32, dx: f32, dy: f32) -> (bool, Sends) {
        if !self.player.wheel(x, y, dx, dy) {
            return (false, Vec::new());
        }
        self.changed();
        (true, self.wanted())
    }

    /// Asks for what the lists need next, once the plugin is told the size
    /// it is drawn for.
    fn wanted(&mut self) -> Sends {
        if self.env_due {
            return Vec::new();
        }
        self.player
            .wanted()
            .into_iter()
            .map(|wanted| self.frame(PanelRequest::Rows { wanted: Raw::new(&wanted) }))
            .collect()
    }

    fn open_frame(&self) -> Vec<u8> {
        let env = Raw::new(self.player.env());
        self.frame(PanelRequest::Open { plugin: self.plugin.clone(), env, extends: self.extends })
    }

    fn frame(&self, request: PanelRequest) -> Vec<u8> {
        ToHost::Panel { view: self.view, request }.encode()
    }

    fn changed(&mut self) {
        self.revision += 1;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use thinkterm_plugin_panel::{MonoMetrics, TextMetrics};

    fn env() -> Env {
        let text = TextMetrics { size: 13.0, line: 18.0 };
        Env {
            width: 300.0,
            height: 400.0,
            scale: 2.0,
            dark: true,
            small: text,
            body: text,
            title: text,
            mono: MonoMetrics { size: 12.0, line: 17.0, advance: 7.0 },
            locale: String::new(),
            cwd: None,
            remote: None,
            can_extend: false,
            close: None,
            features: Vec::new(),
        }
    }

    fn requests(sends: &Sends) -> Vec<serde_json::Value> {
        sends
            .iter()
            .map(|frame| match ToHost::decode(frame).unwrap() {
                ToHost::Panel { request, .. } => serde_json::to_value(request).unwrap(),
                other => panic!("{other:?}"),
            })
            .collect()
    }

    #[test]
    fn a_frame_is_taken_in_asked_after_and_painted() {
        let (mut panel, sends) = PanelModel::open("stocks", 1, None, env(), 0.0);
        assert_eq!(requests(&sends)[0]["open"]["plugin"], "stocks");
        assert_eq!(panel.view_json().state, "starting");

        let frame = json!({"items": [
            {"hit": {"id": "a", "x": 0, "y": 0, "w": 300, "h": 20, "hover": "bg-hover"}},
            {"list": {"id": "l", "x": 0, "y": 20, "w": 300, "h": 100, "count": 50, "row": 20, "key": "k"}}
        ]});
        let sends = panel.heard(PanelEvent::Frame { frame: Raw::new(&frame) }, 0.0);
        let asked = requests(&sends);
        assert_eq!(asked[0], json!("shown"), "taken in first");
        assert_eq!(asked[1]["rows"]["wanted"]["from"], 0);
        let view = panel.view_json();
        assert_eq!(view.state, "open");
        assert!(view.painting.is_some());

        let before = panel.revision();
        assert!(panel.pointer(10.0, 5.0));
        assert!(panel.revision() > before);
        let clicked = panel.click(10.0, 5.0, Button::Left, 1, Mods::default());
        assert_eq!(requests(&clicked)[0]["input"]["input"]["click"]["id"], "a");

        // Opened again: its rows are asked for once the plugin draws on this
        // opening, not with the open.
        let sends = panel.reopen(2, None, 0.0);
        assert_eq!(requests(&sends).len(), 1, "the open alone");
        let sends = panel.heard(PanelEvent::Frame { frame: Raw::new(&frame) }, 0.0);
        assert!(requests(&sends).iter().any(|asked| asked["rows"]["wanted"]["layout"] == 1), "{:?}", requests(&sends));
    }

    #[test]
    fn a_panel_asks_for_its_extended_view_which_opens_as_that_of_its_opening() {
        let (mut panel, _) = PanelModel::open("diff", 1, None, env(), 0.0);
        assert!(!panel.wants_extended(), "nothing drawn asks nothing");
        let asking = json!({"items": [], "extend": true});
        panel.heard(PanelEvent::Frame { frame: Raw::new(&asking) }, 0.0);
        assert!(panel.drawn() && panel.wants_extended());

        let (mut extended, sends) = PanelModel::open("diff", 2, Some(1), env(), 0.0);
        assert_eq!(requests(&sends)[0]["open"]["extends"], 1);
        assert_eq!(extended.extends(), Some(1));
        // The panel opened again as 3: so is the view that extends it.
        let sends = extended.reopen(4, Some(3), 0.0);
        assert_eq!(requests(&sends)[0]["open"]["extends"], 3);

        panel.dismiss();
        assert!(!panel.wants_extended(), "closed with its button");
        panel.heard(PanelEvent::Frame { frame: Raw::new(&asking) }, 0.0);
        assert!(!panel.wants_extended(), "not while the frames go on asking");
        assert_eq!(requests(&vec![extended.closed_by_user()])[0], json!({"input": {"input": "close"}}));
        panel.heard(PanelEvent::Frame { frame: Raw::new(&json!({"items": []})) }, 0.0);
        assert!(!panel.wants_extended(), "a frame that does not ask lets it go");
        panel.heard(PanelEvent::Frame { frame: Raw::new(&asking) }, 0.0);
        assert!(panel.wants_extended(), "and the next that asks opens it");
        panel.heard(PanelEvent::Frame { frame: Raw::new(&asking) }, 0.0);
        panel.heard(PanelEvent::Closed { reason: "turned off".into(), again: false }, 0.0);
        assert!(!panel.wants_extended(), "nor does a panel that stopped");
    }

    #[test]
    fn a_panel_the_host_closed_is_opened_again_in_a_while_or_stays_stopped() {
        let (mut panel, _) = PanelModel::open("stocks", 1, None, env(), 0.0);
        panel.heard(PanelEvent::Closed { reason: "restarting".into(), again: true }, 1_000.0);
        assert!(panel.waiting());
        assert_eq!(panel.again_in(1_000.0), Some(REOPEN_MS));
        assert_eq!(panel.again_in(1_000.0 + REOPEN_MS), Some(0.0));
        let sends = panel.reopen(2, None, 1_000.0 + REOPEN_MS);
        assert_eq!(panel.view(), 2);
        assert_eq!(requests(&sends)[0]["open"]["plugin"], "stocks");
        panel.lost(5_000.0);
        assert_eq!(panel.again_in(2_000.0), Some(3_000.0), "after the host's retry delay");
        panel.reopen(3, None, 5_000.0);
        panel.heard(PanelEvent::Closed { reason: "turned off".into(), again: false }, 5_000.0);
        assert!(!panel.waiting());
        let view = panel.view_json();
        assert_eq!(view.state, "stopped");
        assert!(view.message.unwrap().contains("turned off"));
        panel.lost(0.0);
        assert!(!panel.waiting(), "stopped stays stopped");
    }

    #[test]
    fn a_panel_whose_frames_do_not_reach_the_host_says_why_and_opens_again_later() {
        let (mut panel, _) = PanelModel::open("stocks", 1, None, env(), 0.0);
        let why = "no plugin host here: it cannot be started";
        assert_eq!(panel.unsent(why, 0.0), Some(2_000.0));
        assert_eq!(panel.unsent(why, 500.0), Some(1_500.0), "the rest of what was sent with it");
        let view = panel.view_json();
        assert_eq!(view.state, "stopped");
        assert!(view.message.unwrap().contains("cannot be started"));
        // Still out of reach: each wait twice as long, to half a minute.
        panel.reopen(2, None, 2_000.0);
        assert_eq!(panel.view_json().state, "stopped", "says why while it tries");
        assert_eq!(panel.unsent(why, 2_000.0), Some(4_000.0));
        for n in 3..10 {
            panel.reopen(n, None, 0.0);
            panel.unsent(why, 0.0);
        }
        panel.reopen(10, None, 0.0);
        assert_eq!(panel.unsent(why, 0.0), Some(30_000.0));
        // Reached at last.
        panel.reopen(11, None, 0.0);
        let frame = json!({"items": []});
        panel.heard(PanelEvent::Frame { frame: Raw::new(&frame) }, 0.0);
        assert_eq!(panel.view_json().state, "open");
        assert_eq!(panel.unsent(why, 0.0), Some(2_000.0), "counted afresh");
        panel.heard(PanelEvent::Closed { reason: "turned off".into(), again: false }, 0.0);
        assert_eq!(panel.unsent(why, 0.0), None, "stopped stays stopped");
    }

    #[test]
    fn a_field_has_the_keyboard_only_as_the_user_gives_it_and_tells_the_plugin() {
        let (mut panel, _) = PanelModel::open("stocks", 1, None, env(), 0.0);
        let frame = json!({"items": [
            {"field": {"id": "add", "x": 8, "y": 8, "w": 200, "h": 26, "focus": 1}}
        ], "keys": ["ArrowDown"]});
        let sends = panel.heard(PanelEvent::Frame { frame: Raw::new(&frame) }, 0.0);
        assert!(!requests(&sends).iter().any(|asked| asked.get("input").is_some()), "not given");
        assert!(!panel.has_keyboard());
        let view = panel.view_json();
        let painted = serde_json::to_value(view.painting.unwrap()).unwrap();
        assert_eq!(painted["ops"][0]["op"], "field", "for the page to put its own over");

        let focused = requests(&panel.focus_field("add"));
        assert_eq!(focused[0]["input"]["input"], json!({"focus": {"id": "add"}}));
        let (typed, held) = panel.edit_field("add", "TSM");
        assert_eq!(held, None, "all of it taken");
        let typed = requests(&typed);
        assert_eq!(typed[0]["input"]["input"], json!({"text": {"id": "add", "text": "TSM"}}));
        let (taken, keyed) = panel.key("ArrowDown", Mods::default());
        assert!(taken);
        assert_eq!(requests(&keyed)[0]["input"]["input"]["key"]["key"], "ArrowDown");
        assert_eq!(panel.edit_field("add", "TS\tM").1.as_deref(), Some("TSM"), "one line");
        let submitted = requests(&panel.submit_field("add"));
        assert_eq!(submitted[0]["input"]["input"]["submit"]["text"], "TSM");
        let (_, escaped) = panel.key("Escape", Mods::default());
        assert_eq!(requests(&escaped)[0]["input"]["input"], json!("blur"));
        assert!(!panel.has_keyboard());

        // A panel the host lets go of lets go of the keyboard, saying nothing.
        panel.focus_field("add");
        panel.heard(PanelEvent::Closed { reason: "restarting".into(), again: true }, 0.0);
        assert!(!panel.has_keyboard());
        // What it last drew stays on show meanwhile, and takes no typing.
        let view = panel.view_json();
        assert!(view.painting.is_some() && !view.live);
        assert!(panel.focus_field("add").is_empty());
        assert!(!panel.has_keyboard());
        assert!(panel.edit_field("add", "x").0.is_empty());
        assert!(panel.submit_field("add").is_empty());
    }

    #[test]
    fn sizes_are_told_at_most_every_little_while_and_rows_wait_for_them() {
        let (mut panel, _) = PanelModel::open("stocks", 1, None, env(), 0.0);
        let frame = json!({"items": [
            {"list": {"id": "l", "x": 0, "y": 0, "w": 300, "h": 100, "count": 500, "row": 20, "key": "k"}}
        ]});
        panel.heard(PanelEvent::Frame { frame: Raw::new(&frame) }, 0.0);
        let sized = |width: f32| Env { width, ..env() };
        panel.set_env(sized(310.0));
        let (sends, later) = panel.tell_env(50.0);
        assert!(sends.is_empty());
        assert_eq!(later, Some(ENV_EVERY_MS - 50.0));
        // Rows asked for meanwhile wait for the size they are drawn for.
        let (moved, asked) = panel.wheel(10.0, 10.0, 0.0, 2_000.0);
        assert!(moved && asked.is_empty());
        panel.set_env(sized(320.0));
        let (sends, later) = panel.tell_env(ENV_EVERY_MS);
        assert_eq!(later, None);
        let told = requests(&sends);
        assert_eq!(told[0]["env"]["env"]["width"], 320.0, "the last size only");
        assert!(told[1..].iter().all(|asked| asked["rows"]["wanted"]["layout"] == 2), "{told:?}");
        assert!(told.len() > 1, "the rows for it follow");
        assert_eq!(panel.tell_env(ENV_EVERY_MS * 3.0), (Vec::new(), None), "told already");
    }
}
