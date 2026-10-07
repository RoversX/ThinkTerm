//! Fixing a version mismatch with a remote mux server from this side of the
//! ssh connection.
//!
//! The client and the server refuse to talk across a codec change, and the
//! usual way that happens is the desktop being upgraded while the server on
//! some host keeps running the build from last month. Asking the person to
//! log in there and update by hand is the wrong answer when the client
//! already holds an ssh session to the host: it can run the installer over
//! that session, at the client's own version, so the two match again.
//!
//! What this deliberately does not do:
//!
//! - guess. It asks before installing anything and again before restarting
//!   the server, because a restart ends every session the server holds.
//! - reach GitHub on its own. It asks before looking the release up.
//! - pick "the latest release". The server has to match *this* client, which
//!   may not be the latest; a development build has no release to match at
//!   all and is told so.
//! - end sessions to get the new version running, when it can help it. A
//!   server that can hand over (`thinkterm-mux-server --takeover`) is asked
//!   to; only when it cannot, or the person turned that off, does the old
//!   question come: stop the server, ending every session, or leave it.

use crate::client::IncompatibleVersionError;
use anyhow::Context;
use config::SshDomain;
use mux::connui::ConnectionUI;
use portable_pty::Child as _;
use std::io::Read;
use std::sync::atomic::{AtomicBool, Ordering};
use wezterm_ssh::Session;

static KEEP_SESSIONS: AtomicBool = AtomicBool::new(true);

/// Whether a remote update hands the running server's sessions to the new
/// version (the default) or asks to stop the server. The GUI sets it from
/// its settings; a headless client keeps the default.
pub fn set_keep_sessions_on_update(keep: bool) {
    KEEP_SESSIONS.store(keep, Ordering::SeqCst);
}

pub fn keep_sessions_on_update() -> bool {
    KEEP_SESSIONS.load(Ordering::SeqCst)
}

#[derive(Debug, PartialEq, Eq)]
pub enum RemoteUpdateOutcome {
    /// Nothing was changed on the host: the person said no, the client is
    /// not a release build, or its release is not on GitHub.
    Declined,
    /// The new version is installed. `restarted` says whether the old mux
    /// server was stopped, so that the next connect starts the new one.
    Updated { restarted: bool },
}

