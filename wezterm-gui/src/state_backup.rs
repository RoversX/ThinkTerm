//! Export and import of ThinkTerm's own state, by hand from Settings.
//!
//! Nothing here runs unasked. An export writes the Spaces, projects and
//! threads, the saved SSH hosts, the snippets and the recent commands into a
//! folder the user picks. An import cannot swap those files under a running
//! window, which holds them in memory and writes them back, so it is staged
//! beside the state and applied by the next launch that finds no other window
//! running, before anything reads the state. What it replaces is kept beside
//! it first.
//!
//! The Spaces this app shares with its local session server are merged on the
//! next connection: the server adds back the Spaces it still has, and its
//! names win where both have one. An import brings back what the backup
//! holds; it does not remove what the server kept.
//!
//! `secret.key` is never exported: saved host passwords stay encrypted with
//! this machine's key, so on another machine they have to be entered again.

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

/// Names the files in a backup folder; its presence is what makes a folder
/// one.
const MANIFEST: &str = "thinkterm-backup.json";
const FORMAT: u32 = 1;
const STORE: &str = "workspace_threads.json";

/// The staged import, in the state folder.
const PENDING: &str = ".import-pending";
/// Held shared by every running window for its whole life. An import is
/// applied only by a launch that can hold it alone, so never under a window
/// that has the old state in memory.
const RUNNING_LOCK: &str = ".windows-running.lock";
/// How long a launch with an import waiting gives the windows still running
/// to exit: long enough for a restart's old process, short enough not to
/// stall a launch that is only handing a request to a running window.
const WAIT_FOR_OTHERS: Duration = Duration::from_secs(5);

#[derive(Serialize, Deserialize)]
struct Manifest {
    format: u32,
    created: String,
    files: Vec<String>,
}

static IMPORT_STAGED: AtomicBool = AtomicBool::new(false);
static LAST_IMPORT_ERROR: std::sync::Mutex<Option<String>> = std::sync::Mutex::new(None);

/// Whether an import waits for this window to quit.
pub(crate) fn import_staged() -> bool {
    IMPORT_STAGED.load(Ordering::Relaxed)
}

/// Why the import staged last session could not be applied, if it could not.
pub(crate) fn last_import_error() -> Option<String> {
    LAST_IMPORT_ERROR
        .lock()
        .unwrap_or_else(|err| err.into_inner())
        .clone()
}

/// Each file a backup may hold, by its name in the backup, and where it
/// lives.
fn state_files() -> Vec<(&'static str, Option<PathBuf>)> {
    vec![
        (
            STORE,
            Some(crate::workspace_threads::workspace_thread_store_path()),
        ),
        (
            "ssh_hosts.json",
            thinkterm_core::ssh_hosts::saved_hosts_path().ok(),
        ),
        (
            "snippets.json",
            Some(crate::snippets::snippets_store_path()),
        ),
        (
            "recent-commands.json",
            Some(config::DATA_DIR.join("recent-commands.json")),
        ),
    ]
}

/// Read a backup's file the way the app would, without the migrations its
/// loaders run on the live state.
fn check_file(name: &str, path: &Path) -> Result<()> {
    match name {
        STORE => {
            serde_json::from_slice::<crate::workspace_threads::WorkspaceThreadStore>(&fs::read(
                path,
            )?)?;
        }
        "ssh_hosts.json" => {
            crate::ssh_hosts::check_ssh_host_store(path)?;
        }
        "snippets.json" => {
            crate::snippets::check_snippet_store(path)?;
        }
        _ => {
            serde_json::from_slice::<serde_json::Value>(&fs::read(path)?)?;
        }
    }
    Ok(())
}

#[cfg(unix)]
fn make_private(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(0o600))
        .with_context(|| format!("make {} private", path.display()))
}

#[cfg(not(unix))]
fn make_private(_path: &Path) -> Result<()> {
    Ok(())
}

