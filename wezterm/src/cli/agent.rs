//! `thinkterm cli agent …`: query agent statuses from the mux.

use crate::cli::CliOutputFormatKind;
use clap::Parser;
use std::io::Write;
use std::time::{SystemTime, UNIX_EPOCH};
use tabout::{tabulate_output, Alignment, Column};
use thinkterm_proto::{AgentEvidence, AgentState};
use wezterm_client::client::Client;

#[derive(Debug, Parser, Clone)]
pub struct AgentCommand {
    #[command(subcommand)]
    sub: AgentSubCommand,
}

#[derive(Debug, Parser, Clone)]
enum AgentSubCommand {
    /// List panes running detected coding agents
    #[command(name = "list")]
    List(AgentListCommand),

    /// Tell a coding agent starting in this pane where it is: its pane and
    /// the others in its tab. The session-start hook `thinkterm skill
    /// setup` installs runs this; outside a pane it prints nothing.
    #[command(name = "context")]
    Context(AgentContextCommand),

    /// Talk with the coding agent in a pane as a chat: one JSON object a
    /// line, ops on stdin and what happens on stdout. ThinkTerm's phone
    /// app runs this on the machine the pane is on.
    #[command(name = "chat", hide = true)]
    Chat(AgentChatCommand),
}

#[derive(Debug, Parser, Clone)]
struct AgentContextCommand {}

#[derive(Debug, Parser, Clone)]
struct AgentChatCommand {
    /// The pane whose agent to talk with.
    #[arg(long)]
    pane: mux::pane::PaneId,
}

#[derive(Debug, Parser, Clone)]
struct AgentListCommand {
    /// Controls the output format.
    /// "table" and "json" are possible formats.
    #[arg(long = "format", default_value = "table")]
    format: CliOutputFormatKind,

    /// The same as `--format json`.
    #[arg(long, conflicts_with = "format")]
    json: bool,
}

impl AgentCommand {
    pub async fn run(&self, client: Client) -> anyhow::Result<()> {
        match &self.sub {
            AgentSubCommand::List(cmd) => cmd.run(client).await,
            AgentSubCommand::Context(cmd) => cmd.run(client).await,
            AgentSubCommand::Chat(cmd) => cmd.run(client).await,
        }
    }
}

impl AgentChatCommand {
    #[cfg(unix)]
    async fn run(&self, client: Client) -> anyhow::Result<()> {
        use thinkterm_agent_chat::host::{Out, Target};
        use thinkterm_agent_chat::Need;

        let out = Out::stdout();
        let statuses = client.get_agent_statuses().await?;
        let Some(agent) = statuses
            .statuses
            .iter()
            .find(|entry| entry.pane_id == self.pane)
            .map(|entry| entry.status.agent_id.clone())
        else {
            out.error(Some(Need::Unsupported), format!("no agent found in pane {}", self.pane))?;
            return Ok(());
        };
        // An agent the pane reaches over ssh runs on another machine: what
        // the bridge can reach is this one's.
        let programs = client.get_foreground_programs().await?;
        if let Some(remote) = programs
            .programs
            .iter()
            .find(|entry| entry.pane_id == self.pane)
            .map(|entry| entry.program.executable.as_str())
            .filter(|program| REMOTE_SHELLS.contains(program))
        {
            out.error(Some(Need::NotHere), format!("pane {} runs {remote}", self.pane))?;
            return Ok(());
        }
        let listed = client.list_panes().await?;
        let mut working_dir = None;
        for tabroot in listed.tabs {
            let mut cursor = tabroot.into_tree().cursor();
            loop {
                if let Some(stack) = cursor.leaf_mut() {
                    if let Some(entry) = stack.panes.iter().find(|entry| entry.pane_id == self.pane) {
                        working_dir = entry.working_dir.as_ref().map(|dir| dir.url.clone());
                    }
                }
                match cursor.preorder_next() {
                    Ok(c) => cursor = c,
                    Err(_) => break,
                }
            }
        }
        if let Some(host) = working_dir.as_ref().and_then(|url| url.host_str()).filter(|host| !this_machine(host)) {
            out.error(Some(Need::NotHere), format!("pane {} is on {host}", self.pane))?;
            return Ok(());
        }
        let cwd = working_dir.as_ref().and_then(local_path);
        // The mux the pane belongs to, which its programs know it by.
        let mux_socket = client.unix_socket_path().map(|path| path.to_string_lossy().into_owned());
        let target = Target {
            pane: self.pane as u64,
            agent,
            cwd,
            mux_socket,
        };
        // The agent is talked with directly; the mux is kept for its pane,
        // which the bridge may type into (to have the agent take in its
        // extension).
        // Kept for events only: the mux pushes it no pane's contents, which
        // it would otherwise for every pane while the chat is open.
        client.subscribe_events(codec::SubscribeEvents {}).await?;
        let pane: Box<dyn thinkterm_agent_chat::host::PaneControl> = Box::new(MuxPane {
            client,
            pane: self.pane,
            agent: target.agent.clone(),
        });
        smol::unblock(move || {
            thinkterm_agent_chat::host::run(target, std::io::BufReader::new(std::io::stdin()), out, Some(pane))
        })
        .await
    }

