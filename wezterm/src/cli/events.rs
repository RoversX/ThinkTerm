use crate::cli::agent::state_label;
use crate::cli::{printable, CliOutputFormatKind};
use anyhow::{bail, Context};
use clap::Parser;
use codec::Pdu;
use mux::pane::PaneId;
use std::collections::{HashMap, VecDeque};
use std::io::Write;
use std::time::Duration;
use thinkterm_proto::{TabId, WindowId};
use wezterm_client::client::Client;
use wezterm_term::{Alert, Progress};

/// How many events may wait unprinted before further ones are dropped.
const BACKLOG: usize = 4096;
/// How often a quiet stream checks that the server is still there, and
/// that the connection has not been replaced by a reconnect.
const LIVENESS_INTERVAL: Duration = Duration::from_secs(5);

#[derive(Debug, Parser, Clone)]
pub struct EventsCommand {
    /// Only what happens to this pane; events about tabs and windows are
    /// left out.
    #[arg(long)]
    pane_id: Option<PaneId>,

    /// The pane by name rather than id: its label, its thread's name (a
    /// thread with one pane), or `thread/label`.
    #[arg(long, conflicts_with = "pane_id")]
    pane: Option<String>,

    /// "table" prints a line per event; "json" prints one JSON object per
    /// line, with a "type" and the event's own fields.
    #[arg(long = "format", default_value = "table")]
    format: CliOutputFormatKind,

    /// The same as `--format json`.
    #[arg(long, conflicts_with = "format")]
    json: bool,
}

// One line of `--format json` each: a stable output format. Add events and
// fields; do not remove or retype them.
#[derive(serde::Serialize, Debug, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
enum Event {
    PaneAdded {
        pane_id: PaneId,
    },
    PaneRemoved {
        pane_id: PaneId,
    },
    /// The program the pane started ended.
    ProgramExited {
        pane_id: PaneId,
        exit_code: Option<u32>,
        signal: Option<String>,
    },
    /// The pane's shell reported a command finished (OSC 133).
    CommandFinished {
        pane_id: PaneId,
        exit_code: Option<i32>,
    },
    Bell {
        pane_id: PaneId,
    },
    Notification {
        pane_id: PaneId,
        title: Option<String>,
        body: String,
    },
    TitleChanged {
        pane_id: PaneId,
        title: String,
    },
    CwdChanged {
        pane_id: PaneId,
    },
    Progress {
        pane_id: PaneId,
        state: &'static str,
        percent: Option<u8>,
    },
    UserVar {
        pane_id: PaneId,
        name: String,
        value: String,
    },
    ClipboardWritten {
        pane_id: PaneId,
    },
    AgentStatus {
        pane_id: PaneId,
        agent: Option<String>,
        state: Option<&'static str>,
    },
    ForegroundProgram {
        pane_id: PaneId,
        executable: Option<String>,
        runs: Option<String>,
    },
    PaneFocused {
        pane_id: PaneId,
    },
    TabAdded {
        tab_id: TabId,
        window_id: WindowId,
    },
    TabTitleChanged {
        tab_id: TabId,
        title: String,
    },
    /// The pane was labelled, or with no label had its label taken away.
    PaneLabelChanged {
        pane_id: PaneId,
        label: Option<String>,
    },
    WindowTitleChanged {
        window_id: WindowId,
        title: String,
    },
    WorkspaceChanged {
        window_id: WindowId,
        workspace: String,
    },
    WorkspaceRenamed {
        old_workspace: String,
        new_workspace: String,
    },
    /// A client began showing panes: a window, a browser, a phone. `epoch`
    /// and `id` tell apart clients the rest leaves alike, such as two
    /// browsers on one link.
    ClientJoined {
        hostname: String,
        username: String,
        pid: u32,
        epoch: u64,
        id: usize,
    },
    /// A client that showed panes went away.
    ClientLeft {
        hostname: String,
        username: String,
        pid: u32,
        epoch: u64,
        id: usize,
    },
}

