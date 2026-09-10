//! Replacing a file that holds a secret, in one step.
//!
//! Both the web token store and the web listener's self-signed key land
//! this way, and only one of them used to do it correctly.

use anyhow::Context;
use std::path::Path;

/// Write `bytes` to `path`, replacing whatever was there in one step.
///
/// A uniquely named temporary file in the same directory -- created 0600 on
/// unix, and created rather than opened, so a symlink someone planted at a
/// name this could have chosen is not followed -- synced before the rename,
/// so a crash leaves either the old file or the new one and never a torn
/// one. `prefix` names those temporaries so a directory holding several of
/// these reads at a glance.
///
/// On Windows there is no mode to set and none is claimed: the file takes
/// the ACL it inherits. Under ThinkTerm's data directory that is this user,
/// SYSTEM and the Administrators group, with no entry for anyone else --
/// narrower than world-readable, wider than 0600, and not something this
/// code enforces, because a local administrator can read the panes
/// directly and does not need the key to do it. `PRIVACY.md` says so too.
pub fn replace(path: &Path, prefix: &str, bytes: &[u8]) -> anyhow::Result<()> {
    let dir = match path.parent() {
        Some(dir) if !dir.as_os_str().is_empty() => dir,
        _ => Path::new("."),
    };
    std::fs::create_dir_all(dir)
        .with_context(|| format!("creating {}", dir.display()))?;
    let mut tmp = tempfile::Builder::new()
        .prefix(prefix)
        .tempfile_in(dir)
        .with_context(|| format!("creating a temporary file in {}", dir.display()))?;
    std::io::Write::write_all(&mut tmp, bytes)?;
    tmp.as_file().sync_all()?;
    tmp.persist(path)
        .with_context(|| format!("replacing {}", path.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    /// The replacement is atomic and leaves nothing behind. The temporary
    /// is uniquely named, so two of these racing cannot share one, and a
    /// name planted in advance cannot be the one it picks.
    #[test]
    fn a_secret_is_replaced_in_one_step() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("key.pem");

        super::replace(&path, ".key.", b"first").unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"first");

        // A file sitting at the obvious temporary name is not what gets
        // written, and is not disturbed.
        let decoy = dir.path().join("key.tmp");
        std::fs::write(&decoy, b"decoy").unwrap();
        super::replace(&path, ".key.", b"second").unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"second");
        assert_eq!(std::fs::read(&decoy).unwrap(), b"decoy");

        // Nothing but the file and the decoy remains.
        let mut left: Vec<String> = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        left.sort();
        assert_eq!(left, vec!["key.pem".to_string(), "key.tmp".to_string()]);
    }

    /// On unix the file is readable by its owner alone; there is no
    /// equivalent to assert on Windows, where it inherits its directory's.
    #[cfg(unix)]
    #[test]
    fn a_secret_is_readable_by_its_owner_only() {
        use std::os::unix::fs::PermissionsExt as _;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("key.pem");
        super::replace(&path, ".key.", b"secret").unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600, "mode was {:o}", mode & 0o777);
    }
}