/// Write the current state into a new folder inside `into` and return it.
/// The Spaces come from memory, so the export has what is on screen even if
/// the last save is still on its way to disk. Blocking: call it off the UI
/// thread.
pub(crate) fn export(into: &Path) -> Result<PathBuf> {
    if crate::workspace_threads::workspace_thread_store_is_unreadable() {
        bail!("ThinkTerm could not read its Spaces when it started, so there are none to export");
    }
    let store = crate::workspace_threads::workspace_thread_store_snapshot();
    let files: Vec<(&str, PathBuf)> = state_files()
        .into_iter()
        .filter(|(name, _)| *name != STORE)
        .filter_map(|(name, path)| Some((name, path?)))
        .collect();
    export_to(into, &store, &files)
}

fn export_to(
    into: &Path,
    store: &crate::workspace_threads::WorkspaceThreadStore,
    files: &[(&str, PathBuf)],
) -> Result<PathBuf> {
    let stamp = chrono::Local::now().format("%Y-%m-%d %H%M%S").to_string();
    let mut target = into.join(format!("ThinkTerm Backup {stamp}"));
    let mut serial = 1;
    while target.exists() {
        serial += 1;
        target = into.join(format!("ThinkTerm Backup {stamp} {serial}"));
    }
    // Built beside the target and renamed into place, so a folder with the
    // backup's name is always a whole one.
    let staging = tempfile::Builder::new()
        .prefix(".thinkterm-backup-")
        .tempdir_in(into)
        .with_context(|| format!("create a folder in {}", into.display()))?;
    let mut written = vec![STORE.to_string()];
    crate::workspace_threads::save_workspace_thread_store_to_path(
        &staging.path().join(STORE),
        store,
    )?;
    for (name, from) in files {
        if from.is_file() {
            let to = staging.path().join(name);
            fs::copy(from, &to).with_context(|| format!("copy {name}"))?;
            make_private(&to)?;
            written.push(name.to_string());
        }
    }
    let manifest = Manifest {
        format: FORMAT,
        created: chrono::Local::now().to_rfc3339(),
        files: written,
    };
    fs::write(
        staging.path().join(MANIFEST),
        serde_json::to_vec_pretty(&manifest)?,
    )?;
    let staged = staging.keep();
    if let Err(err) = fs::rename(&staged, &target) {
        let _ = fs::remove_dir_all(&staged);
        return Err(err).with_context(|| format!("create {}", target.display()));
    }
    Ok(target)
}

/// Check `from` is a backup and stage it to be applied once this window has
/// quit. Blocking: call it off the UI thread.
pub(crate) fn stage_import(from: &Path) -> Result<()> {
    stage_import_into(from, &crate::native_paths::data_dir())?;
    note_staged();
    Ok(())
}

/// A new import waits, and stands in for any that failed before it.
fn note_staged() {
    IMPORT_STAGED.store(true, Ordering::Relaxed);
    *LAST_IMPORT_ERROR
        .lock()
        .unwrap_or_else(|err| err.into_inner()) = None;
}

fn stage_import_into(from: &Path, data: &Path) -> Result<()> {
    let manifest: Manifest = serde_json::from_slice(
        &fs::read(from.join(MANIFEST))
            .with_context(|| format!("{} is not a ThinkTerm backup", from.display()))?,
    )
    .context("read the backup's manifest")?;
    if manifest.format != FORMAT {
        bail!("this backup was made by a different version of ThinkTerm");
    }
    let known: Vec<&str> = state_files().iter().map(|(name, _)| *name).collect();
    let names: Vec<&String> = manifest
        .files
        .iter()
        .filter(|name| known.contains(&name.as_str()))
        .collect();
    if !names.iter().any(|name| *name == STORE) {
        bail!("the backup has no Spaces to import");
    }
    // Every file must read back before anything is staged.
    for name in &names {
        check_file(name, &from.join(name))
            .with_context(|| format!("{name} in the backup cannot be read"))?;
    }

    config::create_user_owned_dirs(data)?;
    let staging = tempfile::Builder::new()
        .prefix(".import-staging-")
        .tempdir_in(data)?;
    for name in &names {
        let to = staging.path().join(name);
        fs::copy(from.join(name), &to).with_context(|| format!("copy {name}"))?;
        make_private(&to)?;
    }
    let pending = data.join(PENDING);
    let _ = fs::remove_dir_all(&pending);
    let staged = staging.keep();
    if let Err(err) = fs::rename(&staged, &pending) {
        let _ = fs::remove_dir_all(&staged);
        return Err(err).context("stage the import");
    }
    Ok(())
}

