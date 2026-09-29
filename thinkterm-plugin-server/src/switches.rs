//! Which plugins are turned off, and how long the user lets each run
//! unused: `plugins.json` in the data directory. A plugin that is not in it
//! is on, and runs as its manifest says. A switch stays when its plugin is
//! removed, so a plugin installed again comes back as it was left. Builds
//! share the file: what one does not know is kept as another wrote it.

use crate::stamp::{stamp, Stamp};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;
use std::io::Write;
use std::path::PathBuf;
use thinkterm_plugin_channel::registry::Background;

pub const FILE: &str = "plugins.json";

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
struct Stored {
    #[serde(default)]
    plugins: BTreeMap<String, Switch>,
    /// What a newer build keeps here, as it wrote it.
    #[serde(flatten)]
    rest: BTreeMap<String, Value>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
struct Switch {
    #[serde(default = "on")]
    enabled: bool,
    /// The user's choice, as written; none, the manifest's. One this build
    /// does not know counts as none, and is kept.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    background: Option<Value>,
    /// What a newer build keeps here, as it wrote it.
    #[serde(flatten)]
    rest: BTreeMap<String, Value>,
}

impl Default for Switch {
    fn default() -> Self {
        Self {
            enabled: true,
            background: None,
            rest: BTreeMap::new(),
        }
    }
}

fn on() -> bool {
    true
}

pub struct Switches {
    path: PathBuf,
    stored: Stored,
    /// The file as last read or written, to notice another host's change.
    seen: Option<Stamp>,
}

impl Switches {
    pub fn load(path: PathBuf) -> Self {
        let mut switches = Self {
            path,
            stored: Stored::default(),
            seen: None,
        };
        switches.read();
        switches
    }

    /// Reads the file again. One that does not read leaves every plugin on
    /// until a switch is changed, which writes it anew.
    fn read(&mut self) {
        self.seen = stamp(&self.path);
        self.stored = match std::fs::read(&self.path) {
            Ok(bytes) => serde_json::from_slice(&bytes).unwrap_or_else(|err| {
                log::warn!("{} does not read: {err}", self.path.display());
                Stored::default()
            }),
            Err(_) => Stored::default(),
        };
    }

    /// Picks up a change made by another host -- a debug build's, say.
    /// True when a switch moved.
    pub fn refresh(&mut self) -> bool {
        if stamp(&self.path) == self.seen {
            return false;
        }
        let before = self.stored.clone();
        self.read();
        self.stored != before
    }

    pub fn enabled(&self, id: &str) -> bool {
        self.stored
            .plugins
            .get(id)
            .map_or(true, |switch| switch.enabled)
    }

    /// How long the user lets plugin `id` run unused, if they chose.
    pub fn background(&self, id: &str) -> Option<Background> {
        let chosen = self.stored.plugins.get(id)?.background.as_ref()?;
        Background::deserialize(chosen).ok()
    }

    /// Turns a plugin on or off, on disk before it counts. The file is
    /// written over as last read: [`refresh`](Self::refresh) first, and act
    /// on what another host moved. False when it already was.
    pub fn set(&mut self, id: &str, enabled: bool) -> anyhow::Result<bool> {
        self.change(id, |switch| switch.enabled = enabled)
    }

    /// Keeps the user's choice of how long plugin `id` runs unused, `None`
    /// for its manifest's, as [`set`](Self::set) keeps a switch.
    pub fn set_background(
        &mut self,
        id: &str,
        background: Option<Background>,
    ) -> anyhow::Result<bool> {
        let background = background.map(|background| {
            serde_json::to_value(background).expect("a choice always serialises")
        });
        self.change(id, |switch| switch.background = background)
    }