impl Event {
    /// The event a server push stands for, if it stands for one.
    fn from_pdu(pdu: Pdu) -> Option<Self> {
        Some(match pdu {
            Pdu::PaneAdded(codec::PaneAdded { pane_id }) => Self::PaneAdded { pane_id },
            Pdu::PaneRemoved(codec::PaneRemoved { pane_id }) => Self::PaneRemoved { pane_id },
            Pdu::PaneExited(codec::PaneExited { pane_id, exit }) => Self::ProgramExited {
                pane_id,
                exit_code: exit.code,
                signal: exit.signal,
            },
            Pdu::NotifyAlert(codec::NotifyAlert { pane_id, alert }) => match alert {
                Alert::Bell => Self::Bell { pane_id },
                Alert::ToastNotification { title, body, .. } => Self::Notification {
                    pane_id,
                    title,
                    body,
                },
                Alert::WindowTitleChanged(title) => Self::TitleChanged { pane_id, title },
                Alert::CurrentWorkingDirectoryChanged => Self::CwdChanged { pane_id },
                Alert::Progress(progress) => {
                    let (state, percent) = match progress {
                        Progress::None => ("none", None),
                        Progress::Percentage(p) => ("percentage", Some(p)),
                        Progress::Error(p) => ("error", Some(p)),
                        Progress::Indeterminate => ("indeterminate", None),
                    };
                    Self::Progress {
                        pane_id,
                        state,
                        percent,
                    }
                }
                Alert::SetUserVar { name, value } => Self::UserVar {
                    pane_id,
                    name,
                    value,
                },
                Alert::CommandFinished { exit_code } => {
                    Self::CommandFinished { pane_id, exit_code }
                }
                Alert::IconTitleChanged(_)
                | Alert::TabTitleChanged(_)
                | Alert::PaletteChanged
                | Alert::OutputSinceFocusLost => return None,
            },
            // What was copied is not repeated here.
            Pdu::SetClipboard(codec::SetClipboard { pane_id, .. }) => {
                Self::ClipboardWritten { pane_id }
            }
            Pdu::AgentStatusChanged(codec::AgentStatusChanged { pane_id, status }) => {
                Self::AgentStatus {
                    pane_id,
                    state: status.as_ref().map(|status| state_label(status.state)),
                    agent: status.map(|status| status.agent_id),
                }
            }
            Pdu::ForegroundProgramChanged(codec::ForegroundProgramChanged { pane_id, program }) => {
                Self::ForegroundProgram {
                    pane_id,
                    runs: program.as_ref().and_then(|program| program.runs.clone()),
                    executable: program.map(|program| program.executable),
                }
            }
            Pdu::PaneFocused(codec::PaneFocused { pane_id }) => Self::PaneFocused { pane_id },
            Pdu::TabAddedToWindow(codec::TabAddedToWindow { tab_id, window_id }) => {
                Self::TabAdded { tab_id, window_id }
            }
            Pdu::TabTitleChanged(codec::TabTitleChanged { tab_id, title }) => {
                Self::TabTitleChanged { tab_id, title }
            }
            Pdu::PaneLabelChanged(codec::PaneLabelChanged { pane_id, label }) => {
                Self::PaneLabelChanged { pane_id, label }
            }
            Pdu::ClientJoined(codec::ClientJoined { client_id }) => Self::ClientJoined {
                hostname: client_id.hostname,
                username: client_id.username,
                pid: client_id.pid,
                epoch: client_id.epoch,
                id: client_id.id,
            },
            Pdu::ClientLeft(codec::ClientLeft { client_id }) => Self::ClientLeft {
                hostname: client_id.hostname,
                username: client_id.username,
                pid: client_id.pid,
                epoch: client_id.epoch,
                id: client_id.id,
            },
            Pdu::WindowTitleChanged(codec::WindowTitleChanged { window_id, title }) => {
                Self::WindowTitleChanged { window_id, title }
            }
            Pdu::WindowWorkspaceChanged(codec::WindowWorkspaceChanged {
                window_id,
                workspace,
            }) => Self::WorkspaceChanged {
                window_id,
                workspace,
            },
            Pdu::RenameWorkspace(codec::RenameWorkspace {
                old_workspace,
                new_workspace,
            }) => Self::WorkspaceRenamed {
                old_workspace,
                new_workspace,
            },
            _ => return None,
        })
    }

