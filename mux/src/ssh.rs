use crate::connui::ConnectionUI;
use crate::domain::{alloc_domain_id, Domain, DomainId, DomainState};
use crate::localpane::LocalPane;
use crate::pane::{alloc_pane_id, Pane, PaneId};
use crate::Mux;
use anyhow::{anyhow, bail, Context};
use async_trait::async_trait;
use config::{Shell, SshBackend, SshDomain};
use filedescriptor::{poll, pollfd, socketpair, AsRawSocketDescriptor, FileDescriptor, POLLIN};
use portable_pty::cmdbuilder::CommandBuilder;
use portable_pty::{ChildKiller, ExitStatus, MasterPty, PtySize};
use smol::channel::{bounded, Receiver as AsyncReceiver};
use std::cell::RefCell;
use std::collections::{HashMap, HashSet, VecDeque};
use std::io::{BufWriter, Read, Write};
use std::sync::mpsc::{channel, Receiver, Sender, TryRecvError};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use termwiz::cell::{unicode_column_width, AttributeChange, Intensity};
use termwiz::input::{InputEvent, InputParser};
use termwiz::lineedit::*;
use termwiz::render::terminfo::TerminfoRenderer;
use termwiz::surface::{Change, LineAttribute};
use termwiz::terminal::{ScreenSize, Terminal, TerminalWaker};
use thiserror::Error;
use wezterm_ssh::{
    ConfigMap, HostVerificationFailed, JumpHop, Session, SessionEvent, SshChildProcess, SshPty,
};
use wezterm_term::TerminalSize;

/// The user dismissed an auth prompt instead of answering it. Distinct from a
/// mistyped password or an unreachable host: those deserve another attempt, this
/// one means "not now", so the reconnect loop parks instead of retrying (and,
/// historically, instead of tearing the whole domain down).
#[derive(Error, Debug, Clone, PartialEq, Eq)]
#[error("Authentication was cancelled")]
pub struct AuthCancelledError;

/// The server turned down the password kept for this host, and nobody was
/// asked for another. A further attempt would send the same one, and a
/// server that counts failed sign-ins shuts the address out for them: so,
/// like a dismissed prompt, this parks the reconnect loop.
#[derive(Error, Debug, Clone, PartialEq, Eq)]
#[error("the server refused the stored password: {0}")]
pub struct StoredPasswordRefused(pub String);

#[derive(Default)]
struct PasswordPromptHost {
    history: BasicHistory,
    echo: bool,
}
impl LineEditorHost for PasswordPromptHost {
    fn history(&mut self) -> &mut dyn History {
        &mut self.history
    }

    fn highlight_line(&self, line: &str, cursor_position: usize) -> (Vec<OutputElement>, usize) {
        if self.echo {
            (vec![OutputElement::Text(line.to_string())], cursor_position)
        } else {
            // Rewrite the input so that we can obscure the password
            // characters when output to the terminal widget
            let placeholder = "🔑";
            let grapheme_count = unicode_column_width(line, None);
            let mut output = vec![];
            for _ in 0..grapheme_count {
                output.push(OutputElement::Text(placeholder.to_string()));
            }
            (
                output,
                unicode_column_width(placeholder, None) * cursor_position,
            )
        }
    }
}

pub fn ssh_connect_with_ui(
    ssh_config: wezterm_ssh::ConfigMap,
    ui: &mut ConnectionUI,
) -> anyhow::Result<Session> {
    ssh_connect_with_ui_and_password(ssh_config, ui, StoredPasswords::default())
}

/// The passwords kept for a host and for the jump hosts on its way (see
/// `SshDomain::jump_hosts`), to answer their password prompts with. Each
/// answers only its own host: a password sent to another host is given
/// away to it.
#[derive(Clone, Default, PartialEq, Eq, Hash)]
pub struct StoredPasswords {
    /// The host's own.
    pub host: Option<String>,
    /// The way there they are kept for, all of it (see `way_of`). `None`
    /// where it could not be worked out: no way is the same as that.
    pub way: Option<Vec<String>>,
    /// The jump hosts', in the order of the domain's `jump_hosts`.
    pub jumps: Vec<JumpPassword>,
}

/// A jump host's stored password, with the way it is for: where the jump
/// host at that place on the way resolves to (see `resolved_way`). Asked
/// by any other, it is not given.
#[derive(Clone, Default, PartialEq, Eq, Hash)]
pub struct JumpPassword {
    pub route: String,
    pub password: Option<String>,
}

/// `user@hostname:port` a host's resolved configuration reaches: where a
/// password kept for it would go, whatever its address was written as --
/// HostName and Port, from the ssh config files or set for it, count.
fn resolved_endpoint(config: &ConfigMap) -> String {
    let get = |key: &str| config.get(key).map(String::as_str).unwrap_or("");
    format!("{}@{}:{}", get("user"), get("hostname"), get("port"))
}

/// Where a connection made with `config` goes, and how: the hosts it
/// signs in to, nearest first -- each jump host, then the host it is for
/// -- as `user@hostname:port`, and the command or network the first of
/// them is dialed through. The same name can lead to another machine
/// another way, so passwords kept for one way go on no other. `None`
/// where `config` does not say: a ProxyJump not yet resolved names hosts
/// still to be looked up.
pub fn way_of(config: &ConfigMap) -> Option<Vec<String>> {
    let hops = wezterm_ssh::jump_hosts(config).ok()?;
    if hops.is_empty() && wezterm_ssh::has_jump_hosts(config) {
        return None;
    }
    let dialed = hops.first().unwrap_or(config);
    let get = |key: &str| dialed.get(key).map(String::as_str);
    let mut way = vec![
        format!("network {}", get(wezterm_ssh::NETWORK).unwrap_or("system")),
        format!("proxycommand {}", get("proxycommand").unwrap_or("none")),
    ];
    way.extend(hops.iter().map(resolved_endpoint));
    way.push(resolved_endpoint(config));
    Some(way)
}

/// A domain and the route its passwords were supplied for. Keep them together
/// across retries and when handing a connection to another consumer.
#[derive(Clone, Debug)]
pub struct SshConnectionConfig {
    pub domain: SshDomain,
    pub passwords: StoredPasswords,
}

impl SshConnectionConfig {
    pub fn new(domain: SshDomain) -> Self {
        let passwords = StoredPasswords::of(&domain);
        Self { domain, passwords }
    }

    pub fn resolve_attempt(&self) -> anyhow::Result<(ConfigMap, StoredPasswords)> {
        let config = ssh_domain_to_ssh_config(&self.domain)?;
        let passwords = self.passwords.clone().for_attempt(&config);
        Ok((config, passwords))
    }

    /// An edit supplies only the passwords whose values changed. Keep an
    /// unchanged secret only if it was already authorized for this route.
    pub fn update(&mut self, domain: SshDomain, mut passwords: StoredPasswords) {
        let same_route = self.passwords.same_way(&passwords);
        let previous = |candidate: &String| {
            self.domain.stored_password.as_ref() == Some(candidate)
                || self
                    .domain
                    .jump_hosts
                    .iter()
                    .any(|hop| hop.stored_password.as_ref() == Some(candidate))
        };
        let retain =
            |value: &mut Option<String>, old: Option<&String>, allowed: Option<&String>| {
                if let Some(candidate) = value.as_ref() {
                    if (same_route && Some(candidate) == old && allowed != Some(candidate))
                        || (!same_route && previous(candidate))
                    {
                        *value = None;
                    }
                }
            };
        retain(
            &mut passwords.host,
            self.domain.stored_password.as_ref(),
            self.passwords.host.as_ref(),
        );
        for (i, jump) in passwords.jumps.iter_mut().enumerate() {
            retain(
                &mut jump.password,
                self.domain
                    .jump_hosts
                    .get(i)
                    .and_then(|hop| hop.stored_password.as_ref()),
                self.passwords
                    .jumps
                    .get(i)
                    .and_then(|hop| hop.password.as_ref()),
            );
        }
        *self = Self { domain, passwords };
    }
}

/// A jump host's place among those its domain names, and where it
/// resolves to, as its tag (`wezterm_ssh::JUMP_TAG`) says.
fn read_tag(tag: &str) -> Option<(usize, &str)> {
    let (index, route) = tag.split_once('/')?;
    Some((index.parse().ok()?, route))
}

/// What passwords kept for `dom` are bound to: the way there, as
/// `ssh_domain_to_ssh_config` resolves it now (see `way_of`), and where
/// each jump host `dom` names resolves to (see `resolved_endpoint`), as
/// the tags of its prompts say. A domain that does not resolve is
/// reached no way at all.
fn resolved_way(dom: &SshDomain) -> (Option<Vec<String>>, Vec<String>) {
    let mut routes = vec![String::new(); dom.jump_hosts.len()];
    // Whichever backend dials it, the way is the same.
    let Ok(config) = dialing_config(dom, "") else {
        return (None, routes);
    };
    for hop in wezterm_ssh::jump_hosts(&config).unwrap_or_default() {
        let tag = hop.get(wezterm_ssh::JUMP_TAG).and_then(|tag| read_tag(tag));
        if let Some((index, route)) = tag {
            if let Some(slot) = routes.get_mut(index) {
                *slot = route.to_string();
            }
        }
    }
    (way_of(&config), routes)
}

impl std::fmt::Debug for StoredPasswords {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        let redacted = |password: &Option<String>| password.as_ref().map(|_| "<redacted>");
        f.debug_struct("StoredPasswords")
            .field("host", &redacted(&self.host))
            .field("way", &self.way)
            .field(
                "jumps",
                &self
                    .jumps
                    .iter()
                    .map(|jump| (jump.route.as_str(), redacted(&jump.password)))
                    .collect::<Vec<_>>(),
            )
            .finish()
    }
}

impl StoredPasswords {
    /// Those `dom` keeps.
    pub fn of(dom: &SshDomain) -> Self {
        Self::with_host(dom, dom.stored_password.clone())
    }

    /// Those `dom` keeps for its jump hosts, with `host` for the host.
    pub fn with_host(dom: &SshDomain, host: Option<String>) -> Self {
        let (way, jump_routes) = resolved_way(dom);
        Self {
            host,
            way,
            jumps: dom
                .jump_hosts
                .iter()
                .zip(jump_routes)
                .map(|(jump, route)| JumpPassword {
                    route,
                    password: jump.stored_password.clone(),
                })
                .collect(),
        }
    }

