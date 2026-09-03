use crate::ICON_DATA;
use anyhow::anyhow;
use config::{configuration, wezterm_version};
use http_req::request::{HttpVersion, Request};
use http_req::uri::Uri;
use mux::connui::ConnectionUI;
use serde::*;
use std::convert::TryFrom;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;
use std::time::{Duration, SystemTime};
use termwiz::cell::{Hyperlink, Underline};
use termwiz::color::AnsiColor;
use termwiz::escape::csi::{Cursor, Sgr};
use termwiz::escape::osc::{ITermDimension, ITermFileData, ITermProprietary};
use termwiz::escape::{OneBased, OperatingSystemCommand, CSI};
use wezterm_toast_notification::*;

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct Release {
    pub url: String,
    pub body: String,
    pub html_url: String,
    pub tag_name: String,
    pub assets: Vec<Asset>,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct Asset {
    pub name: String,
    pub size: usize,
    pub url: String,
    pub browser_download_url: String,
}

/// The repository every update link points at. The update check, the toast,
/// and the Settings Update/About pages all build their URLs from this, so a
/// fork or a rename is one edit rather than a grep.
pub const REPO_URL: &str = "https://github.com/RoversX/thinkterm";

pub fn releases_url() -> String {
    format!("{REPO_URL}/releases")
}

pub fn release_tag_url(tag: &str) -> String {
    format!("{REPO_URL}/releases/tag/{tag}")
}

fn get_github_release_info(uri: &str) -> anyhow::Result<Release> {
    let uri = Uri::try_from(uri)?;

    let mut latest = Vec::new();
    let _res = Request::new(&uri)
        .version(HttpVersion::Http10)
        .header("User-Agent", &format!("thinkterm/{}", wezterm_version()))
        .send(&mut latest)
        .map_err(|e| anyhow!("failed to query github releases: {}", e))?;

    /*
    println!("Status: {} {}", _res.status_code(), _res.reason());
    println!("{}", String::from_utf8_lossy(&latest));
    */

    let latest: Release = serde_json::from_slice(&latest)?;
    Ok(latest)
}

pub fn get_latest_release_info() -> anyhow::Result<Release> {
    get_github_release_info("https://api.github.com/repos/RoversX/thinkterm/releases/latest")
}

#[allow(unused)]
pub fn get_nightly_release_info() -> anyhow::Result<Release> {
    get_github_release_info("https://api.github.com/repos/RoversX/thinkterm/releases/tags/nightly")
}

lazy_static::lazy_static! {
    static ref UPDATER_WINDOW: Mutex<Option<ConnectionUI>> = Mutex::new(None);
}

pub fn load_last_release_info_and_set_banner() {
    if !configuration().check_for_updates {
        return;
    }

    let update_file_name = config::DATA_DIR.join("check_update");
    if let Ok(data) = std::fs::read(update_file_name) {
        let latest: Release = match serde_json::from_slice(&data) {
            Ok(d) => d,
            Err(_) => return,
        };

        let current = running_release_version();
        let force_ui = always_show_update_ui();
        if !is_newer_release(&latest.tag_name, &current) && !force_ui {
            return;
        }

        set_banner_from_release_info(&latest);
    }
}

fn set_banner_from_release_info(latest: &Release) {
    let mux = crate::Mux::get();
    let url = release_tag_url(&latest.tag_name);

    let icon = ITermFileData {
        name: None,
        size: Some(ICON_DATA.len()),
        width: ITermDimension::Automatic,
        height: ITermDimension::Cells(2),
        preserve_aspect_ratio: true,
        inline: true,
        do_not_move_cursor: false,
        data: ICON_DATA.to_vec(),
    };
    let icon = OperatingSystemCommand::ITermProprietary(ITermProprietary::File(Box::new(icon)));
    let top_line_pos = CSI::Cursor(Cursor::CharacterAndLinePosition {
        line: OneBased::new(1),
        col: OneBased::new(6),
    });
    let second_line_pos = CSI::Cursor(Cursor::CharacterAndLinePosition {
        line: OneBased::new(2),
        col: OneBased::new(6),
    });
    let link_on = OperatingSystemCommand::SetHyperlink(Some(Hyperlink::new(url)));
    let underline_color = CSI::Sgr(Sgr::UnderlineColor(AnsiColor::Blue.into()));
    let underline_on = CSI::Sgr(Sgr::Underline(Underline::Single));
    let reset = CSI::Sgr(Sgr::Reset);
    let link_off = OperatingSystemCommand::SetHyperlink(None);
    mux.set_banner(Some(format!(
        "{}{}ThinkTerm Update Available\r\n{}{}{}{}Click to see what's new{}{}\r\n",
        icon,
        top_line_pos,
        second_line_pos,
        link_on,
        underline_color,
        underline_on,
        link_off,
        reset,
    )));
}

fn schedule_set_banner_from_release_info(latest: &Release) {
    let current = running_release_version();
    if !is_newer_release(&latest.tag_name, &current) {
        return;
    }
    promise::spawn::spawn_into_main_thread({
        let latest = latest.clone();
        async move {
            set_banner_from_release_info(&latest);
        }
    })
    .detach();
}

/// The release version of the running build, for comparison against the tag
/// of the latest GitHub release.
///
/// `wezterm_version()` is baked in at compile time from `.tag`, which only the
/// release workflow writes -- and macOS is the one platform whose packages are
/// built by hand rather than in CI, so its binaries carry a commit stamp that
/// `is_newer_release` deliberately refuses to compare against a `v*` tag. The
/// bundle's Info.plist does carry the release version, is read at runtime
/// rather than compile time, and is already what Finder and the About panel
/// show, so prefer it when we are running from inside an app bundle.
pub fn running_release_version() -> String {
    #[cfg(target_os = "macos")]
    {
        if let Some(version) = macos_bundle_version() {
            return version;
        }
    }
    wezterm_version().to_string()
}

/// `<bundle>.app/Contents/MacOS/<exe>` puts Info.plist one level up from the
/// executable's directory. Returns None for a bare `cargo build` binary, which
/// has no bundle and should keep reporting its commit stamp.
#[cfg(target_os = "macos")]
fn macos_bundle_version() -> Option<String> {
    let exe = std::env::current_exe().ok()?;
    version_from_info_plist(exe.parent()?.parent()?.join("Info.plist"))
}

#[cfg(target_os = "macos")]
fn version_from_info_plist(path: std::path::PathBuf) -> Option<String> {
    let value = plist::Value::from_file(path).ok()?;
    let version = value
        .as_dictionary()?
        .get("CFBundleShortVersionString")?
        .as_string()?
        .trim()
        .to_string();
    if version.is_empty() {
        None
    } else {
        Some(version)
    }
}

/// Is `latest` a release the running build should be told about?
///
/// The two strings only compare meaningfully when they use the same scheme.
/// A CI build carries the release tag (the workflow writes `.tag`, which
/// wezterm-version's build.rs prefers); anything built from a plain checkout
/// carries a `<date>-<hash>` commit stamp instead. Comparing across the two
/// with `>` is what made every `v*` release look permanently newer than every
/// local build: 'v' sorts above every digit, so the banner never went away.
fn is_newer_release(latest: &str, current: &str) -> bool {
    let parse = |s: &str| semver::Version::parse(s.trim_start_matches('v')).ok();
    match (parse(latest), parse(current)) {
        (Some(latest), Some(current)) => latest > current,
        // Both on the commit-stamp scheme, which upstream still uses for its
        // own tags. It starts with a zero-padded date, so lexicographic order
        // is chronological order.
        (None, None) => latest > current,
        // One of each: there is no ordering between the schemes, and a build
        // that never came from a release is not something to nag about.
        _ => false,
    }
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

    /// The bundle template is the version a hand-built macOS package reports,
    /// so a release that forgets to bump it silently stops notifying users.
    #[cfg(target_os = "macos")]
    #[test]
    fn bundle_template_carries_a_comparable_version() {
        let plist = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../assets/macos/ThinkTerm.app/Contents/Info.plist");
        let version = super::version_from_info_plist(plist)
            .expect("the shipped Info.plist must declare CFBundleShortVersionString");
        assert!(
            semver::Version::parse(version.trim_start_matches('v')).is_ok(),
            "Info.plist version {version:?} must parse as semver, or macOS \
             builds cannot be compared against a release tag"
        );
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
                schedule_set_banner_from_release_info(&latest);
                let current = running_release_version();
                if is_newer_release(&latest.tag_name, &current) || force_ui {
                    log::info!(
                        "latest release {} is newer than current build {}",
                        latest.tag_name,
                        current
                    );

                    let url = release_tag_url(&latest.tag_name);

                    if force_ui || socks.is_empty() || socks[0] == my_sock {
                        persistent_toast_notification_with_click_to_open_url(
                            "ThinkTerm Update Available",
                            "Click to see what's new",
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