    fn pane_id(&self) -> Option<PaneId> {
        match self {
            Self::PaneAdded { pane_id }
            | Self::PaneRemoved { pane_id }
            | Self::ProgramExited { pane_id, .. }
            | Self::CommandFinished { pane_id, .. }
            | Self::Bell { pane_id }
            | Self::Notification { pane_id, .. }
            | Self::TitleChanged { pane_id, .. }
            | Self::CwdChanged { pane_id }
            | Self::Progress { pane_id, .. }
            | Self::UserVar { pane_id, .. }
            | Self::ClipboardWritten { pane_id }
            | Self::AgentStatus { pane_id, .. }
            | Self::ForegroundProgram { pane_id, .. }
            | Self::PaneLabelChanged { pane_id, .. }
            | Self::PaneFocused { pane_id } => Some(*pane_id),
            Self::TabAdded { .. }
            | Self::TabTitleChanged { .. }
            | Self::WindowTitleChanged { .. }
            | Self::WorkspaceChanged { .. }
            | Self::WorkspaceRenamed { .. }
            | Self::ClientJoined { .. }
            | Self::ClientLeft { .. } => None,
        }
    }

    /// The `--format table` line, without its time.
    fn describe(&self) -> String {
        let or_none = |value: &Option<String>| value.clone().unwrap_or_else(|| "-".to_string());
        match self {
            Self::PaneAdded { pane_id } => format!("pane_added          pane {pane_id}"),
            Self::PaneRemoved { pane_id } => format!("pane_removed        pane {pane_id}"),
            Self::ProgramExited {
                pane_id,
                exit_code,
                signal,
            } => match signal {
                Some(signal) => format!("program_exited      pane {pane_id}  {signal}"),
                None => match exit_code {
                    Some(code) => format!("program_exited      pane {pane_id}  status {code}"),
                    None => format!("program_exited      pane {pane_id}  status unknown"),
                },
            },
            Self::CommandFinished { pane_id, exit_code } => match exit_code {
                Some(code) => format!("command_finished    pane {pane_id}  status {code}"),
                None => format!("command_finished    pane {pane_id}"),
            },
            Self::Bell { pane_id } => format!("bell                pane {pane_id}"),
            Self::Notification {
                pane_id,
                title,
                body,
            } => format!(
                "notification        pane {pane_id}  {}{body}",
                title.as_ref().map(|t| format!("{t}: ")).unwrap_or_default()
            ),
            Self::TitleChanged { pane_id, title } => {
                format!("title_changed       pane {pane_id}  {title}")
            }
            Self::CwdChanged { pane_id } => format!("cwd_changed         pane {pane_id}"),
            Self::Progress {
                pane_id,
                state,
                percent,
            } => match percent {
                Some(p) => format!("progress            pane {pane_id}  {state} {p}%"),
                None => format!("progress            pane {pane_id}  {state}"),
            },
            Self::UserVar {
                pane_id,
                name,
                value,
            } => format!("user_var            pane {pane_id}  {name}={value}"),
            Self::ClipboardWritten { pane_id } => {
                format!("clipboard_written   pane {pane_id}")
            }
            Self::AgentStatus {
                pane_id,
                agent,
                state,
            } => format!(
                "agent_status        pane {pane_id}  {} {}",
                or_none(agent),
                state.unwrap_or("-")
            ),
            Self::ForegroundProgram {
                pane_id,
                executable,
                runs,
            } => match runs {
                Some(runs) => format!(
                    "foreground_program  pane {pane_id}  {} ({runs})",
                    or_none(executable)
                ),
                None => format!(
                    "foreground_program  pane {pane_id}  {}",
                    or_none(executable)
                ),
            },
            Self::PaneFocused { pane_id } => format!("pane_focused        pane {pane_id}"),
            Self::TabAdded { tab_id, window_id } => {
                format!("tab_added           tab {tab_id}  window {window_id}")
            }
            Self::TabTitleChanged { tab_id, title } => {
                format!("tab_title_changed   tab {tab_id}  {title}")
            }
            Self::PaneLabelChanged { pane_id, label } => match label {
                Some(label) => format!("pane_label_changed  pane {pane_id}  {label}"),
                None => format!("pane_label_changed  pane {pane_id}  (none)"),
            },
            Self::ClientJoined {
                hostname,
                username,
                pid,
                ..
            } => format!("client_joined       {username}@{hostname}  pid {pid}"),
            Self::ClientLeft {
                hostname,
                username,
                pid,
                ..
            } => format!("client_left         {username}@{hostname}  pid {pid}"),
            Self::WindowTitleChanged { window_id, title } => {
                format!("window_title_changed  window {window_id}  {title}")
            }
            Self::WorkspaceChanged {
                window_id,
                workspace,
            } => format!("workspace_changed   window {window_id}  {workspace}"),
            Self::WorkspaceRenamed {
                old_workspace,
                new_workspace,
            } => format!("workspace_renamed   {old_workspace} -> {new_workspace}"),
        }
    }
}