    /// Whether `other` are kept for the same way there, all of it (see
    /// `way_of`), with the same jump hosts named at each place.
    pub fn same_way(&self, other: &StoredPasswords) -> bool {
        self.way.is_some()
            && self.way == other.way
            && self
                .jumps
                .iter()
                .map(|jump| &jump.route)
                .eq(other.jumps.iter().map(|jump| &jump.route))
    }

    /// Takes `new` in place of these where they are kept for the same way
    /// there, and says whether it did: a session given passwords kept for
    /// another host would send them to the one it goes to.
    pub fn replace_if_same_way(&mut self, new: StoredPasswords) -> bool {
        if !self.same_way(&new) {
            return false;
        }
        *self = new;
        true
    }

    /// These, for a connection attempt made with `config`, as it was
    /// resolved for that attempt: all of them where it goes the way they
    /// were kept for, else none. A session reconnects with the ssh config
    /// files as they are then, and a name in them may lead to another
    /// host by then, or through another.
    pub fn for_attempt(self, config: &ConfigMap) -> Self {
        if self.way.is_some() && self.way == way_of(config) {
            return self;
        }
        if self.host.is_some() || self.jumps.iter().any(|jump| jump.password.is_some()) {
            log::warn!(
                "not answering any prompt with a stored password: {} is reached \
                 another way than when they were given",
                resolved_endpoint(config)
            );
        }
        Self::default()
    }

    /// The one kept for whoever asks: the host itself, or `jump`. A jump
    /// host the domain does not name itself (one the ssh config put on the
    /// way) has none, and nor does one at a place on the way where these
    /// were kept for another: passwords and a way there that came apart --
    /// one set up anew while a session went on the old way -- give none.
    pub fn for_prompt(&self, jump: Option<&JumpHop>) -> Option<&str> {
        match jump {
            None => self.host.as_deref(),
            Some(hop) => {
                let (index, route) = read_tag(hop.tag.as_deref()?)?;
                let kept = self.jumps.get(index)?;
                if kept.route.is_empty() || kept.route != route {
                    return None;
                }
                kept.password.as_deref()
            }
        }
    }
}

/// Who a prompt is from, to keep count of what each host was sent: `None`
/// for the host itself, else the jump host's place on the way.
fn asker(jump: Option<&JumpHop>) -> Option<usize> {
    jump.map(|hop| hop.index)
}

/// `user@host:port` of each jump host `ssh_config` goes through.
fn jump_host_names(ssh_config: &ConfigMap) -> Vec<String> {
    wezterm_ssh::jump_hosts(ssh_config)
        .unwrap_or_default()
        .iter()
        .map(|hop| {
            let get = |key: &str| hop.get(key).map(String::as_str).unwrap_or("");
            format!("{}@{}:{}", get("user"), get("hostname"), get("port"))
        })
        .collect()
}

/// How long one wait for the server may last while connecting: for the
/// handshake, the host key and each authentication reply. sshd drops a login
/// that is not done in 120s (its default LoginGraceTime), so no working server
/// is given up on sooner than it would give up itself, and push-style second
/// factors, which can keep the server waiting for a minute, still fit.
const SERVER_SILENCE_LIMIT: Duration = Duration::from_secs(120);

/// Like [`ssh_connect_with_ui`], but auto-answers password (non-echo) auth
/// prompts with a stored password, if any: each host's with its own (see
/// [`StoredPasswords`]), and only where `ssh_config` goes the way they
/// were kept for (see [`StoredPasswords::for_attempt`]). At most twice for
/// each: servers commonly issue a
/// second prompt (e.g. `password` then `keyboard-interactive`, or a retry),
/// and a single-shot answer left the second one to an interactive prompt
/// that a headless reconnect cannot serve. A wrong stored password still
/// falls back to interactive entry after those attempts, where the server
/// asks again; where it ends the sign-in instead, the error is
/// [`StoredPasswordRefused`].
pub fn ssh_connect_with_ui_and_password(
    ssh_config: wezterm_ssh::ConfigMap,
    ui: &mut ConnectionUI,
    passwords: StoredPasswords,
) -> anyhow::Result<Session> {
    ssh_connect_with_silence_limit(ssh_config, ui, passwords, SERVER_SILENCE_LIMIT)
}

fn ssh_connect_with_silence_limit(
    ssh_config: wezterm_ssh::ConfigMap,
    ui: &mut ConnectionUI,
    passwords: StoredPasswords,
    silence_limit: Duration,
) -> anyhow::Result<Session> {
    let passwords = passwords.for_attempt(&ssh_config);
    let cloned_ui = ui.clone();
    cloned_ui.run_and_log_error(move || {
        // How many prompts each host's stored password has answered.
        let mut stored_answers: HashMap<Option<usize>, usize> = HashMap::new();
        // Whose answers each host has had: the stored password's, the
        // user's, or both.
        let mut stored_password_sent: HashSet<Option<usize>> = HashSet::new();
        let mut user_was_asked: HashSet<Option<usize>> = HashSet::new();
        // The host signing in: the last one to ask anything.
        let mut asking: Option<usize> = None;
        let remote_address = ssh_config
            .get("hostname")
            .expect("ssh config to always set hostname");
        let through = jump_host_names(&ssh_config);
        if through.is_empty() {
            ui.output_str(&format!("Connecting to {} using SSH\n", remote_address));
        } else {
            ui.output_str(&format!(
                "Connecting to {} through {} using SSH\n",
                remote_address,
                through.join(", then ")
            ));
        }
        let (session, events) = Session::connect(ssh_config.clone())?;

        loop {
            // Each wait gets the whole limit: time spent on a prompt below is
            // the user's, not the server's.
            let Some(event) = smol::block_on(smol::future::or(
                async { Some(events.recv().await) },
                async {
                    smol::Timer::after(silence_limit).await;
                    None
                },
            )) else {
                // A connection that went half-open mid-login never answers,
                // and a reconnect waiting on it would never try again.
                // Shutting the socket down wakes the backend blocked on it.
                session.shutdown();
                bail!("no answer from {remote_address} for {silence_limit:?} while connecting");
            };
            let Ok(event) = event else {
                break;
            };
            match event {
                SessionEvent::Banner(banner) => {
                    if let Some(banner) = banner {
                        ui.output_str(&format!("{}\n", banner));
                    }
                }
                SessionEvent::HostVerify(verify) => {
                    if let Some(hop) = &verify.jump {
                        ui.output_str(&format!(
                            "Jump host {hop}, on the way to {remote_address}:\n"
                        ));
                    }
                    ui.output_str(&format!("{}\n", verify.message));
                    let answer = ui.input("Enter [y/n]> ");
                    let ok = matches!(answer.as_deref(), Ok("y" | "Y" | "yes" | "YES"));
                    // Answer first either way, so the ssh session is never
                    // left waiting on us.
                    smol::block_on(verify.answer(ok)).context("send verify response")?;
                    // An explicit "no" is as final as dismissing the prompt:
                    // without the cancellation marker the refusal surfaces as
                    // an ordinary error, the reconnect loop retries, and the
                    // same trust question pops right back up.
                    if !ok {
                        return Err(AuthCancelledError.into());
                    }
                }
                SessionEvent::Authenticate(auth) => {
                    let who = asker(auth.jump.as_ref());
                    asking = who;
                    if let Some(hop) = &auth.jump {
                        ui.output_str(&format!(
                            "Jump host {hop}, on the way to {remote_address}:\n"
                        ));
                    }
                    if !auth.username.is_empty() {
                        ui.output_str(&format!("Authentication for {}\n", auth.username));
                    }
                    if !auth.instructions.is_empty() {
                        ui.output_str(&format!("{}\n", auth.instructions));
                    }
                    let stored = passwords.for_prompt(auth.jump.as_ref());
                    let mut answers = vec![];
                    for prompt in &auth.prompts {
                        let mut prompt_lines = prompt.prompt.split('\n').collect::<Vec<_>>();
                        let editor_prompt = prompt_lines.pop().unwrap();
                        for line in &prompt_lines {
                            ui.output_str(&format!("{}\n", line));
                        }
                        let answered = stored_answers.entry(who).or_default();
                        let res = if prompt.echo {
                            user_was_asked.insert(who);
                            ui.input(editor_prompt)
                        } else if let Some(stored) = stored.filter(|_| *answered < 2) {
                            *answered += 1;
                            stored_password_sent.insert(who);
                            ui.output_str(&format!("{editor_prompt} (using stored password)\n"));
                            Ok(stored.to_string())
                        } else {
                            user_was_asked.insert(who);
                            ui.password(editor_prompt)
                        };
                        if let Ok(line) = res {
                            answers.push(line);
                        } else {
                            return Err(AuthCancelledError.into());
                        }
                    }
                    smol::block_on(auth.answer(answers))?;
                }
                SessionEvent::HostVerificationFailed(failed) => {
                    let message = format_host_verification_for_terminal(&failed);
                    // Nothing is read back here, so a lazily-materialized UI
                    // would stay silent and swallow the banner — and a changed
                    // host key is the one thing the user must not miss.
                    ui.materialize();
                    ui.output(message);
                    return Err(failed.into());
                }
                SessionEvent::Error(err) => {
                    anyhow::bail!("Error: {}", err);
                }
                SessionEvent::AuthenticationRefused(err) => {
                    return Err(refused_sign_in(
                        err,
                        stored_password_sent.contains(&asking),
                        user_was_asked.contains(&asking),
                    ));
                }
                SessionEvent::Authenticated => return Ok(session),
            }
        }
        bail!("unable to authenticate session");
    })
}

/// The error for a sign-in the server refused. What the user typed may be
/// typed again, better; what is stored would only be sent again as it is.
fn refused_sign_in(
    message: String,
    stored_password_sent: bool,
    user_was_asked: bool,
) -> anyhow::Error {
    if stored_password_sent && !user_was_asked {
        StoredPasswordRefused(message).into()
    } else {
        anyhow::anyhow!("Error: {}", message)
    }
}

