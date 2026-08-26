use crate::sessionhandler::{PduSender, SessionHandler};
use anyhow::Context;
use async_ossl::AsyncSslStream;
use codec::{DecodedPdu, Pdu};
use futures::FutureExt;
use mux::{Mux, MuxNotification};
use smol::prelude::*;
use smol::Async;
use wezterm_uds::UnixStream;

#[cfg(unix)]
pub trait AsRawDesc: std::os::unix::io::AsRawFd + std::os::fd::AsFd {}
#[cfg(windows)]
pub trait AsRawDesc: std::os::windows::io::AsRawSocket + std::os::windows::io::AsSocket {}

impl AsRawDesc for UnixStream {}
impl AsRawDesc for AsyncSslStream {}

#[derive(Debug)]
enum Item {
    Notif(MuxNotification),
    WritePdu(DecodedPdu),
    Readable,
}

pub async fn process<T>(stream: T) -> anyhow::Result<()>
where
    T: 'static,
    T: std::io::Read,
    T: std::io::Write,
    T: AsRawDesc,
    T: std::fmt::Debug,
    T: async_io::IoSafe,
{
    let stream = smol::Async::new(stream)?;
    process_async(stream).await
}

pub async fn process_async<T>(mut stream: Async<T>) -> anyhow::Result<()>
where
    T: 'static,
    T: std::io::Read,
    T: std::io::Write,
    T: std::fmt::Debug,
    T: async_io::IoSafe,
{
    log::trace!("process_async called");

    let (item_tx, item_rx) = smol::channel::unbounded::<Item>();

    let pdu_sender = PduSender::new({
        let item_tx = item_tx.clone();
        move |pdu| {
            item_tx
                .try_send(Item::WritePdu(pdu))
                .map_err(|e| anyhow::anyhow!("{:?}", e))
        }
    });
    let mut handler = SessionHandler::new(pdu_sender);

    {
        let mux = Mux::get();
        let tx = item_tx.clone();
        mux.subscribe(move |n| tx.try_send(Item::Notif(n)).is_ok());
    }

    // Notification PDUs go through the same WritePdu queue as RPC
    // responses instead of being written inline: the WritePdu arm below is
    // the one place that tolerates a half-closed peer (BrokenPipe), and an
    // inline `?` write here would kill the whole connection — and with it
    // every pane's output push — the moment a notification raced a
    // disconnecting client. A failed try_send means the channel is closed
    // and the connection is already over.
    let send_notif_pdu = {
        let item_tx = item_tx.clone();
        move |pdu: Pdu| {
            let _ = item_tx.try_send(Item::WritePdu(DecodedPdu { serial: 0, pdu }));
        }
    };

    loop {
        let rx_msg = item_rx.recv();
        let wait_for_read = stream.readable().map(|_| Ok(Item::Readable));

        match smol::future::or(rx_msg, wait_for_read).await {
            Ok(Item::Readable) => {
                let decoded = match Pdu::decode_async(&mut stream, None).await {
                    Ok(data) => data,
                    Err(err) => {
                        if let Some(err) = err.root_cause().downcast_ref::<std::io::Error>() {
                            if err.kind() == std::io::ErrorKind::UnexpectedEof {
                                // Client disconnected: no need to make a noise
                                return Ok(());
                            }
                        }
                        return Err(err).context("reading Pdu from client");
                    }
                };
                handler.process_one(decoded);
            }
            Ok(Item::WritePdu(decoded)) => {
                match decoded.pdu.encode_async(&mut stream, decoded.serial).await {
                    Ok(()) => {}
                    Err(err) => {
                        if let Some(err) = err.root_cause().downcast_ref::<std::io::Error>() {
                            if err.kind() == std::io::ErrorKind::BrokenPipe {
                                // Client disconnected: no need to make a noise
                                return Ok(());
                            }
                        }
                        return Err(err).context("encoding PDU to client");
                    }
                };
                match stream.flush().await {
                    Ok(()) => {}
                    Err(err) => {
                        if err.kind() == std::io::ErrorKind::BrokenPipe {
                            // Client disconnected: no need to make a noise
                            return Ok(());
                        }
                        return Err(err).context("flushing PDU to client");
                    }
                }
            }
            Ok(Item::Notif(MuxNotification::PaneOutput(pane_id))) => {
                handler.schedule_pane_push(pane_id);
            }
            Ok(Item::Notif(MuxNotification::PaneAdded(_pane_id))) => {}
            Ok(Item::Notif(MuxNotification::AgentStatusChanged(pane_id))) => {
                // Read the status at send time so the payload is always the
                // freshest classification, never a queued stale value.
                let status = Mux::get().get_pane(pane_id).and_then(|p| p.agent_status());
                send_notif_pdu(Pdu::AgentStatusChanged(codec::AgentStatusChanged {
                    pane_id,
                    status,
                }));
            }
            Ok(Item::Notif(MuxNotification::PaneRemoved(pane_id))) => {
                send_notif_pdu(Pdu::PaneRemoved(codec::PaneRemoved { pane_id }));
            }
            Ok(Item::Notif(MuxNotification::Alert { pane_id, alert })) => {
                {
                    let per_pane = handler.per_pane(pane_id);
                    let mut per_pane = per_pane.lock().unwrap();
                    per_pane.notifications.push(alert);
                }
                handler.schedule_pane_push(pane_id);
            }
            Ok(Item::Notif(MuxNotification::SaveToDownloads { .. })) => {}
            Ok(Item::Notif(MuxNotification::AssignClipboard {
                pane_id,
                selection,
                clipboard,
            })) => {
                send_notif_pdu(Pdu::SetClipboard(codec::SetClipboard {
                    pane_id,
                    clipboard,
                    selection,
                }));
            }
            Ok(Item::Notif(MuxNotification::TabAddedToWindow { tab_id, window_id })) => {
                send_notif_pdu(Pdu::TabAddedToWindow(codec::TabAddedToWindow {
                    tab_id,
                    window_id,
                }));
            }
            Ok(Item::Notif(MuxNotification::WindowRemoved(_window_id))) => {}
            Ok(Item::Notif(MuxNotification::WindowCreated(_window_id))) => {}
            Ok(Item::Notif(MuxNotification::WindowInvalidated(_window_id))) => {}
            Ok(Item::Notif(MuxNotification::WindowWorkspaceChanged(window_id))) => {
                let workspace = {
                    let mux = Mux::get();
                    mux.get_window(window_id)
                        .map(|w| w.get_workspace().to_string())
                };
                if let Some(workspace) = workspace {
                    send_notif_pdu(Pdu::WindowWorkspaceChanged(codec::WindowWorkspaceChanged {
                        window_id,
                        workspace,
                    }));
                }
            }
            Ok(Item::Notif(MuxNotification::PaneFocused(pane_id))) => {
                send_notif_pdu(Pdu::PaneFocused(codec::PaneFocused { pane_id }));
            }
            Ok(Item::Notif(MuxNotification::TabResized(tab_id))) => {
                send_notif_pdu(Pdu::TabResized(codec::TabResized { tab_id }));
            }
            Ok(Item::Notif(MuxNotification::TabTitleChanged { tab_id, title })) => {
                send_notif_pdu(Pdu::TabTitleChanged(codec::TabTitleChanged { tab_id, title }));
            }
            Ok(Item::Notif(MuxNotification::WindowTitleChanged { window_id, title })) => {
                send_notif_pdu(Pdu::WindowTitleChanged(codec::WindowTitleChanged {
                    window_id,
                    title,
                }));
            }
            Ok(Item::Notif(MuxNotification::WorkspaceRenamed {
                old_workspace,
                new_workspace,
            })) => {
                send_notif_pdu(Pdu::RenameWorkspace(codec::RenameWorkspace {
                    old_workspace,
                    new_workspace,
                }));
            }
            Ok(Item::Notif(MuxNotification::ThinkTermTreeChanged)) => {
                // The tree is small enough to resend whole; this is also the
                // path that tells the client which mutated it that the server
                // accepted the op.
                send_notif_pdu(Pdu::ThinkTermTreeState(codec::ThinkTermTreeState {
                    tree: crate::thinkterm_tree::snapshot(),
                }));
            }
            Ok(Item::Notif(MuxNotification::ThinkTermSessionChanged)) => {
                // A snapshot failure must not kill the connection — that
                // would stop every pane's pushes for this client.
                match crate::thinkterm_session::snapshot() {
                    Ok(state) => send_notif_pdu(Pdu::ThinkTermSessionState(state)),
                    Err(err) => {
                        log::error!("ThinkTermSessionState snapshot failed: {err:#}")
                    }
                }
            }
            Ok(Item::Notif(MuxNotification::FrontendLeaseChanged(state))) => {
                send_notif_pdu(Pdu::ClientViewportState(
                    crate::sessionhandler::codec_viewport_state(state),
                ));
            }
            Ok(Item::Notif(MuxNotification::FrontendAccessChanged(state))) => {
                send_notif_pdu(Pdu::FrontendAccessState(
                    crate::sessionhandler::codec_access_state(state),
                ));
            }
            Ok(Item::Notif(MuxNotification::ActiveWorkspaceChanged(_))) => {}
            Ok(Item::Notif(MuxNotification::Empty)) => {}
            Err(err) => {
                log::error!("process_async Err {}", err);
                return Ok(());
            }
        }
    }
}