/// Current identities plus a bounded tail for removals queued before a
/// listing refresh. A removal releases its identity once printed.
#[derive(Default)]
struct PaneNames {
    live: HashMap<PaneId, Option<String>>,
    removed: VecDeque<(PaneId, String)>,
}

impl PaneNames {
    fn refresh(&mut self, listing: codec::ListPanesResponse) {
        let mut live: HashMap<PaneId, Option<String>> = listing
            .tabs
            .iter()
            .flat_map(|tab| tab.entries())
            .map(|pane| (pane.pane_id, None))
            .collect();
        for (pane_id, on) in listing.pane_machines {
            live.insert(pane_id, Some(format!("{}/{}", on.machine, on.pane_id)));
        }
        for (pane_id, name) in std::mem::replace(&mut self.live, live) {
            if !self.live.contains_key(&pane_id) {
                if let Some(name) = name {
                    self.removed.push_back((pane_id, name));
                }
            }
        }
        while self.removed.len() > BACKLOG {
            self.removed.pop_front();
        }
    }

    fn knows(&self, pane_id: PaneId) -> bool {
        self.live.contains_key(&pane_id) || self.removed.iter().any(|(id, _)| *id == pane_id)
    }

    fn get(&self, pane_id: PaneId) -> Option<&str> {
        if let Some(name) = self.live.get(&pane_id) {
            return name.as_deref();
        }
        self.removed
            .iter()
            .rev()
            .find(|(id, _)| *id == pane_id)
            .map(|(_, name)| name.as_str())
    }

    fn forget(&mut self, pane_id: PaneId) {
        self.live.remove(&pane_id);
        self.removed.retain(|(id, _)| *id != pane_id);
    }
}