fn format_host_verification_for_terminal(failed: &HostVerificationFailed) -> Vec<Change> {
    vec![
        AttributeChange::Intensity(Intensity::Bold).into(),
        LineAttribute::DoubleHeightTopHalfLine.into(),
        Change::Text("REMOTE HOST IDENTIFICATION CHANGED\r\n".to_string()),
        LineAttribute::DoubleHeightBottomHalfLine.into(),
        Change::Text("REMOTE HOST IDENTIFICATION CHANGED\r\n".to_string()),
        Change::Text("SOMEONE MAY BE DOING SOMETHING NASTY!\r\n".to_string()),
        AttributeChange::Intensity(Intensity::Normal).into(),
        Change::Text("\r\nThere are two likely causes for this:\r\n".to_string()),
        Change::Text(
            " 1. Someone is eavesdropping right now (man-in-the-middle attack)\r\n".to_string(),
        ),
        Change::Text(" 2. The host key may have been changed by the administrator\r\n".to_string()),
        Change::Text("\r\n".to_string()),
        AttributeChange::Intensity(Intensity::Bold).into(),
        Change::Text(
            "Please contact your system administrator to discuss how to proceed!\r\n".to_string(),
        ),
        AttributeChange::Intensity(Intensity::Normal).into(),
        Change::Text("\r\n".to_string()),
        match failed.file.as_ref() {
            Some(file) => Change::Text(format!(
                "The host is {}, and its fingerprint is\r\n{}\r\n\
                If the administrator confirms that the key has changed, you can\r\n\
                fix this for yourself by removing the offending entry from\r\n\
                {} and then try connecting again.\r\n",
                failed.remote_address,
                failed.key,
                file.display(),
            )),
            None => Change::Text(format!(
                "The host is {}, and its fingerprint is\r\n{}\r\n",
                failed.remote_address, failed.key
            )),
        },
    ]
}

/// Represents a connection to remote host via ssh.
/// The domain is created with the ssh config prior to making the
/// connection.  The connection is established by the first spawn()
/// call.
/// In order to show the authentication dialog inline in that spawned
/// pane, we play some tricks with wrapped versions of the pty, child
/// and the reader and writer instances so that we can inject the
/// interactive setup.  The bulk of that is driven by `connect_ssh_session`.
/// Coarse lifecycle of the SSH connection backing a [`RemoteSshDomain`], so the
/// GUI can show a "Connecting…" state and detect unreachable hosts instead of
/// staring at a blank pane. `Authenticating` means the TCP connect + handshake
/// succeeded and we are talking to the SSH daemon (any interactive prompts now
/// render in the real pane).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SshConnectionStatus {
    Connecting,
    Authenticating,
    Connected,
    Failed(String),
}

pub struct RemoteSshDomain {
    session: Mutex<Option<Session>>,
    connection: SshConnectionConfig,
    id: DomainId,
    name: String,
    /// Shared, updated by the connection thread so the GUI can poll progress.
    connection_status: Arc<Mutex<SshConnectionStatus>>,
}

/// What ssh connects to `ssh_dom`'s host with: what the ssh config files
/// say for it, with `ssh_dom`'s settings over that, and the jump hosts it
/// is reached through (see `wezterm_ssh::jump_hosts`): those `ssh_dom`
/// names, else those its `proxyjump` option does.
pub fn ssh_domain_to_ssh_config(ssh_dom: &SshDomain) -> anyhow::Result<ConfigMap> {
    let backend = match ssh_dom
        .ssh_backend
        .unwrap_or_else(|| config::configuration().ssh_backend)
    {
        SshBackend::Ssh2 => "ssh2",
        SshBackend::LibSsh => "libssh",
    };
    let config = dialing_config(ssh_dom, backend)?;
    if let Some("true") = config.get("wezterm_ssh_verbose").map(|s| s.as_str()) {
        log::info!("Using ssh config: {config:#?}");
    }
    Ok(config)
}

/// [`ssh_domain_to_ssh_config`], with `backend` to connect with.
fn dialing_config(ssh_dom: &SshDomain, backend: &str) -> anyhow::Result<ConfigMap> {
    let mut ssh_config = wezterm_ssh::Config::new();
    ssh_config.add_default_config_files();

    let mut config = host_ssh_config(
        &ssh_config,
        &ssh_dom.remote_address,
        ssh_dom.username.as_deref(),
        &ssh_dom.ssh_option,
        backend,
    )?;
    if ssh_dom.no_agent_auth {
        config.insert("identitiesonly".to_string(), "yes".to_string());
    }

    if ssh_dom.jump_hosts.is_empty() {
        ssh_config.resolve_jump_hosts(&mut config)?;
    } else {
        // The domain's own way there; what the ssh config says for the
        // host does not count.
        config.remove("proxyjump");
        config.remove("proxycommand");
        let mut chain = vec![];
        for (index, jump) in ssh_dom.jump_hosts.iter().enumerate() {
            let mut hop = host_ssh_config(
                &ssh_config,
                &jump.remote_address,
                jump.username.as_deref(),
                &jump.ssh_option,
                backend,
            )?;
            // Its place, and where it resolves to: a password is given to
            // its prompts only where it was kept for that same endpoint.
            hop.insert(
                wezterm_ssh::JUMP_TAG.to_string(),
                format!("{index}/{}", resolved_endpoint(&hop)),
            );
            if index == 0 {
                // The first is reached as its own configuration says,
                // through any jump hosts of its own.
                ssh_config.resolve_jump_hosts(&mut hop)?;
                chain.extend(wezterm_ssh::take_jump_hosts(&mut hop)?);
            }
            chain.push(hop);
        }
        if chain.len() > wezterm_ssh::MAX_JUMP_HOSTS {
            bail!(
                "{} is reached through more than {} jump hosts",
                ssh_dom.remote_address,
                wezterm_ssh::MAX_JUMP_HOSTS
            );
        }
        wezterm_ssh::set_jump_hosts(&mut config, &chain);
    }
    Ok(config)
}

/// The host and port of `host`, `host:port`, `[v6]` or `[v6]:port`; an
/// IPv6 address written without brackets has no port.
fn split_remote_address(address: &str) -> anyhow::Result<(&str, Option<u16>)> {
    if let Some(rest) = address.strip_prefix('[') {
        let (host, after) = rest
            .split_once(']')
            .ok_or_else(|| anyhow!("{address:?} has an unclosed '['"))?;
        return match after {
            "" => Ok((host, None)),
            after => {
                let port = after
                    .strip_prefix(':')
                    .ok_or_else(|| anyhow!("{address:?} has more than a port after ']'"))?;
                Ok((host, Some(port.parse::<u16>()?)))
            }
        };
    }
    match address.split_once(':') {
        Some((host, port)) if !port.contains(':') => Ok((host, Some(port.parse::<u16>()?))),
        _ => Ok((address, None)),
    }
}

/// One host's ssh configuration: what the ssh config files say for it,
/// with what is set here over that.
fn host_ssh_config(
    ssh_config: &wezterm_ssh::Config,
    remote_address: &str,
    username: Option<&str>,
    ssh_option: &HashMap<String, String>,
    backend: &str,
) -> anyhow::Result<ConfigMap> {
    let (remote_host_name, port) = split_remote_address(remote_address)?;

    // The user and port given here are in place before the options that
    // name them (`%p` in a ProxyCommand) are expanded, as on ssh's command
    // line; they are put in again below, over `ssh_option`.
    let mut resolver = ssh_config.clone();
    if let Some(username) = username {
        resolver.set_option("user", username);
    }
    if let Some(port) = port {
        resolver.set_option("port", port.to_string());
    }
    let mut config = resolver.for_host(remote_host_name);
    config.insert("wezterm_ssh_backend".to_string(), backend.to_string());
    for (k, v) in ssh_option {
        config.insert(k.to_string(), v.to_string());
    }
    // Of the two ways to say how a host is reached, one set here wins
    // over the other from the ssh config files.
    let set_here = |key: &str| ssh_option.contains_key(key);
    if set_here("proxycommand") && !set_here("proxyjump") {
        config.remove("proxyjump");
    } else if set_here("proxyjump") && !set_here("proxycommand") {
        config.remove("proxycommand");
    }

    if let Some(username) = username {
        config.insert("user".to_string(), username.to_string());
    }
    if let Some(port) = port {
        config.insert("port".to_string(), port.to_string());
    }
    Ok(config)
}

/// Parse the `ID=` value out of `/etc/os-release` contents (e.g. "ubuntu").
pub fn parse_os_release_id(text: &str) -> Option<String> {
    for line in text.lines() {
        if let Some(rest) = line.trim().strip_prefix("ID=") {
            let value = rest.trim().trim_matches('"').to_string();
            if !value.is_empty() {
                return Some(value);
            }
        }
    }
    None
}

impl RemoteSshDomain {
    pub fn with_ssh_domain(dom: &SshDomain) -> anyhow::Result<Self> {
        Self::with_ssh_domain_and_password(dom, None)
    }

    /// Like [`Self::with_ssh_domain`] but stores a password used to
    /// auto-answer the first interactive password prompt (ThinkTerm host
    /// manager). The password is held in memory only on this domain.
    pub fn with_ssh_domain_and_password(
        dom: &SshDomain,
        password: Option<String>,
    ) -> anyhow::Result<Self> {
        let mut domain = dom.clone();
        domain.stored_password = password;
        Ok(Self::with_connection(SshConnectionConfig::new(domain)))
    }

    pub fn with_connection(connection: SshConnectionConfig) -> Self {
        Self {
            id: alloc_domain_id(),
            name: connection.domain.name.clone(),
            session: Mutex::new(None),
            connection,
            connection_status: Arc::new(Mutex::new(SshConnectionStatus::Connecting)),
        }
    }

    /// What this domain dials: the host, its settings and its jump hosts.
    pub fn ssh_domain(&self) -> &SshDomain {
        &self.connection.domain
    }

    pub fn ssh_connection_config(&self) -> &SshConnectionConfig {
        &self.connection
    }

    /// Current connection lifecycle status; the GUI polls this while a
    /// "Connecting…" view is shown.
    pub fn connection_status(&self) -> SshConnectionStatus {
        self.connection_status.lock().unwrap().clone()
    }

    fn set_connection_status(&self, status: SshConnectionStatus) {
        *self.connection_status.lock().unwrap() = status;
    }

    /// Run a one-shot, read-only `cat /etc/os-release` over the already
    /// established session and return the parsed distro `ID` (e.g. "ubuntu").
    /// Returns `None` on any error (no session, command failure, etc).
    pub async fn detect_os_release(&self) -> Option<String> {
        let session = self.session.lock().unwrap().as_ref().cloned()?;
        let exec = session
            .exec("cat /etc/os-release 2>/dev/null", None)
            .await
            .ok()?;
        let mut stdout = exec.stdout;
        // Read on a blocking thread so we don't stall the async executor.
        let text = smol::unblock(move || {
            let mut buf = String::new();
            let _ = stdout.read_to_string(&mut buf);
            buf
        })
        .await;
        parse_os_release_id(&text)
    }

