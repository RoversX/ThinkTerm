use mux::connui::ConnectionUI;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;
use std::time::{Duration, SystemTime};
use wezterm_toast_notification::*;

// The release lookup, the running version and the version comparison are
// shared with `thinkterm update` and the mux client's remote update; the GUI
// keeps its names for them so the Settings pages and the toast read the same.
pub use thinkterm_update::{
    get_latest_release_info, is_newer_build, judge_release, release_tag_url, releases_url,
    running_build_commit_time, running_release_version, same_release_version, Release, REPO_URL,
};

lazy_static::lazy_static! {
    static ref UPDATER_WINDOW: Mutex<Option<ConnectionUI>> = Mutex::new(None);
}

/// Whether a release newer than this build is known. What the update
/// capsule reads on every paint, so it is a flag and not a file: set from
/// the cache at startup and by the checker after every check.
///
/// Nothing is written into terminal output any more. The upstream banner
/// printed two rows and an icon into every new local shell and left them in
/// its scrollback; the capsule floating in the window's corner says the same
/// thing without taking any of the terminal's lines, on remote panes too,
/// and installs the update itself.
static UPDATE_AVAILABLE: AtomicBool = AtomicBool::new(false);

pub fn update_available() -> bool {
    UPDATE_AVAILABLE.load(Ordering::Relaxed)
}

lazy_static::lazy_static! {
    /// The release `UPDATE_AVAILABLE` is about, kept in memory with the
    /// flag: the update notice names it on every paint and installs it when
    /// clicked, whether or not the check could be saved to disk.
    static ref AVAILABLE_RELEASE: Mutex<Option<Release>> = Mutex::new(None);
}

/// The newer release, while one is known.
pub fn available_release() -> Option<Release> {
    if !update_available() {
        return None;
    }
    AVAILABLE_RELEASE.lock().ok()?.clone()
}

/// Whether the update on offer carries the running build's own version: a
/// release made from later code than this hand-made package of it. A flag
/// for the same reason `UPDATE_AVAILABLE` is one: the capsule reads it on
/// every paint, and on macOS the running version comes from a file.
static AVAILABLE_IS_NEW_BUILD: AtomicBool = AtomicBool::new(false);

pub fn available_is_new_build() -> bool {
    update_available() && AVAILABLE_IS_NEW_BUILD.load(Ordering::Relaxed)
}

/// The newer release's version, without its `v`, while one is known.
pub fn available_version() -> Option<String> {
    if !update_available() {
        return None;
    }
    let release = AVAILABLE_RELEASE.lock().ok()?;
    release
        .as_ref()
        .map(|release| release.tag_name.trim_start_matches('v').to_string())
}

/// Record the flag and the release it is about; when either changes, ask
/// every window to repaint so the update notice appears, goes away or
/// names the new release. Only the checker thread and a check asked for
/// from Settings call this: both run once the GUI is up, and spawning onto
/// the main thread needs its scheduler.
fn set_update_available(available: bool, new_build: bool, latest: &Release) {
    AVAILABLE_IS_NEW_BUILD.store(new_build, Ordering::Relaxed);
    let renamed = AVAILABLE_RELEASE
        .lock()
        .map(|mut release| {
            let renamed = release.as_ref().map(|release| release.tag_name.as_str())
                != Some(latest.tag_name.as_str());
            *release = Some(latest.clone());
            renamed
        })
        .unwrap_or(false);
    let was = UPDATE_AVAILABLE.swap(available, Ordering::Relaxed);
    if was != available || (available && renamed) {
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
    if !crate::native_settings::checks_for_updates(&crate::native_settings::load_shared()) {
        return;
    }
    let status = cached_update_status();
    AVAILABLE_IS_NEW_BUILD.store(
        status.update_available
            && status
                .latest
                .as_ref()
                .is_some_and(|latest| same_release_version(&latest.tag_name, &status.current_version)),
        Ordering::Relaxed,
    );
    if let Ok(mut release) = AVAILABLE_RELEASE.lock() {
        release.clone_from(&status.latest);
    }
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
        thinkterm_update::is_release_version(&self.current_version)
    }
}