    #[cfg(not(unix))]
    async fn run(&self, _client: Client) -> anyhow::Result<()> {
        anyhow::bail!("agent chat is not available on this system")
    }
}

/// Programs whose pane is a window onto another machine.
#[cfg(unix)]
const REMOTE_SHELLS: [&str; 6] = ["ssh", "mosh", "mosh-client", "et", "autossh", "tsh"];

/// The agent's pane, through the mux that owns it: whether its agent
/// rests, its screen, and typing into it. Asked from the bridge's own
/// thread, which waits for each answer.
#[cfg(unix)]
struct MuxPane {
    client: Client,
    pane: mux::pane::PaneId,
    /// The agent the chat is with (`AgentStatus::agent_id`).
    agent: String,
}

#[cfg(unix)]
impl thinkterm_agent_chat::host::PaneControl for MuxPane {
    fn agent(&self) -> thinkterm_agent_chat::host::AgentHere {
        use thinkterm_agent_chat::host::AgentHere;
        let Ok(statuses) = smol::block_on(self.client.get_agent_statuses()) else {
            return AgentHere::Unknown;
        };
        // No agent in the pane, or another one: what it holds now is not
        // the agent the chat is with.
        let Some(entry) = statuses
            .statuses
            .iter()
            .find(|entry| entry.pane_id == self.pane && entry.status.agent_id == self.agent)
        else {
            return AgentHere::Gone;
        };
        match entry.status.state {
            AgentState::Idle | AgentState::Error => AgentHere::Resting,
            AgentState::Working | AgentState::Blocked => AgentHere::Working,
            AgentState::Unknown => AgentHere::Unknown,
        }
    }

    fn screen(&self) -> Option<Vec<String>> {
        smol::block_on(async {
            let info = self
                .client
                .get_dimensions(codec::GetPaneRenderableDimensions { pane_id: self.pane })
                .await
                .ok()?;
            let top = info.dimensions.physical_top;
            let rows = info.dimensions.viewport_rows as wezterm_term::StableRowIndex;
            let lines = self
                .client
                .get_lines(codec::GetLines {
                    pane_id: self.pane,
                    lines: vec![top..top + rows],
                })
                .await
                .ok()?;
            Some(
                lines
                    .lines
                    .extract_data()
                    .0
                    .into_iter()
                    .map(|(_, line)| line.as_str().into_owned())
                    .collect(),
            )
        })
    }

    fn type_line(&self, line: &str) -> anyhow::Result<()> {
        use termwiz::input::{KeyCode, KeyEvent, Modifiers};
        smol::block_on(async {
            self.client
                .write_to_pane(codec::WriteToPane {
                    pane_id: self.pane,
                    data: line.as_bytes().to_vec(),
                })
                .await?;
            // As send-text --enter: what was typed is taken in first.
            smol::Timer::after(thinkterm_reverse_access::protocol::enter_delay(line.len())).await;
            self.client
                .key_down(codec::SendKeyDown {
                    pane_id: self.pane,
                    event: KeyEvent {
                        key: KeyCode::Enter,
                        modifiers: Modifiers::NONE,
                    },
                    input_serial: codec::InputSerial::now(),
                })
                .await?;
            Ok(())
        })
    }
}