    /// Install (`true`) or remove the shell integration that lets a tab show
    /// what runs on this host (`crate::shell_integration`). Waits while the
    /// session is still connecting: a password or a host key may be waiting
    /// on the user.
    pub async fn set_up_shell_integration(
        &self,
        install: bool,
    ) -> anyhow::Result<crate::shell_integration::ShellIntegrationOutcome> {
        let deadline = Instant::now() + Duration::from_secs(300);
        let session = loop {
            match self.connection_status() {
                SshConnectionStatus::Connected => {
                    if let Some(session) = self.session.lock().unwrap().as_ref().cloned() {
                        break session;
                    }
                }
                SshConnectionStatus::Failed(message) => {
                    return Err(crate::shell_integration::NotConnected(message).into());
                }
                SshConnectionStatus::Connecting | SshConnectionStatus::Authenticating => {}
            }
            if Instant::now() >= deadline {
                let waited = "still connecting after five minutes".to_string();
                return Err(crate::shell_integration::NotConnected(waited).into());
            }
            smol::Timer::after(Duration::from_millis(500)).await;
        };
        crate::shell_integration::set_up(&session, install).await
    }

    pub fn ssh_config(&self) -> anyhow::Result<ConfigMap> {
        ssh_domain_to_ssh_config(self.ssh_domain())
    }

    fn prepare_connection_attempt(&self) -> anyhow::Result<(ConfigMap, StoredPasswords)> {
        let attempt = self
            .connection
            .resolve_attempt()
            .context("obtain ssh config")?;
        self.set_connection_status(SshConnectionStatus::Connecting);
        Ok(attempt)
    }

    fn build_command(
        &self,
        pane_id: PaneId,
        command: Option<CommandBuilder>,
        command_dir: Option<String>,
    ) -> anyhow::Result<(Option<String>, HashMap<String, String>)> {
        let config = config::configuration();
        let cmd = match command {
            Some(mut cmd) => {
                config.apply_cmd_defaults(&mut cmd, self.ssh_domain().default_prog.as_ref(), None);
                cmd
            }
            None => config.build_prog(None, self.ssh_domain().default_prog.as_ref(), None)?,
        };
        let mut env: HashMap<String, String> = cmd
            .iter_extra_env_as_str()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();

        // FIXME: this isn't useful without a way to talk to the remote mux.
        // One option is to forward the mux via unix domain, another is to
        // embed the mux protocol in an escape sequence and just use the
        // existing terminal connection
        env.insert("WEZTERM_REMOTE_PANE".to_string(), pane_id.to_string());

        fn build_env_command(
            dir: Option<String>,
            cmd: &CommandBuilder,
            env: &HashMap<String, String>,
        ) -> anyhow::Result<String> {
            // "Soft" chdir: if it doesn't exist then it doesn't matter
            let cd_cmd = if let Some(dir) = dir {
                format!("cd {};", shell_words::quote(&dir))
            } else if let Some(dir) = cmd.get_cwd() {
                let dir = dir.to_str().context("converting cwd to string")?;
                format!("cd {};", shell_words::quote(&dir))
            } else {
                String::new()
            };

            let mut env_cmd = vec!["env".to_string()];

            for (k, v) in env {
                env_cmd.push(format!("{}={}", k, v));
            }

            let cmd = if cmd.is_default_prog() {
                // We'd like to spawn a login shell, but since we are invoking env
                // we end up in a regular shell.
                // This guff tries to find a reasonably portable way to execute
                // the shell as a login shell.
                // Per: <https://unix.stackexchange.com/a/666850/123914>
                // the most portable way is to use perl, but in case perl is not
                // installed, zsh, bash and ksh all support `exec -a`.
                // Other shells may support `exec -a` but there isn't a simple
                // way to test for them, so we assume that if we have one of those
                // three that we can use it, otherwise we fall back to just running
                // the shell directly.
                let login_shell = "command -v perl > /dev/null && \
                  exec perl -e 'use File::Basename; $shell = basename($ENV{SHELL}); exec {$ENV{SHELL}} \"-$shell\"'; \
                  case \"$SHELL\" in */zsh|*/bash|*/ksh ) exec -a \"-$(basename $SHELL)\" $SHELL ;; esac ; \
                  exec $SHELL";

                format!("$SHELL -c {}", shell_words::quote(login_shell))
            } else {
                cmd.as_unix_command_line()?
            };

            Ok(cd_cmd + &shell_words::join(env_cmd) + " " + &cmd)
        }

        let command_line = match (
            cmd.is_default_prog(),
            self.ssh_domain().assume_shell,
            command_dir,
        ) {
            (_, Shell::Posix, dir) => Some(build_env_command(dir, &cmd, &env)?),
            (true, _, _) => None,
            (false, _, _) => Some(cmd.as_unix_command_line()?),
        };

        Ok((command_line, env))
    }

    async fn start_new_session(
        &self,
        command_line: Option<String>,
        env: HashMap<String, String>,
        size: TerminalSize,
    ) -> anyhow::Result<StartNewSessionResult> {
        let (ssh_config, passwords) = self.prepare_connection_attempt()?;
        let (session, events) = match Session::connect(ssh_config).context("connect to ssh server")
        {
            Ok(result) => result,
            Err(err) => {
                self.set_connection_status(SshConnectionStatus::Failed(format!("{:#}", err)));
                return Err(err);
            }
        };
        self.session.lock().unwrap().replace(session.clone());
        let status = Arc::clone(&self.connection_status);

        // We get to establish the session!
        //
        // Since we want spawn to return the Pane in which
        // we'll carry out interactive auth, we generate
        // some shim/wrapper versions of the pty, child
        // and reader/writer.

        let (stdout_read, stdout_write) = socketpair()?;
        let (reader_tx, reader_rx) = channel();
        let (stdin_read, stdin_write) = socketpair()?;
        let (writer_tx, writer_rx) = channel();

        let pty_reader = PtyReader {
            reader: Box::new(stdout_read),
            rx: reader_rx,
        };

        let pty_writer = PtyWriter {
            writer: Box::new(stdin_write),
            rx: writer_rx,
        };
        let writer = Box::new(pty_writer);

        let (child_tx, child_rx) = channel();

        let child = Box::new(WrappedSshChild {
            status: None,
            rx: child_rx,
            exited: None,
            killer: WrappedSshChildKiller {
                inner: Arc::new(Mutex::new(KillerInner {
                    killer: None,
                    pending_kill: false,
                })),
            },
        });

        let (pty_tx, pty_rx) = channel();

        let size = Arc::new(Mutex::new(size));

        let pty = Box::new(WrappedSshPty {
            inner: RefCell::new(WrappedSshPtyInner::Connecting {
                size: Arc::clone(&size),
                reader: Some(pty_reader),
                connected: pty_rx,
            }),
        });

        // And with those created, we can now spawn a new thread
        // to perform the blocking (from its perspective) terminal
        // UI to carry out any authentication.
        let mut stdout_write = BufWriter::new(stdout_write);
        let thread_status = Arc::clone(&status);
        std::thread::spawn(move || {
            if let Err(err) = connect_ssh_session(
                session,
                events,
                stdin_read,
                writer_tx,
                &mut stdout_write,
                reader_tx,
                child_tx,
                pty_tx,
                size,
                command_line,
                env,
                passwords,
                Arc::clone(&thread_status),
            ) {
                *thread_status.lock().unwrap() = SshConnectionStatus::Failed(format!("{:#}", err));
                let _ = write!(stdout_write, "{:#}", err);
                log::error!("Failed to connect ssh: {:#}", err);
            }
            let _ = stdout_write.flush();
        });

        Ok(StartNewSessionResult { pty, child, writer })
    }
}

struct StartNewSessionResult {
    pty: Box<dyn portable_pty::MasterPty + Send>,
    child: Box<dyn portable_pty::Child + Send>,
    writer: BoxedWriter,
}