/// Called first by every way the GUI starts, before anything reads the
/// state: apply an import waiting from last session if no other window is
/// running, then join the running windows for the rest of this process.
pub(crate) fn enter_gui() {
    let data = crate::native_paths::data_dir();
    if let Err(err) = config::create_user_owned_dirs(&data) {
        log::warn!("cannot create {}: {err:#}", data.display());
        return;
    }
    let lock = match fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(false)
        .open(data.join(RUNNING_LOCK))
    {
        Ok(lock) => lock,
        Err(err) => {
            log::warn!("cannot open the window lock: {err:#}");
            return;
        }
    };
    let pending = data.join(PENDING);
    if pending.is_dir() {
        // Waiting stops early if another launch applies it meanwhile.
        let alone = match wait_for(|| lock.try_lock(), || pending.is_dir()) {
            Ok(alone) => alone,
            Err(err) => {
                // A folder that cannot lock cannot say whether a window runs;
                // the import is what was asked for, so it goes ahead.
                log::warn!("cannot lock {}: {err}; importing unchecked", data.display());
                true
            }
        };
        // A launch that took the lock first may have applied it meanwhile.
        if alone && pending.is_dir() {
            let targets: Vec<(&str, PathBuf)> = state_files()
                .into_iter()
                .filter_map(|(name, path)| Some((name, path?)))
                .collect();
            match apply_pending_into(&data, &targets) {
                Ok(()) => log::warn!("imported the ThinkTerm backup staged last session"),
                Err(err) => {
                    log::error!("cannot import the staged ThinkTerm backup: {err:#}");
                    *LAST_IMPORT_ERROR
                        .lock()
                        .unwrap_or_else(|err| err.into_inner()) = Some(format!("{err:#}"));
                }
            }
        }
        if alone {
            let _ = lock.unlock();
        } else if pending.is_dir() {
            log::warn!("another ThinkTerm window still runs; the import waits for a later launch");
        }
    }
    IMPORT_STAGED.store(pending.is_dir(), Ordering::Relaxed);
    // Joined for good; waits only while another launch is mid-import.
    if let Ok(false) | Err(_) = wait_for(|| lock.try_lock_shared(), || true) {
        log::warn!("cannot hold the window lock; an import could apply under this window");
    }
    static HELD: std::sync::OnceLock<fs::File> = std::sync::OnceLock::new();
    let _ = HELD.set(lock);
}

/// Try `lock` until it succeeds, `WAIT_FOR_OTHERS` passes or `wanted` stops
/// holding: `Ok(false)` if it is still held elsewhere or no longer wanted,
/// `Err` if the folder cannot lock at all.
fn wait_for(
    lock: impl Fn() -> Result<(), fs::TryLockError>,
    wanted: impl Fn() -> bool,
) -> std::io::Result<bool> {
    let deadline = Instant::now() + WAIT_FOR_OTHERS;
    loop {
        match lock() {
            Ok(()) => return Ok(true),
            Err(fs::TryLockError::WouldBlock) if Instant::now() < deadline && wanted() => {
                std::thread::sleep(Duration::from_millis(100));
            }
            Err(fs::TryLockError::WouldBlock) => return Ok(false),
            Err(fs::TryLockError::Error(err)) => return Err(err),
        }
    }
}

