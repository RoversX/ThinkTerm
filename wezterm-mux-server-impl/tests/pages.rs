#![cfg(unix)]
//! A plugin's page as the server keeps it: opened beside a terminal, it is
//! given an id of its own and where it was opened from; its plugin changes
//! its title and what it opens with, and every client is told -- once for a
//! burst of changes, and of the last of them. One put back where it was is
//! of the domain it was of.

use anyhow::{bail, Context, Result};
use codec::{ChangePage, DecodedPdu, PagePlace, Pdu, SetClientId, SpawnPage};
use config::keyassignment::SpawnTabDomain;
use mux::domain::{Domain, LocalDomain};
use mux::pane::{CachePolicy, Pane, PaneId};
use mux::{Mux, MuxNotification};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use thinkterm_proto::{PageEdit, PageSpec};
use wezterm_mux_server_impl::sessionhandler::{ConnectionPeer, PduSender, SessionHandler};

struct Client {
    handler: SessionHandler,
    replies: smol::channel::Receiver<DecodedPdu>,
    serial: u64,
}

impl Client {
    fn new() -> Self {
        let (tx, replies) = smol::channel::unbounded();
        Self {
            handler: SessionHandler::for_peer(
                PduSender::new(move |pdu| {
                    let mut bytes = vec![];
                    pdu.pdu.encode(&mut bytes, pdu.serial)?;
                    tx.try_send(Pdu::decode(bytes.as_slice())?)?;
                    Ok(())
                }),
                ConnectionPeer::Local,
            ),
            replies,
            serial: 0,
        }
    }

    async fn call(&mut self, pdu: Pdu) -> Result<Pdu> {
        self.serial += 1;
        let mut bytes = vec![];
        pdu.encode(&mut bytes, self.serial)?;
        self.handler.process_one(Pdu::decode(bytes.as_slice())?);
        loop {
            let response = self.replies.recv().await?;
            if response.serial == self.serial {
                return Ok(response.pdu);
            }
        }
    }

    async fn open_page(&mut self, page: PageSpec, place: PagePlace) -> Result<codec::SpawnPageResponse> {
        let size = wezterm_term::TerminalSize::default();
        match self.call(Pdu::SpawnPage(SpawnPage { page, place, size })).await? {
            Pdu::SpawnPageResponse(opened) => Ok(opened),
            other => bail!("unexpected reply {other:?}"),
        }
    }

    async fn change_page(&mut self, pane_id: PaneId, edit: PageEdit) -> Result<Pdu> {
        self.call(Pdu::ChangePage(ChangePage { pane_id, edit })).await
    }
}

fn board() -> PageSpec {
    PageSpec {
        plugin: "board".into(),
        args: "{\"board\":\"release\"}".into(),
        title: "Board".into(),
        ..Default::default()
    }
}

/// The directory pane `pane` is in, as a page opened beside it says it.
async fn directory_of(pane: &Arc<dyn Pane>) -> Result<String> {
    for _ in 0..100 {
        if let Some(url) = pane.get_current_working_dir(CachePolicy::FetchImmediate) {
            return Ok(url.to_file_path().unwrap().to_string_lossy().trim_end_matches('/').into());
        }
        smol::Timer::after(Duration::from_millis(50)).await;
    }
    bail!("the terminal never said where it is")
}