/// Whether a host OSC 7 names is this machine: empty, localhost, this
/// machine's name (with or without its domain), or one of its addresses
/// (an IPv6 one in a URL's brackets).
#[cfg(unix)]
fn this_machine(host: &str) -> bool {
    let short = |name: &str| name.split('.').next().unwrap_or(name).to_ascii_lowercase();
    let bare = host.strip_prefix('[').and_then(|h| h.strip_suffix(']')).unwrap_or(host);
    if bare.is_empty() || bare.eq_ignore_ascii_case("localhost") {
        return true;
    }
    if let Ok(address) = bare.parse::<std::net::IpAddr>() {
        return address.is_loopback() || own_addresses().contains(&address);
    }
    hostname::get()
        .ok()
        .and_then(|name| name.into_string().ok())
        .is_none_or(|name| short(&name) == short(bare))
}

/// The addresses of this machine's own network interfaces.
#[cfg(unix)]
fn own_addresses() -> Vec<std::net::IpAddr> {
    use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
    let mut found = Vec::new();
    let mut list: *mut libc::ifaddrs = std::ptr::null_mut();
    // SAFETY: getifaddrs fills `list` with a list it allocates, read here
    // while it lives and given back to freeifaddrs once.
    unsafe {
        if libc::getifaddrs(&mut list) != 0 {
            return found;
        }
        let mut at = list;
        while !at.is_null() {
            let entry = &*at;
            if !entry.ifa_addr.is_null() {
                match i32::from((*entry.ifa_addr).sa_family) {
                    libc::AF_INET => {
                        let v4 = &*(entry.ifa_addr as *const libc::sockaddr_in);
                        found.push(IpAddr::V4(Ipv4Addr::from(u32::from_be(v4.sin_addr.s_addr))));
                    }
                    libc::AF_INET6 => {
                        let v6 = &*(entry.ifa_addr as *const libc::sockaddr_in6);
                        found.push(IpAddr::V6(Ipv6Addr::from(v6.sin6_addr.s6_addr)));
                    }
                    _ => {}
                }
            }
            at = entry.ifa_next;
        }
        libc::freeifaddrs(list);
    }
    found
}

/// The path of a `file://` URL, whatever host OSC 7 put in it.
#[cfg(unix)]
fn local_path(url: &url::Url) -> Option<std::path::PathBuf> {
    if url.scheme() != "file" {
        return None;
    }
    let mut url = url.clone();
    let _ = url.set_host(None);
    url.to_file_path().ok()
}

/// A pane as the context describes it.
struct ContextPane {
    pane_id: mux::pane::PaneId,
    tab_id: mux::tab::TabId,
    tab_title: String,
    title: String,
    label: Option<String>,
}

/// At most this many neighbours are named; an agent can list the rest.
const CONTEXT_NEIGHBOURS: usize = 12;
/// Titles past this many characters are cut short.
const CONTEXT_TITLE_CHARS: usize = 80;

impl AgentContextCommand {
    async fn run(&self, client: Client) -> anyhow::Result<()> {
        let Some(pane_id) = std::env::var("WEZTERM_PANE")
            .ok()
            .and_then(|id| id.parse::<mux::pane::PaneId>().ok())
        else {
            return Ok(());
        };
        let listed = client.list_panes().await?;
        let mut panes = vec![];
        for (tabroot, tab_title) in listed.tabs.into_iter().zip(listed.tab_titles.iter()) {
            let mut cursor = tabroot.into_tree().cursor();
            loop {
                if let Some(stack) = cursor.leaf_mut() {
                    for entry in &stack.panes {
                        panes.push(ContextPane {
                            pane_id: entry.pane_id,
                            tab_id: entry.tab_id,
                            tab_title: tab_title.clone(),
                            title: entry.title.clone(),
                            label: listed.pane_labels.get(&entry.pane_id).cloned(),
                        });
                    }
                }
                match cursor.preorder_next() {
                    Ok(c) => cursor = c,
                    Err(_) => break,
                }
            }
        }
        if let Some(text) = session_context(pane_id, &panes) {
            let out = std::io::stdout();
            write!(out.lock(), "{text}").map_err(crate::stdout_error)?;
        }
        Ok(())
    }
}

