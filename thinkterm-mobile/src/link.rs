//! The core thread's end of the PDU stream, whatever carries it.
//!
//! The network thread hands in bytes and takes out bytes; this side does
//! the framing, matches answers to requests by serial, hands pushes to the
//! App and keeps the lease. It is the transport-neutral 450 lines of the
//! browser's `link.rs`, minus the WebSocket.
//!
//! Sending is ordered by construction: `request` encodes and queues the
//! bytes on the spot, in the order it was called, and the network thread
//! writes the queue in that order. That is the `PduLink` contract ("on the
//! wire now, behind everything sent before it"); the plan's saturation
//! rules for a bounded queue come with the real Transport, not this probe.

use crate::ssh::Out;
use anyhow::{anyhow, Result};
use codec::Pdu;
use futures::channel::oneshot;
use std::cell::{Cell, RefCell};
use std::future::Future;
use std::pin::Pin;
use std::rc::Rc;
use thinkterm_proto::TabId;
use thinkterm_session::connection::{Answer, SerialTable};
use thinkterm_session::host::{LinkError, PduLink};
use thinkterm_session::input::PaneLink;
use thinkterm_web::lease::Lease;
use tokio::sync::mpsc::UnboundedSender;

pub type LocalFuture<T> = Pin<Box<dyn Future<Output = T> + 'static>>;

#[derive(Clone)]
pub struct SshLink(Rc<Inner>);

struct Inner {
    out: RefCell<Option<UnboundedSender<Out>>>,
    serials: RefCell<SerialTable<oneshot::Sender<Pdu>>>,
    pushes: RefCell<Option<Box<dyn FnMut(Pdu)>>>,
    pending_pushes: RefCell<Vec<Pdu>>,
    lease: RefCell<Lease>,
    inbound: RefCell<Vec<u8>>,
    open: Cell<bool>,
    generation: Cell<u64>,
}

impl SshLink {
    pub fn new() -> Self {
        Self(Rc::new(Inner {
            out: RefCell::new(None),
            serials: RefCell::new(SerialTable::new()),
            pushes: RefCell::new(None),
            pending_pushes: RefCell::new(Vec::new()),
            lease: RefCell::new(Lease::default()),
            inbound: RefCell::new(Vec::new()),
            open: Cell::new(false),
            generation: Cell::new(0),
        }))
    }

    /// A transport came up: everything sent from now on goes to `out`.
    pub fn opened(&self, out: UnboundedSender<Out>) {
        self.0.inbound.borrow_mut().clear();
        self.0.serials.borrow_mut().drain();
        *self.0.out.borrow_mut() = Some(out);
        self.0.open.set(true);
        self.0.generation.set(self.0.generation.get() + 1);
    }

    /// The transport went away. Every request still waiting is answered
    /// with an error by its sender being dropped.
    pub fn closed(&self) {
        self.0.open.set(false);
        *self.0.out.borrow_mut() = None;
        self.0.serials.borrow_mut().drain();
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
                    Err(err) => {
                        self.closed();
                        return Err(anyhow!("decoding the stream: {err:#}"));
                    }
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

    pub fn lease(&self) -> std::cell::Ref<'_, Lease> {
        self.0.lease.borrow()
    }

    pub fn lease_mut(&self) -> std::cell::RefMut<'_, Lease> {
        self.0.lease.borrow_mut()
    }

    /// The pushes that arrived since the last call, in order. The core
    /// loop drains these after every `feed` rather than installing a
    /// handler, so nothing re-enters the App from inside the decoder.
    pub fn take_pushes(&self) -> Vec<Pdu> {
        std::mem::take(&mut *self.0.pending_pushes.borrow_mut())
    }

    #[allow(dead_code)]
    pub fn set_push_handler(&self, handler: impl FnMut(Pdu) + 'static) {
        *self.0.pushes.borrow_mut() = Some(Box::new(handler));
        let pending = std::mem::take(&mut *self.0.pending_pushes.borrow_mut());
        for pdu in pending {
            self.push(pdu);
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

    pub async fn ensure_owner(&self, tab_id: TabId) -> Result<bool> {
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
        self.claim(tab_id).await
    }

    pub async fn claim(&self, tab_id: TabId) -> Result<bool> {
        let viewport = self
            .0
            .lease
            .borrow()
            .claim_viewport()
            .ok_or_else(|| anyhow!("no viewport has been reported yet"))?;
        let state = self.claim_with(tab_id, viewport.clone()).await?;
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

    pub async fn report_viewport(&self, tab_id: TabId) -> Result<()> {
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
        Box::pin(async move { link.ensure_owner(remote_tab_id).await })
    }
}