/// Put the staged files in place. Every file is first copied beside its
/// target, and what it replaces kept, before any target is touched; if that
/// fails the copies are removed and nothing changes. Then each is renamed
/// over its target; if one cannot be, the ones before it get back what they
/// replaced, so a failure changes nothing then either. The staged import is
/// cleared either way, so a failure is reported once rather than retried at
/// every launch.
fn apply_pending_into(data: &Path, targets: &[(&str, PathBuf)]) -> Result<()> {
    let pending = data.join(PENDING);
    // Already applied, by a launch that took the lock first.
    if !pending.is_dir() {
        return Ok(());
    }
    let result = prepare_and_replace(&pending, targets);
    let cleared = fs::remove_dir_all(&pending).context("clear the staged import");
    result.and(cleared)
}

/// A staged file on its way in: its copy beside the target, and where what
/// it replaces is kept (`None` when nothing was there).
struct Incoming<'a> {
    temp: PathBuf,
    target: &'a PathBuf,
    kept: Option<PathBuf>,
}

fn prepare_and_replace(pending: &Path, targets: &[(&str, PathBuf)]) -> Result<()> {
    let stamp = chrono::Local::now().format("%Y%m%d-%H%M%S").to_string();
    let mut incoming: Vec<Incoming> = vec![];
    let prepare = (|| -> Result<()> {
        for (name, target) in targets {
            let from = pending.join(name);
            if !from.is_file() {
                continue;
            }
            if let Some(parent) = target.parent() {
                config::create_user_owned_dirs(parent)?;
            }
            let stem = Path::new(name)
                .file_stem()
                .and_then(|stem| stem.to_str())
                .unwrap_or(name);
            // Listed before either is written, so a copy cut short (a full
            // disk) is removed with the rest.
            let file = Incoming {
                temp: target.with_file_name(format!(".{name}.importing")),
                target,
                kept: target
                    .is_file()
                    .then(|| target.with_file_name(format!("{stem}.before-import-{stamp}.json"))),
            };
            incoming.push(file);
            let file = incoming.last().expect("just pushed");
            if let Some(kept) = &file.kept {
                fs::copy(target, kept).with_context(|| format!("keep the current {name}"))?;
            }
            fs::copy(&from, &file.temp).with_context(|| format!("copy {name}"))?;
            make_private(&file.temp)?;
        }
        Ok(())
    })();
    if let Err(err) = prepare {
        discard(&incoming);
        return Err(err);
    }
    for (replaced, file) in incoming.iter().enumerate() {
        if let Err(err) = fs::rename(&file.temp, file.target) {
            let err = anyhow::Error::new(err).context(format!("replace {}", file.target.display()));
            discard(&incoming[replaced..]);
            return match put_back(&incoming[..replaced]) {
                Ok(()) => Err(err),
                Err(put_back) => Err(err.context(format!("{put_back:#}"))),
            };
        }
    }
    Ok(())
}

/// Remove what was prepared for files not put in place.
fn discard(files: &[Incoming]) {
    for file in files {
        let _ = fs::remove_file(&file.temp);
        if let Some(kept) = &file.kept {
            let _ = fs::remove_file(kept);
        }
    }
}

/// Give files already replaced back what they replaced: the kept copy, or
/// nothing where there was nothing. A copy that cannot go back stays kept.
fn put_back(replaced: &[Incoming]) -> Result<()> {
    let mut failed = None;
    for file in replaced.iter().rev() {
        let result = match &file.kept {
            Some(kept) => fs::rename(kept, file.target),
            None => fs::remove_file(file.target),
        };
        if let Err(err) = result {
            failed.get_or_insert_with(|| {
                anyhow::Error::new(err).context(format!("put back {}", file.target.display()))
            });
        }
    }
    failed.map_or(Ok(()), Err)
}

#[cfg(test)]
mod test {
    use super::*;

    fn write(dir: &Path, name: &str, body: &str) {
        fs::create_dir_all(dir).unwrap();
        fs::write(dir.join(name), body).unwrap();
    }

