//! `thinkterm tailnet`: a tailnet joined without the Tailscale app, for a
//! machine nobody sits at. The mux server keeps the node running, in
//! `thinkterm-tailnet`, which lets SSH from the tailnet through to this
//! machine's sshd, and the other ports `serve add` opens to the same ports
//! here; this command only changes the settings they follow and reads back
//! what the node reports.

use anyhow::{bail, Context};
use clap::{Parser, Subcommand};
use std::time::{Duration, Instant};
use thinkterm_tailnet::{Report, Settings, Status, Summary, MAX_SERVED_PORTS, SSH_PORT};

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
    /// Let other ports of this machine in from the tailnet, each to the
    /// same port here: a VNC server's, say
    ///
    /// SSH's is `up --serve-ssh`'s. The ports close when the tailnet is
    /// turned off, pointed at another control server or joined as a new
    /// machine
    Serve {
        #[command(subcommand)]
        action: ServeAction,
    },
    /// Leave the tailnet, keeping this machine's identity for the next `up`
    Down,
    /// Leave the tailnet and forget this machine's identity and auth key
    Logout,
    /// Show the settings and what the node is doing
    Status,
}

#[derive(Debug, Subcommand, Clone)]
enum ServeAction {
    /// Open ports to the tailnet
    ///
    /// What listens on them sees its clients come from 127.0.0.1 or ::1, so
    /// it lets them do what it lets local connections do; the tailnet's
    /// access rules still apply. The node restarts for it, ending the
    /// connections through it
    Add {
        /// TCP ports, by space or comma
        #[arg(required = true, value_delimiter = ',', value_parser = port_to_serve)]
        ports: Vec<u16>,
    },
    /// Close ports opened with `add`
    ///
    /// The node restarts for it, ending the connections through it: those
    /// on the ports closed among them
    Remove {
        /// TCP ports, by space or comma
        #[arg(
            required_unless_present = "all",
            conflicts_with = "all",
            value_delimiter = ',',
            value_parser = port_to_serve
        )]
        ports: Vec<u16>,
        /// Close every port opened with `add`
        #[arg(long)]
        all: bool,
    },
    /// Show the ports opened with `add`
    List,
}

/// A port `serve` opens or closes: any TCP port but SSH's, which is
/// `up --serve-ssh`'s.
fn port_to_serve(text: &str) -> Result<u16, String> {
    match text.trim().parse::<u16>() {
        Ok(SSH_PORT) => Err(format!(
            "port {SSH_PORT} is SSH's: `thinkterm tailnet up --serve-ssh` opens it"
        )),
        Ok(0) | Err(_) => Err(format!("{text:?} is not a TCP port (1 to 65535)")),
        Ok(port) => Ok(port),
    }
}

/// What a change to the ports taken in, or to what the node runs on, does
/// to the connections through it.
const RESTARTS: &str = "The node restarts with the new settings: connections through the \
                        tailnet end, this one too if it comes through it.";

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
                    println!("{RESTARTS}");
                }
                // Opened to the tailnet as it was, not to the one on the new
                // server (see Settings::close_ports_unless_same_tailnet).
                let other_server = control_url.as_ref().is_some_and(|url| {
                    !thinkterm_tailnet::same_control_server(
                        &before.control_url,
                        &Some(url.clone()),
                    )
                });
                if before.enabled && other_server && !before.serve_ports.is_empty() {
                    println!(
                        "Ports {} close: they were opened to the tailnet on the old control server.",
                        listed(&before.serve_ports)
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
                let mut closed = vec![];
                thinkterm_tailnet::update(|settings| {
                    closed = settings.serve_ports.clone();
                    settings.enabled = false;
                    settings.serve_ssh = false;
                })?;
                println!(
                    "Left the tailnet. `thinkterm tailnet up` joins again as the same machine."
                );
                say_closed(&closed);
                Ok(())
            }
            Sub::Logout => {
                let mut closed = vec![];
                thinkterm_tailnet::update(|settings| {
                    closed = settings.serve_ports.clone();
                    settings.enabled = false;
                    settings.serve_ssh = false;
                    settings.auth_key = None;
                    settings.node_keys = None;
                })?;
                println!(
                    "Left the tailnet and forgot this machine's identity. \
                     It stays listed in the tailnet until removed there."
                );
                say_closed(&closed);
                Ok(())
            }
            Sub::Serve { action } => serve(action),
            Sub::Status => print_status(&thinkterm_tailnet::load()?),
        }
    }
}

/// Says which ports `serve add` had opened closed as the tailnet was left:
/// they do not open again by themselves.
fn say_closed(closed: &[u16]) {
    if !closed.is_empty() {
        println!(
            "Port{} {} closed with it, and stay{} closed: `thinkterm tailnet serve add` opens \
             them again.",
            if closed.len() == 1 { "" } else { "s" },
            listed(closed),
            if closed.len() == 1 { "s" } else { "" },
        );
    }
}

