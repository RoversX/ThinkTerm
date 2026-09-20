//! The core thread's end of the PDU stream over ssh, as the App's [`Link`].
//!
//! The network thread hands in bytes and takes out bytes; this side does
//! the framing, matches answers to requests by serial, hands pushes to the
//! App and keeps the lease. It is the transport-neutral part of the
//! browser's `link.rs`, with the ssh session's life (dial, redial, hang
//! up) in place of the WebSocket's.
//!
//! Sending is ordered by construction: `request` encodes and queues the
//! bytes on the spot, in the order it was called, and the network thread
//! writes the queue in that order. That is the `PduLink` contract ("on the
//! wire now, behind everything sent before it"); the plan's saturation
//! rules for a bounded queue come with the real Transport, not this probe.

use crate::ssh::{self, Net, Out, SshParams};
use anyhow::{anyhow, Result};
use codec::Pdu;
use futures::channel::oneshot;
use std::cell::{Cell, RefCell};
use std::rc::Rc;
use thinkterm_proto::TabId;
use thinkterm_session::connection::{Answer, SerialTable};
use thinkterm_session::host::{LinkError, PduLink};
use thinkterm_session::input::PaneLink;
use thinkterm_web::lease::Lease;
use thinkterm_web::platform::{Link, LocalFuture};
use tokio::sync::mpsc::UnboundedSender;

#[derive(Clone)]
pub struct SshLink(Rc<Inner>);

#[cfg(test)]
pub(crate) fn test_link() -> SshLink {
    SshLink::new(
        SshParams {
            host: "unused".into(),
            port: 22,
            user: "test".into(),
            auth: ssh::Auth::Password(String::new()),
            known_host: None,
            remote_command: String::new(),
            keepalive_secs: 0,
        },
        Box::new(|| panic!("this test must not open a network connection")),
    )
}

struct Inner {
    params: SshParams,
    /// How the network thread reaches the core thread with what arrives.
    deliver: Box<dyn Fn() -> Box<dyn Fn(Net) + Send>>,
    out: RefCell<Option<UnboundedSender<Out>>>,
    serials: RefCell<SerialTable<oneshot::Sender<Pdu>>>,
    pushes: RefCell<Option<Box<dyn FnMut(Pdu)>>>,
    pending_pushes: RefCell<Vec<Pdu>>,
    on_close: RefCell<Option<Box<dyn FnMut(String)>>>,
    closed_early: RefCell<Option<String>>,
    /// A dial in progress: answered by `opened` or by a close.
    connecting: RefCell<Option<oneshot::Sender<Result<()>>>>,
    lease: RefCell<Lease>,
    inbound: RefCell<Vec<u8>>,
    open: Cell<bool>,
    generation: Cell<u64>,
}

impl SshLink {
    /// `deliver` makes the callback each dial hands the network thread.
    pub fn new(params: SshParams, deliver: Box<dyn Fn() -> Box<dyn Fn(Net) + Send>>) -> Self {
        Self(Rc::new(Inner {
            params,
            deliver,
            out: RefCell::new(None),
            serials: RefCell::new(SerialTable::new()),
            pushes: RefCell::new(None),
            pending_pushes: RefCell::new(Vec::new()),
            on_close: RefCell::new(None),
            closed_early: RefCell::new(None),
            connecting: RefCell::new(None),
            lease: RefCell::new(Lease::default()),
            inbound: RefCell::new(Vec::new()),
            open: Cell::new(false),
            generation: Cell::new(0),
        }))
    }

    /// Dial. The future resolves when the exec channel is up (or the dial
    /// failed); the core thread feeds the outcome through `opened` and
    /// `closed`.
    pub fn connect(&self) -> LocalFuture<Result<()>> {
        let (tx, rx) = oneshot::channel();
        *self.0.connecting.borrow_mut() = Some(tx);
        let deliver = (self.0.deliver)();
        let out = ssh::spawn(self.0.params.clone(), deliver);
        *self.0.out.borrow_mut() = Some(out);
        Box::pin(async move { rx.await.map_err(|_| anyhow!("the dial was abandoned"))? })
    }

    /// The transport came up.
    pub fn opened(&self) {
        self.0.inbound.borrow_mut().clear();
        self.0.serials.borrow_mut().drain();
        self.0.open.set(true);
        self.0.generation.set(self.0.generation.get() + 1);
        if let Some(tx) = self.0.connecting.borrow_mut().take() {
            let _ = tx.send(Ok(()));
        }
    }

    /// The transport went away, and why. A dial still waiting learns it
    /// failed; an open connection's close handler is told.
    pub fn closed(&self, reason: String) {
        let was_open = self.0.open.get();
        self.0.open.set(false);
        *self.0.out.borrow_mut() = None;
        self.0.serials.borrow_mut().drain();
        if let Some(tx) = self.0.connecting.borrow_mut().take() {
            let _ = tx.send(Err(anyhow!("{reason}")));
            return;
        }
        if !was_open {
            return;
        }
        let taken = self.0.on_close.borrow_mut().take();
        match taken {
            Some(mut handler) => {
                handler(reason);
                *self.0.on_close.borrow_mut() = Some(handler);
            }
            None => *self.0.closed_early.borrow_mut() = Some(reason),
        }
    }

