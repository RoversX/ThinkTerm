//! `thinkterm` is the branded name for the ThinkTerm CLI. The full CLI
//! lives in the `wezterm` binary built from this same crate; exec it so
//! the two names stay perfectly in sync without compiling the crate twice.

fn main() {
    let exe_name = if cfg!(windows) {
        "wezterm.exe"
    } else {
        "wezterm"
    };
    let exe = match std::env::current_exe()
        .ok()
        .and_then(|p| Some(p.parent()?.join(exe_name)))
    {
        Some(exe) => exe,
        None => {
            eprintln!("thinkterm: unable to locate the {exe_name} binary next to this one");
            std::process::exit(1);
        }
    };

    let mut cmd = std::process::Command::new(exe);
    cmd.args(std::env::args_os().skip(1));

    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        let err = cmd.exec();
        eprintln!("thinkterm: failed to exec {cmd:?}: {err}");
        std::process::exit(1);
    }
    #[cfg(windows)]
    {
        match cmd.status() {
            Ok(status) => std::process::exit(status.code().unwrap_or(1)),
            Err(err) => {
                eprintln!("thinkterm: failed to run {cmd:?}: {err}");
                std::process::exit(1);
            }
        }
    }
}