/// Offer to bring the mux server on `ssh_dom`'s host to this client's
/// version, after the handshake reported `err`. Interactive: every question
/// goes through `ui`, and a UI that cannot ask (a headless attach) simply
/// declines.
pub fn offer_remote_update(
    ssh_dom: &SshDomain,
    ui: &ConnectionUI,
    err: &IncompatibleVersionError,
) -> anyhow::Result<RemoteUpdateOutcome> {
    let local = thinkterm_update::running_release_version();
    let host = ssh_dom.remote_address.clone();

    // A configured remote path is used verbatim for every connect, so an
    // install under ~/.local would never be the binary that answers.
    if let Some(path) = &ssh_dom.remote_wezterm_path {
        ui.output_str(&format!(
            "\nThis domain connects through the configured remote path {path}, so an \
             install under ~/.local on {host} would not be used. Update that binary \
             to {local} by hand.\n"
        ));
        return Ok(RemoteUpdateOutcome::Declined);
    }

    if !thinkterm_update::is_release_version(&local) {
        ui.output_str(&format!(
            "\nThis client is a development build ({local}), so there is no release \
             to install on {host} that matches it. Build and install a matching \
             thinkterm-mux-server there by hand.\n"
        ));
        return Ok(RemoteUpdateOutcome::Declined);
    }

    ui.output_str(&format!(
        "\nThe mux server on {host} is {} (codec {}); this client is {local} (codec {}).\n",
        err.version,
        err.codec_vers,
        codec::CODEC_VERSION
    ));
    let answer = ui.input(&format!(
        "Look up ThinkTerm {local} on GitHub to install on {host}? [y/N] "
    ));
    let answer = match answer {
        Ok(answer) => answer,
        Err(err) => {
            log::info!("remote update not offered: {err:#}");
            return Ok(RemoteUpdateOutcome::Declined);
        }
    };
    if !is_yes(&answer) {
        ui.output_str(&format!(
            "Not looking it up. To update {host} yourself, install ThinkTerm {local} \
             there, or build a matching thinkterm-mux-server from the same source.\n"
        ));
        return Ok(RemoteUpdateOutcome::Declined);
    }
    if let Err(lookup) = thinkterm_update::get_release_by_tag(&local) {
        if !is_not_published(&lookup) {
            ui.output_str(&format!("Could not look up {local} on GitHub: {lookup:#}\n"));
            return Ok(RemoteUpdateOutcome::Declined);
        }
        let latest = thinkterm_update::get_latest_release_info()
            .map(|latest| format!(" (the latest is {})", latest.tag_name))
            .unwrap_or_default();
        ui.output_str(&format!(
            "GitHub has no release {local} yet{latest}. Update {host} once it is \
             released, or build a matching thinkterm-mux-server there from the same \
             source now.\n"
        ));
        return Ok(RemoteUpdateOutcome::Declined);
    }

    // The ssh session comes first: the host has to be asked what is
    // installed there before the question can say what will change. A
    // workstation with the desktop variant keeps its GUI; --server would
    // have removed it.
    let ssh_config = mux::ssh::ssh_domain_to_ssh_config(ssh_dom)?;
    let mut ui_for_auth = ui.clone();
    let session = mux::ssh::ssh_connect_with_ui_and_password(
        ssh_config,
        &mut ui_for_auth,
        ssh_dom.stored_password.clone(),
    )
    .with_context(|| format!("opening an ssh session to {host} for the update"))?;
    // A host the release binaries cannot run on gets the answer straight
    // away, rather than an installer run that fails at its first check.
    if let Some(reason) = host_cannot_take_a_release(&session) {
        ui.output_str(&format!(
            "{reason}, so the release build of {local} cannot be installed on {host}.\n{}\n",
            thinkterm_update::build_from_source_instructions(&local)
        ));
        return Ok(RemoteUpdateOutcome::Declined);
    }

    let variant = remote_variant(&session);
    let command = thinkterm_update::install_command(variant, &local);
    // Uncommitted changes can include a newer protocol than the release
    // that carries this version label.
    let caveat = if config::wezterm_version().ends_with("-dirty") {
        format!(
            "This client was built with unreleased source changes (codec {}), so the \
             released {local} may still not match it.\n",
            codec::CODEC_VERSION
        )
    } else {
        String::new()
    };

    let answer = ui.input(&format!(
        "ThinkTerm can install {local} there now ({variant} variant, as the ssh user, \
         under ~/.local, with no root):\n  \
         {command}\n\
         {caveat}Install it? [y/N] ",
    ));
    let answer = match answer {
        Ok(answer) => answer,
        Err(err) => {
            log::info!("remote update not offered: {err:#}");
            return Ok(RemoteUpdateOutcome::Declined);
        }
    };
    if !is_yes(&answer) {
        ui.output_str(&format!(
            "Not installing. To do it yourself, run this on {host}:\n  {command}\n"
        ));
        return Ok(RemoteUpdateOutcome::Declined);
    }

    ui.output_str(&format!("\nInstalling ThinkTerm {local} on {host}...\n"));
    let status = run_and_relay(&session, ui, &command)?;
    if !status {
        ui.output_str(&format!(
            "\nThe installer on {host} failed; its output is above. Nothing was changed there.\n\
             If it refused the host (glibc, musl, architecture), a release binary cannot run on it.\n{}\n",
            thinkterm_update::build_from_source_instructions(&local)
        ));
        anyhow::bail!("the installer on {host} failed");
    }

    // The installed server can take the running one over, sessions and
    // all; the running server has to be new enough to hand over, and the
    // person may have turned this off.
    if keep_sessions_on_update() {
        ui.output_str(&format!(
            "\nHanding the sessions of the running mux server on {host} to {local}...\n"
        ));
        match run_and_relay(&session, ui, &thinkterm_update::takeover_command()) {
            Ok(true) => {
                ui.output_str("Handed over; reconnecting.\n");
                return Ok(RemoteUpdateOutcome::Updated { restarted: true });
            }
            Ok(false) => ui.output_str(&format!(
                "\nThe running server on {host} could not hand over (its output is above; \
                 a server from before the handoff feature cannot). The old way remains.\n"
            )),
            Err(err) => ui.output_str(&format!(
                "\nCould not run the takeover on {host}: {err:#}. The old way remains.\n"
            )),
        }
    }

    // The install is done whatever happens to this question: a UI that has
    // gone away by now leaves the old server running, which is the safe
    // answer, not a failure.
    let answer = ui
        .input(&format!(
            "\nInstalled. The old mux server on {host} is still running and still \
             answers with the old version.\n\
             Restart it now? Every session running in it will end. [y/N] ",
        ))
        .unwrap_or_default();
    if !is_yes(&answer) {
        ui.output_str(&format!(
            "Leaving it running. Stop it when you are ready (`{STOP_HINT}` on {host}) \
             and reconnect; the next connect starts the new version.\n"
        ));
        return Ok(RemoteUpdateOutcome::Updated { restarted: false });
    }

    let stopped = run_and_relay(&session, ui, STOP_COMMAND)?;
    if !stopped {
        ui.output_str(&format!(
            "No running mux server was found to stop on {host}; reconnecting anyway.\n"
        ));
    } else {
        ui.output_str("Old server stopped; reconnecting.\n");
    }
    Ok(RemoteUpdateOutcome::Updated { restarted: true })
}

/// What a person types to stop the server themselves.
const STOP_HINT: &str = "pkill -f thinkterm-mux-server";

/// Stop the invoking user's mux server. Matched on the full command line,
/// not the process name: Linux truncates the name to 15 characters, so
/// `-x thinkterm-mux-server` can never match there. The bracket keeps the
/// shell running this very command line from matching itself. Both binary
/// names, because a host set up before the rename may still run a
/// wezterm-mux-server under a thinkterm domain. Exit status is pkill's:
/// 0 when something was signalled, 1 when nothing matched.
const STOP_COMMAND: &str = "pkill -u \"$(id -u)\" -f '[t]hinkterm-mux-server' \
                            || pkill -u \"$(id -u)\" -f '[w]ezterm-mux-server'";