    pub fn is_open(&self) -> bool {
        self.0.open.get()
    }

    /// Bytes from the wire. Decodes every whole PDU in them, answers
    /// requests and hands pushes on; a partial PDU waits for more.
    pub fn feed(&self, bytes: &[u8]) -> Result<()> {
        self.0.inbound.borrow_mut().extend_from_slice(bytes);
        loop {
            let decoded = {
                let mut inbound = self.0.inbound.borrow_mut();
                match Pdu::stream_decode(&mut inbound) {
                    Ok(Some(decoded)) => decoded,
                    Ok(None) => return Ok(()),
                    Err(err) => return Err(anyhow!("decoding the stream: {err:#}")),
                }
            };
            let (serial, pdu) = (decoded.serial, decoded.pdu);
            if serial == 0 {
                match pdu {
                    Pdu::Ping(_) => {
                        let _ = self.send(&Pdu::Pong(codec::Pong {}), 0);
                    }
                    Pdu::FrontendAccessState(access) => {
                        self.0.lease.borrow_mut().apply_access(&access);
                        self.push(Pdu::FrontendAccessState(access));
                    }
                    Pdu::ClientViewportState(state) => {
                        self.0.lease.borrow_mut().apply_viewport(&state);
                        self.push(Pdu::ClientViewportState(state));
                    }
                    other => self.push(other),
                }
                continue;
            }
            let answer = self
                .0
                .serials
                .borrow_mut()
                .answer(serial, pdu, |tx, pdu| tx.send(pdu).is_ok());
            if answer == Answer::Unmatched {
                log::error!("the server answered serial {serial}, which nothing asked for");
            }
        }
    }

    fn push(&self, pdu: Pdu) {
        let handler = self.0.pushes.borrow_mut().take();
        match handler {
            Some(mut handler) => {
                handler(pdu);
                let mut slot = self.0.pushes.borrow_mut();
                if slot.is_none() {
                    *slot = Some(handler);
                }
                drop(slot);
                let pending = std::mem::take(&mut *self.0.pending_pushes.borrow_mut());
                for pdu in pending {
                    self.push(pdu);
                }
            }
            None => self.0.pending_pushes.borrow_mut().push(pdu),
        }
    }

    fn send(&self, pdu: &Pdu, serial: u64) -> Result<(), LinkError> {
        let mut bytes = Vec::new();
        pdu.encode(&mut bytes, serial).map_err(|e| LinkError {
            message: format!("encoding {}: {e:#}", pdu.pdu_name()),
            retryable: false,
        })?;
        let out = self.0.out.borrow();
        let Some(out) = out.as_ref().filter(|_| self.0.open.get()) else {
            return Err(LinkError {
                message: format!("sending {}: the connection is not open", pdu.pdu_name()),
                retryable: true,
            });
        };
        out.send(Out::Bytes(bytes)).map_err(|_| LinkError {
            message: format!("sending {}: the network thread is gone", pdu.pdu_name()),
            retryable: false,
        })
    }

    async fn ensure_owner_now(&self, tab_id: TabId) -> Result<bool> {
        {
            let mut lease = self.0.lease.borrow_mut();
            if lease.owns_viewport() {
                return Ok(true);
            }
            if !lease.may_type() {
                return Ok(false);
            }
            lease.fit = true;
        }
        self.claim_now(tab_id).await
    }

    async fn claim_now(&self, tab_id: TabId) -> Result<bool> {
        let viewport = self
            .0
            .lease
            .borrow()
            .claim_viewport()
            .ok_or_else(|| anyhow!("no viewport has been reported yet"))?;
        let (viewport, state) = match self.claim_with(tab_id, viewport.clone()).await {
            Ok(state) => (viewport, state),
            Err(err) if matches!(viewport, codec::ClientViewport::Native { .. }) => {
                log::warn!("pane-by-pane claim refused, claiming the grid: {err:#}");
                let grid = codec::ClientViewport::CellGrid {
                    size: viewport.size(),
                };
                let state = self.claim_with(tab_id, grid.clone()).await?;
                (grid, state)
            }
            Err(err) => return Err(err),
        };
        let mut lease = self.0.lease.borrow_mut();
        lease.reported_viewport = Some(viewport);
        lease.apply_viewport(&state);
        Ok(lease.owns_viewport())
    }

    async fn claim_with(
        &self,
        tab_id: TabId,
        viewport: codec::ClientViewport,
    ) -> Result<codec::ClientViewportState> {
        thinkterm_session::host::request(
            self,
            Pdu::ClaimClientViewport(codec::ClaimClientViewport { tab_id, viewport }),
            |pdu| match pdu {
                Pdu::ClientViewportState(state) => Ok(state),
                other => Err(other),
            },
        )
        .await
    }

