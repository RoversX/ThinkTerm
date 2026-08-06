use anyhow::{bail, Context, Result};
use base64::engine::general_purpose::STANDARD;
use base64::Engine;
use std::io::{Read, Write};
use std::process::{Command, Stdio};

const MAX_CLIPBOARD_BYTES: usize = 8 * 1024 * 1024;
const _: () = assert!(MAX_CLIPBOARD_BYTES >= 1024 * 1024);
const _: () = assert!(MAX_CLIPBOARD_BYTES <= 16 * 1024 * 1024);

pub trait ClipboardProvider: Send + Sync {
    fn write_text(&self, text: &str) -> Result<()>;
    fn read_text(&self) -> Result<String>;
}

#[derive(Default)]
pub struct SystemClipboard;

impl ClipboardProvider for SystemClipboard {
    fn write_text(&self, text: &str) -> Result<()> {
        if text.len() > MAX_CLIPBOARD_BYTES {
            bail!("selection is too large for the clipboard");
        }
        platform_write(text.as_bytes()).or_else(|platform_error| {
            write_osc52(text).with_context(|| format!("{platform_error:#}; OSC 52 fallback"))
        })
    }

    fn read_text(&self) -> Result<String> {
        let bytes = platform_read()?;
        if bytes.len() > MAX_CLIPBOARD_BYTES {
            bail!("clipboard text exceeds 8 MiB");
        }
        String::from_utf8(bytes).context("clipboard text is not UTF-8")
    }
}

#[cfg(target_os = "macos")]
fn platform_write(bytes: &[u8]) -> Result<()> {
    run_writer("pbcopy", &[], bytes)
}

#[cfg(target_os = "macos")]
fn platform_read() -> Result<Vec<u8>> {
    run_reader("pbpaste", &[])
}

#[cfg(all(unix, not(target_os = "macos")))]
fn platform_write(bytes: &[u8]) -> Result<()> {
    let mut failures = Vec::new();
    for (program, args) in linux_write_commands() {
        match run_writer(program, args, bytes) {
            Ok(()) => return Ok(()),
            Err(err) => failures.push(format!("{program}: {err}")),
        }
    }
    bail!("no clipboard writer succeeded ({})", failures.join("; "))
}

#[cfg(all(unix, not(target_os = "macos")))]
fn platform_read() -> Result<Vec<u8>> {
    let mut failures = Vec::new();
    for (program, args) in linux_read_commands() {
        match run_reader(program, args) {
            Ok(bytes) => return Ok(bytes),
            Err(err) => failures.push(format!("{program}: {err}")),
        }
    }
    bail!("no clipboard reader succeeded ({})", failures.join("; "))
}

#[cfg(all(unix, not(target_os = "macos")))]
fn linux_write_commands() -> Vec<(&'static str, &'static [&'static str])> {
    let mut commands = Vec::new();
    if std::env::var_os("WAYLAND_DISPLAY").is_some() {
        commands.push(("wl-copy", &[][..]));
    }
    commands.push(("xclip", &["-selection", "clipboard", "-in"]));
    commands.push(("xsel", &["--clipboard", "--input"]));
    commands
}

#[cfg(all(unix, not(target_os = "macos")))]
fn linux_read_commands() -> Vec<(&'static str, &'static [&'static str])> {
    let mut commands = Vec::new();
    if std::env::var_os("WAYLAND_DISPLAY").is_some() {
        commands.push(("wl-paste", &["--no-newline", "--type", "text"] as &[_]));
        commands.push(("wl-paste", &["--no-newline"]));
    }
    commands.push(("xclip", &["-selection", "clipboard", "-out"]));
    commands.push(("xsel", &["--clipboard", "--output"]));
    commands
}

#[cfg(windows)]
fn platform_write(bytes: &[u8]) -> Result<()> {
    let text = String::from_utf8(bytes.to_vec()).context("clipboard text is not UTF-8")?;
    clipboard_win::set_clipboard_string(&text).context("write Windows clipboard")
}

#[cfg(windows)]
fn platform_read() -> Result<Vec<u8>> {
    let text = clipboard_win::get_clipboard_string().context("read Windows clipboard")?;
    Ok(text.into_bytes())
}

#[cfg(not(any(unix, windows)))]
fn platform_write(_bytes: &[u8]) -> Result<()> {
    bail!("system clipboard is unavailable on this platform")
}

#[cfg(not(any(unix, windows)))]
fn platform_read() -> Result<Vec<u8>> {
    bail!("system clipboard is unavailable on this platform")
}

fn run_writer(program: &str, args: &[&str], bytes: &[u8]) -> Result<()> {
    let mut child = Command::new(program)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .with_context(|| format!("start {program}"))?;
    child
        .stdin
        .take()
        .context("clipboard process has no stdin")?
        .write_all(bytes)
        .with_context(|| format!("write to {program}"))?;
    let status = child
        .wait()
        .with_context(|| format!("wait for {program}"))?;
    if !status.success() {
        bail!("exited with {status}");
    }
    Ok(())
}

fn run_reader(program: &str, args: &[&str]) -> Result<Vec<u8>> {
    let mut child = Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .with_context(|| format!("start {program}"))?;
    let mut stdout = child
        .stdout
        .take()
        .context("clipboard process has no stdout")?;
    let mut bytes = Vec::new();
    stdout
        .by_ref()
        .take((MAX_CLIPBOARD_BYTES + 1) as u64)
        .read_to_end(&mut bytes)
        .with_context(|| format!("read from {program}"))?;
    if bytes.len() > MAX_CLIPBOARD_BYTES {
        // `take` deliberately stops consuming output at the limit. Close the
        // pipe and terminate the producer before waiting, otherwise a command
        // with more output can remain blocked forever on its full stdout pipe.
        drop(stdout);
        let _ = child.kill();
        let _ = child.wait();
        bail!("clipboard text exceeds 8 MiB");
    }
    drop(stdout);
    let status = child
        .wait()
        .with_context(|| format!("wait for {program}"))?;
    if !status.success() {
        bail!("exited with {status}");
    }
    Ok(bytes)
}

fn write_osc52(text: &str) -> Result<()> {
    let encoded = STANDARD.encode(text.as_bytes());
    let mut stdout = std::io::stdout().lock();
    stdout
        .write_all(format!("\x1b]52;c;{encoded}\x07").as_bytes())
        .context("write OSC 52")?;
    stdout.flush().context("flush OSC 52")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(all(unix, not(target_os = "macos")))]
    #[test]
    fn linux_always_has_x11_fallbacks() {
        let writers = linux_write_commands();
        assert!(writers.iter().any(|(program, _)| *program == "xclip"));
        assert!(writers.iter().any(|(program, _)| *program == "xsel"));
    }

    #[cfg(unix)]
    #[test]
    fn oversized_reader_terminates_the_producer_instead_of_waiting_on_its_pipe() {
        let error = run_reader("sh", &["-c", "head -c 9000000 /dev/zero"])
            .expect_err("oversized clipboard output must be rejected");
        assert!(error.to_string().contains("exceeds 8 MiB"));
    }
}
