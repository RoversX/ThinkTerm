//! The plugin host's own API: the plugins it knows, turning them on and
//! off, how long they run unused, and reloading them. It is called as the
//! plugin named [`PLUGIN`], and its events go to every client that asked
//! for the list.

use serde::{Deserialize, Serialize};
use std::time::Duration;

/// The name calls address and events carry.
pub const PLUGIN: &str = "plugins";

/// Directories looked at in the plugins directory; one with more than this
/// is not a plugins directory someone keeps by hand.
pub const DIR_LIMIT: usize = 256;

/// How long an installed plugin's program runs while nothing uses it --
/// no panel of it on show, no call to it unanswered, no client watching
/// it. Its manifest says which it needs; the user can choose another.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Background {
    /// Started with ThinkTerm and kept running for as long as ThinkTerm
    /// runs on the machine: its desktop or its mux server.
    Always,
    /// Stopped when it has not been used for [`Background::BRIEFLY`].
    #[default]
    Briefly,
    /// Stopped once it has not been used for [`Background::NEVER`]: a
    /// moment, so that a panel shown again at once finds it running.
    Never,
}

impl Background {
    pub const BRIEFLY: Duration = Duration::from_secs(120);
    pub const NEVER: Duration = Duration::from_secs(10);

    /// How long it runs unused, `None` for as long as ThinkTerm does.
    pub fn unused_for(self) -> Option<Duration> {
        match self {
            Self::Always => None,
            Self::Briefly => Some(Self::BRIEFLY),
            Self::Never => Some(Self::NEVER),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum Request {
    /// Every plugin, in the order a list shows them, with names in
    /// `locale` where the plugin has them. Answered with `Vec<Info>`. The
    /// caller is told of every change after it, and asks again.
    List {
        #[serde(default)]
        locale: String,
    },
    /// Turn a plugin on or off. On lets a new one run, from where it is
    /// installed. Answered with `null`; the change is announced.
    SetEnabled { id: String, enabled: bool },
    /// Choose how long a plugin runs unused; `None` goes back to what its
    /// manifest says. Answered with `null`; the change is announced.
    SetBackground {
        id: String,
        #[serde(default)]
        background: Option<Background>,
    },
    /// The caller is ThinkTerm running on this machine -- a desktop or a
    /// mux server -- and stays connected for as long as it runs: while one
    /// is, the plugins that run [`Background::Always`] do. The host looks
    /// for new plugins first, so that one installed meanwhile is among
    /// them. Answered with `null`.
    Keep,
    /// Stop a plugin, read its manifest again and forget that it failed;
    /// every installed plugin when `id` is absent, looking for new and
    /// removed ones too. Answered with `null`; the change is announced.
    Reload {
        #[serde(default)]
        id: Option<String>,
    },
}

/// One plugin as a list shows it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Info {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub description: String,
    /// Empty for a built-in plugin, which is as new as the host.
    #[serde(default)]
    pub version: String,
    #[serde(default)]
    pub builtin: bool,
    /// Where an installed plugin lives: its own directory in the plugins
    /// directory.
    #[serde(default)]
    pub dir: Option<String>,
    /// Where `dir` leads, links followed, when that is elsewhere: what the
    /// user lets run.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target: Option<String>,
    pub enabled: bool,
    pub state: State,
    /// The panel it adds to the right sidebar, if it adds one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub panel: Option<Panel>,
    /// The page it can be opened as, in a pane's place, if it has one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub page: Option<Page>,
    /// What it does when asked by name, as its manifest lists them.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub actions: Vec<Action>,
    /// How long it runs unused: the user's choice, else its manifest's.
    /// A built-in plugin runs inside the host, and is never started or
    /// stopped: this is its default.
    #[serde(default)]
    pub background: Background,
    /// What its manifest says, which the user's choice is shown against.
    #[serde(default)]
    pub background_default: Background,
}

/// A plugin's panel in the right sidebar, which the selector offers under
/// the plugin's name.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Panel {
    /// A Lucide icon's name, for the selector; one a client does not have
    /// shows as a puzzle piece.
    pub icon: String,
}

/// A plugin's page: a view of it in a pane's place, a tab of its own or
/// beside a terminal, under the plugin's name.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Page {
    /// A Lucide icon's name, for its tab; one a client does not have shows
    /// as a puzzle piece.
    pub icon: String,
}

/// Something a plugin does when the user asks for it by name -- from the
/// command palette, with a key, with `thinkterm plugin do` -- and not from
/// one of its own views.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Action {
    /// Its name among the plugin's actions: with the plugin's id before it
    /// and a "." between ([`Action::name`]), its name among them all.
    pub id: String,
    /// What it is listed as, after the plugin's name.
    pub title: String,
    #[serde(flatten)]
    pub does: Does,
}

/// What asking for an action comes to.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Does {
    /// The plugin is called with this, and with where the user was added
    /// to it as `at`.
    Call(serde_json::Value),
    /// The plugin's page is opened with this.
    Page(serde_json::Value),
}

impl Action {
    /// The name plugin `plugin`'s action `action` goes by wherever every
    /// plugin's are told apart: a key's binding, a command line.
    pub fn name(plugin: &str, action: &str) -> String {
        format!("{plugin}.{action}")
    }

    /// The plugin and the action a name is of. Neither id has a "." in it.
    pub fn named(name: &str) -> Option<(&str, &str)> {
        name.split_once('.')
            .filter(|(plugin, action)| !plugin.is_empty() && !action.is_empty())
    }
}