/// How long `serve add` waits for the node to have acted on the settings it
/// read, before it can tell which node the ports are opened to: the holder
/// looks at them every few seconds.
const CAUGHT_UP_WAIT: Duration = Duration::from_secs(10);

/// How long `serve add` waits for the node to take the ports in: it
/// restarts for them, and gets back in.
const OPEN_WAIT: Duration = Duration::from_secs(60);

/// The live report of a process that has acted on the settings' `revision`,
/// waited for for up to CAUGHT_UP_WAIT; or else the last live one.
fn caught_up_report(revision: u64) -> Option<Report> {
    let start = Instant::now();
    loop {
        if let Some(report) = fresh_report(revision) {
            return Some(report);
        }
        if start.elapsed() > CAUGHT_UP_WAIT {
            return thinkterm_tailnet::read_live_report();
        }
        std::thread::sleep(Duration::from_millis(300));
    }
}

/// What became of ports opened by a change to the settings.
enum Opening {
    /// The node takes them all in.
    Open,
    /// The process holding the node is from before other ports could be
    /// opened.
    Old,
    /// Not yet, after OPEN_WAIT: the node is still getting back in, say.
    NotYet,
}

/// Waits for the node to take `new` in since the settings' `revision`, for
/// up to OPEN_WAIT, saying once that it waits.
fn wait_until_open(revision: u64, new: &[u16]) -> Opening {
    let start = Instant::now();
    let mut said = false;
    loop {
        match fresh_report(revision) {
            Some(Report {
                forwarding: None, ..
            }) => return Opening::Old,
            Some(Report {
                forwarding: Some(taken),
                ..
            }) if new.iter().all(|port| taken.contains(port)) => return Opening::Open,
            _ => {}
        }
        if start.elapsed() > OPEN_WAIT {
            return Opening::NotYet;
        }
        if !said && start.elapsed() > Duration::from_secs(2) {
            println!("Waiting for the node to take them in...");
            said = true;
        }
        std::thread::sleep(Duration::from_millis(300));
    }
}