async fn scenario() -> Result<()> {
    let mux = Mux::get();
    let mut client = Client::new();
    let identified = client
        .call(Pdu::SetClientId(SetClientId {
            client_id: mux::client::generate_client_id(),
            is_proxy: false,
        }))
        .await?;
    assert!(matches!(identified, Pdu::UnitResponse(_)), "{:?}", identified);
    let (_, workspace) = match client
        .call(Pdu::EnsureThinkTermThread(codec::EnsureThinkTermThread {
            preferred_thread_id: None,
            size: wezterm_term::TerminalSize::default(),
        }))
        .await?
    {
        Pdu::EnsureThinkTermThreadResponse(landed) => (landed.thread_id, landed.workspace),
        other => bail!("unexpected reply {other:?}"),
    };
    let terminal = mux
        .iter_panes()
        .into_iter()
        .find(|pane| !pane.is_page())
        .context("the Thread opened with a shell")?;
    let directory = directory_of(&terminal).await?;

    // Opened in a tab of its own from the terminal in focus: an id, and
    // that terminal and its directory.
    let beside = Some(terminal.pane_id());
    let place = PagePlace::Tab {
        window_id: None,
        workspace: workspace.clone(),
        pane_id: beside,
    };
    let opened = client.open_page(board(), place).await?;
    let page = &opened.page;
    assert_eq!(page.id.len(), 16, "{page:?}");
    assert_eq!(page.terminal, Some(terminal.pane_id() as u64));
    assert_eq!(page.cwd.trim_end_matches('/'), directory);
    assert_eq!((page.args.as_str(), page.title.as_str()), ("{\"board\":\"release\"}", "Board"));
    let pane = mux.get_pane(opened.pane_id).context("the page's pane")?;
    assert_eq!(pane.page().as_ref(), Some(page), "the client is told the page as it is kept");

    // Beside the page, another: of its own, opened from where that one was.
    let stacked = client
        .open_page(board(), PagePlace::Stack { pane_id: opened.pane_id })
        .await?;
    assert_ne!(stacked.page.id, page.id);
    assert_eq!((stacked.page.terminal, &stacked.page.cwd), (page.terminal, &page.cwd));

    // Its plugin changes it, as fast as it draws: kept at once each time,
    // told at once and then once for the rest.
    let told = Arc::new(Mutex::new(0usize));
    let counted = Arc::clone(&told);
    let page_pane = opened.pane_id;
    mux.subscribe(move |notification| {
        if let MuxNotification::PageChanged(pane_id) = notification {
            if pane_id == page_pane {
                *counted.lock().unwrap() += 1;
            }
        }
        true
    });
    for card in 1..=5 {
        let edit = PageEdit {
            title: Some(format!("Card {card}")),
            args: Some(format!("{{\"card\":{card}}}")),
            ..Default::default()
        };
        let answer = client.change_page(page_pane, edit).await?;
        assert!(matches!(answer, Pdu::UnitResponse(_)), "{:?}", answer);
    }
    let kept = pane.page().context("still a page")?;
    assert_eq!((kept.title.as_str(), kept.args.as_str()), ("Card 5", "{\"card\":5}"));
    assert_eq!(pane.get_title(), "Card 5");
    assert_eq!((kept.id.as_str(), kept.terminal), (page.id.as_str(), page.terminal));
    assert_eq!(*told.lock().unwrap(), 1, "told at once of the first");
    smol::Timer::after(Duration::from_millis(500)).await;
    assert_eq!(*told.lock().unwrap(), 2, "and once more, of the rest");
    // The same again changes nothing, and is told nothing.
    let same = PageEdit {
        title: Some("Card 5".into()),
        ..Default::default()
    };
    client.change_page(page_pane, same).await?;
    smol::Timer::after(Duration::from_millis(400)).await;
    assert_eq!(*told.lock().unwrap(), 2);

    // What a page may not be, it is not made; and a terminal has no page.
    let too_long = PageEdit {
        title: Some("t".repeat(1000)),
        ..Default::default()
    };
    let refused = client.change_page(page_pane, too_long).await?;
    assert!(matches!(refused, Pdu::ErrorResponse(_)), "{:?}", refused);
    assert_eq!(pane.get_title(), "Card 5");
    let edit = PageEdit {
        title: Some("Not a page".into()),
        ..Default::default()
    };
    let refused = client.change_page(terminal.pane_id(), edit).await?;
    assert!(matches!(refused, Pdu::ErrorResponse(_)), "{:?}", refused);

    // Put back in a stack, as a layout is restored: of the domain it was
    // of, and of the stack's own when that is all there is to go by.
    let elsewhere = mux.get_domain_by_name("elsewhere").context("the second domain")?;
    let size = wezterm_term::TerminalSize::default();
    let domain = SpawnTabDomain::DomainName("elsewhere".into());
    let restored = mux.spawn_page_in_stack(terminal.pane_id(), domain, board(), size).await?;
    assert_eq!(restored.domain_id(), elsewhere.domain_id());
    assert_ne!(restored.domain_id(), terminal.domain_id());
    let domain = SpawnTabDomain::CurrentPaneDomain;
    let beside = mux.spawn_page_in_stack(terminal.pane_id(), domain, board(), size).await?;
    assert_eq!(beside.domain_id(), terminal.domain_id());
    Ok(())
}

#[test]
fn a_page_is_given_an_id_and_its_origin_and_its_plugins_changes_are_kept_and_told() {
    // This integration binary has one test and owns its process environment.
    let home = tempfile::Builder::new()
        .prefix("tt-pages-")
        .tempdir_in("/tmp")
        .unwrap();
    let path = home.path().canonicalize().unwrap();
    for key in ["HOME", "XDG_CONFIG_HOME", "XDG_DATA_HOME", "XDG_RUNTIME_DIR"] {
        std::env::set_var(key, &path);
    }
    std::env::remove_var("SSH_AUTH_SOCK");
    config::use_test_configuration();
    let mut cfg = (*config::configuration()).clone();
    cfg.mux_enable_ssh_agent = false;
    cfg.default_prog = Some(vec!["/bin/sh".into()]);
    config::use_this_configuration(cfg);
    let local: Arc<dyn Domain> = Arc::new(LocalDomain::new("local").unwrap());
    let mux = Arc::new(Mux::new(Some(local)));
    Mux::set_mux(&mux);
    let elsewhere: Arc<dyn Domain> = Arc::new(LocalDomain::new("elsewhere").unwrap());
    mux.add_domain(&elsewhere);
    let executor = promise::spawn::SimpleExecutor::new();
    let result = Arc::new(Mutex::new(None));
    let completed = result.clone();
    promise::spawn::spawn(async move {
        let outcome = smol::future::or(scenario(), async {
            smol::Timer::after(Duration::from_secs(30)).await;
            anyhow::bail!("the page scenario timed out")
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