    /// Changes plugin `id`'s entry and writes the file, if that moved it.
    fn change(&mut self, id: &str, change: impl FnOnce(&mut Switch)) -> anyhow::Result<bool> {
        let mut next = self.stored.clone();
        let switch = next.plugins.entry(id.to_string()).or_default();
        let before = switch.clone();
        change(switch);
        if *switch == before && self.stored.plugins.contains_key(id) {
            return Ok(false);
        }
        if *switch == Switch::default() && !self.stored.plugins.contains_key(id) {
            return Ok(false);
        }
        let dir = self
            .path
            .parent()
            .ok_or_else(|| anyhow::anyhow!("{} has no directory", self.path.display()))?;
        std::fs::create_dir_all(dir)?;
        let mut file = tempfile::NamedTempFile::new_in(dir)?;
        file.write_all(&serde_json::to_vec_pretty(&next)?)?;
        file.as_file().sync_all()?;
        file.persist(&self.path)?;
        self.stored = next;
        self.seen = stamp(&self.path);
        Ok(true)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_switch_is_kept_on_disk_and_read_back() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(FILE);
        let mut switches = Switches::load(path.clone());
        assert!(switches.enabled("a"), "on unless turned off");
        assert!(switches.set("a", false).unwrap());
        assert!(!switches.set("a", false).unwrap(), "already off");
        assert!(!switches.enabled("a"));
        assert!(!Switches::load(path.clone()).enabled("a"));

        // Another host turns it back on.
        let mut other = Switches::load(path.clone());
        assert!(other.set("a", true).unwrap());
        assert!(switches.refresh());
        assert!(switches.enabled("a"));
        assert!(!switches.refresh(), "nothing new");
    }

    #[test]
    fn a_choice_of_background_is_kept_beside_the_switch() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(FILE);
        let mut switches = Switches::load(path.clone());
        assert_eq!(switches.background("a"), None, "the manifest's");
        assert!(
            !switches.set_background("a", None).unwrap(),
            "nothing to keep"
        );
        assert!(switches
            .set_background("a", Some(Background::Always))
            .unwrap());
        assert!(!switches
            .set_background("a", Some(Background::Always))
            .unwrap());
        assert!(switches.enabled("a"), "still on");
        let read = Switches::load(path.clone());
        assert_eq!(read.background("a"), Some(Background::Always));
        assert!(switches.set("a", false).unwrap());
        assert_eq!(
            Switches::load(path.clone()).background("a"),
            Some(Background::Always)
        );
        assert!(switches.set_background("a", None).unwrap());
        assert_eq!(Switches::load(path.clone()).background("a"), None);
        assert!(!Switches::load(path.clone()).enabled("a"));
        // A file from before there was a choice reads as it did.
        std::fs::write(&path, r#"{"plugins": {"b": {"enabled": false}}}"#).unwrap();
        let old = Switches::load(path);
        assert!(!old.enabled("b"));
        assert_eq!(old.background("b"), None);
    }

    #[test]
    fn what_a_newer_build_wrote_is_kept_and_what_it_chose_counts_as_none() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(FILE);
        let newer = serde_json::json!({
            "plugins": {
                "a": {"enabled": false, "background": "hourly", "pinned": true},
                "b": {"background": "always"}
            },
            "order": ["b", "a"]
        });
        std::fs::write(&path, newer.to_string()).unwrap();
        let mut switches = Switches::load(path.clone());
        assert!(!switches.enabled("a"), "the rest of the file still reads");
        assert_eq!(switches.background("a"), None, "a choice it does not know");
        assert_eq!(switches.background("b"), Some(Background::Always));
        assert!(switches.set("b", false).unwrap());
        let written: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert_eq!(
            written,
            serde_json::json!({
                "plugins": {
                    "a": {"enabled": false, "background": "hourly", "pinned": true},
                    "b": {"enabled": false, "background": "always"}
                },
                "order": ["b", "a"]
            }),
            "kept as the newer build wrote it"
        );
    }

    #[test]
    fn a_file_that_does_not_read_leaves_everything_on() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(FILE);
        std::fs::write(&path, "not json").unwrap();
        let mut switches = Switches::load(path.clone());
        assert!(switches.enabled("a"));
        switches.set("a", false).unwrap();
        assert!(!Switches::load(path).enabled("a"));
    }
}
