#![cfg(unix)]

use anyhow::{ensure, Result};
use codec::{DecodedPdu, Pdu};
use mux::domain::{Domain, LocalDomain};
use mux::Mux;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use thinkterm_import::*;
use wezterm_mux_server_impl::sessionhandler::{ConnectionPeer, PduSender, SessionHandler, WebPeer};

fn request_id() -> String {
    format!(
        "{}-{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs(),
        uuid::Uuid::new_v4()
    )
}

fn identified(request: ImportRequest) -> codec::ImportSessionRequest {
    codec::ImportSessionRequest {
        request_id: request_id(),
        request,
    }
}

struct Example;
static EXAMPLE: Example = Example;
static LIVE: Mutex<Option<LiveSetup>> = Mutex::new(None);

struct SourceProcess {
    child: Mutex<std::process::Child>,
    _slave: std::fs::File,
}

impl Drop for SourceProcess {
    fn drop(&mut self) {
        let child = self.child.get_mut().unwrap();
        let _ = child.kill();
        let _ = child.wait();
    }
}

struct LiveSetup {
    master: std::os::fd::OwnedFd,
    transfer: DelayedHandoff,
}

struct DelayedHandoff {
    source: Arc<SourceProcess>,
    started: smol::channel::Sender<()>,
    release: std::sync::mpsc::Receiver<bool>,
}

impl Handoff for DelayedHandoff {
    fn commit(&mut self) -> Result<()> {
        self.started.try_send(())?;
        ensure!(
            self.release.recv_timeout(Duration::from_secs(10))?,
            "example handoff failed"
        );
        Ok(())
    }

    fn finish(self: Box<Self>) {}
}

impl ImportSource for Example {
    fn info(&self) -> SourceInfo {
        SourceInfo {
            id: "example",
            name: "Example",
            icon: "terminal",
        }
    }

    fn discover(&self, context: &ImportContext) -> Result<Vec<Session>> {
        ensure!(
            context.home.join("remote-project").is_dir(),
            "wrong discovery owner"
        );
        Ok(vec![Session {
            name: "development".into(),
        }])
    }

    fn preview(&self, context: &ImportContext, session: &str) -> Result<Preview> {
        ensure!(
            session == "development" || session == "development-live",
            "unknown session"
        );
        Ok(Preview {
            session: session.into(),
            live: session == "development-live",
            version: None,
            fingerprint: "example-fingerprint".into(),
            unavailable: None,
            projects: vec![PreviewProject {
                name: "example".into(),
                threads: 1,
                tabs: 1,
                panes: 1,
                terminals: vec![PreviewTerminal {
                    title: Some("shell".into()),
                    tab_name: None,
                    cwd: Some(context.home.join("remote-project").to_str().unwrap().into()),
                }],
            }],
        })
    }

    fn notes(&self, _: Option<ImportMode>) -> Vec<&'static str> {
        vec!["session-import-requirements-layout"]
    }

    fn prepare(&self, context: &ImportContext, request: &ImportRequest) -> Result<PreparedImport> {
        let preview = self.preview(context, &request.session)?;
        ensure!(
            request.fingerprint == preview.fingerprint,
            "preview changed"
        );
        let cwd = preview.projects[0].terminals[0].cwd.clone().unwrap();
        let mut prepared = PreparedImport {
            plan: ImportPlan {
                mode: request.mode,
                active: Selection::default(),
                projects: vec![Project {
                    name: "example".into(),
                    directory: cwd.clone(),
                    threads: vec![Thread {
                        name: "development".into(),
                        active_tab: 0,
                        tabs: vec![Tab {
                            name: None,
                            layout: Layout::Pane(1),
                            focused: Some(1),
                            zoomed: false,
                            panes: [(
                                1,
                                Pane {
                                    cwd,
                                    title: Some("Imported shell".into()),
                                },
                            )]
                            .into(),
                        }],
                    }],
                }],
            },
            terminals: vec![],
            handoff: None,
        };
        if request.mode.is_live() {
            let setup = LIVE.lock().unwrap().take().unwrap();
            prepared.terminals.push(LiveTerminal {
                pane_id: 1,
                seed: TerminalSeed {
                    child_pid: setup.transfer.source.child.lock().unwrap().id(),
                    rows: 24,
                    cols: 80,
                    cell_width_px: 0,
                    cell_height_px: 0,
                    title: None,
                    ansi: String::new(),
                },
                pty: setup.master,
            });
            prepared.handoff = Some(Box::new(setup.transfer));
        }
        Ok(prepared)
    }
}