/// What an agent starting in `pane_id` is told, or None when that pane is
/// not among `panes` (a pane of another server).
fn session_context(pane_id: mux::pane::PaneId, panes: &[ContextPane]) -> Option<String> {
    let me = panes.iter().find(|pane| pane.pane_id == pane_id)?;
    let mut text = format!("You are running inside ThinkTerm, in pane {}", describe(me));
    if !me.tab_title.is_empty() {
        text.push_str(&format!(" in tab \"{}\"", me.tab_title));
    }
    text.push_str(".\n");
    let beside: Vec<_> = panes
        .iter()
        .filter(|pane| pane.tab_id == me.tab_id && pane.pane_id != pane_id)
        .collect();
    if beside.is_empty() {
        text.push_str("No other pane shares this tab.\n");
    } else {
        text.push_str("Other panes in this tab:\n");
        for pane in beside.iter().take(CONTEXT_NEIGHBOURS) {
            text.push_str(&format!("- pane {}\n", describe(pane)));
        }
        if beside.len() > CONTEXT_NEIGHBOURS {
            text.push_str(&format!("- and {} more\n", beside.len() - CONTEXT_NEIGHBOURS));
        }
    }
    let elsewhere = panes.iter().filter(|pane| pane.tab_id != me.tab_id).count();
    if elsewhere > 0 {
        text.push_str(&format!("Other tabs hold {elsewhere} more.\n"));
    }
    text.push_str(
        "Read a pane's screen with `thinkterm cli get-text --pane <id or label> --trim`; \
         `thinkterm cli list` shows every pane and `thinkterm cli --help` every command. \
         If `thinkterm` is not on PATH, it is in $WEZTERM_EXECUTABLE_DIR.\n",
    );
    Some(text)
}

/// `12 "dev" (pnpm dev)`: the id, the label if any, and the title.
fn describe(pane: &ContextPane) -> String {
    let title: String = pane.title.chars().take(CONTEXT_TITLE_CHARS).collect();
    match &pane.label {
        Some(label) => format!("{} \"{}\" ({})", pane.pane_id, label, title),
        None => format!("{} ({})", pane.pane_id, title),
    }
}

pub(crate) fn state_label(state: AgentState) -> &'static str {
    match state {
        AgentState::Working => "working",
        AgentState::Blocked => "blocked",
        AgentState::Idle => "idle",
        AgentState::Unknown => "unknown",
        AgentState::Error => "error",
    }
}

fn evidence_label(evidence: AgentEvidence) -> &'static str {
    match evidence {
        AgentEvidence::Contract => "contract",
        AgentEvidence::Screen => "screen",
        AgentEvidence::Fallback => "fallback",
        AgentEvidence::Report => "report",
    }
}

// This will be serialized to JSON via the 'List' command.
// As such it is intended to be a stable output format,
// Do not remove or change the types of the fields.
#[derive(serde::Serialize)]
struct CliAgentResultItem {
    pane_id: mux::pane::PaneId,
    /// The label the pane was given, if any.
    label: Option<String>,
    agent: String,
    state: String,
    evidence: String,
    session_id: Option<String>,
    /// Unix seconds of the last state change, on the detecting host's
    /// clock (approximate when that host's clock differs from ours).
    since_unix: u64,
    workspace: String,
    title: String,
}