fn serve(action: &ServeAction) -> anyhow::Result<()> {
    let before = thinkterm_tailnet::load_summary()?;
    match action {
        ServeAction::List => {
            let report = thinkterm_tailnet::read_live_report().filter(|_| before.enabled);
            println!(
                "Ports in:  {}",
                ports_in_line(
                    &before.serve_ports,
                    before.serve_ports_to.as_deref(),
                    report.as_ref()
                )
            );
            Ok(())
        }
        ServeAction::Add { ports } => {
            if !before.enabled {
                bail!("the tailnet is off: `thinkterm tailnet up` joins it first");
            }
            // Opened to the node the machine is now, as its tailnet knows it:
            // the same keys let into another tailnet take in none of them
            // (see node::NodeConfig::ports_opened_to).
            // By the node as it is after the settings read above: not by one
            // the holder has yet to replace for them -- a new control server,
            // say -- whose ports would go to the node it was.
            let report = caught_up_report(before.revision);
            if report
                .as_ref()
                .is_some_and(|report| report.forwarding.is_none())
            {
                bail!(
                    "the thinkterm-tailnet running now is from before ports could be opened: \
                     open them once it is restarted (the mux server starts it again when it \
                     ends)"
                );
            }
            let Some(membership) = report
                .filter(|report| {
                    report.revision >= before.revision
                        && matches!(report.status, Status::Connected { .. })
                })
                .and_then(|report| report.membership)
            else {
                bail!(
                    "this machine is not connected to the tailnet: open ports once \
                     `thinkterm tailnet status` shows it connected"
                );
            };
            // Ports opened to the machine as another tailnet knew it open
            // nothing now: these replace them.
            let open: &[u16] = if before.serve_ports_to.as_deref() == Some(membership.as_str()) {
                &before.serve_ports
            } else {
                &[]
            };
            // Counted whole: none is dropped for going past the most.
            let mut new: Vec<u16> = ports
                .iter()
                .copied()
                .filter(|port| !open.contains(port))
                .collect();
            new.sort_unstable();
            new.dedup();
            if new.is_empty() {
                println!(
                    "Already open to the tailnet: {}",
                    listed(&thinkterm_tailnet::served_ports(ports))
                );
                return Ok(());
            }
            if open.len() + new.len() > MAX_SERVED_PORTS {
                bail!(
                    "at most {MAX_SERVED_PORTS} ports can be open to the tailnet, and {} already are",
                    open.len()
                );
            }
            if open.len() < before.serve_ports.len() {
                println!(
                    "Ports {} were opened to this machine as another tailnet knew it, and close.",
                    listed(&before.serve_ports)
                );
            }
            // Said before the node opens them, not once it has.
            println!(
                "Machines in the tailnet can reach port{} {} of this machine through the mux \
                 server, as far as the tailnet's access rules let them. What listens there sees \
                 them come from 127.0.0.1 or ::1, so it lets them do what it lets local \
                 connections do: keep its own password on.\n",
                if new.len() == 1 { "" } else { "s" },
                listed(&new)
            );
            println!("{RESTARTS}");
            // Opened to the tailnet as it was looked at above, under the lock
            // its changes are made under: not to one the app or another
            // command turned it into since, nor past the most beside ports
            // another command opened meanwhile.
            let saved = thinkterm_tailnet::update_if(|settings| {
                if settings.revision != before.revision {
                    return false;
                }
                if settings.serve_ports_to.as_deref() != Some(membership.as_str()) {
                    settings.serve_ports.clear();
                }
                settings.serve_ports.extend(&new);
                settings.serve_ports_to = Some(membership.clone());
                true
            })?;
            let Some(saved) = saved.filter(|saved| new.iter().all(|port| saved.serve_ports.contains(port)))
            else {
                bail!(
                    "nothing was opened: the tailnet's settings changed meanwhile; \
                     `thinkterm tailnet status` shows them as they are now"
                );
            };
            match wait_until_open(saved.revision, &new) {
                Opening::Open => println!("{}", ports_in(&saved.summary())),
                Opening::Old => println!("{OLD_HOLDER}"),
                Opening::NotYet => println!(
                    "Saved, but the node does not take {} in yet: `thinkterm tailnet status` \
                     shows when it does.",
                    listed(&new)
                ),
            }
            Ok(())
        }
        ServeAction::Remove { ports, all } => {
            let closes = |port: &u16| *all || ports.contains(port);
            let mut not_open: Vec<u16> = ports
                .iter()
                .copied()
                .filter(|port| !before.serve_ports.contains(port))
                .collect();
            not_open.sort_unstable();
            not_open.dedup();
            if !not_open.is_empty() {
                println!("Not open, so nothing to close: {}.", listed(&not_open));
            }
            if !before.serve_ports.iter().any(closes) {
                println!("Nothing to close. {}", ports_in(&before));
                return Ok(());
            }
            println!("{RESTARTS}");
            // Which ones, as the file is under the lock: ports opened
            // meanwhile close too with --all.
            let mut gone = vec![];
            let saved = thinkterm_tailnet::update(|settings| {
                settings.serve_ports.retain(|port| {
                    if closes(port) {
                        gone.push(*port);
                    }
                    !closes(port)
                });
            })?;
            match wait_until_closed(saved.revision, &gone) {
                Closing::Closed => {
                    println!("Closed: {}. {}", listed(&gone), ports_in(&saved.summary()))
                }
                Closing::Still(still) => println!(
                    "Saved, but the node still takes {} in: it closes them once it acts on the \
                     change, and `thinkterm tailnet status` shows when.",
                    listed(&still)
                ),
                Closing::Unknown => println!(
                    "Saved, but nothing here says yet whether the node still takes {} in: \
                     `thinkterm tailnet status` shows it.",
                    listed(&gone)
                ),
            }
            Ok(())
        }
    }
}

/// What became of ports closed by a change to the settings.
enum Closing {
    /// The node no longer takes them in.
    Closed,
    /// It still takes these in.
    Still(Vec<u16>),
    /// No process here reports what its node takes in.
    Unknown,
}

/// How long `serve remove` waits for the node to stop taking the ports in:
/// the holder looks at the settings every few seconds, and a node stops in
/// two.
const CLOSE_WAIT: Duration = Duration::from_secs(30);

/// Waits for the node to have stopped taking `gone` in since the settings'
/// `revision`, for up to CLOSE_WAIT: by a report on that revision, which a
/// node that has yet to get back in -- waiting for a login, its control
/// server away -- writes too. Ports are closed as its old self stops,
/// before the new one tries to get in.
fn wait_until_closed(revision: u64, gone: &[u16]) -> Closing {
    let takes = |report: &Report| -> Vec<u16> {
        let taken = report.forwarding.as_deref().unwrap_or_default();
        gone.iter().copied().filter(|port| taken.contains(port)).collect()
    };
    let start = Instant::now();
    let mut none_since: Option<Instant> = None;
    loop {
        if let Some(report) = fresh_report(revision) {
            if takes(&report).is_empty() {
                return Closing::Closed;
            }
        }
        // No process reporting at all for a while -- no mux server runs, say
        // -- is not waited out: none is coming.
        let live = thinkterm_tailnet::read_live_report().is_some();
        let none_for = match (live, none_since) {
            (true, _) => {
                none_since = None;
                Duration::ZERO
            }
            (false, Some(since)) => since.elapsed(),
            (false, None) => {
                none_since = Some(Instant::now());
                Duration::ZERO
            }
        };
        if none_for > Duration::from_secs(5) {
            return Closing::Unknown;
        }
        if start.elapsed() > CLOSE_WAIT {
            return match thinkterm_tailnet::read_live_report() {
                Some(report) => match takes(&report) {
                    still if still.is_empty() => Closing::Closed,
                    still => Closing::Still(still),
                },
                None => Closing::Unknown,
            };
        }
        std::thread::sleep(Duration::from_millis(300));
    }
}