struct Client {
    handler: SessionHandler,
    replies: smol::channel::Receiver<DecodedPdu>,
    serial: u64,
}

impl Client {
    fn new(peer: ConnectionPeer) -> Self {
        let (tx, replies) = smol::channel::unbounded();
        Self {
            handler: SessionHandler::for_peer(
                PduSender::new(move |pdu| {
                    // Exercise the actual wire encoding in both directions.
                    let mut bytes = vec![];
                    pdu.pdu.encode(&mut bytes, pdu.serial)?;
                    tx.try_send(Pdu::decode(bytes.as_slice())?)?;
                    Ok(())
                }),
                peer,
            ),
            replies,
            serial: 0,
        }
    }

    fn send(&mut self, pdu: Pdu) -> Result<u64> {
        self.serial += 1;
        let mut bytes = vec![];
        pdu.encode(&mut bytes, self.serial)?;
        self.handler.process_one(Pdu::decode(bytes.as_slice())?);
        Ok(self.serial)
    }

    async fn reply(&self, serial: u64) -> Result<Pdu> {
        loop {
            let response = self.replies.recv().await?;
            if response.serial == serial {
                return Ok(response.pdu);
            }
        }
    }

    async fn call(&mut self, pdu: Pdu) -> Result<Pdu> {
        let name = pdu.pdu_name();
        let serial = self.send(pdu)?;
        smol::future::or(self.reply(serial), async move {
            smol::Timer::after(Duration::from_secs(5)).await;
            anyhow::bail!("{} (serial {}) timed out", name, serial)
        })
        .await
    }
}