/// The release the last check recorded, if it could be read back.
fn cached_release() -> Option<Release> {
    std::fs::read(config::DATA_DIR.join("check_update"))
        .ok()
        .and_then(|data| serde_json::from_slice::<Release>(&data).ok())
}

/// When the last check on record was made: when its answer was written.
fn last_checked() -> Option<SystemTime> {
    let path = config::DATA_DIR.join("check_update");
    path.metadata().and_then(|meta| meta.modified()).ok()
}

pub fn cached_update_status() -> CachedUpdateStatus {
    let current_version = running_release_version();
    let last_checked = last_checked();
    let latest = cached_release();
    let update_available = latest.as_ref().is_some_and(|latest| {
        is_newer_build(
            &latest.tag_name,
            latest.thinkterm_commit_time,
            &current_version,
            running_build_commit_time(),
        )
    });

    CachedUpdateStatus {
        current_version,
        latest,
        update_available,
        last_checked,
    }
}

#[cfg(test)]
mod update_version_tests {
    use thinkterm_update::is_newer_release;

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

/// Ask GitHub now, as the background checker does on its schedule: record
/// the answer where Settings reads it and set the dot. Blocks on the
/// network, so callers run it off the UI thread. No toast: whoever asked is
/// looking at the answer.
pub fn check_now() -> anyhow::Result<()> {
    let mut latest = get_latest_release_info()?;
    let verdict = judge_release(&mut latest, &running_release_version(), cached_release().as_ref());
    set_update_available(
        verdict.newer || always_show_update_ui(),
        verdict.new_build,
        &latest,
    );
    // Settings shows what it reads back from the cache: an answer that could
    // not be kept there is, as far as the page can tell, no answer.
    record_latest_release(&latest)
}

/// The release to install in place of `offered`, the one on offer: a newer
/// one may have come out since it was found, and installing it would only
/// be followed by another update. What GitHub says is recorded as a check
/// records it; when it cannot say, the offer stands. Blocks on the network,
/// so callers run it off the UI thread.
pub fn newest_release(offered: Release) -> Release {
    let Ok(mut latest) = get_latest_release_info() else {
        return offered;
    };
    let verdict = judge_release(
        &mut latest,
        &running_release_version(),
        cached_release().as_ref(),
    );
    if !verdict.newer {
        return offered;
    }
    set_update_available(true, verdict.new_build, &latest);
    if let Err(err) = record_latest_release(&latest) {
        log::warn!("{err:#}");
    }
    latest
}

/// Persist the newest release seen; the file's mtime is the time of the
/// check. Written beside it and renamed over it, so a reader -- Settings, or
/// the other of the two threads that check -- never sees half a file.
fn record_latest_release(latest: &Release) -> anyhow::Result<()> {
    use anyhow::Context;
    let update_file_name = config::DATA_DIR.join("check_update");
    let dir = config::DATA_DIR.as_path();
    let saved = config::create_user_owned_dirs(dir)
        .and_then(|()| Ok(tempfile::NamedTempFile::new_in(dir)?))
        .and_then(|mut file| {
            serde_json::to_writer_pretty(&mut file, latest)?;
            file.persist(&update_file_name)?;
            Ok(())
        });
    saved.with_context(|| format!("saving the update check to {}", update_file_name.display()))
}

/// Returns true if the provided socket path is dead.
fn update_checker() {
    // A check is due an interval after the last one, a few seconds in at the
    // earliest so it does not compete with starting up. The last one is the
    // record on disk -- whichever process made it, a "Check for Updates"
    // included -- so a weekly check stays weekly across launches; or this
    // process's last attempt, so a failing one is not retried every minute.
    // The switch and the interval are read again every minute, so a change
    // in Settings applies without first waiting out the interval it replaced.
    let initial_interval = Duration::from_secs(10);
    let poll = Duration::from_secs(60);

    let force_ui = always_show_update_ui();

    std::thread::sleep(initial_interval);

    let my_sock = config::RUNTIME_DIR.join(format!("gui-sock-{}", unsafe { libc::getpid() }));

    // Wall-clock time, so a night the machine slept through counts.
    let mut last_attempt: Option<SystemTime> = None;
    loop {
        let settings = crate::native_settings::load_shared();
        let interval =
            Duration::from_secs(crate::native_settings::update_check_interval_seconds(&settings));
        let last_check = last_checked().max(last_attempt);
        let due = crate::native_settings::checks_for_updates(&settings)
            && last_check.map_or(true, |at| {
                at.elapsed().map_or(true, |elapsed| elapsed >= interval)
            });
        if !due {
            std::thread::sleep(poll);
            continue;
        }
        last_attempt = Some(SystemTime::now());

        // Figure out which other wezterm-guis are running.
        // We have a little "consensus protocol" to decide which
        // of us will show the toast notification or show the update
        // window: the one of us that sorts first in the list will
        // own doing that, so that if there are a dozen gui processes
        // running, we don't spam the user with a lot of notifications.
        let socks = wezterm_client::discovery::discover_gui_socks();

        if let Ok(mut latest) = get_latest_release_info() {
            let current = running_release_version();
            // A package made by hand with this release's version is told
            // about the release once its code is the later one.
            let verdict = judge_release(&mut latest, &current, cached_release().as_ref());
            let newer = verdict.newer;
            set_update_available(newer || force_ui, verdict.new_build, &latest);
            if newer || force_ui {
                log::info!(
                    "latest release {} is newer than current build {}",
                    latest.tag_name,
                    current
                );

                let url = release_tag_url(&latest.tag_name);

                if force_ui || socks.is_empty() || socks[0] == my_sock {
                    persistent_toast_notification_with_click_to_open_url(
                        &crate::i18n::tr(if verdict.new_build {
                            "update-toast-new-build-title"
                        } else {
                            "update-toast-title"
                        }),
                        &crate::i18n::tr("update-toast-body"),
                        &url,
                    );
                }
            }

            if let Err(err) = record_latest_release(&latest) {
                log::warn!("{err:#}");
            }
        }

        std::thread::sleep(poll);
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

/// The updater's GETs through reqwest rather than http_req (see
/// `thinkterm_update::set_transport`). It goes through the proxy the system
/// or the environment names, as a browser would; and dropping a reqwest body
/// part-way just closes the connection, so a source that crawls can give
/// way to the next one, not only one that stalls.
pub fn reqwest_transport(
    url: &str,
    headers: &[(&str, String)],
    give_way: Option<thinkterm_update::GiveWay>,
    body: &mut dyn std::io::Write,
) -> anyhow::Result<thinkterm_update::Answer> {
    fetch(
        reqwest::blocking::Client::builder(),
        url,
        headers,
        give_way,
        body,
    )
}

/// `reqwest_transport` with a client from `builder`.
fn fetch(
    builder: reqwest::blocking::ClientBuilder,
    url: &str,
    headers: &[(&str, String)],
    give_way: Option<thinkterm_update::GiveWay>,
    body: &mut dyn std::io::Write,
) -> anyhow::Result<thinkterm_update::Answer> {
    use std::io::Read;
    use std::time::Instant;

    // http_req's minute to connect and for each read, unless the source is
    // to give way sooner. reqwest's blocking timeout covers the answer and
    // each read, not the whole body.
    let (connect, stall) = give_way.map_or(
        (Duration::from_secs(60), Duration::from_secs(60)),
        |give_way| (give_way.connect, give_way.stall),
    );
    let client = builder
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(connect)
        .timeout(stall)
        .build()?;
    // The bytes as sent: they are hashed against GitHub's digest.
    let mut request = client
        .get(url)
        .header(reqwest::header::ACCEPT_ENCODING, "identity");
    for (name, value) in headers {
        request = request.header(*name, value.as_str());
    }
    let mut response = request.send()?;
    let status = response.status();
    let answer = thinkterm_update::Answer {
        status: status.as_u16(),
        reason: status.canonical_reason().unwrap_or_default().to_string(),
        location: response
            .headers()
            .get(reqwest::header::LOCATION)
            .and_then(|location| location.to_str().ok())
            .map(str::to_string),
    };
    if !status.is_success() {
        return Ok(answer);
    }

    let mut buf = vec![0u8; 64 * 1024];
    // When the current window began, and what it has brought so far.
    let mut window = (Instant::now(), 0u64);
    loop {
        let read = response.read(&mut buf)?;
        if read == 0 {
            return Ok(answer);
        }
        body.write_all(&buf[..read])?;
        let Some(give_way) = give_way else {
            continue;
        };
        window.1 += read as u64;
        let elapsed = window.0.elapsed();
        if elapsed >= give_way.window {
            if (window.1 as f64) < give_way.min_rate as f64 * elapsed.as_secs_f64() {
                anyhow::bail!(
                    "under {} KB/s for {} s",
                    give_way.min_rate / 1024,
                    give_way.window.as_secs()
                );
            }
            window = (Instant::now(), 0);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};

    /// The transport, but straight to the test server whatever proxy this
    /// machine names.
    fn reqwest_transport(
        url: &str,
        headers: &[(&str, String)],
        give_way: Option<thinkterm_update::GiveWay>,
        body: &mut dyn std::io::Write,
    ) -> anyhow::Result<thinkterm_update::Answer> {
        fetch(
            reqwest::blocking::Client::builder().no_proxy(),
            url,
            headers,
            give_way,
            body,
        )
    }

    /// A server on localhost answering one GET with `len` bytes, `chunk` of
    /// them every `pause`; the URL to ask it at.
    fn serve(len: usize, chunk: usize, pause: Duration) -> String {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = [0u8; 4096];
            let _ = stream.read(&mut request);
            let head = format!("HTTP/1.1 200 OK\r\nContent-Length: {len}\r\nConnection: close\r\n\r\n");
            if stream.write_all(head.as_bytes()).is_err() {
                return;
            }
            let mut sent = 0;
            while sent < len {
                let n = chunk.min(len - sent);
                // The client hanging up is how a test that gives way ends.
                if stream.write_all(&vec![b'x'; n]).is_err() {
                    return;
                }
                sent += n;
                std::thread::sleep(pause);
            }
        });
        format!("http://{addr}/asset")
    }

    const QUICK: thinkterm_update::GiveWay = thinkterm_update::GiveWay {
        connect: Duration::from_secs(5),
        stall: Duration::from_secs(5),
        min_rate: 64 * 1024,
        window: Duration::from_millis(300),
    };

    #[test]
    fn reqwest_transport_reads_a_whole_body() {
        let url = serve(200_000, 50_000, Duration::from_millis(1));
        let mut body = Vec::new();
        let answer = reqwest_transport(&url, &[], Some(QUICK), &mut body).unwrap();
        assert_eq!(answer.status, 200);
        assert_eq!(body.len(), 200_000);
    }

    #[test]
    fn reqwest_transport_gives_way_when_a_body_crawls() {
        // 1 KB every 50 ms is about 20 KB/s, under the 64 asked for.
        let url = serve(1_000_000, 1024, Duration::from_millis(50));
        let err = reqwest_transport(&url, &[], Some(QUICK), &mut Vec::new()).unwrap_err();
        let err = format!("{err:#}");
        assert!(err.contains("KB/s"), "{}", err);
    }

    #[test]
    fn reqwest_transport_waits_out_a_slow_last_source() {
        // With no source after it, the same crawl is read to the end.
        let url = serve(20_000, 1024, Duration::from_millis(20));
        let mut body = Vec::new();
        reqwest_transport(&url, &[], None, &mut body).unwrap();
        assert_eq!(body.len(), 20_000);
    }
}