impl EventsCommand {
    pub async fn run(self, client: Client) -> anyhow::Result<()> {
        let (client, only_pane) = match &self.pane {
            Some(name) => {
                let (client, pane_id) =
                    crate::cli::target::resolve_reaching_servers(client, None, Some(name)).await?;
                (client, Some(pane_id))
            }
            None => (client, self.pane_id),
        };
        let format = if self.json {
            CliOutputFormatKind::Json
        } else {
            self.format
        };
        // A pane on a server, heard of through the window, by the name
        // that reaches it again -- `MACHINE/ID` -- rather than the window's
        // own number for it, which names some other pane anywhere else.
        let pushes = wezterm_client::client::tap_unilateral_pdus(BACKLOG);
        client.subscribe_events(codec::SubscribeEvents {}).await?;
        let mut subscribed_on = client.connection_generation();
        let mut on_servers = PaneNames::default();
        on_servers.refresh(client.list_panes().await?);

        let out = std::io::stdout();
        loop {
            // A reconnect is a new connection on the server, which has
            // not been asked for events.
            if client.connection_generation() != subscribed_on {
                subscribed_on = client.connection_generation();
                client.subscribe_events(codec::SubscribeEvents {}).await?;
                on_servers.refresh(client.list_panes().await?);
            }
            let next = smol::future::or(async { Some(pushes.recv().await) }, async {
                smol::Timer::after(LIVENESS_INTERVAL).await;
                None
            })
            .await;
            let pdu = match next {
                Some(Ok(pdu)) => pdu,
                Some(Err(_)) => bail!("the event stream ended"),
                None => {
                    client
                        .ping()
                        .await
                        .context("lost the connection to the server")?;
                    continue;
                }
            };
            let Some(event) = Event::from_pdu(pdu) else {
                continue;
            };
            if matches!(event, Event::PaneAdded { .. } | Event::TabAdded { .. })
                || event.pane_id().is_some_and(|id| !on_servers.knows(id))
            {
                on_servers.refresh(client.list_panes().await?);
            }
            if only_pane.is_some() && event.pane_id() != only_pane {
                if let Event::PaneRemoved { pane_id } = event {
                    on_servers.forget(pane_id);
                }
                continue;
            }
            let on_server = event.pane_id().and_then(|pane_id| {
                on_servers
                    .get(pane_id)
                    .map(|name| (format!("pane {pane_id}"), name))
            });
            let line = match format {
                CliOutputFormatKind::Json => {
                    let mut value = serde_json::to_value(&event)?;
                    if let (Some((_, name)), Some(fields)) = (&on_server, value.as_object_mut()) {
                        fields.insert("pane".to_string(), (*name).into());
                    }
                    value.to_string()
                }
                CliOutputFormatKind::Table => {
                    let mut text = event.describe();
                    if let Some((window_number, name)) = &on_server {
                        text = text.replacen(window_number.as_str(), &format!("pane {name}"), 1);
                    }
                    format!(
                        "{}  {}",
                        chrono::Local::now().format("%H:%M:%S"),
                        printable(&text)
                    )
                }
            };
            writeln!(out.lock(), "{line}").map_err(crate::stdout_error)?;
            if let Event::PaneRemoved { pane_id } = event {
                on_servers.forget(pane_id);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn machines(ids: &[PaneId]) -> codec::ListPanesResponse {
        codec::ListPanesResponse {
            tabs: vec![],
            tab_titles: vec![],
            window_titles: HashMap::new(),
            pane_labels: HashMap::new(),
            pane_machines: ids
                .iter()
                .map(|id| {
                    (
                        *id,
                        codec::PaneMachine {
                            machine: "devbox".to_string(),
                            pane_id: id + 10,
                        },
                    )
                })
                .collect(),
            pane_pages: HashMap::new(),
        }
    }

    #[test]
    fn event_names_follow_new_panes_and_survive_refresh_until_removal() {
        let mut names = PaneNames::default();
        names.refresh(machines(&[1]));
        names.refresh(machines(&[1, 2]));
        assert_eq!(names.get(2), Some("devbox/12"));
        // A reconnect/list refresh can precede queued removal events.
        names.refresh(machines(&[2, 3]));
        assert_eq!(names.get(1), Some("devbox/11"));
        assert_eq!(names.get(3), Some("devbox/13"));
        names.forget(1);
        assert!(!names.knows(1));
        names.forget(2);
        assert!(!names.knows(2));
    }

    #[test]
    fn lost_removal_events_cannot_grow_retired_names_without_bound() {
        let mut names = PaneNames::default();
        for id in 0..BACKLOG + 2 {
            names.refresh(machines(&[id]));
        }
        assert_eq!(names.removed.len(), BACKLOG);
        assert_eq!(names.get(0), None);
        assert!(names.get(BACKLOG).is_some());
        assert!(names.get(BACKLOG + 1).is_some());
    }

    #[test]
    fn an_unavailable_program_exit_status_is_explicit() {
        let event = Event::from_pdu(Pdu::PaneExited(codec::PaneExited {
            pane_id: 4,
            exit: thinkterm_proto::PaneExit {
                code: None,
                signal: None,
            },
        }))
        .unwrap();
        assert_eq!(
            serde_json::to_string(&event).unwrap(),
            r#"{"type":"program_exited","pane_id":4,"exit_code":null,"signal":null}"#
        );
        assert!(event.describe().ends_with("status unknown"));
    }

    #[test]
    fn pushes_become_events_and_json_keeps_its_shape() {
        let exited = Event::from_pdu(Pdu::PaneExited(codec::PaneExited {
            pane_id: 4,
            exit: thinkterm_proto::PaneExit {
                code: Some(2),
                signal: None,
            },
        }))
        .unwrap();
        assert_eq!(
            serde_json::to_string(&exited).unwrap(),
            r#"{"type":"program_exited","pane_id":4,"exit_code":2,"signal":null}"#
        );

        let finished = Event::from_pdu(Pdu::NotifyAlert(codec::NotifyAlert {
            pane_id: 4,
            alert: Alert::CommandFinished { exit_code: Some(1) },
        }))
        .unwrap();
        assert_eq!(
            serde_json::to_string(&finished).unwrap(),
            r#"{"type":"command_finished","pane_id":4,"exit_code":1}"#
        );

        let bell = Event::from_pdu(Pdu::NotifyAlert(codec::NotifyAlert {
            pane_id: 9,
            alert: Alert::Bell,
        }))
        .unwrap();
        assert_eq!(bell, Event::Bell { pane_id: 9 });
        assert_eq!(bell.pane_id(), Some(9));
    }

    #[test]
    fn table_lines_carry_no_control_characters() {
        let event = Event::UserVar {
            pane_id: 1,
            name: "x".to_string(),
            value: "\x1b]52;c;cHduZWQ=\x07\u{9b}2J".to_string(),
        };
        let line = printable(&event.describe());
        assert!(!line.chars().any(char::is_control), "{:?}", line);
        assert!(line.contains("\\u{1b}]52;c;cHduZWQ="), "{:?}", line);
    }

    #[test]
    fn labels_and_clients_become_events() {
        let labelled = Event::from_pdu(Pdu::PaneLabelChanged(codec::PaneLabelChanged {
            pane_id: 3,
            label: Some("reviewer".to_string()),
        }))
        .unwrap();
        assert_eq!(
            serde_json::to_string(&labelled).unwrap(),
            r#"{"type":"pane_label_changed","pane_id":3,"label":"reviewer"}"#
        );
        assert_eq!(labelled.pane_id(), Some(3));

        let joined = Event::from_pdu(Pdu::ClientJoined(codec::ClientJoined {
            client_id: thinkterm_proto::ClientId {
                hostname: "myhost".to_string(),
                username: "user".to_string(),
                pid: 9,
                epoch: 1,
                id: 2,
                ssh_auth_sock: None,
            },
        }))
        .unwrap();
        assert_eq!(
            serde_json::to_string(&joined).unwrap(),
            r#"{"type":"client_joined","hostname":"myhost","username":"user","pid":9,"epoch":1,"id":2}"#
        );
        assert_eq!(joined.pane_id(), None);
        assert!(joined.describe().starts_with("client_joined"));
    }

    #[test]
    fn noise_and_clipboard_contents_stay_out() {
        assert_eq!(
            Event::from_pdu(Pdu::NotifyAlert(codec::NotifyAlert {
                pane_id: 1,
                alert: Alert::OutputSinceFocusLost,
            })),
            None
        );
        let copied = Event::from_pdu(Pdu::SetClipboard(codec::SetClipboard {
            pane_id: 1,
            clipboard: Some("secret".to_string()),
            selection: wezterm_term::ClipboardSelection::Clipboard,
        }))
        .unwrap();
        assert!(!serde_json::to_string(&copied).unwrap().contains("secret"));
    }
}