async fn live_scenario(existing_thread: String, succeed: bool) -> Result<()> {
    use futures::{pin_mut, poll};

    let pty = nix::pty::openpty(None, None)?;
    let source = Arc::new(SourceProcess {
        child: Mutex::new(std::process::Command::new("sleep").arg("30").spawn()?),
        _slave: std::fs::File::from(pty.slave),
    });
    let (started, waiting) = smol::channel::bounded(1);
    let (release, receiver) = std::sync::mpsc::channel();
    *LIVE.lock().unwrap() = Some(LiveSetup {
        master: pty.master,
        transfer: DelayedHandoff {
            source: source.clone(),
            started,
            release: receiver,
        },
    });
    let request = identified(ImportRequest {
        source: "example".into(),
        session: "development-live".into(),
        mode: ImportMode::Live,
        fingerprint: "example-fingerprint".into(),
        space_name: "Live import".into(),
    });
    let mut disconnected = Client::new(ConnectionPeer::Local);
    disconnected.send(Pdu::ImportSessionRequest(request.clone()))?;
    drop(disconnected);
    waiting.recv().await?;
    let mut querying = Client::new(ConnectionPeer::Local);
    assert!(matches!(
        querying
            .call(Pdu::GetImportSessionStatus(codec::GetImportSessionStatus {
                request_id: request.request_id.clone(),
            }))
            .await?,
        Pdu::GetImportSessionStatusResponse(codec::GetImportSessionStatusResponse {
            status: codec::ImportSessionStatus::Running,
        })
    ));
    let duplicate = request.clone();
    let import = promise::spawn::spawn(async move {
        Client::new(ConnectionPeer::Local)
            .call(Pdu::ImportSessionRequest(duplicate))
            .await
    });
    let tree = wezterm_mux_server_impl::thinkterm_tree::snapshot();
    let space = tree
        .spaces
        .iter()
        .find(|space| space.name == "Live import")
        .unwrap();
    let thread = &tree
        .projects
        .iter()
        .find(|p| p.space_id == space.id)
        .unwrap()
        .threads[0];
    let workspace = thread.materialized_workspace_name.clone().unwrap();
    let mux = Mux::get();
    let initial_panes = mux.iter_panes().len();
    assert!(mux.iter_windows_in_workspace(&workspace).is_empty());

    let mut restoring = Client::new(ConnectionPeer::Local);
    let serial = restoring.send(Pdu::EnsureThinkTermThread(codec::EnsureThinkTermThread {
        preferred_thread_id: Some(thread.id.clone()),
        size: wezterm_term::TerminalSize::default(),
    }))?;
    let restored = restoring.reply(serial);
    pin_mut!(restored);
    let mut other = Client::new(ConnectionPeer::Local);
    other
        .call(Pdu::SetClientId(codec::SetClientId {
            client_id: mux::client::ClientId {
                hostname: "server-a".into(),
                username: "user".into(),
                pid: 124,
                epoch: 0,
                id: 1,
                ssh_auth_sock: None,
            },
            is_proxy: false,
        }))
        .await?;
    let spawn_response = other
        .call(Pdu::SpawnV2(codec::SpawnV2 {
            domain: thinkterm_proto::SpawnTabDomain::DefaultDomain,
            window_id: None,
            command: None,
            command_dir: None,
            size: wezterm_term::TerminalSize::default(),
            workspace: workspace.clone(),
        }))
        .await?;
    assert!(
        matches!(&spawn_response,
            Pdu::ErrorResponse(error) if error.reason.contains("import is still in progress")
        ),
        "unexpected spawn response: {:?}",
        spawn_response
    );
    assert!(matches!(
        other
            .call(Pdu::EnsureThinkTermThread(codec::EnsureThinkTermThread {
                preferred_thread_id: Some(existing_thread),
                size: wezterm_term::TerminalSize::default(),
            }))
            .await?,
        Pdu::EnsureThinkTermThreadResponse(codec::EnsureThinkTermThreadResponse {
            spawned: false,
            ..
        })
    ));
    assert!(poll!(&mut restored).is_pending());
    assert_eq!(mux.iter_panes().len(), initial_panes);
    release.send(succeed)?;
    let result = import.await?;
    let status = querying
        .call(Pdu::GetImportSessionStatus(codec::GetImportSessionStatus {
            request_id: request.request_id.clone(),
        }))
        .await?;
    assert!(
        matches!(&status, Pdu::GetImportSessionStatusResponse(response) if
        if succeed { matches!(&response.status, codec::ImportSessionStatus::Completed(_)) }
        else { matches!(&response.status, codec::ImportSessionStatus::Failed(_)) })
    );
    let restored = restored.await?;
    if succeed {
        assert!(matches!(result, Pdu::ImportSessionResponse(_)));
        assert!(matches!(
            restored,
            Pdu::EnsureThinkTermThreadResponse(codec::EnsureThinkTermThreadResponse {
                spawned: false,
                ..
            })
        ));
        assert_eq!(mux.iter_windows_in_workspace(&workspace).len(), 1);
        assert_eq!(mux.iter_panes().len(), initial_panes + 1);
        for window_id in mux.iter_windows_in_workspace(&workspace) {
            let panes: Vec<_> = mux
                .get_window(window_id)
                .unwrap()
                .iter()
                .flat_map(|tab| tab.iter_all_panes())
                .map(|pane| pane.pane_id())
                .collect();
            for pane_id in panes {
                mux.remove_pane(pane_id);
            }
        }
        wezterm_mux_server_impl::thinkterm_tree::mutate(&[codec::TreeOp::DeleteSpace {
            space_id: space.id.clone(),
        }])?;
    } else {
        assert!(matches!(result, Pdu::ErrorResponse(_)));
        assert!(matches!(restored, Pdu::ErrorResponse(error)
            if error.reason.contains("import did not finish")));
        assert!(mux.iter_windows_in_workspace(&workspace).is_empty());
        assert_eq!(mux.iter_panes().len(), initial_panes);
        assert!(!wezterm_mux_server_impl::thinkterm_tree::snapshot()
            .spaces
            .iter()
            .any(|s| s.id == space.id));
    }
    Ok(())
}