/// Why the host cannot run a release binary, if it cannot: the same two
/// checks `install.sh` makes first, made here so the answer comes before
/// the question. An unreadable answer is not a refusal; the script will
/// check again.
fn host_cannot_take_a_release(session: &Session) -> Option<String> {
    let probe = "uname -s; uname -m; getconf GNU_LIBC_VERSION 2>/dev/null || ldd --version 2>&1 | head -n1";
    let out = run_capture(session, probe).ok()?;
    let mut lines = out.lines().map(str::trim);
    let os = lines.next()?;
    let arch = lines.next()?;
    let libc = lines.next().unwrap_or("");
    if os == "Linux" && !matches!(arch, "x86_64" | "aarch64") {
        return Some(format!("{host_arch} has no release build", host_arch = arch));
    }
    if os == "Linux" && libc.to_ascii_lowercase().contains("musl") {
        return Some("The host uses musl libc".to_string());
    }
    if os == "Linux" {
        if let Some(version) = thinkterm_update::parse_glibc_version(libc) {
            if version < thinkterm_update::MIN_GLIBC {
                return Some(format!(
                    "The host has glibc {}.{} and the release binaries need {}.{}",
                    version.0,
                    version.1,
                    thinkterm_update::MIN_GLIBC.0,
                    thinkterm_update::MIN_GLIBC.1
                ));
            }
        }
    }
    None
}

/// Which variant the script installed on the host, read from its manifest.
/// Falls back to the server variant, which is the only one that makes sense
/// on a host with no install to preserve.
fn remote_variant(session: &Session) -> &'static str {
    let read = "sed -n 's/^variant=//p' \"$HOME/.local/share/thinkterm/install-manifest\" 2>/dev/null";
    match run_capture(session, read).as_deref().map(str::trim) {
        Ok("desktop") => "desktop",
        _ => "server",
    }
}

/// Run `command` and return its stdout, for the small questions asked of
/// the host before anything is changed on it.
fn run_capture(session: &Session, command: &str) -> anyhow::Result<String> {
    let exec = smol::block_on(session.exec(command, None))
        .with_context(|| format!("running `{command}` over ssh"))?;
    let mut stdout = exec.stdout;
    let mut child = exec.child;
    drop(exec.stdin);
    drop(exec.stderr);
    let mut out = Vec::new();
    stdout.read_to_end(&mut out)?;
    let _ = child.wait();
    Ok(String::from_utf8_lossy(&out).into_owned())
}

fn is_yes(answer: &str) -> bool {
    matches!(answer.trim(), "y" | "Y" | "yes" | "YES")
}

/// Whether a release lookup failed because GitHub has no such release, as
/// opposed to GitHub being unreachable. `thinkterm_update` reports HTTP
/// errors as "fetching <uri>: HTTP <status> <reason>".
fn is_not_published(err: &anyhow::Error) -> bool {
    format!("{err:#}").contains(": HTTP 404 ")
}

/// Run `command` on the session, copying its stdout and stderr into the UI
/// as they arrive, and return whether it exited successfully.
fn run_and_relay(session: &Session, ui: &ConnectionUI, command: &str) -> anyhow::Result<bool> {
    let exec = smol::block_on(session.exec(command, None))
        .with_context(|| format!("running `{command}` over ssh"))?;
    let mut stdout = exec.stdout;
    let mut stderr = exec.stderr;
    let mut child = exec.child;
    drop(exec.stdin);

    let ui_err = ui.clone();
    let stderr_thread = std::thread::spawn(move || {
        let mut buf = [0u8; 1024];
        while let Ok(len) = stderr.read(&mut buf) {
            if len == 0 {
                break;
            }
            ui_err.output_str(&String::from_utf8_lossy(&buf[..len]));
        }
    });
    let mut buf = [0u8; 1024];
    while let Ok(len) = stdout.read(&mut buf) {
        if len == 0 {
            break;
        }
        ui.output_str(&String::from_utf8_lossy(&buf[..len]));
    }
    let _ = stderr_thread.join();
    let status = child.wait().with_context(|| format!("waiting for `{command}`"))?;
    Ok(status.success())
}

#[cfg(test)]
mod tests {
    use super::{is_not_published, is_yes};

    #[test]
    fn only_an_explicit_yes_counts() {
        assert!(is_yes("y"));
        assert!(is_yes(" yes\n"));
        assert!(!is_yes(""));
        assert!(!is_yes("n"));
        assert!(!is_yes("maybe"));
    }

    #[test]
    fn only_a_404_means_the_release_is_not_out() {
        let uri = "https://api.github.com/repos/o/r/releases/tags/9.9.9";
        assert!(is_not_published(&anyhow::anyhow!(
            "fetching {}: HTTP 404 Not Found",
            uri
        )));
        assert!(!is_not_published(&anyhow::anyhow!(
            "fetching {}: HTTP 403 Forbidden",
            uri
        )));
        assert!(!is_not_published(&anyhow::anyhow!(
            "fetching {}: failed to lookup address information",
            uri
        )));
    }
}