impl AgentListCommand {
    async fn run(&self, client: Client) -> anyhow::Result<()> {
        let out = std::io::stdout();
        let response = client.get_agent_statuses().await?;
        // Labels are the panes' own, listed with the panes.
        let labels = if response.statuses.is_empty() {
            Default::default()
        } else {
            client.list_panes().await?.pane_labels
        };
        let format = if self.json {
            CliOutputFormatKind::Json
        } else {
            self.format
        };
        match format {
            CliOutputFormatKind::Json => {
                let items: Vec<_> = response
                    .statuses
                    .into_iter()
                    .map(|entry| CliAgentResultItem {
                        label: labels.get(&entry.pane_id).cloned(),
                        pane_id: entry.pane_id,
                        agent: entry.status.agent_id,
                        state: state_label(entry.status.state).to_string(),
                        evidence: evidence_label(entry.status.evidence).to_string(),
                        session_id: entry.status.session_id,
                        since_unix: entry.status.since_unix,
                        workspace: entry.workspace,
                        title: entry.title,
                    })
                    .collect();
                crate::cli::print_json(&items)?;
            }
            CliOutputFormatKind::Table => {
                let now = SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .map(|d| d.as_secs())
                    .unwrap_or(0);
                let cols = vec![
                    Column {
                        name: "PANEID".to_string(),
                        alignment: Alignment::Right,
                    },
                    Column {
                        name: "AGENT".to_string(),
                        alignment: Alignment::Left,
                    },
                    Column {
                        name: "STATE".to_string(),
                        alignment: Alignment::Left,
                    },
                    Column {
                        name: "EVIDENCE".to_string(),
                        alignment: Alignment::Left,
                    },
                    Column {
                        name: "AGE".to_string(),
                        alignment: Alignment::Right,
                    },
                    Column {
                        name: "WORKSPACE".to_string(),
                        alignment: Alignment::Left,
                    },
                    Column {
                        name: "TITLE".to_string(),
                        alignment: Alignment::Left,
                    },
                    // Last, as in `list`, so the others stay in place.
                    Column {
                        name: "LABEL".to_string(),
                        alignment: Alignment::Left,
                    },
                ];
                let data = response
                    .statuses
                    .iter()
                    .map(|entry| {
                        // Clock skew across hosts renders as a zero age.
                        let age = now.saturating_sub(entry.status.since_unix);
                        vec![
                            entry.pane_id.to_string(),
                            entry.status.agent_id.clone(),
                            state_label(entry.status.state).to_string(),
                            evidence_label(entry.status.evidence).to_string(),
                            format!("{}", humantime::format_duration(
                                std::time::Duration::from_secs(age)
                            )),
                            entry.workspace.clone(),
                            entry.title.clone(),
                            labels.get(&entry.pane_id).cloned().unwrap_or_default(),
                        ]
                    })
                    .collect::<Vec<_>>();
                tabulate_output(&cols, &data, &mut out.lock()).map_err(crate::stdout_error)?;
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    #[test]
    fn this_machine_by_name_or_address() {
        assert!(this_machine(""));
        assert!(this_machine("localhost"));
        assert!(this_machine("127.0.0.1"));
        // As a URL writes an IPv6 host.
        assert!(this_machine("[::1]"));
        // TEST-NET-3: nobody's own.
        assert!(!this_machine("203.0.113.9"));
        assert!(!this_machine("[2001:db8::1]"));
    }

    fn pane(pane_id: mux::pane::PaneId, tab_id: mux::tab::TabId, title: &str, label: Option<&str>) -> ContextPane {
        ContextPane {
            pane_id,
            tab_id,
            tab_title: if tab_id == 3 { "project".into() } else { String::new() },
            title: title.into(),
            label: label.map(str::to_string),
        }
    }

    #[test]
    fn an_agent_is_told_its_pane_and_the_panes_beside_it() {
        let panes = [
            pane(12, 3, "claude", Some("agent")),
            pane(14, 3, "pnpm dev", Some("dev")),
            pane(15, 3, "tail -f app.log", None),
            pane(20, 4, "zsh", None),
        ];
        let text = session_context(12, &panes).unwrap();
        assert!(text.starts_with(
            "You are running inside ThinkTerm, in pane 12 \"agent\" (claude) in tab \"project\".\n"
        ));
        assert!(text.contains("- pane 14 \"dev\" (pnpm dev)\n"));
        assert!(text.contains("- pane 15 (tail -f app.log)\n"));
        assert!(text.contains("Other tabs hold 1 more.\n"));
        assert!(!text.contains("pane 20"));
    }

    #[test]
    fn an_agent_alone_in_its_tab_is_told_so() {
        let text = session_context(20, &[pane(20, 4, "zsh", None)]).unwrap();
        assert!(text.starts_with("You are running inside ThinkTerm, in pane 20 (zsh).\n"));
        assert!(text.contains("No other pane shares this tab.\n"));
        assert!(!text.contains("Other tabs"));
    }

    #[test]
    fn a_pane_of_another_server_gets_nothing() {
        assert!(session_context(99, &[pane(12, 3, "zsh", None)]).is_none());
    }
}