/// Carry out the authentication process and create the initial pty.
fn connect_ssh_session(
    session: Session,
    events: smol::channel::Receiver<SessionEvent>,
    mut stdin_read: FileDescriptor,
    stdin_tx: Sender<BoxedWriter>,
    stdout_write: &mut BufWriter<FileDescriptor>,
    stdout_tx: Sender<BoxedReader>,
    child_tx: Sender<SshChildProcess>,
    pty_tx: Sender<SshPty>,
    size: Arc<Mutex<TerminalSize>>,
    command_line: Option<String>,
    env: HashMap<String, String>,
    passwords: StoredPasswords,
    status: Arc<Mutex<SshConnectionStatus>>,
) -> anyhow::Result<()> {
    // Whose stored password has been sent: each host's goes once (see
    // below). `None` is the host itself, else a jump host's place.
    let mut stored_sent: HashSet<Option<usize>> = HashSet::new();
    let set_status = |next: SshConnectionStatus| {
        *status.lock().unwrap() = next;
    };
    struct StdoutShim<'a> {
        size: Arc<Mutex<TerminalSize>>,
        stdout: &'a mut BufWriter<FileDescriptor>,
    }

    impl<'a> Write for StdoutShim<'a> {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.stdout.write(buf)
        }
        fn flush(&mut self) -> std::io::Result<()> {
            self.stdout.flush()
        }
    }

    impl<'a> termwiz::render::RenderTty for StdoutShim<'a> {
        fn get_size_in_cells(&mut self) -> termwiz::Result<(usize, usize)> {
            let size = *self.size.lock().unwrap();
            Ok((size.cols as _, size.rows as _))
        }
    }

    /// a termwiz Terminal for use with the line editor
    struct TerminalShim<'a> {
        stdout: &'a mut StdoutShim<'a>,
        stdin: &'a mut FileDescriptor,
        size: Arc<Mutex<TerminalSize>>,
        renderer: TerminfoRenderer,
        parser: InputParser,
        input_queue: VecDeque<InputEvent>,
    }

    impl<'a> termwiz::terminal::Terminal for TerminalShim<'a> {
        fn set_raw_mode(&mut self) -> termwiz::Result<()> {
            use termwiz::escape::csi::{DecPrivateMode, DecPrivateModeCode, Mode, CSI};

            macro_rules! decset {
                ($variant:ident) => {
                    write!(
                        self.stdout,
                        "{}",
                        CSI::Mode(Mode::SetDecPrivateMode(DecPrivateMode::Code(
                            DecPrivateModeCode::$variant
                        )))
                    )?;
                };
            }

            decset!(BracketedPaste);
            self.flush()?;

            Ok(())
        }

        fn flush(&mut self) -> termwiz::Result<()> {
            self.stdout.flush()?;
            Ok(())
        }

        fn set_cooked_mode(&mut self) -> termwiz::Result<()> {
            Ok(())
        }

        fn enter_alternate_screen(&mut self) -> termwiz::Result<()> {
            termwiz::bail!("TerminalShim has no alt screen");
        }

        fn exit_alternate_screen(&mut self) -> termwiz::Result<()> {
            termwiz::bail!("TerminalShim has no alt screen");
        }

        fn get_screen_size(&mut self) -> termwiz::Result<ScreenSize> {
            let size = *self.size.lock().unwrap();
            Ok(ScreenSize {
                cols: size.cols as _,
                rows: size.rows as _,
                xpixel: size.pixel_width as _,
                ypixel: size.pixel_height as _,
            })
        }

        fn set_screen_size(&mut self, _size: ScreenSize) -> termwiz::Result<()> {
            termwiz::bail!("TerminalShim cannot set screen size");
        }

        fn render(&mut self, changes: &[Change]) -> termwiz::Result<()> {
            self.renderer.render_to(changes, self.stdout)?;
            Ok(())
        }

        fn poll_input(&mut self, wait: Option<Duration>) -> termwiz::Result<Option<InputEvent>> {
            if let Some(event) = self.input_queue.pop_front() {
                return Ok(Some(event));
            }

            let deadline = wait.map(|d| Instant::now() + d);
            let starting_size = *self.size.lock().unwrap();

            self.stdin.set_non_blocking(true)?;

            loop {
                if let Some(deadline) = deadline.as_ref() {
                    if Instant::now() >= *deadline {
                        return Ok(None);
                    }
                }
                let mut pfd = [pollfd {
                    fd: self.stdin.as_socket_descriptor(),
                    events: POLLIN,
                    revents: 0,
                }];

                if let Ok(1) = poll(&mut pfd, Some(Duration::from_millis(200))) {
                    let mut buf = [0u8; 64];
                    let n = self.stdin.read(&mut buf)?;
                    let input_queue = &mut self.input_queue;
                    self.parser
                        .parse(&buf[0..n], |evt| input_queue.push_back(evt), n == buf.len());
                    return Ok(self.input_queue.pop_front());
                } else {
                    let size = *self.size.lock().unwrap();
                    if starting_size != size {
                        return Ok(Some(InputEvent::Resized {
                            cols: size.cols as usize,
                            rows: size.rows as usize,
                        }));
                    }
                }
            }
        }

        fn waker(&self) -> TerminalWaker {
            // TODO: TerminalWaker assumes that we're a SystemTerminal but that
            // isn't the case here.
            panic!("TerminalShim::waker called!?");
        }
    }

    let renderer = termwiz_funcs::new_wezterm_terminfo_renderer();
    let mut shim = TerminalShim {
        stdout: &mut StdoutShim {
            stdout: stdout_write,
            size: Arc::clone(&size),
        },
        size: Arc::clone(&size),
        renderer,
        stdin: &mut stdin_read,
        parser: InputParser::new(),
        input_queue: VecDeque::new(),
    };

    impl<'a> TerminalShim<'a> {
        fn output_line(&mut self, s: &str) -> termwiz::Result<()> {
            let mut s = s.replace("\n", "\r\n");
            s.push_str("\r\n");
            self.render(&[Change::Text(s)])
        }
    }

    // Process authentication related events
    while let Ok(event) = smol::block_on(events.recv()) {
        match event {
            SessionEvent::Banner(banner) => {
                if let Some(banner) = banner {
                    shim.output_line(&banner)?;
                }
            }
            SessionEvent::HostVerify(verify) => {
                // Handshake succeeded; we're now interacting with the daemon.
                set_status(SshConnectionStatus::Authenticating);
                if let Some(hop) = &verify.jump {
                    shim.output_line(&format!("Jump host {hop}:"))?;
                }
                shim.output_line(&verify.message)?;
                let mut editor = LineEditor::new(&mut shim);
                let mut host = PasswordPromptHost::default();
                host.echo = true;
                editor.set_prompt("Enter [y/n]> ");
                let ok = if let Some(line) = editor.read_line(&mut host)? {
                    match line.as_ref() {
                        "y" | "Y" | "yes" | "YES" => true,
                        "n" | "N" | "no" | "NO" | _ => false,
                    }
                } else {
                    false
                };
                smol::block_on(verify.answer(ok)).context("send verify response")?;
            }
            SessionEvent::Authenticate(auth) => {
                set_status(SshConnectionStatus::Authenticating);
                let who = asker(auth.jump.as_ref());
                if let Some(hop) = &auth.jump {
                    shim.output_line(&format!("Jump host {hop}:"))?;
                }
                if !auth.username.is_empty() {
                    shim.output_line(&format!("Authentication for {}", auth.username))?;
                }
                if !auth.instructions.is_empty() {
                    shim.output_line(&auth.instructions)?;
                }
                let mut answers = vec![];
                for prompt in &auth.prompts {
                    let mut prompt_lines = prompt.prompt.split('\n').collect::<Vec<_>>();
                    let editor_prompt = prompt_lines.pop().unwrap();
                    for line in &prompt_lines {
                        shim.output_line(line)?;
                    }
                    // Auto-answer the host's first password (non-echo)
                    // prompt with its stored password, if any. Only once: a
                    // wrong stored password then falls back to interactive
                    // entry so the user can correct it instead of looping.
                    if !prompt.echo && !stored_sent.contains(&who) {
                        if let Some(stored) = passwords.for_prompt(auth.jump.as_ref()) {
                            stored_sent.insert(who);
                            shim.output_line(editor_prompt)?;
                            answers.push(stored.to_string());
                            continue;
                        }
                    }
                    let mut editor = LineEditor::new(&mut shim);
                    let mut host = PasswordPromptHost::default();
                    editor.set_prompt(editor_prompt);
                    host.echo = prompt.echo;
                    if let Some(line) = editor.read_line(&mut host)? {
                        answers.push(line);
                    } else {
                        anyhow::bail!("Authentication was cancelled");
                    }
                }
                smol::block_on(auth.answer(answers))?;
            }
            SessionEvent::Error(err) | SessionEvent::AuthenticationRefused(err) => {
                set_status(SshConnectionStatus::Failed(err.clone()));
                shim.output_line(&format!("Error: {}", err))?;
            }
            SessionEvent::HostVerificationFailed(failed) => {
                set_status(SshConnectionStatus::Failed(failed.to_string()));
                let message = format_host_verification_for_terminal(&failed);
                shim.render(&message)?;
            }
            SessionEvent::Authenticated => {
                set_status(SshConnectionStatus::Connected);
                // Our session has been authenticated: we can now
                // set up the real pty for the pane
                match smol::block_on(session.request_pty(
                    &config::configuration().term,
                    crate::terminal_size_to_pty_size(*size.lock().unwrap())?,
                    command_line.as_ref().map(|s| s.as_str()),
                    Some(env),
                )) {
                    Err(err) => {
                        shim.output_line(&format!("Failed to spawn command: {:#}", err))?;
                        break;
                    }
                    Ok((pty, child)) => {
                        drop(shim);

                        // Obtain the real stdin/stdout for the pty
                        let reader = pty.try_clone_reader()?;
                        let writer = pty.take_writer()?;

                        // And send them to the wrapped reader/writer
                        stdin_tx
                            .send(Box::new(writer))
                            .map_err(|e| anyhow!("{:#}", e))?;
                        stdout_tx
                            .send(Box::new(reader))
                            .map_err(|e| anyhow!("{:#}", e))?;

                        // Likewise, send the real pty and child to
                        // the wrappers
                        pty_tx.send(pty)?;
                        child_tx.send(child)?;

                        // Now when we return, our stdin_read and
                        // stdout_write will close and that will cause
                        // the PtyReader and PtyWriter to recv the
                        // the new reader/writer above and continue.
                        //
                        // The pty and child will be picked up when
                        // they are next polled or resized.

                        return Ok(());
                    }
                }
            }
        }
    }

    Ok(())
}

#[async_trait(?Send)]
impl Domain for RemoteSshDomain {
    async fn spawn_pane(
        &self,
        size: TerminalSize,
        command: Option<CommandBuilder>,
        command_dir: Option<String>,
    ) -> anyhow::Result<Arc<dyn Pane>> {
        let pane_id = alloc_pane_id();

        let (command_line, env) = self
            .build_command(pane_id, command, command_dir)
            .context("build_command")?;

        // This needs to be separate from the if let block below in order
        // for the lock to be released at the appropriate time
        let mut session: Option<Session> = self.session.lock().unwrap().as_ref().cloned();

        let StartNewSessionResult { pty, child, writer } = if let Some(session) = session.take() {
            match session
                .request_pty(
                    &config::configuration().term,
                    crate::terminal_size_to_pty_size(size)
                        .context("compute pty size from terminal size")?,
                    command_line.as_ref().map(|s| s.as_str()),
                    Some(env.clone()),
                )
                .await
                .context("request ssh pty")
            {
                Ok((concrete_pty, concrete_child)) => {
                    let pty = Box::new(concrete_pty);
                    let child = Box::new(concrete_child);
                    let writer = Box::new(pty.take_writer().context("take writer from pty")?);

                    // Reusing a session that was already authenticated.
                    self.set_connection_status(SshConnectionStatus::Connected);
                    StartNewSessionResult { pty, child, writer }
                }
                Err(err) => {
                    if err
                        .root_cause()
                        .downcast_ref::<wezterm_ssh::DeadSession>()
                        .is_some()
                    {
                        // Session died (perhaps they closed the initial tab?)
                        // So we'll try making a new one
                        self.start_new_session(command_line, env, size).await?
                    } else {
                        log::error!("{err:#?}");
                        return Err(err);
                    }
                }
            }
        } else {
            self.start_new_session(command_line, env, size).await?
        };

        // Wrap up the pty etc. in a LocalPane.  That allows for
        // eg: tmux integration to be tunnelled via the remote
        // session without duplicating a lot of logic over here.

        let terminal = wezterm_term::Terminal::new(
            size,
            std::sync::Arc::new(config::TermConfig::new()),
            config::TERM_PROGRAM,
            config::wezterm_version(),
            writer,
        );
        let pane_writer = terminal.writer_handle();

        let pane: Arc<dyn Pane> = Arc::new(LocalPane::new(
            pane_id,
            terminal,
            child,
            pty,
            pane_writer,
            self.id,
            "RemoteSshDomain".to_string(),
        ));
        let mux = Mux::get();
        mux.add_pane(&pane)?;

        Ok(pane)
    }

