//! The plugin host's own API: the plugins it knows, turning them on and
//! off, and reloading them. It is called as the plugin named [`PLUGIN`],
//! and its events go to every client that asked for the list.

use serde::{Deserialize, Serialize};

/// The name calls address and events carry.
pub const PLUGIN: &str = "plugins";

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
    /// Turn a plugin on or off. Answered with `null`; the change is
    /// announced.
    SetEnabled { id: String, enabled: bool },
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
    /// Where an installed plugin lives.
    #[serde(default)]
    pub dir: Option<String>,
    pub enabled: bool,
    pub state: State,
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
            Self::Off | Self::Failed { .. } | Self::Invalid { .. } | Self::Unsupported { .. }
        )
    }

    /// Why it is not working, when it is not.
    pub fn reason(&self) -> Option<&str> {
        match self {
            Self::Crashed { reason }
            | Self::Failed { reason }
            | Self::Invalid { reason }
            | Self::Unsupported { reason } => Some(reason),
            Self::Off | Self::Idle | Self::Starting | Self::Running => None,
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
    use serde_json::{json, to_value};

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
    fn a_plugin_is_usable_while_on_and_not_broken() {
        let mut info = Info {
            id: "a".into(),
            name: "A".into(),
            description: String::new(),
            version: String::new(),
            builtin: false,
            dir: None,
            enabled: true,
            state: State::Idle,
        };
        assert!(info.usable());
        info.state = State::Failed {
            reason: "no".into(),
        };
        assert!(!info.usable());
        assert_eq!(info.state.reason(), Some("no"));
    }
}
