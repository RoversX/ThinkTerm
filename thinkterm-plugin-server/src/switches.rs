//! Which plugins the user let run, which are turned off, and how long the
//! user lets each run unused: `plugins.json` in the data directory. An
//! installed plugin runs only once the user let it, from the directory it
//! is in then: one that is not in the file, or is somewhere else now --
//! moved, or another under its id -- waits to be let, and is written in
//! as off, so that an older build sharing the file does not run it either;
//! never over a file that does not read, which only a switch the user
//! changes writes anew.
//! A built-in plugin is ThinkTerm's own: it is on unless turned off. A
//! switch stays when its plugin is removed, so a plugin installed again
//! where it was comes back as it was left. Builds share the file: what one
//! does not know is kept as another wrote it.

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
    /// The directory the user let the plugin run from, links followed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    allowed: Option<String>,
    /// Turned off by ThinkTerm, not the user, while the plugin was not
    /// where it was let run from: on again once it is back there.
    #[serde(default, skip_serializing_if = "is_false")]
    held: bool,
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
            allowed: None,
            held: false,
            background: None,
            rest: BTreeMap::new(),
        }
    }
}

fn on() -> bool {
    true
}

fn is_false(value: &bool) -> bool {
    !*value
}

pub struct Switches {
    path: PathBuf,
    stored: Stored,
    /// The file as last read or written, to notice another host's change.
    seen: Option<Stamp>,
    /// The file is there and does not read: nothing writes over it but a
    /// switch the user changes.
    unreadable: bool,
}

impl Switches {
    pub fn load(path: PathBuf) -> Self {
        let mut switches = Self {
            path,
            stored: Stored::default(),
            seen: None,
            unreadable: false,
        };
        switches.read();
        switches
    }

