use config::configuration;
use mux::connui::ConnectionUI;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;
use std::time::{Duration, SystemTime};
use wezterm_toast_notification::*;

// The release lookup, the running version and the version comparison are
// shared with `thinkterm update` and the mux client's remote update; the GUI
// keeps its names for them so the Settings pages and the toast read the same.
pub use thinkterm_update::{
    get_latest_release_info, is_newer_release, release_tag_url, releases_url,
    running_release_version, Release, REPO_URL,
};

lazy_static::lazy_static! {
    static ref UPDATER_WINDOW: Mutex<Option<ConnectionUI>> = Mutex::new(None);
}

/// Whether a release newer than this build is known. What the sidebar's
/// settings button reads on every paint, so it is a flag and not a file:
/// set from the cache at startup and by the checker after every check.
///
/// Nothing is written into terminal output any more. The upstream banner
/// printed two rows and an icon into every new local shell and left them in
/// its scrollback; a dot on the settings button says the same thing without
/// taking any space, on remote panes too, and leads to the page that can
/// act on it.
static UPDATE_AVAILABLE: AtomicBool = AtomicBool::new(false);

pub fn update_available() -> bool {
    UPDATE_AVAILABLE.load(Ordering::Relaxed)
}

/// Record the flag; when it changes, ask every window to repaint so the dot
/// appears or goes away. Only the checker thread calls this: it runs once
/// the GUI is up, and spawning onto the main thread needs its scheduler.
fn set_update_available(available: bool) {
    let was = UPDATE_AVAILABLE.swap(available, Ordering::Relaxed);
    if was != available {
        promise::spawn::spawn_into_main_thread(async move {
            crate::frontend::front_end().invalidate_all_windows();
        })
        .detach();
    }
}

/// Seed the flag from the last check on disk, so the dot is right from the
/// first frame rather than after the checker's first delay. Called before
/// any window exists, so it only stores: the first paint reads it.
pub fn load_last_release_info() {
    if !configuration().check_for_updates {
        return;
    }
    let status = cached_update_status();
    UPDATE_AVAILABLE.store(
        status.update_available || always_show_update_ui(),
        Ordering::Relaxed,
    );
}

/// Everything the Settings > Update page can say without touching the network.
///
/// `update_checker` already persists the last release it saw to
/// `DATA_DIR/check_update`, and the filesystem stamps that write with the time
/// of the check. Reading it back is pure local state, which is what lets the
/// page render on the UI thread: it can name the running build, the newest
/// release this install has ever heard of, and when it last looked, without
/// opening a socket. A live check is a separate concern and does not belong on
/// the paint path.
#[derive(Debug, Clone)]
pub struct CachedUpdateStatus {
    /// The running build, in the same form `is_newer_release` compares.
    pub current_version: String,
    /// The newest release recorded by the last successful check, if any.
    pub latest: Option<Release>,
    /// Whether `latest` is something this build should upgrade to. False
    /// whenever the two versions use different schemes -- see
    /// `is_newer_release` for why a local build is never nagged.
    pub update_available: bool,
    /// When the last check completed. None until one has ever run.
    pub last_checked: Option<SystemTime>,
}

impl CachedUpdateStatus {
    /// Whether the running build carries a release tag rather than a commit
    /// stamp. A commit-stamped build has no ordering against any release, so
    /// the UI must say so instead of claiming to be up to date.
    pub fn running_a_release_build(&self) -> bool {
        semver::Version::parse(self.current_version.trim_start_matches('v')).is_ok()
    }
}

pub fn cached_update_status() -> CachedUpdateStatus {
    let current_version = running_release_version();
    let path = config::DATA_DIR.join("check_update");
    let last_checked = path.metadata().and_then(|meta| meta.modified()).ok();
    let latest = std::fs::read(&path)
        .ok()
        .and_then(|data| serde_json::from_slice::<Release>(&data).ok());
    let update_available = latest
        .as_ref()
        .is_some_and(|latest| is_newer_release(&latest.tag_name, &current_version));

    CachedUpdateStatus {
        current_version,
        latest,
        update_available,
        last_checked,
    }
}

#[cfg(test)]
mod update_version_tests {
    use super::is_newer_release;