    fn domain_id(&self) -> DomainId {
        self.id
    }

    fn domain_name(&self) -> &str {
        &self.name
    }

    async fn attach(&self, _window_id: Option<crate::WindowId>) -> anyhow::Result<()> {
        Ok(())
    }

    fn detachable(&self) -> bool {
        false
    }

    fn detach(&self) -> anyhow::Result<()> {
        bail!("detach not implemented for RemoteSshDomain");
    }

    fn state(&self) -> DomainState {
        // Just pretend that we are always attached, as we don't
        // have a defined attach operation that is distinct from
        // a spawn.
        DomainState::Attached
    }
}

#[derive(Debug)]
struct KillerInner {
    killer: Option<Box<dyn ChildKiller + Send + Sync>>,
    /// If we haven't populated `killer` by the time someone has called
    /// `kill`, then we use this to remember to kill as soon as we recv
    /// the child process.
    pending_kill: bool,
}

#[derive(Debug, Clone)]
struct WrappedSshChildKiller {
    inner: Arc<Mutex<KillerInner>>,
}

#[derive(Debug)]
pub(crate) struct WrappedSshChild {
    status: Option<AsyncReceiver<ExitStatus>>,
    rx: Receiver<SshChildProcess>,
    exited: Option<ExitStatus>,
    killer: WrappedSshChildKiller,
}

impl WrappedSshChild {
    fn check_connected(&mut self) {
        if self.status.is_none() {
            match self.rx.try_recv() {
                Ok(c) => {
                    self.got_child(c);
                }
                Err(TryRecvError::Empty) => {}
                Err(err) => {
                    log::debug!("WrappedSshChild::check_connected err: {:#?}", err);
                    self.exited.replace(ExitStatus::with_exit_code(1));
                }
            }
        }
    }

    fn got_child(&mut self, mut child: SshChildProcess) {
        {
            let mut killer = self.killer.inner.lock().unwrap();
            killer.killer.replace(child.clone_killer());
            if killer.pending_kill {
                let _ = child.kill().ok();
            }
        }

        let (tx, rx) = bounded(1);
        promise::spawn::spawn_into_main_thread(async move {
            if let Ok(status) = child.async_wait().await {
                tx.send(status).await.ok();
                let mux = Mux::get();
                mux.prune_dead_windows();
            }
        })
        .detach();
        self.status.replace(rx);
    }
}

impl portable_pty::Child for WrappedSshChild {
    fn try_wait(&mut self) -> std::io::Result<Option<ExitStatus>> {
        if let Some(status) = self.exited.as_ref() {
            return Ok(Some(status.clone()));
        }

        self.check_connected();

        if let Some(rx) = self.status.as_mut() {
            match rx.try_recv() {
                Ok(status) => {
                    self.exited.replace(status.clone());
                    Ok(Some(status))
                }
                Err(smol::channel::TryRecvError::Empty) => Ok(None),
                Err(err) => {
                    log::debug!("WrappedSshChild::try_wait err: {:#?}", err);
                    let status = ExitStatus::with_exit_code(1);
                    self.exited.replace(status.clone());
                    Ok(Some(status))
                }
            }
        } else {
            Ok(None)
        }
    }

    fn wait(&mut self) -> std::io::Result<portable_pty::ExitStatus> {
        if let Some(status) = self.exited.as_ref() {
            return Ok(status.clone());
        }

        if self.status.is_none() {
            match smol::block_on(async { self.rx.recv() }) {
                Ok(c) => {
                    self.got_child(c);
                }
                Err(err) => {
                    log::debug!("WrappedSshChild err: {:#?}", err);
                    let status = ExitStatus::with_exit_code(1);
                    self.exited.replace(status.clone());
                    return Ok(status);
                }
            }
        }

        let rx = self.status.as_mut().unwrap();
        match smol::block_on(rx.recv()) {
            Ok(status) => {
                self.exited.replace(status.clone());
                Ok(status)
            }
            Err(err) => {
                log::error!("WrappedSshChild err: {:#?}", err);
                let status = ExitStatus::with_exit_code(1);
                self.exited.replace(status.clone());
                Ok(status)
            }
        }
    }

    fn process_id(&self) -> Option<u32> {
        None
    }

    #[cfg(windows)]
    fn as_raw_handle(&self) -> Option<std::os::windows::io::RawHandle> {
        None
    }
}

impl ChildKiller for WrappedSshChild {
    fn kill(&mut self) -> std::io::Result<()> {
        let mut killer = self.killer.inner.lock().unwrap();
        if let Some(killer) = killer.killer.as_mut() {
            killer.kill()
        } else {
            killer.pending_kill = true;
            Ok(())
        }
    }

    fn clone_killer(&self) -> Box<dyn ChildKiller + Send + Sync> {
        Box::new(self.killer.clone())
    }
}

impl ChildKiller for WrappedSshChildKiller {
    fn kill(&mut self) -> std::io::Result<()> {
        let mut killer = self.inner.lock().unwrap();
        if let Some(killer) = killer.killer.as_mut() {
            killer.kill()
        } else {
            killer.pending_kill = true;
            Ok(())
        }
    }

    fn clone_killer(&self) -> Box<dyn ChildKiller + Send + Sync> {
        Box::new(self.clone())
    }
}

type BoxedReader = Box<dyn Read + Send + 'static>;
type BoxedWriter = Box<dyn Write + Send + 'static>;

pub(crate) struct WrappedSshPty {
    inner: RefCell<WrappedSshPtyInner>,
}

impl WrappedSshPty {
    pub fn is_connecting(&mut self) -> bool {
        self.inner.borrow_mut().is_connecting()
    }
}

enum WrappedSshPtyInner {
    Connecting {
        reader: Option<PtyReader>,
        connected: Receiver<SshPty>,
        size: Arc<Mutex<TerminalSize>>,
    },
    Connected {
        reader: Option<PtyReader>,
        pty: SshPty,
    },
}

struct PtyReader {
    reader: BoxedReader,
    rx: Receiver<BoxedReader>,
}

struct PtyWriter {
    writer: BoxedWriter,
    rx: Receiver<BoxedWriter>,
}

impl WrappedSshPtyInner {
    fn check_connected(&mut self) -> anyhow::Result<()> {
        match self {
            Self::Connecting {
                reader,
                connected,
                size,
                ..
            } => {
                if let Ok(pty) = connected.try_recv() {
                    let res = pty.resize(crate::terminal_size_to_pty_size(*size.lock().unwrap())?);
                    *self = Self::Connected {
                        pty,
                        reader: reader.take(),
                    };
                    res
                } else {
                    Ok(())
                }
            }
            _ => Ok(()),
        }
    }

    fn is_connecting(&mut self) -> bool {
        self.check_connected().ok();
        match self {
            Self::Connecting { .. } => true,
            Self::Connected { .. } => false,
        }
    }
}

impl portable_pty::MasterPty for WrappedSshPty {
    fn resize(&self, new_size: PtySize) -> anyhow::Result<()> {
        let mut inner = self.inner.borrow_mut();
        match &mut *inner {
            WrappedSshPtyInner::Connecting { ref mut size, .. } => {
                {
                    let mut size = size.lock().unwrap();
                    size.cols = new_size.cols as usize;
                    size.rows = new_size.rows as usize;
                    size.pixel_height = new_size.pixel_height as usize;
                    size.pixel_width = new_size.pixel_width as usize;
                }
                inner.check_connected()
            }
            WrappedSshPtyInner::Connected { pty, .. } => pty.resize(new_size),
        }
    }

    fn get_size(&self) -> anyhow::Result<PtySize> {
        let mut inner = self.inner.borrow_mut();
        match &*inner {
            WrappedSshPtyInner::Connecting { size, .. } => {
                let size = crate::terminal_size_to_pty_size(*size.lock().unwrap())?;
                inner.check_connected()?;
                Ok(size)
            }
            WrappedSshPtyInner::Connected { pty, .. } => pty.get_size(),
        }
    }

    fn try_clone_reader(&self) -> anyhow::Result<Box<dyn Read + Send + 'static>> {
        let mut inner = self.inner.borrow_mut();
        inner.check_connected()?;
        match &mut *inner {
            WrappedSshPtyInner::Connected { ref mut reader, .. }
            | WrappedSshPtyInner::Connecting { ref mut reader, .. } => match reader.take() {
                Some(r) => Ok(Box::new(r)),
                None => anyhow::bail!("reader already taken"),
            },
        }
    }

    fn take_writer(&self) -> anyhow::Result<Box<dyn Write + Send + 'static>> {
        anyhow::bail!("writer must be created during bootstrap");
    }

    #[cfg(unix)]
    fn process_group_leader(&self) -> Option<i32> {
        let mut inner = self.inner.borrow_mut();
        let _ = inner.check_connected();
        None
    }

    #[cfg(unix)]
    fn as_raw_fd(&self) -> Option<std::os::fd::RawFd> {
        None
    }

    #[cfg(unix)]
    fn tty_name(&self) -> Option<std::path::PathBuf> {
        None
    }
}

impl std::io::Write for PtyWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        // Check for a new writer first: on Windows, the socket
        // will let us successfully write a byte to a disconnected
        // socket and we won't discover the issue until we write
        // the next byte.
        // <https://github.com/wezterm/wezterm/issues/771>
        if let Ok(writer) = self.rx.try_recv() {
            self.writer = writer;
        }
        self.writer.write(buf)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        match self.writer.flush() {
            Ok(_) => Ok(()),
            res => match self.rx.recv() {
                Ok(writer) => {
                    self.writer = writer;
                    self.writer.flush()
                }
                _ => res,
            },
        }
    }
}

