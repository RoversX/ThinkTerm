//! `thinkterm tailnet`: a tailnet joined without the Tailscale app, for a
//! machine nobody sits at. The mux server keeps the node running, in
//! `thinkterm-tailnet`, which lets SSH from the tailnet through to this
//! machine's sshd; this command only changes the settings they follow and
//! reads back what the node reports.

use anyhow::{bail, Context};
use clap::{Parser, Subcommand};
use std::time::{Duration, Instant};
use thinkterm_tailnet::{Report, Settings, Status};

#[derive(Debug, Parser, Clone)]
pub struct TailnetCommand {
    #[command(subcommand)]
    sub: Sub,
}

#[derive(Debug, Subcommand, Clone)]
enum Sub {
    /// Have this machine's mux server join the tailnet and wait until it
    /// has joined: it prints a link to
    /// approve the machine if the control server wants one
    Up {
        /// Forward incoming tailnet SSH to this machine's sshd. It sees
        /// loopback clients, so local-address trust rules apply to them
        #[arg(long)]
        serve_ssh: bool,
        /// The control server: a Headscale's address. Tailscale's own when
        /// not given before; an empty value goes back to it
        #[arg(long, value_name = "URL")]
        control_url: Option<String>,
        /// The machine's name in the tailnet; "thinkterm-" and four
        /// characters of the machine's key when not given before. An empty
        /// value goes back to that
        #[arg(long, value_name = "NAME")]
        hostname: Option<String>,
        /// Read an auth key from standard input instead of approving the
        /// machine in a browser
        #[arg(long)]
        auth_key_stdin: bool,
    },
    /// Leave the tailnet, keeping this machine's identity for the next `up`
    Down,
    /// Leave the tailnet and forget this machine's identity and auth key
    Logout,
    /// Show the settings and what the node is doing
    Status,
}

/// How long `up` waits for the server to report the node connected.
const UP_WAIT: Duration = Duration::from_secs(120);

impl TailnetCommand {
    pub fn run(&self) -> anyhow::Result<()> {
        if !thinkterm_tailnet::SUPPORTED {
            bail!("joining a tailnet is not supported on this platform");
        }
        match &self.sub {
            Sub::Up {
                serve_ssh,
                control_url,
                hostname,
                auth_key_stdin,
            } => {
                let before = thinkterm_tailnet::load_summary()?;
                // Compared as the settings keep it: the same server typed
                // another way is no change.
                let new_control = control_url.as_deref().is_some_and(|url| {
                    thinkterm_tailnet::control_url_as_kept(url).ok()
                        != Some(before.control_url.clone())
                });
                // A control server that does not answer is not switched to:
                // on a machine reached through the tailnet, nobody would
                // get back in to switch it back.
                if new_control {
                    let url = control_url.as_deref().unwrap_or_default();
                    thinkterm_tailnet::check_control_server(url).context("nothing was changed")?;
                }
                // Said before the node opens to the tailnet, not once it has.
                if *serve_ssh {
                    println!(
                        "Machines in the tailnet can reach this machine's sshd through the mux \
                     server. sshd sees their connections come from 127.0.0.1 or ::1, so its \
                     settings for local connections apply to them too (Match Address \
                     127.0.0.1,::1, from=\"127.0.0.1\" keys, fail2ban's ignoreip).\n"
                    );
                }
                if before.enabled
                    && (new_control
                        || changes(hostname, &before.hostname)
                        || before.serve_ssh != *serve_ssh)
                {
                    println!(
                        "The node restarts with the new settings: connections through the \
                         tailnet end, this one too if it comes through it."
                    );
                }
                let auth_key = if *auth_key_stdin {
                    Some(read_auth_key()?)
                } else {
                    None
                };
                let saved = thinkterm_tailnet::update(|settings| {
                    settings.enabled = true;
                    settings.serve_ssh = *serve_ssh;
                    if let Some(url) = control_url {
                        settings.control_url = Some(url.clone());
                    }
                    if let Some(name) = hostname {
                        settings.hostname = Some(name.clone());
                    }
                    if auth_key.is_some() {
                        settings.auth_key = auth_key;
                    }
                })?;
                wait_until_up(saved.revision)
            }
            Sub::Down => {
                thinkterm_tailnet::update(|settings| {
                    settings.enabled = false;
                    settings.serve_ssh = false;
                })?;
                println!(
                    "Left the tailnet. `thinkterm tailnet up` joins again as the same machine."
                );
                Ok(())
            }
            Sub::Logout => {
                thinkterm_tailnet::update(|settings| {
                    settings.enabled = false;
                    settings.serve_ssh = false;
                    settings.auth_key = None;
                    settings.node_keys = None;
                })?;
                println!(
                    "Left the tailnet and forgot this machine's identity. \
                     It stays listed in the tailnet until removed there."
                );
                Ok(())
            }
            Sub::Status => print_status(&thinkterm_tailnet::load()?),
        }
    }
}

