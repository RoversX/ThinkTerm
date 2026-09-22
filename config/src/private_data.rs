use anyhow::{bail, Context, Result};
use std::fs;
use std::path::Path;
use std::sync::Mutex;

/// Copy a related set of private files across volumes, publish the directory
/// atomically, then remove the old copies. Never mix an existing destination
/// with a source key/catalog pair or fall back to roaming on failure.
pub fn migrate_private_files(from: &Path, to: &Path, files: &[&str]) -> Result<()> {
    static MIGRATION: Mutex<()> = Mutex::new(());
    let _lock = MIGRATION.lock().unwrap_or_else(|err| err.into_inner());
    if from == to {
        return Ok(());
    }
    if !to.try_exists()? {
        let parent = to
            .parent()
            .context("private data directory has no parent")?;
        crate::create_user_owned_dirs(parent)?;
        let staging = tempfile::Builder::new()
            .prefix(".private-migration-")
            .tempdir_in(parent)?;
        for name in files {
            let source = from.join(name);
            if source.try_exists()? {
                let target = staging.path().join(name);
                fs::copy(&source, &target).with_context(|| format!("copy private file {name}"))?;
                fs::File::open(&target)?.sync_all()?;
            }
        }
        fs::rename(staging.path(), to).context("publish migrated private data")?;
    }
    // Check the entire group before deleting any old file. This also safely
    // resumes cleanup after a previous run published the directory but stopped.
    for name in files {
        let source = from.join(name);
        if source.try_exists()? && fs::read(&source)? != fs::read(to.join(name))? {
            bail!("private data migration conflict for {name}; preserve both copies for recovery");
        }
    }
    for name in files {
        let source = from.join(name);
        if source.try_exists()? {
            fs::remove_file(source).with_context(|| format!("remove roaming copy of {name}"))?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn private_migration_preserves_pairs_and_unrelated_gui_state() {
        let dir = tempfile::tempdir().unwrap();
        let from = dir.path().join("roaming");
        let to = dir.path().join("local/ssh");
        fs::create_dir(&from).unwrap();
        for (name, value) in [
            ("secret.key", "key"),
            ("ssh_hosts.json", "catalog"),
            ("workspace_threads.json", "workspace"),
            ("snippets.json", "snippets"),
        ] {
            fs::write(from.join(name), value).unwrap();
        }
        let files = &["secret.key", "ssh_hosts.json"];
        migrate_private_files(&from, &to, files).unwrap();
        migrate_private_files(&from, &to, files).unwrap();
        assert_eq!(fs::read(to.join("secret.key")).unwrap(), b"key");
        assert_eq!(fs::read(to.join("ssh_hosts.json")).unwrap(), b"catalog");
        assert!(!from.join("secret.key").exists());
        assert!(!from.join("ssh_hosts.json").exists());
        assert!(from.join("workspace_threads.json").is_file());
        assert!(from.join("snippets.json").is_file());
    }

    #[test]
    fn private_migration_does_not_publish_a_partial_pair() {
        let dir = tempfile::tempdir().unwrap();
        let from = dir.path().join("roaming");
        let to = dir.path().join("local/ssh");
        fs::create_dir(&from).unwrap();
        fs::write(from.join("secret.key"), "key").unwrap();
        fs::create_dir(from.join("ssh_hosts.json")).unwrap();
        assert!(migrate_private_files(&from, &to, &["secret.key", "ssh_hosts.json"]).is_err());
        assert!(!to.exists());
        assert!(from.join("secret.key").is_file());
    }

    #[test]
    fn private_migration_refuses_conflicting_destinations() {
        let dir = tempfile::tempdir().unwrap();
        let from = dir.path().join("roaming");
        let to = dir.path().join("local");
        fs::create_dir(&from).unwrap();
        fs::create_dir(&to).unwrap();
        fs::write(from.join("secret.key"), "old").unwrap();
        fs::write(to.join("secret.key"), "new").unwrap();
        fs::write(from.join("ssh_hosts.json"), "catalog").unwrap();
        assert!(migrate_private_files(&from, &to, &["secret.key", "ssh_hosts.json"]).is_err());
        assert_eq!(fs::read(from.join("secret.key")).unwrap(), b"old");
        assert_eq!(fs::read(to.join("secret.key")).unwrap(), b"new");
        assert!(!to.join("ssh_hosts.json").exists());
    }
}