impl std::io::Read for PtyReader {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        match self.reader.read(buf) {
            Ok(len) if len > 0 => Ok(len),
            res => match self.rx.recv() {
                Ok(reader) => {
                    self.reader = reader;
                    self.reader.read(buf)
                }
                _ => res,
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prepare_connection_attempt_resets_failed_status() {
        let dom = SshDomain {
            name: "test-ssh".to_string(),
            remote_address: "example.invalid:22".to_string(),
            ..SshDomain::default()
        };
        let domain = RemoteSshDomain::with_ssh_domain(&dom).unwrap();

        domain.set_connection_status(SshConnectionStatus::Failed("old failure".to_string()));
        domain.prepare_connection_attempt().unwrap();

        assert_eq!(domain.connection_status(), SshConnectionStatus::Connecting);
    }

    #[test]
    fn editing_one_password_cannot_rebind_another() {
        let mut raw = SshDomain {
            remote_address: "private.invalid".into(),
            stored_password: Some("original host".into()),
            jump_hosts: vec![config::SshJumpHost {
                remote_address: "relay.invalid".into(),
                stored_password: Some("original hop".into()),
                ..Default::default()
            }],
            ..Default::default()
        };
        let mut connection = SshConnectionConfig::new(raw.clone());
        raw.jump_hosts[0].remote_address = "relay.invalid:2200".into();
        connection.update(raw.clone(), StoredPasswords::of(&raw));
        assert!(
            connection.passwords.host.is_none() && connection.passwords.jumps[0].password.is_none()
        );
        raw.jump_hosts[0].stored_password = Some("new hop".into());
        connection.update(raw.clone(), StoredPasswords::of(&raw));
        let (_, passwords) = connection.resolve_attempt().unwrap();
        assert_eq!(passwords.host, None);
        assert_eq!(passwords.jumps[0].password.as_deref(), Some("new hop"));
        raw.stored_password = Some("new host".into());
        connection.update(raw.clone(), StoredPasswords::of(&raw));
        assert_eq!(connection.passwords.host.as_deref(), Some("new host"));
        assert_eq!(
            connection.passwords.jumps[0].password.as_deref(),
            Some("new hop")
        );
    }

    #[test]
    fn plain_ssh_keeps_the_password_binding_across_session_attempts() {
        let mut domain = RemoteSshDomain::with_ssh_domain_and_password(
            &SshDomain {
                remote_address: "private.invalid:2222".into(),
                jump_hosts: vec![config::SshJumpHost {
                    remote_address: "relay.invalid".into(),
                    stored_password: Some("jump secret".into()),
                    ..Default::default()
                }],
                ..Default::default()
            },
            Some("host secret".into()),
        )
        .unwrap();
        let (_, before) = domain.prepare_connection_attempt().unwrap();
        assert_eq!(before.for_prompt(None), Some("host secret"));
        assert!(before.jumps[0].password.is_some());

        // Simulate the alias resolving to another server for the next session.
        domain
            .connection
            .domain
            .ssh_option
            .insert("hostname".into(), "other.invalid".into());
        let (config, after) = domain.prepare_connection_attempt().unwrap();
        assert_eq!(config["hostname"], "other.invalid");
        assert_eq!(after, StoredPasswords::default());
    }

    #[test]
    fn only_a_stored_password_sent_alone_is_refused_for_good() {
        let refused = |stored, asked| {
            refused_sign_in("password auth status: Denied".to_string(), stored, asked)
        };
        assert_eq!(
            refused(true, false).downcast_ref::<StoredPasswordRefused>(),
            Some(&StoredPasswordRefused(
                "password auth status: Denied".to_string()
            ))
        );
        // Typed by the user, wholly or in part: they can be asked again.
        for (stored, asked) in [(false, true), (true, true), (false, false)] {
            let err = refused(stored, asked);
            assert!(err.downcast_ref::<StoredPasswordRefused>().is_none());
            assert_eq!(err.to_string(), "Error: password auth status: Denied");
        }
    }

    #[test]
    fn stored_passwords_answer_only_their_own_host() {
        let dom = SshDomain {
            remote_address: "a.invalid".to_string(),
            stored_password: Some("host secret".to_string()),
            jump_hosts: vec![
                config::SshJumpHost {
                    remote_address: "b.invalid".to_string(),
                    stored_password: Some("jump secret".to_string()),
                    ..Default::default()
                },
                config::SshJumpHost {
                    remote_address: "c.invalid".to_string(),
                    ..Default::default()
                },
            ],
            ..Default::default()
        };
        let passwords = StoredPasswords::of(&dom);
        let hop = |index: usize, tag: Option<String>| JumpHop {
            index,
            name: "someone@somewhere:22".to_string(),
            tag,
        };
        let tag = |index: usize| Some(format!("{index}/{}", resolved_way(&dom).1[index]));
        assert_eq!(passwords.for_prompt(None), Some("host secret"));
        assert_eq!(
            passwords.for_prompt(Some(&hop(0, tag(0)))),
            Some("jump secret")
        );
        assert_eq!(passwords.for_prompt(Some(&hop(1, tag(1)))), None);
        // A jump host the ssh config put on the way has none, least of all
        // the host's own.
        assert_eq!(passwords.for_prompt(Some(&hop(0, None))), None);
        assert!(!format!("{passwords:?}").contains("secret"));

        // A session still going the old way -- through another host at the
        // same place -- is given nothing kept for the new way.
        let mut moved = dom.clone();
        moved.jump_hosts[0].remote_address = "x.invalid".to_string();
        moved.jump_hosts[0].stored_password = Some("x secret".to_string());
        let new_way = StoredPasswords::of(&moved);
        assert_eq!(new_way.for_prompt(Some(&hop(0, tag(0)))), None);
        let moved_tag = Some(format!("0/{}", resolved_way(&moved).1[0]));
        assert_eq!(
            new_way.for_prompt(Some(&hop(0, moved_tag))),
            Some("x secret")
        );
    }

    #[test]
    fn passwords_are_bound_to_where_the_options_lead() {
        let base = SshDomain {
            name: "same label".to_string(),
            remote_address: "c.invalid".to_string(),
            username: Some("me".to_string()),
            stored_password: Some("host secret".to_string()),
            jump_hosts: vec![config::SshJumpHost {
                remote_address: "b.invalid".to_string(),
                username: Some("me".to_string()),
                stored_password: Some("jump secret".to_string()),
                ..Default::default()
            }],
            ..Default::default()
        };
        let held = StoredPasswords::of(&base);
        assert!(held.same_way(&StoredPasswords::of(&base)));
        let changes: [(&str, fn(&mut SshDomain)); 7] = [
            ("the host's HostName", |dom| {
                dom.ssh_option
                    .insert("hostname".into(), "elsewhere.invalid".into());
            }),
            ("the host's Port", |dom| {
                dom.ssh_option.insert("port".into(), "2200".into());
            }),
            ("a jump host's HostName", |dom| {
                dom.jump_hosts[0]
                    .ssh_option
                    .insert("hostname".into(), "elsewhere.invalid".into());
            }),
            ("a jump host's Port", |dom| {
                dom.jump_hosts[0]
                    .ssh_option
                    .insert("port".into(), "2200".into());
            }),
            ("the jump hosts the first is reached through", |dom| {
                dom.jump_hosts[0]
                    .ssh_option
                    .insert("proxyjump".into(), "me@gateway.invalid".into());
            }),
            ("the command the first jump host is reached by", |dom| {
                dom.jump_hosts[0]
                    .ssh_option
                    .insert("proxycommand".into(), "nc gateway.invalid %p".into());
            }),
            ("the network the first jump host is reached over", |dom| {
                dom.jump_hosts[0]
                    .ssh_option
                    .insert(wezterm_ssh::NETWORK.into(), "tailnet".into());
            }),
        ];
        for (what, change) in changes {
            let mut moved = base.clone();
            change(&mut moved);
            assert!(
                !held.same_way(&StoredPasswords::of(&moved)),
                "passwords kept before {} changed went the new way",
                what
            );
        }
    }

    #[test]
    fn passwords_are_bound_to_the_way_the_options_say_too() {
        // A host reached through jump hosts its own options name, or by a
        // command: none of them named among its jump hosts.
        let reached = |option: &str, value: &str| SshDomain {
            name: "same label".to_string(),
            remote_address: "c.invalid".to_string(),
            username: Some("me".to_string()),
            ssh_option: HashMap::from([(option.to_string(), value.to_string())]),
            stored_password: Some("host secret".to_string()),
            ..Default::default()
        };
        let moves = [
            ("proxyjump", "me@gateway-a.invalid", "me@gateway-b.invalid"),
            (
                "proxyjump",
                "me@gateway-a.invalid",
                "me@gateway-a.invalid:2200",
            ),
            (
                "proxyjump",
                "me@gateway-a.invalid",
                "me@gateway-a.invalid,me@inner.invalid",
            ),
            (
                "proxycommand",
                "nc gateway-a.invalid %p",
                "nc gateway-b.invalid %p",
            ),
            (wezterm_ssh::NETWORK, "system", "tailnet"),
        ];
        for (option, before, after) in moves {
            let held = StoredPasswords::of(&reached(option, before));
            assert!(held.way.is_some());
            assert!(held.same_way(&StoredPasswords::of(&reached(option, before))));
            let mut taken = held.clone();
            assert!(
                !taken.replace_if_same_way(StoredPasswords::of(&reached(option, after))),
                "passwords kept for {} {} were taken for {}",
                option,
                before,
                after
            );
            assert_eq!(taken, held);
        }
    }

    #[test]
    fn an_attempt_that_goes_another_way_is_given_no_password() {
        let dom = SshDomain {
            remote_address: "c.invalid".to_string(),
            username: Some("me".to_string()),
            ssh_backend: Some(SshBackend::LibSsh),
            stored_password: Some("host secret".to_string()),
            jump_hosts: vec![config::SshJumpHost {
                remote_address: "b.invalid".to_string(),
                username: Some("me".to_string()),
                stored_password: Some("jump secret".to_string()),
                ..Default::default()
            }],
            ..Default::default()
        };
        let kept = StoredPasswords::of(&dom);
        let attempt = ssh_domain_to_ssh_config(&dom).unwrap();
        let jump_prompt = JumpHop {
            index: 0,
            name: "me@b.invalid:22".to_string(),
            tag: wezterm_ssh::jump_hosts(&attempt).unwrap()[0]
                .get(wezterm_ssh::JUMP_TAG)
                .cloned(),
        };
        // Going the way they were kept for, it is given them all.
        let given = kept.clone().for_attempt(&attempt);
        assert_eq!(given, kept);
        assert_eq!(given.for_prompt(None), Some("host secret"));
        assert_eq!(given.for_prompt(Some(&jump_prompt)), Some("jump secret"));

        // The ssh config files, read anew for a reconnect, have the same
        // names lead somewhere else, or another way.
        fn hops(config: &mut ConfigMap, change: impl FnOnce(&mut Vec<ConfigMap>)) {
            let mut hops = wezterm_ssh::jump_hosts(config).unwrap();
            change(&mut hops);
            wezterm_ssh::set_jump_hosts(config, &hops);
        }
        let moves: [(&str, fn(&mut ConfigMap)); 7] = [
            ("the host's HostName", |config| {
                config.insert("hostname".into(), "elsewhere.invalid".into());
            }),
            ("the host's User", |config| {
                config.insert("user".into(), "someone-else".into());
            }),
            ("the host's Port", |config| {
                config.insert("port".into(), "2200".into());
            }),
            ("the jump host's HostName", |config| {
                hops(config, |hops| {
                    hops[0].insert("hostname".into(), "elsewhere.invalid".into());
                })
            }),
            ("a jump host put before it", |config| {
                hops(config, |hops| {
                    let mut gateway = hops[0].clone();
                    gateway.remove(wezterm_ssh::JUMP_TAG);
                    gateway.insert("hostname".into(), "gateway.invalid".into());
                    hops.insert(0, gateway);
                })
            }),
            ("the command the first is reached by", |config| {
                hops(config, |hops| {
                    hops[0].insert("proxycommand".into(), "nc gateway.invalid %p".into());
                })
            }),
            ("the network the first is reached over", |config| {
                hops(config, |hops| {
                    hops[0].insert(wezterm_ssh::NETWORK.into(), "tailnet".into());
                })
            }),
        ];
        for (what, change) in moves {
            let mut moved = attempt.clone();
            change(&mut moved);
            let given = kept.clone().for_attempt(&moved);
            assert_eq!(given.for_prompt(None), None, "{what} moved");
            assert_eq!(given.for_prompt(Some(&jump_prompt)), None, "{what} moved");
        }

        // Reached directly, by a command or over a network of its own.
        let direct = SshDomain {
            jump_hosts: vec![],
            ssh_option: HashMap::from([(
                "proxycommand".to_string(),
                "nc gateway-a.invalid %p".to_string(),
            )]),
            ..dom.clone()
        };
        let kept = StoredPasswords::of(&direct);
        let attempt = ssh_domain_to_ssh_config(&direct).unwrap();
        assert_eq!(
            kept.clone().for_attempt(&attempt).for_prompt(None),
            Some("host secret")
        );
        for (option, value) in [
            ("proxycommand", "nc gateway-b.invalid %p"),
            (wezterm_ssh::NETWORK, "tailnet"),
            // A ProxyJump not resolved names hosts still to be looked up.
            ("proxyjump", "me@gateway.invalid"),
        ] {
            let mut moved = attempt.clone();
            moved.insert(option.to_string(), value.to_string());
            let given = kept.clone().for_attempt(&moved);
            assert_eq!(given.for_prompt(None), None, "{option} {value}");
        }

        // Kept for a way that could not be worked out, they go no way.
        let nowhere = StoredPasswords {
            way: None,
            ..kept.clone()
        };
        assert!(!nowhere.same_way(&nowhere.clone()));
        assert_eq!(nowhere.for_attempt(&attempt).for_prompt(None), None);
    }

    #[test]
    fn passwords_kept_for_another_host_are_not_taken() {
        let dom = |address: &str, password: &str| SshDomain {
            name: "same label".to_string(),
            remote_address: address.to_string(),
            stored_password: Some(password.to_string()),
            ..Default::default()
        };
        let mut held = StoredPasswords::of(&dom("old.invalid", "old secret"));
        // The host moved under the same name, and was given a password: the
        // session still reaching the old one is not given it.
        let moved = StoredPasswords::of(&dom("new.invalid", "new secret"));
        assert!(!held.same_way(&moved));
        assert!(!held.replace_if_same_way(moved));
        assert_eq!(held.for_prompt(None), Some("old secret"));
        // Its own host given another password: that one is taken.
        assert!(held.replace_if_same_way(StoredPasswords::of(&dom("old.invalid", "newer"))));
        assert_eq!(held.for_prompt(None), Some("newer"));
    }

    #[test]
    fn a_domain_reached_through_jump_hosts_carries_them_in_its_config() {
        let dom = SshDomain {
            name: "c".to_string(),
            remote_address: "c.invalid:2200".to_string(),
            username: Some("cuser".to_string()),
            ssh_backend: Some(SshBackend::LibSsh),
            ssh_option: HashMap::from([("proxycommand".to_string(), "nc %h %p".to_string())]),
            stored_password: Some("host secret".to_string()),
            jump_hosts: vec![
                config::SshJumpHost {
                    remote_address: "b.invalid:2222".to_string(),
                    username: Some("buser".to_string()),
                    ssh_option: HashMap::from([(
                        "identityfile".to_string(),
                        "/keys/b".to_string(),
                    )]),
                    stored_password: Some("jump secret".to_string()),
                },
                config::SshJumpHost {
                    remote_address: "d.invalid".to_string(),
                    ..Default::default()
                },
            ],
            ..Default::default()
        };
        let config = ssh_domain_to_ssh_config(&dom).unwrap();
        assert_eq!(config["hostname"], "c.invalid");
        assert_eq!(config["port"], "2200");
        assert_eq!(config["user"], "cuser");
        // Reached through its jump hosts, not by a command of its own.
        assert!(!config.contains_key("proxycommand"));

        let hops = wezterm_ssh::jump_hosts(&config).unwrap();
        assert_eq!(hops.len(), 2);
        assert_eq!(hops[0]["hostname"], "b.invalid");
        assert_eq!(hops[0]["port"], "2222");
        assert_eq!(hops[0]["user"], "buser");
        assert_eq!(hops[0]["identityfile"], "/keys/b");
        assert_eq!(hops[0][wezterm_ssh::JUMP_TAG], "0/buser@b.invalid:2222");
        assert_eq!(
            format!("0/{}", resolved_way(&dom).1[0]),
            hops[0][wezterm_ssh::JUMP_TAG],
            "a prompt's tag and the password kept for it name one endpoint"
        );
        assert_eq!(hops[1]["hostname"], "d.invalid");
        assert!(hops[1][wezterm_ssh::JUMP_TAG].starts_with("1/"));
        assert!(hops[1][wezterm_ssh::JUMP_TAG].ends_with("@d.invalid:22"));
        assert_eq!(hops[1]["wezterm_ssh_backend"], "libssh");

        // The config gets logged; the passwords stay out of it.
        assert!(!config.values().any(|value| value.contains("secret")));
    }

    #[test]
    fn remote_addresses_split_as_written_ipv6_too() {
        assert_eq!(split_remote_address("host").unwrap(), ("host", None));
        assert_eq!(
            split_remote_address("host:2222").unwrap(),
            ("host", Some(2222))
        );
        assert_eq!(
            split_remote_address("[2001:db8::7]:2222").unwrap(),
            ("2001:db8::7", Some(2222))
        );
        assert_eq!(
            split_remote_address("[2001:db8::7]").unwrap(),
            ("2001:db8::7", None)
        );
        assert_eq!(
            split_remote_address("2001:db8::7").unwrap(),
            ("2001:db8::7", None)
        );
        assert!(split_remote_address("[2001:db8::7").is_err());
        assert!(split_remote_address("host:port").is_err());
    }

    #[test]
    fn a_proxy_command_reaches_the_port_the_domain_names() {
        let mut config = wezterm_ssh::Config::new();
        config.add_config_string("Host c.invalid\n    ProxyCommand nc %h %p\n");
        let host = host_ssh_config(
            &config,
            "c.invalid:2200",
            Some("me"),
            &HashMap::new(),
            "libssh",
        )
        .unwrap();
        assert_eq!(host["proxycommand"], "nc c.invalid 2200");
        assert_eq!(host["port"], "2200");
        assert_eq!(host["user"], "me");
    }

    #[test]
    fn a_way_there_set_for_the_domain_beats_the_other_from_ssh_config() {
        let mut config = wezterm_ssh::Config::new();
        config.add_config_string("Host c.invalid\n    ProxyJump b.invalid\n");
        let options = HashMap::from([("proxycommand".to_string(), "nc %h %p".to_string())]);
        let with_command = host_ssh_config(&config, "c.invalid", None, &options, "libssh").unwrap();
        assert_eq!(with_command["proxycommand"], "nc %h %p");
        assert!(!with_command.contains_key("proxyjump"));

        let mut config = wezterm_ssh::Config::new();
        config.add_config_string("Host c.invalid\n    ProxyCommand nc %h %p\n");
        let options = HashMap::from([("proxyjump".to_string(), "b.invalid".to_string())]);
        let with_jump = host_ssh_config(&config, "c.invalid", None, &options, "libssh").unwrap();
        assert_eq!(with_jump["proxyjump"], "b.invalid");
        assert!(!with_jump.contains_key("proxycommand"));
    }

    #[test]
    fn a_server_that_stops_answering_ends_the_attempt() {
        // Accepts and then says nothing. libssh gives up on a handshake by
        // itself after 10s, so ending well inside that shows the limit did
        // it: the same limit that ends a login whose answer never comes,
        // which libssh would wait for forever.
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(10)))
                .unwrap();
            let mut buf = [0u8; 256];
            // True once the client has let go of the connection.
            loop {
                match stream.read(&mut buf) {
                    Ok(0) => return true,
                    Ok(_) => continue,
                    Err(err) => {
                        return !matches!(
                            err.kind(),
                            std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                        )
                    }
                }
            }
        });

        let mut ssh_config = ConfigMap::new();
        ssh_config.insert("hostname".to_string(), "127.0.0.1".to_string());
        ssh_config.insert("port".to_string(), port.to_string());
        ssh_config.insert("user".to_string(), "user".to_string());
        let mut ui = ConnectionUI::new_headless();

        let started = Instant::now();
        let err = match ssh_connect_with_silence_limit(
            ssh_config,
            &mut ui,
            StoredPasswords::default(),
            Duration::from_millis(500),
        ) {
            Ok(_) => panic!("a server that never answers cannot have let us in"),
            Err(err) => err,
        };
        assert!(started.elapsed() < Duration::from_secs(5));
        assert!(
            format!("{:#}", err).contains("no answer from 127.0.0.1"),
            "{:#}",
            err
        );
        assert!(server.join().unwrap(), "the connection was never closed");
    }
}