/// Whether `given` -- an option of `up`, blank for the default -- is other
/// than what is saved.
fn changes(given: &Option<String>, saved: &Option<String>) -> bool {
    given.as_deref().is_some_and(|text| {
        let text = text.trim();
        (!text.is_empty()).then_some(text) != saved.as_deref()
    })
}

/// One line from standard input. Typed at a terminal it is not echoed, and
/// Ctrl-C gives up on it with the terminal as it was; a terminal that
/// would echo it is not read from.
fn read_auth_key() -> anyhow::Result<String> {
    use std::io::{BufRead, IsTerminal, Read};
    let stdin = std::io::stdin();
    let line = if stdin.is_terminal() {
        let typed = {
            let quiet = Quiet::new(&stdin);
            // Git Bash's and Cygwin's terminals reach a Windows program as
            // pipes, whose echo is theirs to turn off, not this program's.
            if !quiet.hides_typing() {
                bail!(
                    "not reading the auth key here: this terminal would show it as it is \
                     typed. Give it on standard input instead: \
                     `thinkterm tailnet up --auth-key-stdin < FILE`"
                );
            }
            eprint!("Auth key: ");
            let keys = stdin.lock().bytes();
            // The console reads Ctrl-Z, Windows' Ctrl-D, as the input's
            // end: giving up, as Ctrl-D does, not taking what was typed.
            #[cfg(windows)]
            let keys = keys.chain(std::iter::once(Ok(0x04)));
            typed_line(keys)
        };
        eprintln!();
        typed?
    } else {
        let mut line = String::new();
        stdin.lock().read_line(&mut line)?;
        line
    };
    let key = line.trim();
    if key.is_empty() {
        bail!("no auth key on standard input");
    }
    Ok(key.to_string())
}

/// A line typed with the terminal's own line editing off (see `Quiet`):
/// up to Enter, with Backspace taking back a character and Ctrl-U the
/// lot. Ctrl-C and Ctrl-D give up on it.
fn typed_line(bytes: impl Iterator<Item = std::io::Result<u8>>) -> anyhow::Result<String> {
    let mut typed = Vec::new();
    for byte in bytes {
        match byte? {
            b'\r' | b'\n' => break,
            0x03 | 0x04 => bail!("no auth key: given up"),
            0x08 | 0x7f => {
                typed.pop();
            }
            0x15 => typed.clear(),
            byte if byte < 0x20 => {}
            byte => typed.push(byte),
        }
    }
    String::from_utf8(typed).context("the auth key is not text")
}

/// Turns the terminal's echo off for as long as it lives, with its line
/// editing and Ctrl-C's signal: Ctrl-C would end the process with the echo
/// still off, so it comes in as a key instead (see `typed_line`). On
/// Windows the same goes for the console's input mode.
struct Quiet {
    #[cfg(unix)]
    restore: Option<(std::os::fd::RawFd, termios::Termios)>,
    #[cfg(windows)]
    restore: Option<(std::os::windows::io::RawHandle, u32)>,
}

