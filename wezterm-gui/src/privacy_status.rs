//! The macOS privacy permissions the programs in local terminals get, for
//! Settings › General. macOS holds whoever spawned a terminal's shell
//! responsible for it: this app when local terminals run in it, else the
//! session server, which is responsible for itself and answers for itself
//! (`wezterm_mux_server_impl::privacy`).

use anyhow::Context;
use smol::process::Command;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::{mpsc, Mutex};
use std::time::{Duration, Instant};
use wezterm_mux_server_impl::privacy::{self, PrivacyReport};

/// Whether local terminals answer to this process's identity: they run in
/// it, or in a server that kept the launcher's (a development build opting
/// out of taking its own).
fn terminals_share_this_identity() -> bool {
    crate::local_sessions::host_domain_name().is_none()
        || std::env::var_os("THINKTERM_NO_PRIVACY_DISCLAIM").is_some_and(|value| !value.is_empty())
}

/// Whether the permissions are the session server's, not this app's.
pub(crate) fn held_by_server() -> bool {
    !terminals_share_this_identity()
}

fn server_program() -> anyhow::Result<PathBuf> {
    Ok(std::env::current_exe()?.with_file_name("thinkterm-mux-server"))
}

/// What System Settings lists the permissions under, for the person to add
/// to a list by hand: this app's bundle, or the server binary inside it.
pub(crate) fn responsible_program() -> anyhow::Result<PathBuf> {
    if !terminals_share_this_identity() {
        return server_program();
    }
    let exe = std::env::current_exe()?;
    // `ThinkTerm.app/Contents/MacOS/thinkterm-gui` -> `ThinkTerm.app`.
    Ok(exe
        .ancestors()
        .find(|path| path.extension().is_some_and(|ext| ext == "app"))
        .map(PathBuf::from)
        .unwrap_or(exe))
}

const QUERY_TIMEOUT: Duration = Duration::from_secs(3);

struct PrivacyQuery {
    pending: Option<mpsc::Receiver<anyhow::Result<PrivacyReport>>>,
}

impl PrivacyQuery {
    fn query_with(
        &mut self,
        timeout: Duration,
        probe: impl FnOnce() -> anyhow::Result<PrivacyReport> + Send + 'static,
    ) -> anyhow::Result<PrivacyReport> {
        if self.pending.is_none() {
            let (tx, rx) = mpsc::channel();
            std::thread::Builder::new()
                .name("thinkterm-privacy-probe".into())
                .spawn(move || {
                    let _ = tx.send(probe());
                })
                .context("starting the privacy probe")?;
            self.pending = Some(rx);
        }
        match self.pending.as_ref().unwrap().recv_timeout(timeout) {
            Ok(report) => {
                self.pending = None;
                report
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {
                // macOS assessment can block spawn itself. Keep this one
                // worker until it answers; refocusing must not spawn more.
                anyhow::bail!("privacy check timed out after {timeout:?}")
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                self.pending = None;
                anyhow::bail!("privacy probe ended without a report")
            }
        }
    }
}

static QUERY: Mutex<PrivacyQuery> = Mutex::new(PrivacyQuery { pending: None });

/// Ask off the main thread. The deadline covers macOS assessment during
/// launch as well as the probe, and a stuck launch keeps only one worker.
pub(crate) fn query() -> anyhow::Result<PrivacyReport> {
    QUERY
        .try_lock()
        .map_err(|_| anyhow::anyhow!("privacy check is already running"))?
        .query_with(QUERY_TIMEOUT, query_report)
}

fn query_report() -> anyhow::Result<PrivacyReport> {
    if terminals_share_this_identity() {
        return Ok(privacy::probe());
    }
    let server = server_program()?;
    let mut command = Command::new(&server);
    command.arg(privacy::REPORT_OPTION);
    query_command(command, QUERY_TIMEOUT).with_context(|| format!("running {}", server.display()))
}

fn query_command(mut command: Command, timeout: Duration) -> anyhow::Result<PrivacyReport> {
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    let deadline = Instant::now() + timeout;
    let output = smol::block_on(smol::future::or(
        async {
            command
                .output()
                .await
                .context("reading the privacy probe output")
        },
        async {
            smol::Timer::at(deadline).await;
            anyhow::bail!("privacy probe timed out after {timeout:?}")
        },
    ))?;
    anyhow::ensure!(
        output.status.success(),
        "privacy probe failed: {}",
        String::from_utf8_lossy(&output.stderr).trim()
    );
    serde_json::from_slice(output.stdout.trim_ascii())
        .context("reading the session server's privacy report")
}

/// One permission's page in System Settings › Privacy & Security.
pub(crate) fn settings_url(pane: &str) -> String {
    format!("x-apple.systempreferences:com.apple.preference.security?{pane}")
}

pub(crate) const FULL_DISK_ACCESS_PANE: &str = "Privacy_AllFiles";
pub(crate) const SCREEN_RECORDING_PANE: &str = "Privacy_ScreenCapture";
pub(crate) const MICROPHONE_PANE: &str = "Privacy_Microphone";
pub(crate) const CAMERA_PANE: &str = "Privacy_Camera";

#[cfg(test)]
mod tests {
    use super::*;
    use wezterm_mux_server_impl::privacy::PrivacyStatus;

    fn report() -> PrivacyReport {
        PrivacyReport {
            full_disk_access: PrivacyStatus::Granted,
            screen_recording: PrivacyStatus::NotGranted,
            microphone: PrivacyStatus::Unknown,
            camera: PrivacyStatus::Unknown,
        }
    }

    #[test]
    fn a_stalled_launch_times_out_without_starting_more_probes() {
        let mut query = PrivacyQuery { pending: None };
        let (release, wait) = mpsc::channel();
        let started = Instant::now();
        assert!(query
            .query_with(Duration::from_millis(20), move || {
                wait.recv().unwrap();
                Ok(report())
            })
            .is_err());
        assert!(query
            .query_with(Duration::from_millis(20), || {
                panic!("a second probe must not start while the first is pending")
            })
            .is_err());
        assert!(started.elapsed() < Duration::from_secs(1));
        release.send(()).unwrap();
        assert_eq!(
            query
                .query_with(Duration::from_secs(1), || unreachable!())
                .unwrap(),
            report()
        );
        assert!(query.pending.is_none());
    }

    #[test]
    fn the_probe_reads_a_report_and_a_failed_probe_can_be_retried() {
        let mut command = Command::new("/bin/sh");
        command.args([
            "-c",
            "printf '%s' '{\"full_disk_access\":\"granted\",\"screen_recording\":\"not_granted\"}'",
        ]);
        assert_eq!(
            query_command(command, Duration::from_secs(1)).unwrap(),
            report()
        );
        let mut failed = Command::new("/bin/sh");
        failed.args(["-c", "exit 1"]);
        assert!(query_command(failed, Duration::from_secs(1)).is_err());

        let mut query = PrivacyQuery { pending: None };
        assert!(query
            .query_with(Duration::from_secs(1), || { anyhow::bail!("probe failed") })
            .is_err());
        assert!(query.pending.is_none());
        assert_eq!(
            query
                .query_with(Duration::from_secs(1), || Ok(report()))
                .unwrap(),
            report()
        );
    }

    #[test]
    fn a_child_that_never_exits_is_cancelled_at_the_deadline() {
        let mut command = Command::new("/bin/sh");
        command.args(["-c", "exec sleep 30"]);
        let started = Instant::now();
        let error = query_command(command, Duration::from_millis(50)).unwrap_err();
        assert!(error.to_string().contains("timed out"));
        assert!(started.elapsed() < Duration::from_secs(2));
    }
}
