//! Bringing the session server of this machine to the client's build.
//!
//! The GUI keeps its local terminals in a `thinkterm-mux-server` (see
//! `UnixDomain::local_session_host`). After the application is updated the
//! next launch finds a server of the previous build still running with
//! every session in it. Rather than refuse it (codec mismatch) or serve
//! stale behaviour (same codec, older build), the client runs the new
//! server binary with `--takeover`: the running one hands its panes over
//! and exits, and the client reconnects to the same runtime id.

use anyhow::Context as _;
use config::UnixDomain;
use mux::connui::ConnectionUI;
use std::io::Read;
use std::process::{Command, Stdio};

/// Run the server this client ships with as a successor of the one
/// listening on `unix`'s socket. `Ok(true)` when it now serves; `Ok(false)`
/// when the running server refused or the takeover failed (its output was
/// relayed to the UI); `Err` when the command could not be run at all.
pub fn take_over_local_server(unix: &UnixDomain, ui: &ConnectionUI) -> anyhow::Result<bool> {
    let argv = takeover_argv(unix)?;
    ui.output_str(&format!(
        "Handing the sessions of the running mux server to this build ({})...\n",
        config::wezterm_version()
    ));
    let mut cmd = Command::new(&argv[0]);
    cmd.args(&argv[1..])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    // The GUI runs with a locked-down umask; the server and every shell it
    // spawns must get the one the user had (as unix_connect does).
    #[cfg(unix)]
    if let Some(mask) = umask::UmaskSaver::saved_umask() {
        use std::os::unix::process::CommandExt;
        unsafe {
            cmd.pre_exec(move || {
                libc::umask(mask);
                Ok(())
            });
        }
    }
    let mut child = cmd
        .spawn()
        .with_context(|| format!("running {:?}", argv))?;
    let stdout = child.stdout.take();
    let stderr = child.stderr.take();
    let relay = |ui: ConnectionUI, mut pipe: Box<dyn Read + Send>| {
        std::thread::spawn(move || {
            let mut buf = [0u8; 1024];
            while let Ok(len) = pipe.read(&mut buf) {
                if len == 0 {
                    break;
                }
                ui.output_str(&String::from_utf8_lossy(&buf[..len]));
            }
        })
    };
    let out_thread = stdout.map(|pipe| relay(ui.clone(), Box::new(pipe)));
    let err_thread = stderr.map(|pipe| relay(ui.clone(), Box::new(pipe)));
    // A daemonizing takeover reports within seconds; a serve command that
    // does not daemonize would never return, so wait with a limit.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(120);
    let status = loop {
        if let Some(status) = child.try_wait().context("waiting for the takeover")? {
            break status;
        }
        if std::time::Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            anyhow::bail!("the takeover did not report within 120 s");
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    };
    if let Some(thread) = out_thread {
        let _ = thread.join();
    }
    if let Some(thread) = err_thread {
        let _ = thread.join();
    }
    if status.success() {
        ui.output_str("Handed over; reconnecting.\n");
        Ok(true)
    } else {
        ui.output_str(&format!(
            "The running server could not hand over ({status}); keeping it.\n"
        ));
        Ok(false)
    }
}

/// The serve command of the domain (`thinkterm-mux-server --daemonize` next
/// to this executable unless configured otherwise) with `--takeover`
/// appended, so a custom serve command (`wsl -e ...`) takes over the same
/// way it starts.
fn takeover_argv(unix: &UnixDomain) -> anyhow::Result<Vec<std::ffi::OsString>> {
    let mut argv = unix.serve_command()?;
    if argv.is_empty() {
        anyhow::bail!("the serve command of domain {} is empty", unix.name);
    }
    argv.push("--takeover".into());
    Ok(argv)
}

#[cfg(test)]
mod tests {
    use super::takeover_argv;
    use config::UnixDomain;

    #[test]
    fn the_takeover_is_the_serve_command_plus_the_flag() {
        let unix = UnixDomain {
            name: "unix".into(),
            serve_command: Some(vec!["wsl".into(), "-e".into(), "thinkterm-mux-server".into(), "--daemonize".into()]),
            ..Default::default()
        };
        let argv = takeover_argv(&unix).unwrap();
        assert_eq!(
            argv,
            vec!["wsl", "-e", "thinkterm-mux-server", "--daemonize", "--takeover"]
                .into_iter()
                .map(std::ffi::OsString::from)
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn the_default_serve_command_is_the_bundled_server() {
        let argv = takeover_argv(&UnixDomain::default()).unwrap();
        let exe = std::path::Path::new(&argv[0]);
        assert!(exe.ends_with("thinkterm-mux-server"), "{exe:?}");
        assert_eq!(argv[1], "--daemonize");
        assert_eq!(argv[2], "--takeover");
    }
}