/// What a process from before other ports could be opened does with them.
const OLD_HOLDER: &str = "Saved, but the thinkterm-tailnet running now is from before ports \
                          could be opened, and opens none: they open once it is restarted (the \
                          mux server starts it again when it ends).";

/// `ports` as a list to read.
fn listed(ports: &[u16]) -> String {
    ports
        .iter()
        .map(u16::to_string)
        .collect::<Vec<_>>()
        .join(", ")
}

/// The ports `serve add` opened, as the settings keep them (`saved`, opened
/// to the node `saved_to`) and as the node takes them in by its last live
/// report: open, saved but not open yet, opened to the machine as another
/// tailnet knew it, or closing.
fn ports_in_line(saved: &[u16], saved_to: Option<&str>, report: Option<&Report>) -> String {
    let taken = report.and_then(|report| report.forwarding.as_ref());
    let elsewhere = report
        .and_then(|report| report.membership.as_deref())
        .is_some_and(|membership| saved_to != Some(membership));
    let mut line = if saved.is_empty() {
        "none".to_string()
    } else {
        let ports = listed(saved);
        match (report, taken) {
            _ if elsewhere => format!(
                "{ports} saved, none open: they were opened to this machine as another tailnet \
                 knew it (`thinkterm tailnet serve add` opens them to this one)"
            ),
            (Some(_), Some(taken)) if saved.iter().all(|port| taken.contains(port)) => {
                format!("{ports} (each to the same port here, which sees loopback clients)")
            }
            (Some(_), None) => format!(
                "{ports} saved, none open: the thinkterm-tailnet running is from before ports \
                 could be opened"
            ),
            _ => format!("{ports} saved, not all open yet"),
        }
    };
    let closing: Vec<u16> = taken
        .map(|taken| {
            taken
                .iter()
                .copied()
                .filter(|port| *port != SSH_PORT && !saved.contains(port))
                .collect()
        })
        .unwrap_or_default();
    if !closing.is_empty() {
        line.push_str(&format!("; {} still open, closing", listed(&closing)));
    }
    line
}

/// The ports `serve add` opened, in a line.
fn ports_in(summary: &Summary) -> String {
    if summary.serve_ports.is_empty() {
        "None open to the tailnet (`thinkterm tailnet serve add PORT` opens one).".to_string()
    } else {
        format!(
            "Open to the tailnet: {}, each to the same port here.",
            listed(&summary.serve_ports)
        )
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
    let report = thinkterm_tailnet::read_live_report().filter(|_| settings.enabled);
    println!(
        "Ports in:  {}",
        ports_in_line(
            &settings.serve_ports,
            settings.serve_ports_to.as_deref(),
            report.as_ref()
        )
    );
    println!(
        "Control:   {}",
        settings.control_url.as_deref().unwrap_or("Tailscale")
    );
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
    fn ports_are_served_by_space_or_comma_and_never_ssh_or_no_port() {
        let parse = |args: &[&str]| {
            TailnetCommand::try_parse_from(["tailnet", "serve"].iter().chain(args))
        };
        let added = |args: &[&str]| match parse(args).unwrap().sub {
            Sub::Serve {
                action: ServeAction::Add { ports },
            } => ports,
            other => panic!("expected serve add, got {:?}", other),
        };
        assert_eq!(added(&["add", "5900", "5902"]), [5900, 5902]);
        assert_eq!(added(&["add", "5900,5902"]), [5900, 5902]);
        for bad in ["0", "22", "65536", "vnc", "-1"] {
            assert!(parse(&["add", bad]).is_err(), "{}", bad);
        }
        assert!(parse(&["add"]).is_err());
        assert!(parse(&["remove"]).is_err());
        assert!(parse(&["remove", "5900", "--all"]).is_err());
        assert!(matches!(
            parse(&["remove", "--all"]).unwrap().sub,
            Sub::Serve {
                action: ServeAction::Remove { all: true, .. }
            }
        ));
        assert!(parse(&["list"]).is_ok());
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