impl Quiet {
    fn new(stdin: &std::io::Stdin) -> Self {
        #[cfg(unix)]
        {
            use std::os::fd::AsRawFd;
            let fd = stdin.as_raw_fd();
            let restore = termios::Termios::from_fd(fd).ok().and_then(|saved| {
                let mut quiet = saved;
                quiet.c_lflag &= !(termios::ECHO | termios::ICANON | termios::ISIG);
                quiet.c_cc[termios::VMIN] = 1;
                quiet.c_cc[termios::VTIME] = 0;
                termios::tcsetattr(fd, termios::TCSANOW, &quiet)
                    .ok()
                    .map(|()| (fd, saved))
            });
            Self { restore }
        }
        #[cfg(windows)]
        {
            use std::os::windows::io::AsRawHandle;
            use winapi::um::consoleapi::{GetConsoleMode, SetConsoleMode};
            use winapi::um::wincon::{
                ENABLE_ECHO_INPUT, ENABLE_LINE_INPUT, ENABLE_PROCESSED_INPUT,
            };
            let handle = stdin.as_raw_handle();
            let mut saved = 0;
            let restore = (unsafe { GetConsoleMode(handle as _, &mut saved) } != 0)
                .then(|| saved & !(ENABLE_ECHO_INPUT | ENABLE_LINE_INPUT | ENABLE_PROCESSED_INPUT))
                .filter(|&quiet| unsafe { SetConsoleMode(handle as _, quiet) } != 0)
                .map(|_| (handle, saved));
            Self { restore }
        }
        #[cfg(not(any(unix, windows)))]
        {
            let _ = stdin;
            Self {}
        }
    }

    /// Whether what is typed is hidden: the echo did go off.
    fn hides_typing(&self) -> bool {
        #[cfg(any(unix, windows))]
        {
            self.restore.is_some()
        }
        #[cfg(not(any(unix, windows)))]
        {
            false
        }
    }
}

impl Drop for Quiet {
    fn drop(&mut self) {
        #[cfg(unix)]
        if let Some((fd, saved)) = self.restore.take() {
            let _ = termios::tcsetattr(fd, termios::TCSANOW, &saved);
        }
        #[cfg(windows)]
        if let Some((handle, saved)) = self.restore.take() {
            unsafe { winapi::um::consoleapi::SetConsoleMode(handle as _, saved) };
        }
    }
}

/// A report the running server wrote once it had acted on the settings'
/// `revision` -- not one about the settings before it, however new -- from
/// a process that is still there.
fn fresh_report(revision: u64) -> Option<Report> {
    thinkterm_tailnet::read_live_report().filter(|report| report.revision >= revision)
}

fn wait_until_up(revision: u64) -> anyhow::Result<()> {
    let start = Instant::now();
    let mut last = None;
    loop {
        let report = fresh_report(revision);
        if report.is_none() && start.elapsed() > Duration::from_secs(10) {
            println!(
                "Saved. Nothing has picked it up yet: it joins once a mux server runs \
                 (`thinkterm-mux-server --daemonize`) and has started thinkterm-tailnet, \
                 whose log says why if it did not."
            );
            return Ok(());
        }
        if let Some(report) = report {
            if last.as_ref() != Some(&report.status) {
                match &report.status {
                    Status::Off | Status::Starting => println!("Connecting..."),
                    Status::NeedsLogin { url } => {
                        println!("To approve this machine, open:\n\n    {url}\n");
                    }
                    Status::Connected { name, address } => {
                        match address {
                            Some(address) => println!("Connected as {name} ({address})."),
                            None => println!("Connected as {name}."),
                        }
                        return Ok(());
                    }
                    Status::Failed { error } => println!("Not connected yet: {error}"),
                    Status::HeldElsewhere => {
                        println!("Another ThinkTerm process on this machine holds the node; waiting for it to let go.")
                    }
                }
                last = Some(report.status);
            }
        }
        if start.elapsed() > UP_WAIT {
            bail!(
                "not connected after {}s; `thinkterm tailnet status` shows how it goes",
                UP_WAIT.as_secs()
            );
        }
        std::thread::sleep(Duration::from_millis(500));
    }
}