    const STAMP: &str = "20260819-153532-9d50c4cc";

    #[test]
    fn semver_releases_compare_numerically() {
        assert!(is_newer_release("v0.2.0", "v0.1.0"));
        assert!(!is_newer_release("v0.1.0", "v0.2.0"));
        assert!(!is_newer_release("v0.1.0", "v0.1.0"));
        // The reason a plain string compare is not good enough.
        assert!(is_newer_release("v0.10.0", "v0.9.0"));
    }

    #[test]
    fn a_release_never_nags_a_local_build() {
        assert!(!is_newer_release("v0.1.0", STAMP));
        assert!(!is_newer_release("v9.9.9", STAMP));
        assert!(!is_newer_release(STAMP, "v0.1.0"));
    }

    #[test]
    fn commit_stamps_still_compare_chronologically() {
        assert!(is_newer_release("20260901-000000-aaaaaaaa", STAMP));
        assert!(!is_newer_release("20260101-000000-aaaaaaaa", STAMP));
    }

}

/// Returns true if the provided socket path is dead.
fn update_checker() {
    // Compute how long we should sleep for;
    // if we've never checked, give it a few seconds after the first
    // launch, otherwise compute the interval based on the time of
    // the last check.
    let update_interval = Duration::from_secs(configuration().check_for_updates_interval_seconds);
    let initial_interval = Duration::from_secs(10);

    let force_ui = always_show_update_ui();

    let update_file_name = config::DATA_DIR.join("check_update");
    let delay = update_file_name
        .metadata()
        .and_then(|metadata| metadata.modified())
        .map_err(|_| ())
        .and_then(|systime| {
            let elapsed = systime.elapsed().unwrap_or(Duration::new(0, 0));
            update_interval.checked_sub(elapsed).ok_or(())
        })
        .unwrap_or(initial_interval);

    std::thread::sleep(if force_ui { initial_interval } else { delay });

    let my_sock = config::RUNTIME_DIR.join(format!("gui-sock-{}", unsafe { libc::getpid() }));

    loop {
        // Figure out which other wezterm-guis are running.
        // We have a little "consensus protocol" to decide which
        // of us will show the toast notification or show the update
        // window: the one of us that sorts first in the list will
        // own doing that, so that if there are a dozen gui processes
        // running, we don't spam the user with a lot of notifications.
        let socks = wezterm_client::discovery::discover_gui_socks();

        if configuration().check_for_updates {
            if let Ok(latest) = get_latest_release_info() {
                let current = running_release_version();
                let newer = is_newer_release(&latest.tag_name, &current);
                set_update_available(newer || force_ui);
                if newer || force_ui {
                    log::info!(
                        "latest release {} is newer than current build {}",
                        latest.tag_name,
                        current
                    );

                    let url = release_tag_url(&latest.tag_name);

                    if force_ui || socks.is_empty() || socks[0] == my_sock {
                        persistent_toast_notification_with_click_to_open_url(
                            &crate::i18n::tr("update-toast-title"),
                            &crate::i18n::tr("update-toast-body"),
                            &url,
                        );
                    }
                }

                config::create_user_owned_dirs(update_file_name.parent().unwrap()).ok();

                // Record the time of this check
                if let Ok(f) = std::fs::OpenOptions::new()
                    .write(true)
                    .create(true)
                    .truncate(true)
                    .open(&update_file_name)
                {
                    serde_json::to_writer_pretty(f, &latest).ok();
                }
            }
        }

        std::thread::sleep(Duration::from_secs(
            configuration().check_for_updates_interval_seconds,
        ));
    }
}

fn always_show_update_ui() -> bool {
    std::env::var_os("THINKTERM_ALWAYS_SHOW_UPDATE_UI").is_some()
        || std::env::var_os("WEZTERM_ALWAYS_SHOW_UPDATE_UI").is_some()
}

pub fn start_update_checker() {
    static CHECKER_STARTED: AtomicBool = AtomicBool::new(false);
    if let Ok(false) =
        CHECKER_STARTED.compare_exchange(false, true, Ordering::Relaxed, Ordering::Relaxed)
    {
        std::thread::Builder::new()
            .name("update_checker".into())
            .spawn(update_checker)
            .expect("failed to spawn update checker thread");
    }
}
