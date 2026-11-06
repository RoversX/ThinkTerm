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
//! - take a host backwards. A newer server stays, and this client is the one
//!   told to update.
//! - end sessions to get the new version running, when it can help it. A
//!   server that can hand over (`thinkterm-mux-server --takeover`) is asked
//!   to; only when it cannot, or the person turned that off, does the old
//!   question come: stop the server, ending every session, or leave it.

use crate::client::IncompatibleVersionError;
use anyhow::Context;
use config::SshDomain;
use mux::connui::ConnectionUI;
use portable_pty::Child as _;
use std::convert::TryFrom;
use std::io::{BufRead, BufReader, Read, Write};
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
    // Another client brought that server forward, and it would not hand its
    // sessions to this release anyway: all an install could do is leave an
    // older build beside it and a question about ending them.
    if thinkterm_update::is_older_release(&local, &err.version) {
        ui.output_str(&format!(
            "That server is newer than this client, so nothing changes on {host}. \
             Update ThinkTerm here to {} to connect.\n",
            err.version
        ));
        return Ok(RemoteUpdateOutcome::Declined);
    }
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
    ui.output_str(&format!("Looking up release information for {local}...\n"));
    let release = match thinkterm_update::remote::get_release(&local) {
        Ok(release) => release,
        Err(lookup) => {
            ui.output_str(&format!("Could not resolve release {local}: {lookup:#}\n"));
            return Ok(RemoteUpdateOutcome::Declined);
        }
    };

    // The ssh session comes first: the host has to be asked what is
    // installed there before the question can say what will change. A
    // workstation with the desktop variant keeps its GUI, whoever installed
    // it: --server would have removed the script's, and left a package's
    // launcher starting a CLI that has no GUI to start.
    let ssh_config = mux::ssh::ssh_domain_to_ssh_config(ssh_dom)?;
    let mut ui_for_auth = ui.clone();
    let session = mux::ssh::ssh_connect_with_ui_and_password(
        ssh_config,
        &mut ui_for_auth,
        ssh_dom.stored_password.clone(),
    )
    .with_context(|| format!("opening an ssh session to {host} for the update"))?;
    let manifest = run_capture(&session, VARIANT_READ).unwrap_or_default();
    let host_probe = run_capture(&session, HOST_PROBE).unwrap_or_default();
    let gui_probe = run_capture(&session, GUI_PROBE).unwrap_or_default();
    let (variant, package_gui) = remote_variant(&manifest, &host_probe, &gui_probe);
    // A host the release binaries cannot run on gets the answer straight
    // away, rather than an installer run that fails at its first check.
    if let Some(reason) = release_refusal(&host_probe, variant) {
        ui.output_str(&format!(
            "{reason}, so the release build of {local} cannot be installed on {host}.\n{}\n",
            thinkterm_update::build_from_source_instructions(&local)
        ));
        return Ok(RemoteUpdateOutcome::Declined);
    }
    let package_note = match package_gui {
        PackageGui::Absent => String::new(),
        PackageGui::Updated => format!(
            "{host} has the ThinkTerm desktop app, so this installs the app too, keeping the \
             app and the server on one version. Its launcher moves to the new install.\n"
        ),
        PackageGui::LeftBehind(reason) => format!(
            "{reason}, so the ThinkTerm desktop app on {host} cannot come along: only the server \
             is updated, the app stays at its own version, and its launcher may stop working.\n"
        ),
    };

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
         under ~/.local, with no root). A verified online installer (or bundled fallback) \
         and trusted release information will be sent over SSH; the host downloads the archive.\n\
         {package_note}{caveat}Install it? [y/N] ",
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
    let status = run_remote_installer(&session, ui, variant, &release)?;
    if !status {
        ui.output_str(&format!(
            "\nThe installer on {host} failed; its output is above.\n\
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
        // Tried again as the local takeover is. Whether someone else took
        // the server over meanwhile is not looked into: a host is updated
        // from one place at a time.
        let started = std::time::Instant::now();
        let mut attempts = 0;
        let outcome = loop {
            attempts += 1;
            match run_and_relay_status(&session, ui, &thinkterm_update::takeover_command()) {
                Ok(status) if status.success() => break Ok(true),
                Ok(status)
                    if crate::local_update::worth_retrying(
                        i32::try_from(status.exit_code()).ok(),
                        attempts,
                        started.elapsed(),
                    ) =>
                {
                    std::thread::sleep(crate::local_update::TAKEOVER_RETRY_DELAY);
                    ui.output_str(&format!(
                        "\nThe running server on {host} could not hand over; trying again ({} of \
                         {})...\n",
                        attempts + 1,
                        crate::local_update::TAKEOVER_ATTEMPTS
                    ));
                }
                Ok(_) => break Ok(false),
                Err(err) => break Err(err),
            }
        };
        match outcome {
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

/// What became of the offer to install ThinkTerm on a host that had none.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RemoteInstallOutcome {
    /// The person said no.
    Declined,
    /// Nothing could be installed: there is no way to ask here, the client
    /// is not a release build, its release is not on GitHub, or the host
    /// cannot run it. The UI already says which.
    Unavailable,
    Installed,
}

/// Offer to install this client's ThinkTerm on `ssh_dom`'s host, which has
/// none, so that ThinkTerm Connect can run there. Asked once, before
/// anything is looked up or installed.
pub fn offer_remote_install(
    ssh_dom: &SshDomain,
    ui: &ConnectionUI,
) -> anyhow::Result<RemoteInstallOutcome> {
    let local = thinkterm_update::running_release_version();
    let host = ssh_dom.remote_address.clone();
    let variant = "server";
    let command = thinkterm_update::install_command(variant, &local);

    ui.output_str(&format!(
        "\nThinkTerm is not installed on {host}. ThinkTerm Connect runs there, so \
         that its terminals keep running when you disconnect.\n"
    ));
    if !thinkterm_update::is_release_version(&local) {
        ui.output_str(&format!(
            "This client is a development build ({local}), so there is no release to \
             install on {host} that matches it. Build and install a matching \
             thinkterm-mux-server there by hand, or switch the host to SSH.\n"
        ));
        return Ok(RemoteInstallOutcome::Unavailable);
    }
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
        "ThinkTerm can install {local} there now (as the ssh user, under ~/.local, \
         with no root). A verified online installer (or bundled fallback) and trusted release \
         information will be sent over SSH; the host downloads the archive.\n\
         {caveat}Install it? [y/N] "
    ));
    let answer = match answer {
        Ok(answer) => answer,
        Err(err) => {
            log::info!("remote install not offered: {err:#}");
            ui.output_str(&format!(
                "To install it yourself, run this on {host}:\n  {command}\n"
            ));
            return Ok(RemoteInstallOutcome::Unavailable);
        }
    };
    if !is_yes(&answer) {
        ui.output_str(&format!(
            "Not installing. To do it yourself, run this on {host}:\n  {command}\n"
        ));
        return Ok(RemoteInstallOutcome::Declined);
    }
    ui.output_str(&format!("Looking up release information for {local}...\n"));
    let release = match thinkterm_update::remote::get_release(&local) {
        Ok(release) => release,
        Err(lookup) => {
            ui.output_str(&format!("Could not resolve release {local}: {lookup:#}\n"));
            return Ok(RemoteInstallOutcome::Unavailable);
        }
    };

    let ssh_config = mux::ssh::ssh_domain_to_ssh_config(ssh_dom)?;
    let mut ui_for_auth = ui.clone();
    let session = mux::ssh::ssh_connect_with_ui_and_password(
        ssh_config,
        &mut ui_for_auth,
        ssh_dom.stored_password.clone(),
    )
    .with_context(|| format!("opening an ssh session to {host} for the install"))?;
    if let Some(reason) = host_cannot_take_a_release(&session, variant) {
        ui.output_str(&format!(
            "{reason}, so the release build of {local} cannot be installed on {host}.\n{}\n",
            thinkterm_update::build_from_source_instructions(&local)
        ));
        return Ok(RemoteInstallOutcome::Unavailable);
    }

    ui.output_str(&format!("\nInstalling ThinkTerm {local} on {host}...\n"));
    if !run_remote_installer(&session, ui, variant, &release)? {
        ui.output_str(&format!(
            "\nThe installer on {host} failed; its output is above.\n{}\n",
            thinkterm_update::build_from_source_instructions(&local)
        ));
        anyhow::bail!("the installer on {host} failed");
    }
    ui.output_str("Installed; connecting.\n");
    Ok(RemoteInstallOutcome::Installed)
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

const HOST_PROBE: &str =
    "uname -s; uname -m; getconf GNU_LIBC_VERSION 2>/dev/null || ldd --version 2>&1 | head -n1";

const VARIANT_READ: &str =
    "sed -n 's/^variant=//p' \"$HOME/.local/share/thinkterm/install-manifest\" 2>/dev/null";

/// Prints `gui` where a desktop app is installed that may not be the
/// script's: a package writes no manifest for `VARIANT_READ` to find.
const GUI_PROBE: &str = "if command -v thinkterm-gui >/dev/null 2>&1 \
     || [ -f /usr/share/applications/com.roversx.thinkterm.desktop ]; \
     then echo gui; fi";

/// Why the host cannot run a release binary of `variant`, if it cannot: the
/// same two checks `install.sh` makes first, made here so the answer comes
/// before the question. An unreadable answer is not a refusal; the script
/// will check again.
fn host_cannot_take_a_release(session: &Session, variant: &str) -> Option<String> {
    let out = run_capture(session, HOST_PROBE).ok()?;
    release_refusal(&out, variant)
}

/// `host_cannot_take_a_release`, given what `HOST_PROBE` printed there.
fn release_refusal(probe: &str, variant: &str) -> Option<String> {
    let mut lines = probe.lines().map(str::trim);
    let os = lines.next()?;
    let arch = lines.next()?;
    let libc = lines.next().unwrap_or("");
    if os == "Linux" && !matches!(arch, "x86_64" | "aarch64") {
        return Some(format!(
            "{host_arch} has no release build",
            host_arch = arch
        ));
    }
    if os == "Linux" && libc.to_ascii_lowercase().contains("musl") {
        return Some("The host uses musl libc".to_string());
    }
    if os == "Linux" {
        if let Some(version) = thinkterm_update::parse_glibc_version(libc) {
            let need = thinkterm_update::min_glibc(variant);
            if version < need {
                return Some(format!(
                    "The host has glibc {}.{} and the {variant} release binaries need {}.{}",
                    version.0, version.1, need.0, need.1
                ));
            }
        }
    }
    None
}

/// What an update does about a desktop app the script did not install.
#[derive(Debug, PartialEq, Eq)]
enum PackageGui {
    Absent,
    /// The desktop variant goes in beside it, and its launcher takes over.
    Updated,
    /// The desktop variant cannot run on the host, for this reason.
    LeftBehind(String),
}

/// The variant to install, from what `VARIANT_READ`, `HOST_PROBE` and
/// `GUI_PROBE` printed on the host. The script's desktop variant stays what
/// it is. So does a distro package's app, on Linux, and a server install
/// that has already gone in beside one moves to the desktop variant with it:
/// the package's launcher may go through PATH, where a server under ~/.local
/// takes the name from it, and the package cannot update itself to keep up
/// with that server. A macOS bundle has neither problem, so it is not
/// replaced from here. The server variant is what is left.
fn remote_variant(manifest: &str, host_probe: &str, gui_probe: &str) -> (&'static str, PackageGui) {
    if manifest.trim() == "desktop" {
        return ("desktop", PackageGui::Absent);
    }
    let linux = host_probe.lines().next().map(str::trim) == Some("Linux");
    if !linux || gui_probe.trim() != "gui" {
        return ("server", PackageGui::Absent);
    }
    match release_refusal(host_probe, "desktop") {
        None => ("desktop", PackageGui::Updated),
        Some(reason) => ("server", PackageGui::LeftBehind(reason)),
    }
}

/// Run `command` and return its stdout, for the small questions asked of
/// the host before anything is changed on it.
fn run_capture(session: &Session, command: &str) -> anyhow::Result<String> {
    let exec = smol::block_on(session.exec(&without_input(command), None))
        .with_context(|| format!("running `{command}` over ssh"))?;
    let stdin = exec.stdin;
    let mut stdout = exec.stdout;
    let mut stderr = exec.stderr;
    let mut child = exec.child;
    let drain = std::thread::spawn(move || std::io::copy(&mut stderr, &mut std::io::sink()));
    let mut out = Vec::new();
    stdout.read_to_end(&mut out)?;
    let _ = drain.join();
    let _ = child.wait();
    drop(stdin);
    Ok(String::from_utf8_lossy(&out).into_owned())
}

/// Our end of a command's stdin stays open until it has finished: closing
/// it closes the whole channel, which loses the output and, under libssh,
/// hangs while the host still has output to send. So the command reads its
/// input from /dev/null instead.
fn without_input(command: &str) -> String {
    format!("{{ {command}; }} </dev/null")
}

fn is_yes(answer: &str) -> bool {
    matches!(answer.trim(), "y" | "Y" | "yes" | "YES")
}

fn run_remote_installer(
    session: &Session,
    ui: &ConnectionUI,
    variant: &str,
    release: &thinkterm_update::Release,
) -> anyhow::Result<bool> {
    ui.output_str("Checking for a signed installer (up to 4 seconds)...\n");
    let installer = thinkterm_update::remote::installer_script();
    ui.output_str(if installer.online {
        "Verified the online installer; sending it and release information over SSH...\n"
    } else {
        "No verified online installer available; sending the bundled installer over SSH...\n"
    });
    let (command, payload) =
        thinkterm_update::remote::install_payload(variant, release, &installer.text)?;
    let exec =
        smol::block_on(session.exec(&command, None)).context("starting the remote installer")?;
    let mut stdin = exec.stdin;
    let mut child = exec.child;
    let stdout = exec.stdout;
    let stderr = exec.stderr;
    // The receiver reads exact byte counts, not EOF. Keep stdin alive until
    // both output streams have drained and the remote command has exited.
    let writer = std::thread::spawn(move || {
        let result = stdin.write_all(&payload).and_then(|_| stdin.flush());
        (stdin, result)
    });
    let ui_err = ui.clone();
    let stderr_thread = std::thread::spawn(move || relay_lines(stderr, &ui_err));
    let stdout_result = relay_lines(stdout, ui);
    let stderr_result = stderr_thread.join();
    let status = child.wait();
    let (stdin, sent) = writer
        .join()
        .map_err(|_| anyhow::anyhow!("installer upload thread failed"))?;
    drop(stdin);
    sent.context("sending the installer over SSH")?;
    stdout_result?;
    stderr_result.map_err(|_| anyhow::anyhow!("installer output thread failed"))??;
    Ok(status
        .context("waiting for the remote installer")?
        .success())
}

fn relay_lines(reader: impl Read, ui: &ConnectionUI) -> std::io::Result<()> {
    // Buffer UTF-8 across SSH reads so the wordmark cannot split a character.
    // The installer emits bounded, newline-terminated progress in this mode.
    let mut reader = BufReader::new(reader);
    let mut line = Vec::new();
    loop {
        line.clear();
        let len = reader
            .by_ref()
            .take(16 * 1024)
            .read_until(b'\n', &mut line)?;
        if len == 0 {
            return Ok(());
        }
        ui.output_str(&String::from_utf8_lossy(&line));
    }
}

/// Run `command` on the session, copying its stdout and stderr into the UI
/// as they arrive, and return whether it exited successfully.
fn run_and_relay(session: &Session, ui: &ConnectionUI, command: &str) -> anyhow::Result<bool> {
    Ok(run_and_relay_status(session, ui, command)?.success())
}

/// `run_and_relay`, with how the command exited.
fn run_and_relay_status(
    session: &Session,
    ui: &ConnectionUI,
    command: &str,
) -> anyhow::Result<portable_pty::ExitStatus> {
    let exec = smol::block_on(session.exec(&without_input(command), None))
        .with_context(|| format!("running `{command}` over ssh"))?;
    // Held until the command has finished; see `without_input`.
    let stdin = exec.stdin;
    let mut stdout = exec.stdout;
    let mut stderr = exec.stderr;
    let mut child = exec.child;

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
    let status = child
        .wait()
        .with_context(|| format!("waiting for `{command}`"));
    drop(stdin);
    status
}

#[cfg(test)]
mod tests {
    use super::{is_yes, release_refusal, remote_variant, without_input, PackageGui};

    #[test]
    fn only_an_explicit_yes_counts() {
        assert!(is_yes("y"));
        assert!(is_yes(" yes\n"));
        assert!(!is_yes(""));
        assert!(!is_yes("n"));
        assert!(!is_yes("maybe"));
    }

    #[test]
    fn a_host_is_refused_by_the_glibc_of_the_variant_it_would_get() {
        // RHEL 9 and its kin: the server runs there, the GUI does not.
        let rhel9 = "Linux\naarch64\nglibc 2.34\n";
        assert_eq!(release_refusal(rhel9, "server"), None);
        let refused = release_refusal(rhel9, "desktop").expect("the GUI needs 2.35");
        assert!(refused.contains("glibc 2.34"), "{refused}");
        // Ubuntu 20.04, Debian 11: the server built for older glibc, no GUI.
        let focal = "Linux\nx86_64\nglibc 2.31\n";
        assert_eq!(release_refusal(focal, "server"), None);
        assert!(release_refusal(focal, "desktop").is_some());
        assert!(release_refusal("Linux\nx86_64\nglibc 2.28\n", "server").is_some());
        assert_eq!(
            release_refusal("Linux\nx86_64\nglibc 2.39\n", "desktop"),
            None
        );
        assert!(release_refusal("Linux\nx86_64\nmusl libc (x86_64)\n", "server").is_some());
        assert_eq!(release_refusal("Darwin\narm64\n\n", "desktop"), None);
    }

    #[test]
    fn a_host_keeps_the_desktop_app_it_has() {
        let ubuntu = "Linux\nx86_64\nglibc 2.35\n";
        // Nothing to preserve.
        assert_eq!(
            remote_variant("", ubuntu, ""),
            ("server", PackageGui::Absent)
        );
        assert_eq!(
            remote_variant("server\n", ubuntu, ""),
            ("server", PackageGui::Absent)
        );
        // The script's own.
        assert_eq!(
            remote_variant("desktop\n", ubuntu, "gui\n"),
            ("desktop", PackageGui::Absent)
        );
        // A package's, which writes no manifest, and one that a server
        // install has already gone in beside.
        assert_eq!(
            remote_variant("", ubuntu, "gui\n"),
            ("desktop", PackageGui::Updated)
        );
        assert_eq!(
            remote_variant("server\n", ubuntu, "gui\n"),
            ("desktop", PackageGui::Updated)
        );
        // Unreadable answers change nothing.
        assert_eq!(
            remote_variant("", "", "gui\n"),
            ("server", PackageGui::Absent)
        );
    }

    #[test]
    fn a_package_gui_the_release_cannot_replace_is_named_and_left_behind() {
        let (variant, gui) = remote_variant("", "Linux\nx86_64\nglibc 2.31\n", "gui\n");
        assert_eq!(variant, "server");
        match gui {
            PackageGui::LeftBehind(reason) => assert!(reason.contains("glibc 2.31"), "{}", reason),
            other => panic!("{:?}", other),
        }
    }

    #[test]
    fn a_mac_app_is_not_replaced_from_here() {
        assert_eq!(
            remote_variant("", "Darwin\narm64\n\n", "gui\n"),
            ("server", PackageGui::Absent)
        );
        assert_eq!(
            remote_variant("desktop", "Darwin\narm64\n\n", "gui\n"),
            ("desktop", PackageGui::Absent)
        );
    }

    #[cfg(unix)]
    #[test]
    fn the_gui_probe_answers_from_the_path() {
        use std::process::Command;
        let dir = std::env::temp_dir().join(format!("thinkterm-gui-probe-{}", std::process::id()));
        let empty = dir.join("empty");
        let with_gui = dir.join("with-gui");
        std::fs::create_dir_all(&empty).unwrap();
        std::fs::create_dir_all(&with_gui).unwrap();
        let gui = with_gui.join("thinkterm-gui");
        std::fs::write(&gui, "#!/bin/sh\n").unwrap();
        let mut mode = std::fs::metadata(&gui).unwrap().permissions();
        std::os::unix::fs::PermissionsExt::set_mode(&mut mode, 0o755);
        std::fs::set_permissions(&gui, mode).unwrap();
        let answer = |path: &std::path::Path| {
            let out = Command::new("/bin/sh")
                .args(["-c", &without_input(super::GUI_PROBE)])
                .env("PATH", path)
                .output()
                .unwrap();
            String::from_utf8(out.stdout).unwrap()
        };
        assert_eq!(answer(&with_gui), "gui\n");
        if !std::path::Path::new("/usr/share/applications/com.roversx.thinkterm.desktop").exists() {
            assert_eq!(answer(&empty), "");
        }
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn every_remote_command_still_parses_once_its_input_is_cut() {
        for command in [
            super::HOST_PROBE.to_string(),
            super::VARIANT_READ.to_string(),
            super::GUI_PROBE.to_string(),
            super::STOP_COMMAND.to_string(),
            thinkterm_update::install_command("server", "9.9.9"),
            thinkterm_update::takeover_command(),
        ] {
            let checked = std::process::Command::new("/bin/sh")
                .args(["-n", "-c", &without_input(&command)])
                .status()
                .unwrap();
            assert!(checked.success(), "does not parse: {}", command);
        }
    }

    #[cfg(unix)]
    #[test]
    fn a_command_does_not_wait_for_the_input_we_hold_open() {
        use std::process::{Command, Stdio};
        let mut child = Command::new("/bin/sh")
            .args(["-c", &without_input("cat; echo done")])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        // Held open, as `run_capture` holds the ssh channel's stdin.
        let _stdin = child.stdin.take();
        let started = std::time::Instant::now();
        while child.try_wait().unwrap().is_none() {
            if started.elapsed() > std::time::Duration::from_secs(5) {
                let _ = child.kill();
                panic!("the command waited for input");
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        let mut out = String::new();
        std::io::Read::read_to_string(child.stdout.as_mut().unwrap(), &mut out).unwrap();
        assert_eq!(out, "done\n");
    }
}