async fn scenario(local_id: mux::domain::DomainId) -> Result<()> {
    let mut client = Client::new(ConnectionPeer::Local);
    // This is how the SSH proxy marks a connection before relaying RPCs.
    client
        .call(Pdu::SetClientId(codec::SetClientId {
            client_id: mux::client::ClientId {
                hostname: "server-a".into(),
                username: "user".into(),
                pid: 123,
                epoch: 0,
                id: 0,
                ssh_auth_sock: None,
            },
            is_proxy: true,
        }))
        .await?;
    let listed = client
        .call(Pdu::ListImportSessions(codec::ListImportSessions {
            source: "example".into(),
        }))
        .await?;
    match listed {
        Pdu::ListImportSessionsResponse(response) => {
            assert_eq!(response.sessions[0].name, "development");
            assert_eq!(response.notes, vec!["session-import-requirements-layout"]);
        }
        other => anyhow::bail!("discovery: {:?}", other),
    }
    let preview = client
        .call(Pdu::PreviewImportSession(codec::PreviewImportSession {
            source: "example".into(),
            session: "development".into(),
        }))
        .await?;
    let preview = match preview {
        Pdu::PreviewImportSessionResponse(response) => response.preview,
        other => anyhow::bail!("preview: {:?}", other),
    };
    assert_eq!(
        preview.projects[0].terminals[0].cwd.as_deref(),
        config::HOME_DIR.join("remote-project").to_str()
    );
    let request = identified(ImportRequest {
        source: "example".into(),
        session: preview.session,
        mode: ImportMode::Layout,
        fingerprint: preview.fingerprint,
        space_name: "Imported".into(),
    });
    assert!(matches!(
        client
            .call(Pdu::GetImportSessionStatus(codec::GetImportSessionStatus {
                request_id: request.request_id.clone(),
            }))
            .await?,
        Pdu::GetImportSessionStatusResponse(codec::GetImportSessionStatusResponse {
            status: codec::ImportSessionStatus::NotFound,
        })
    ));
    let mut stale = request.clone();
    stale.request_id = request_id();
    stale.request.fingerprint = "stale".into();
    assert!(matches!(
        client.call(Pdu::ImportSessionRequest(stale)).await?,
        Pdu::ErrorResponse(_)
    ));
    assert!(Mux::get().iter_panes().is_empty());
    assert!(wezterm_mux_server_impl::thinkterm_tree::snapshot()
        .spaces
        .is_empty());

    let mut unsaved = request.clone();
    unsaved.request_id = request_id();
    let save_error =
        wezterm_mux_server_impl::session_import::execute(unsaved, |tree, add, layouts| {
            assert!(add);
            assert_eq!(layouts.len(), 1);
            assert_eq!(layouts[0].thread_id, tree.projects[0].threads[0].id);
            assert_eq!(layouts[0].active_tab, 0);
            assert_eq!(layouts[0].tabs.len(), 1);
            let entries = layouts[0].tabs[0].entries();
            assert_eq!(entries.len(), 1);
            assert_eq!(entries[0].title, "Imported shell");
            anyhow::bail!("example disk write failure")
        })
        .await
        .unwrap_err();
    assert!(format!("{save_error:#}").contains("example disk write failure"));
    assert!(Mux::get().iter_panes().is_empty());

    let result = client
        .call(Pdu::ImportSessionRequest(request.clone()))
        .await?;
    let result = match result {
        Pdu::ImportSessionResponse(result) => result,
        other => anyhow::bail!("import: {:?}", other),
    };
    assert_eq!(result.pane_count, 1);
    assert!(!result.live);
    let mux = Mux::get();
    let panes = mux.iter_panes();
    assert_eq!(panes.len(), 1);
    assert_eq!(panes[0].domain_id(), local_id);
    assert!(wezterm_mux_server_impl::thinkterm_tree::snapshot()
        .spaces
        .iter()
        .any(|s| s.id == result.space_id));
    assert!(wezterm_mux_server_impl::thinkterm_tree::tree_path().is_file());

    let repeated = client
        .call(Pdu::ImportSessionRequest(request.clone()))
        .await?;
    assert!(matches!(repeated, Pdu::ImportSessionResponse(ref cached) if cached == &result));
    assert_eq!(mux.iter_panes().len(), 1);
    let mut changed = request.clone();
    changed.request.space_name = "Changed".into();
    assert!(matches!(
        client.call(Pdu::ImportSessionRequest(changed)).await?,
        Pdu::ErrorResponse(_)
    ));
    assert_eq!(mux.iter_panes().len(), 1);

    let mut tls = Client::new(ConnectionPeer::Tls);
    assert!(matches!(
        tls.call(Pdu::ListImportSessions(codec::ListImportSessions {
            source: "example".into()
        }))
        .await?,
        Pdu::ListImportSessionsResponse(_)
    ));
    let (_kill, revoked) = smol::channel::bounded(1);
    let mut browser = Client::new(ConnectionPeer::Web(WebPeer {
        token_id: "example".into(),
        label: "example".into(),
        username: "user".into(),
        revoked,
    }));
    for pdu in vec![
        Pdu::ListImportSessions(codec::ListImportSessions {
            source: "example".into(),
        }),
        Pdu::PreviewImportSession(codec::PreviewImportSession {
            source: "example".into(),
            session: "development".into(),
        }),
        Pdu::GetImportSessionStatus(codec::GetImportSessionStatus {
            request_id: request.request_id.clone(),
        }),
        Pdu::ImportSessionRequest(request),
    ] {
        assert!(matches!(browser.call(pdu).await?, Pdu::ErrorResponse(_)));
    }
    assert_eq!(mux.iter_panes().len(), 1);
    let existing_thread = result.tree.projects[0].threads[0].id.clone();
    live_scenario(existing_thread.clone(), true).await?;
    live_scenario(existing_thread, false).await?;
    Ok(())
}