impl Info {
    /// Whether it can be used: on, and not broken past using.
    pub fn usable(&self) -> bool {
        self.enabled && self.state.usable()
    }
}

/// Where a plugin is in its life. A built-in plugin is `Off` or `Idle`:
/// it runs inside the host and is never started or stopped.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum State {
    /// Installed, and never let run from where it is: nothing starts it
    /// until the user turns it on, which lets it.
    New,
    /// Turned off.
    Off,
    /// On, and not running: started when something uses it.
    Idle,
    /// Its program started and has not said it is ready.
    Starting,
    Running,
    /// Its program ended without being asked to; using the plugin starts it
    /// again.
    Crashed {
        reason: String,
    },
    /// It crashed too often, or cannot be started. It stays so until it is
    /// reloaded or its files change.
    Failed {
        reason: String,
    },
    /// Its manifest cannot be used.
    Invalid {
        reason: String,
    },
    /// It does not run on this system.
    Unsupported {
        reason: String,
    },
}

impl State {
    /// Whether using the plugin can work: it is not broken past using.
    pub fn usable(&self) -> bool {
        !matches!(
            self,
            Self::New
                | Self::Off
                | Self::Failed { .. }
                | Self::Invalid { .. }
                | Self::Unsupported { .. }
        )
    }

    /// Why it is not working, when it is not.
    pub fn reason(&self) -> Option<&str> {
        match self {
            Self::Crashed { reason }
            | Self::Failed { reason }
            | Self::Invalid { reason }
            | Self::Unsupported { reason } => Some(reason),
            Self::New | Self::Off | Self::Idle | Self::Starting | Self::Running => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum Event {
    /// Plugins came or went, were turned on or off, or changed state: a
    /// list on show is to be asked for again.
    Changed,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{from_value, json, to_value};

    #[test]
    fn requests_states_and_events_are_tagged_json() {
        assert_eq!(
            serde_json::from_value::<Request>(json!({"op": "list"})).unwrap(),
            Request::List {
                locale: String::new()
            }
        );
        assert_eq!(
            serde_json::from_value::<Request>(json!({"op": "reload"})).unwrap(),
            Request::Reload { id: None }
        );
        assert!(serde_json::from_value::<Request>(json!({"op": "run"})).is_err());
        assert_eq!(
            serde_json::from_value::<Request>(
                json!({"op": "set_background", "id": "a", "background": "always"})
            )
            .unwrap(),
            Request::SetBackground {
                id: "a".into(),
                background: Some(Background::Always)
            }
        );
        assert_eq!(
            serde_json::from_value::<Request>(json!({"op": "set_background", "id": "a"})).unwrap(),
            Request::SetBackground {
                id: "a".into(),
                background: None
            },
            "none goes back to the manifest's"
        );
        assert_eq!(to_value(Request::Keep).unwrap(), json!({"op": "keep"}));
        assert_eq!(Background::default(), Background::Briefly);
        assert_eq!(Background::Always.unused_for(), None);
        assert_eq!(Background::Never.unused_for(), Some(Background::NEVER));
        assert_eq!(
            to_value(State::Crashed {
                reason: "exit status 1".into()
            })
            .unwrap(),
            json!({"kind": "crashed", "reason": "exit status 1"})
        );
        assert_eq!(to_value(State::Idle).unwrap(), json!({"kind": "idle"}));
        assert_eq!(
            to_value(Event::Changed).unwrap(),
            json!({"event": "changed"})
        );
    }

    #[test]
    fn an_action_reads_as_its_manifest_writes_it_and_has_one_name() {
        let call = Action {
            id: "new-card".into(),
            title: "New card".into(),
            does: Does::Call(json!({"op": "new"})),
        };
        let written = json!({"id": "new-card", "title": "New card", "call": {"op": "new"}});
        assert_eq!(to_value(&call).unwrap(), written);
        assert_eq!(from_value::<Action>(written).unwrap(), call);
        let page: Action =
            from_value(json!({"id": "open", "title": "Open", "page": {"board": "a"}})).unwrap();
        assert_eq!(page.does, Does::Page(json!({"board": "a"})));
        // A list from a host that knows no actions has none.
        let old = json!({"id": "a", "name": "A", "enabled": true, "state": {"kind": "idle"}});
        assert!(from_value::<Info>(old).unwrap().actions.is_empty());

        assert_eq!(Action::name("board", "new-card"), "board.new-card");
        assert_eq!(Action::named("board.new-card"), Some(("board", "new-card")));
        assert_eq!(Action::named("board"), None);
        assert_eq!(Action::named(".new-card"), None);
    }

    #[test]
    fn a_plugin_is_usable_while_on_and_not_broken() {
        let mut info = Info {
            id: "a".into(),
            name: "A".into(),
            description: String::new(),
            version: String::new(),
            builtin: false,
            dir: None,
            target: None,
            enabled: true,
            state: State::Idle,
            panel: None,
            page: None,
            actions: Vec::new(),
            background: Background::Briefly,
            background_default: Background::Briefly,
        };
        assert!(info.usable());
        info.state = State::Failed {
            reason: "no".into(),
        };
        assert!(!info.usable());
        assert_eq!(info.state.reason(), Some("no"));
        info.state = State::New;
        assert!(!info.usable(), "not until it is let run");
        assert_eq!(to_value(State::New).unwrap(), json!({"kind": "new"}));
    }
}