    fn exported(base: &Path) -> PathBuf {
        let state = base.join("state");
        write(&state, "ssh_hosts.json", "{\"version\":1,\"hosts\":[]}");
        write(&state, "secret.key", "key");
        let into = base.join("exports");
        fs::create_dir_all(&into).unwrap();
        export_to(
            &into,
            &crate::workspace_threads::WorkspaceThreadStore::default(),
            &[
                ("ssh_hosts.json", state.join("ssh_hosts.json")),
                ("snippets.json", state.join("snippets.json")),
            ],
        )
        .unwrap()
    }

    fn names_in(dir: &Path) -> Vec<String> {
        let mut names: Vec<String> = fs::read_dir(dir)
            .unwrap()
            .flatten()
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        names
    }

    #[test]
    fn an_export_is_one_whole_folder_without_the_key() {
        let base = tempfile::tempdir().unwrap();
        let backup = exported(base.path());
        assert!(backup
            .file_name()
            .unwrap()
            .to_string_lossy()
            .starts_with("ThinkTerm Backup "));
        assert!(backup.join(STORE).is_file());
        assert!(backup.join("ssh_hosts.json").is_file());
        // Not there to copy, so not listed either.
        assert!(!backup.join("snippets.json").exists());
        assert!(!backup.join("secret.key").exists());
        let manifest: Manifest =
            serde_json::from_slice(&fs::read(backup.join(MANIFEST)).unwrap()).unwrap();
        assert_eq!(manifest.files, vec![STORE, "ssh_hosts.json"]);
        // No staging folder is left behind.
        assert_eq!(names_in(&base.path().join("exports")).len(), 1);
    }