    /// Reads the file again. One that does not read lets no installed
    /// plugin run, and leaves the built-in ones on, until a switch is
    /// changed, which writes it anew.
    fn read(&mut self) {
        self.seen = stamp(&self.path);
        let (stored, unreadable) = match std::fs::read(&self.path) {
            Ok(bytes) => match serde_json::from_slice(&bytes) {
                Ok(stored) => (stored, false),
                Err(err) => {
                    log::warn!("{} does not read: {err}", self.path.display());
                    (Stored::default(), true)
                }
            },
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => (Stored::default(), false),
            Err(err) => {
                log::warn!("cannot read {}: {err}", self.path.display());
                (Stored::default(), true)
            }
        };
        self.stored = stored;
        self.unreadable = unreadable;
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

    /// Whether plugin `id`'s switch is on: a plugin not in the file is.
    /// An installed one runs only if it was let run too
    /// ([`allowed`](Self::allowed)).
    pub fn enabled(&self, id: &str) -> bool {
        self.stored
            .plugins
            .get(id)
            .map_or(true, |switch| switch.enabled)
    }

    /// Whether the user let plugin `id` run from `dir`, where it is now.
    pub fn allowed(&self, id: &str, dir: &str) -> bool {
        self.stored
            .plugins
            .get(id)
            .and_then(|switch| switch.allowed.as_deref())
            == Some(dir)
    }

    /// Whether plugin `id` is off because ThinkTerm held it, not the user.
    pub fn held(&self, id: &str) -> bool {
        self.stored
            .plugins
            .get(id)
            .is_some_and(|switch| switch.held)
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
        self.change(id, |switch| {
            switch.enabled = enabled;
            switch.held = false;
        })
    }

    /// Lets installed plugin `id` run from `dir`, and turns it on, as
    /// [`set`](Self::set) does.
    pub fn allow(&mut self, id: &str, dir: &str) -> anyhow::Result<bool> {
        self.change(id, |switch| {
            switch.enabled = true;
            switch.allowed = Some(dir.to_string());
            switch.held = false;
        })
    }

    /// Writes plugins `waiting` -- installed, and not let run from where
    /// they are -- in as off, keeping what else their entries say: they
    /// wait to be let run, in builds that know to wait and in older ones
    /// alike. Those held so that are `back` where they were let run from
    /// are on again. One write, and not over a file that does not read, nor
    /// over one another host wrote since it was last read here: the next
    /// look does it, once that is taken in. False when nothing was written.
    pub fn hold(&mut self, waiting: &[String], back: &[String]) -> anyhow::Result<bool> {
        if self.unreadable || stamp(&self.path) != self.seen {
            return Ok(false);
        }
        let mut next = self.stored.clone();
        let mut changed = false;
        for id in waiting {
            let switch = next.plugins.entry(id.clone()).or_default();
            if switch.enabled {
                switch.enabled = false;
                switch.held = true;
                changed = true;
            }
        }
        for id in back {
            if let Some(switch) = next.plugins.get_mut(id).filter(|switch| switch.held) {
                switch.enabled = true;
                switch.held = false;
                changed = true;
            }
        }
        if !changed {
            return Ok(false);
        }
        self.write(next)?;
        Ok(true)
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
        self.write(next)?;
        self.unreadable = false;
        Ok(true)
    }

    /// Writes `next` over the file, whole, before it counts.
    fn write(&mut self, next: Stored) -> anyhow::Result<()> {
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
        Ok(())
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
    fn a_plugin_is_let_run_from_where_it_was_and_a_new_one_is_written_in_as_off() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(FILE);
        let mut switches = Switches::load(path.clone());
        assert!(!switches.allowed("a", "/home/user/plugins/a"));
        assert!(switches.hold(&["a".into(), "b".into()], &[]).unwrap());
        assert!(!switches.hold(&["a".into()], &[]).unwrap(), "said already");
        let held: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert_eq!(
            held,
            serde_json::json!({"plugins": {
                "a": {"enabled": false, "held": true},
                "b": {"enabled": false, "held": true},
            }}),
            "off, for a build that does not know to wait"
        );
        assert!(switches.allow("a", "/home/user/plugins/a").unwrap());
        assert!(switches.enabled("a"));
        assert!(switches.allowed("a", "/home/user/plugins/a"));
        assert!(
            !switches.allowed("a", "/home/user/elsewhere/a"),
            "moved, it waits again"
        );
        let read = Switches::load(path);
        assert!(read.allowed("a", "/home/user/plugins/a"));
        assert!(!read.allowed("b", "/home/user/plugins/b"));
        // Turned off, it stays let: on again needs no new say.
        let mut switches = read;
        assert!(switches.set("a", false).unwrap());
        assert!(switches.allowed("a", "/home/user/plugins/a"));
        // Let run from elsewhere, and on: held off, what else it says kept.
        assert!(switches.set("a", true).unwrap());
        assert!(switches.hold(&["a".into()], &[]).unwrap());
        assert!(!switches.enabled("a"));
        assert!(switches.allowed("a", "/home/user/plugins/a"));
        // Back where it was let run from: on again.
        assert!(switches.hold(&[], &["a".into()]).unwrap());
        assert!(switches.enabled("a") && !switches.held("a"));
        // One the user turned off stays off, back or not.
        assert!(switches.set("a", false).unwrap());
        assert!(!switches.hold(&[], &["a".into()]).unwrap());
        assert!(!switches.enabled("a"));
        assert!(switches.set("a", true).unwrap());
        assert!(switches.hold(&["a".into()], &[]).unwrap());
        assert!(switches.set("a", false).unwrap(), "the user's say");
        assert!(!switches.held("a"));
        assert!(!switches.hold(&[], &["a".into()]).unwrap());
    }

    #[test]
    fn nothing_is_held_over_a_file_that_does_not_read_or_another_host_just_wrote() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(FILE);
        std::fs::write(
            &path,
            r#"{"plugins": {"a": {"enabled": true, "allowed": "/x"},}"#,
        )
        .unwrap();
        let mut switches = Switches::load(path.clone());
        assert!(!switches.hold(&["a".into(), "b".into()], &[]).unwrap());
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            r#"{"plugins": {"a": {"enabled": true, "allowed": "/x"},}"#,
            "left as it is, for the user to mend"
        );

        std::fs::write(&path, r#"{"plugins": {}}"#).unwrap();
        let mut switches = Switches::load(path.clone());
        // Another host writes meanwhile: not written over.
        std::fs::write(
            &path,
            r#"{"plugins": {"b": {"enabled": true, "allowed": "/b"}}}"#,
        )
        .unwrap();
        assert!(!switches.hold(&["b".into()], &[]).unwrap());
        assert!(switches.refresh());
        assert!(switches.allowed("b", "/b"), "the other host's say stands");
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