fn print_status(settings: &Settings) -> anyhow::Result<()> {
    if settings.enabled {
        println!("Tailnet:   on");
    } else {
        println!("Tailnet:   off");
    }
    if settings.serve_ssh {
        println!("SSH in:    on (sshd sees loopback clients: 127.0.0.1 or ::1)");
    } else {
        println!("SSH in:    off");
    }
    println!(
        "Control:   {}",
        settings.control_url.as_deref().unwrap_or("Tailscale")
    );
    let report = thinkterm_tailnet::read_live_report().filter(|_| settings.enabled);
    let name = match (
        &settings.hostname,
        report.as_ref().map(|report| &report.status),
    ) {
        (Some(name), _) => name.clone(),
        (None, Some(Status::Connected { name, .. })) => format!("{name} (automatic)"),
        (None, _) => {
            "automatic: \"thinkterm-\" and four characters of the machine's key".to_string()
        }
    };
    println!("Name:      {name}");
    println!(
        "Auth key:  {}",
        if settings.auth_key.is_some() {
            "saved"
        } else {
            "none"
        }
    );
    let Some(report) = report else {
        if settings.enabled {
            println!("Node:      not running (no mux server is, or its thinkterm-tailnet is not)");
        }
        return Ok(());
    };
    let node = match &report.status {
        Status::Off => "off".to_string(),
        Status::Starting => "connecting".to_string(),
        Status::NeedsLogin { url } => format!("waiting for approval at {url}"),
        Status::Connected {
            name,
            address: Some(address),
        } => format!("connected as {name} ({address})"),
        Status::Connected {
            name,
            address: None,
        } => format!("connected as {name}"),
        Status::Failed { error } => format!("not connected: {error}"),
        Status::HeldElsewhere => "held by another ThinkTerm process".to_string(),
    };
    println!("Node:      {node}");
    if !report.peers.is_empty() {
        println!("Machines:");
        let width = report
            .peers
            .iter()
            .map(|peer| peer.name.len())
            .max()
            .unwrap_or(0);
        for peer in &report.peers {
            let address = peer
                .address()
                .map(|address| address.to_string())
                .unwrap_or_default();
            let online = match peer.online {
                Some(true) => "online",
                Some(false) => "offline",
                None => "",
            };
            println!("  {:width$}  {address:15}  {online}", peer.name);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn typed(keys: &[u8]) -> anyhow::Result<String> {
        typed_line(keys.iter().map(|byte| Ok(*byte)))
    }

    #[test]
    fn joining_only_serves_ssh_when_explicitly_requested() {
        for (args, expected) in [
            (vec!["tailnet", "up"], false),
            (vec!["tailnet", "up", "--serve-ssh"], true),
        ] {
            let command = TailnetCommand::try_parse_from(args).unwrap();
            let Sub::Up { serve_ssh, .. } = command.sub else {
                panic!("expected up")
            };
            assert_eq!(serve_ssh, expected);
        }
    }

    #[test]
    fn a_typed_key_is_edited_as_at_a_shell_prompt() {
        assert_eq!(typed(b"tskey-abc\r").unwrap(), "tskey-abc");
        assert_eq!(typed(b"tskey-abx\x7fc\n").unwrap(), "tskey-abc");
        assert_eq!(typed(b"oops\x15tskey-abc\r").unwrap(), "tskey-abc");
        // Pasted without a newline, then the input ends.
        assert_eq!(typed(b"tskey-abc").unwrap(), "tskey-abc");
    }

    #[test]
    fn ctrl_c_and_ctrl_d_give_up() {
        assert!(typed(b"tskey\x03").is_err());
        assert!(typed(b"\x04").is_err());
    }
}
