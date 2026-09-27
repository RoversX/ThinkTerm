//! Which plugins are turned off: `plugins.json` in the data directory. A
//! plugin that is not in it is on. A switch stays when its plugin is
//! removed, so a plugin installed again comes back as it was left.

use crate::stamp::{stamp, Stamp};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::io::Write;
use std::path::PathBuf;

pub const FILE: &str = "plugins.json";

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
struct Stored {
    #[serde(default)]
    plugins: BTreeMap<String, Switch>,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
struct Switch {
    enabled: bool,
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

    /// Turns a plugin on or off, on disk before it counts. The file is
    /// written over as last read: [`refresh`](Self::refresh) first, and act
    /// on what another host moved. False when it already was.
    pub fn set(&mut self, id: &str, enabled: bool) -> anyhow::Result<bool> {
        if self.enabled(id) == enabled {
            return Ok(false);
        }
        let mut next = self.stored.clone();
        next.plugins.insert(id.to_string(), Switch { enabled });
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