    #[test]
    fn an_import_is_staged_then_applied_keeping_what_it_replaces() {
        let base = tempfile::tempdir().unwrap();
        let backup = exported(base.path());
        let data = base.path().join("data");
        let store = data.join(STORE);
        write(&data, STORE, "{\"current\":true}");
        stage_import_into(&backup, &data).unwrap();
        // Staging changes nothing yet.
        assert_eq!(fs::read_to_string(&store).unwrap(), "{\"current\":true}");

        let hosts = base.path().join("elsewhere/ssh_hosts.json");
        let targets = [(STORE, store.clone()), ("ssh_hosts.json", hosts.clone())];
        apply_pending_into(&data, &targets).unwrap();
        assert_eq!(
            fs::read(&store).unwrap(),
            fs::read(backup.join(STORE)).unwrap()
        );
        assert!(hosts.is_file());
        let kept: Vec<String> = names_in(&data)
            .into_iter()
            .filter(|name| name.starts_with("workspace_threads.before-import-"))
            .collect();
        assert_eq!(kept.len(), 1);
        assert_eq!(
            fs::read_to_string(data.join(&kept[0])).unwrap(),
            "{\"current\":true}"
        );
        // Applied once only, and nothing half-done is left.
        assert!(!data.join(PENDING).exists());
        assert!(!names_in(&data)
            .iter()
            .any(|name| name.ends_with(".importing")));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(&hosts).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
        }
    }

    #[test]
    fn a_failed_import_changes_nothing_and_is_not_retried() {
        let base = tempfile::tempdir().unwrap();
        let backup = exported(base.path());
        let data = base.path().join("data");
        let store = data.join(STORE);
        write(&data, STORE, "{\"current\":true}");
        stage_import_into(&backup, &data).unwrap();
        // A target whose folder cannot be made (a file is in the way) fails
        // while the files are being prepared, after the store was.
        fs::write(base.path().join("blocked"), "x").unwrap();
        let blocked = base.path().join("blocked/ssh_hosts.json");
        let targets = [(STORE, store.clone()), ("ssh_hosts.json", blocked)];
        assert!(apply_pending_into(&data, &targets).is_err());
        assert_eq!(fs::read_to_string(&store).unwrap(), "{\"current\":true}");
        assert!(!data.join(PENDING).exists());
        // Nothing prepared or kept is left behind.
        assert_eq!(names_in(&data), vec![STORE.to_string()]);
    }

    #[test]
    fn an_import_another_launch_applied_meanwhile_is_not_a_failure() {
        let base = tempfile::tempdir().unwrap();
        let backup = exported(base.path());
        let data = base.path().join("data");
        let store = data.join(STORE);
        write(&data, STORE, "{\"current\":true}");
        stage_import_into(&backup, &data).unwrap();
        let targets = [(STORE, store.clone())];
        // The first launch applies it; the second got the lock after that.
        apply_pending_into(&data, &targets).unwrap();
        apply_pending_into(&data, &targets).unwrap();
        assert_eq!(
            fs::read(&store).unwrap(),
            fs::read(backup.join(STORE)).unwrap()
        );
    }

    #[test]
    fn a_failure_while_replacing_puts_back_what_was_replaced() {
        let base = tempfile::tempdir().unwrap();
        let backup = exported(base.path());
        let data = base.path().join("data");
        let store = data.join(STORE);
        write(&data, STORE, "{\"current\":true}");
        stage_import_into(&backup, &data).unwrap();
        // A folder where the hosts file goes: it is prepared like any other,
        // but nothing can be renamed over it, and the store goes first.
        let elsewhere = base.path().join("elsewhere");
        let hosts = elsewhere.join("ssh_hosts.json");
        fs::create_dir_all(hosts.join("in-the-way")).unwrap();
        let targets = [(STORE, store.clone()), ("ssh_hosts.json", hosts.clone())];
        assert!(apply_pending_into(&data, &targets).is_err());
        assert_eq!(fs::read_to_string(&store).unwrap(), "{\"current\":true}");
        assert!(hosts.is_dir());
        // Nothing prepared or kept is left behind.
        assert_eq!(names_in(&data), vec![STORE.to_string()]);
        assert_eq!(names_in(&elsewhere), vec!["ssh_hosts.json".to_string()]);
    }

    #[test]
    fn a_new_import_clears_the_last_failure() {
        *LAST_IMPORT_ERROR.lock().unwrap() = Some("disk full".to_string());
        note_staged();
        assert!(import_staged());
        assert_eq!(last_import_error(), None);
    }

    #[test]
    fn a_folder_that_is_not_a_backup_is_refused() {
        let base = tempfile::tempdir().unwrap();
        let data = base.path().join("data");
        let stranger = base.path().join("stranger");
        write(&stranger, STORE, "{}");
        assert!(stage_import_into(&stranger, &data).is_err());
        // Valid JSON of the wrong shape is refused too.
        let backup = exported(base.path());
        fs::write(backup.join("ssh_hosts.json"), "{\"hosts\":\"x\"}").unwrap();
        assert!(stage_import_into(&backup, &data).is_err());
        assert!(!data.join(PENDING).exists());
    }

    #[test]
    fn only_a_lock_held_elsewhere_makes_a_launch_wait() {
        let base = tempfile::tempdir().unwrap();
        let path = base.path().join(RUNNING_LOCK);
        let running = fs::File::create(&path).unwrap();
        running.try_lock_shared().unwrap();
        // Another window running: the exclusive lock an import needs waits,
        // then gives up; joining the running windows does not wait.
        let starting = fs::File::create(&path).unwrap();
        assert!(matches!(
            starting.try_lock(),
            Err(fs::TryLockError::WouldBlock)
        ));
        assert!(wait_for(|| starting.try_lock_shared(), || true).unwrap());
        drop(running);
        starting.unlock().unwrap();
        assert!(wait_for(|| starting.try_lock(), || true).unwrap());
    }

    #[test]
    fn a_launch_stops_waiting_once_the_import_is_no_longer_wanted() {
        let base = tempfile::tempdir().unwrap();
        let path = base.path().join(RUNNING_LOCK);
        let running = fs::File::create(&path).unwrap();
        running.try_lock_shared().unwrap();
        let starting = fs::File::create(&path).unwrap();
        let begun = Instant::now();
        assert!(!wait_for(|| starting.try_lock(), || false).unwrap());
        assert!(begun.elapsed() < WAIT_FOR_OTHERS);
    }
}