    async fn report_viewport_now(&self, tab_id: TabId) -> Result<()> {
        let viewport = {
            let lease = self.0.lease.borrow();
            match lease.claim_viewport() {
                Some(v) if lease.reported_viewport.as_ref() != Some(&v) => v,
                _ => return Ok(()),
            }
        };
        let state = thinkterm_session::host::request(
            self,
            Pdu::SetClientViewport(codec::SetClientViewport {
                tab_id,
                viewport: viewport.clone(),
            }),
            |pdu| match pdu {
                Pdu::ClientViewportState(state) => Ok(state),
                other => Err(other),
            },
        )
        .await?;
        let mut lease = self.0.lease.borrow_mut();
        lease.reported_viewport = Some(viewport);
        lease.apply_viewport(&state);
        Ok(())
    }
}

impl PduLink for SshLink {
    type Request = LocalFuture<Result<Pdu, LinkError>>;

    fn request(&self, pdu: Pdu) -> Self::Request {
        let (tx, rx) = oneshot::channel();
        let serial = self.0.serials.borrow_mut().register(tx);
        let sent = self.send(&pdu, serial);
        if sent.is_err() {
            self.0
                .serials
                .borrow_mut()
                .answer(serial, Pdu::Invalid { ident: 0 }, |_, _| false);
        }
        Box::pin(async move {
            sent?;
            rx.await.map_err(|_| LinkError {
                message: "the connection closed before the server answered".to_string(),
                retryable: false,
            })
        })
    }

    fn is_reconnectable(&self) -> bool {
        true
    }

    fn connection_generation(&self) -> u64 {
        self.0.generation.get()
    }
}

impl PaneLink for SshLink {
    type Prepare = LocalFuture<Result<bool>>;

    fn prepare(&self, remote_tab_id: TabId) -> Self::Prepare {
        let link = self.clone();
        Box::pin(async move { link.ensure_owner_now(remote_tab_id).await })
    }
}

impl Link for SshLink {
    fn reconnect(&self) -> LocalFuture<Result<()>> {
        self.connect()
    }

    fn shutdown(&self) {
        if let Some(out) = self.0.out.borrow_mut().take() {
            let _ = out.send(Out::Close);
        }
        self.0.open.set(false);
        self.0.serials.borrow_mut().drain();
        // A reconnect task can still hold this link alive. Its dial must
        // finish without waiting for network events the core now discards.
        self.0.connecting.borrow_mut().take();
    }

    fn lease(&self) -> std::cell::Ref<'_, Lease> {
        self.0.lease.borrow()
    }

    fn lease_mut(&self) -> std::cell::RefMut<'_, Lease> {
        self.0.lease.borrow_mut()
    }

    fn set_push_handler(&self, handler: Box<dyn FnMut(Pdu)>) {
        *self.0.pushes.borrow_mut() = Some(handler);
        let pending = std::mem::take(&mut *self.0.pending_pushes.borrow_mut());
        for pdu in pending {
            self.push(pdu);
        }
    }

    fn set_close_handler(&self, handler: Box<dyn FnMut(String)>) {
        let mut handler = handler;
        if let Some(reason) = self.0.closed_early.borrow_mut().take() {
            handler(reason);
        }
        *self.0.on_close.borrow_mut() = Some(handler);
    }

    fn ensure_owner(&self, tab_id: TabId) -> LocalFuture<Result<bool>> {
        let link = self.clone();
        Box::pin(async move { link.ensure_owner_now(tab_id).await })
    }

    fn claim(&self, tab_id: TabId) -> LocalFuture<Result<bool>> {
        let link = self.clone();
        Box::pin(async move { link.claim_now(tab_id).await })
    }

    fn report_viewport(&self, tab_id: TabId) -> LocalFuture<Result<()>> {
        let link = self.clone();
        Box::pin(async move { link.report_viewport_now(tab_id).await })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::executor::LocalPool;
    use futures::task::LocalSpawnExt;

    #[test]
    fn shutdown_releases_a_dial_waiter_that_still_holds_the_link() {
        let link = test_link();
        let weak = Rc::downgrade(&link.0);
        // The pending response installed by connect, without a network thread.
        let (tx, rx) = oneshot::channel();
        *link.0.connecting.borrow_mut() = Some(tx);
        let finished = Rc::new(Cell::new(false));
        let mut pool = LocalPool::new();
        let retained = link.clone();
        let done = Rc::clone(&finished);
        pool.spawner().spawn_local(async move {
            assert!(rx.await.is_err());
            drop(retained);
            done.set(true);
        }).unwrap();
        pool.run_until_stalled();
        assert!(!finished.get());

        link.shutdown();
        drop(link);
        pool.run_until_stalled();
        assert!(finished.get());
        assert!(weak.upgrade().is_none());
    }
}
