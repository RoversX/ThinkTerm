//! `wezterm` is the legacy compatibility name for the ThinkTerm CLI. The
//! full CLI lives in the `thinkterm` binary built from this same crate; exec
//! it so existing scripts keep working while all visible branding stays on
//! ThinkTerm.

fn main() {
    let exe_name = if cfg!(windows) {
        "thinkterm.exe"
    } else {
        "thinkterm"
    };
    let exe = match std::env::current_exe()
        .ok()
        .and_then(|p| Some(p.parent()?.join(exe_name)))
    {
        Some(exe) => exe,
        None => {
            eprintln!("wezterm: unable to locate the {exe_name} binary next to this one");
            std::process::exit(1);
        }
    };

    let mut cmd = std::process::Command::new(exe);
    cmd.args(std::env::args_os().skip(1));

    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        let err = cmd.exec();
        eprintln!("wezterm: failed to exec {cmd:?}: {err}");
        std::process::exit(1);
    }
    #[cfg(windows)]
    {
        match cmd.status() {
            Ok(status) => std::process::exit(status.code().unwrap_or(1)),
            Err(err) => {
                eprintln!("wezterm: failed to run {cmd:?}: {err}");
                std::process::exit(1);
            }
        }
    }
}