#[test]
fn remote_import_stays_on_the_owner_and_web_tokens_cannot_invoke_it() {
    // This integration binary has one test and owns its process environment.
    let home = tempfile::Builder::new()
        .prefix("tt-import-")
        .tempdir_in("/tmp")
        .unwrap();
    let path = home.path().canonicalize().unwrap();
    for key in [
        "HOME",
        "XDG_CONFIG_HOME",
        "XDG_DATA_HOME",
        "XDG_RUNTIME_DIR",
    ] {
        std::env::set_var(key, &path);
    }
    std::env::remove_var("SSH_AUTH_SOCK");
    std::fs::create_dir(path.join("remote-project")).unwrap();
    config::use_test_configuration();
    let mut cfg = (*config::configuration()).clone();
    cfg.mux_enable_ssh_agent = false;
    config::use_this_configuration(cfg);
    thinkterm_import::register(&EXAMPLE).unwrap();
    let local: Arc<dyn Domain> = Arc::new(LocalDomain::new("local").unwrap());
    let local_id = local.domain_id();
    let mux = Arc::new(Mux::new(Some(local)));
    Mux::set_mux(&mux);
    let executor = promise::spawn::SimpleExecutor::new();
    let result = Arc::new(Mutex::new(None));
    let completed = result.clone();
    promise::spawn::spawn(async move {
        let outcome = smol::future::or(scenario(local_id), async {
            smol::Timer::after(Duration::from_secs(20)).await;
            anyhow::bail!("import RPC test timed out")
        })
        .await;
        *completed.lock().unwrap() = Some(outcome);
    })
    .detach();
    while result.lock().unwrap().is_none() {
        executor.tick().unwrap();
    }
    for pane in mux.iter_panes() {
        mux.remove_pane(pane.pane_id());
    }
    result.lock().unwrap().take().unwrap().unwrap();
}
