//! The WebSocket as the session's `PduLink`: one connection, requests
//! matched to answers by serial, server pushes handed to the app, and the
//! lease bookkeeping that decides whether this browser may type.

use crate::host::LocalFuture;
use thinkterm_proto::TabId;
use anyhow::{anyhow, Result};
use codec::{DecodedPdu, Pdu};
use futures::channel::oneshot;
use futures::io::{AsyncRead, AsyncWrite};
use std::cell::{Cell, RefCell};
use std::collections::VecDeque;
use std::pin::Pin;
use std::rc::Rc;
use std::task::{Context, Poll, Waker};
use thinkterm_session::connection::{Answer, SerialTable};
use thinkterm_session::host::{LinkError, PduLink};
use thinkterm_session::input::PaneLink;
use wasm_bindgen::prelude::*;
use wasm_bindgen::JsCast;
use web_sys::{BinaryType, MessageEvent, WebSocket};

pub const SUBPROTOCOL: &str = "thinkterm.v1";
pub const TOKEN_PROTOCOL_PREFIX: &str = "tt-token.";

/// The socket's bytes, in order, as the codec's async reader wants them.
struct Socket {
    ws: WebSocket,
    incoming: RefCell<VecDeque<u8>>,
    waker: RefCell<Option<Waker>>,
    open: Cell<bool>,
    closed: Cell<bool>,
    close_reason: RefCell<String>,
    _closures: RefCell<Vec<Closure<dyn FnMut(JsValue)>>>,
}

#[derive(Clone)]
struct SocketReader(Rc<Socket>);

impl std::fmt::Debug for SocketReader {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SocketReader")
            .field("buffered", &self.0.incoming.borrow().len())
            .field("closed", &self.0.closed.get())
            .finish()
    }
}

impl AsyncRead for SocketReader {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut [u8],
    ) -> Poll<std::io::Result<usize>> {
        let mut queue = self.0.incoming.borrow_mut();
        if queue.is_empty() {
            if self.0.closed.get() {
                return Poll::Ready(Ok(0));
            }
            *self.0.waker.borrow_mut() = Some(cx.waker().clone());
            return Poll::Pending;
        }
        let n = queue.len().min(buf.len());
        for (dst, src) in buf.iter_mut().zip(queue.drain(..n)) {
            *dst = src;
        }
        Poll::Ready(Ok(n))
    }
}

impl AsyncWrite for SocketReader {
    fn poll_write(self: Pin<&mut Self>, _: &mut Context<'_>, buf: &[u8]) -> Poll<std::io::Result<usize>> {
        match self.0.ws.send_with_u8_array(buf) {
            Ok(()) => Poll::Ready(Ok(buf.len())),
            Err(e) => Poll::Ready(Err(std::io::Error::new(
                std::io::ErrorKind::BrokenPipe,
                format!("{e:?}"),
            ))),
        }
    }
    fn poll_flush(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Poll::Ready(Ok(()))
    }
    fn poll_close(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        let _ = self.0.ws.close();
        Poll::Ready(Ok(()))
    }
}

use crate::lease::Lease;

struct Inner {
    socket: Rc<Socket>,
    serials: RefCell<SerialTable<oneshot::Sender<Pdu>>>,
    pushes: RefCell<Option<Box<dyn FnMut(Pdu)>>>,
    /// Pushes that arrived before a handler was installed (the server
    /// pushes from the first subscription on, while the page is still
    /// setting up), replayed in order once one is. The server never
    /// resends a push, so none may be dropped.
    pending_pushes: RefCell<Vec<Pdu>>,
    on_close: RefCell<Option<Box<dyn FnOnce(String)>>>,
    /// The close that happened before a handler was installed.
    closed_early: RefCell<Option<String>>,
    pub lease: RefCell<Lease>,
}

/// A handle on the connection; clone freely.
#[derive(Clone)]
pub struct WsLink(Rc<Inner>);

impl WsLink {
    /// Open the socket with the token in the subprotocol list, and wait for
    /// the server to accept it.
    pub async fn connect(url: &str, token: &str) -> Result<Self> {
        let protocols = js_sys::Array::new();
        protocols.push(&JsValue::from_str(SUBPROTOCOL));
        protocols.push(&JsValue::from_str(&format!("{TOKEN_PROTOCOL_PREFIX}{token}")));
        let ws = WebSocket::new_with_str_sequence(url, &protocols)
            .map_err(|e| anyhow!("WebSocket::new: {e:?}"))?;
        ws.set_binary_type(BinaryType::Arraybuffer);
        let socket = Rc::new(Socket {
            ws,
            incoming: RefCell::new(VecDeque::new()),
            waker: RefCell::new(None),
            open: Cell::new(false),
            closed: Cell::new(false),
            close_reason: RefCell::new(String::new()),
            _closures: RefCell::new(Vec::new()),
        });
        let wake = |s: &Rc<Socket>| {
            if let Some(w) = s.waker.borrow_mut().take() {
                w.wake();
            }
        };
        let mut closures = Vec::new();
        {
            let s = socket.clone();
            let c = Closure::<dyn FnMut(JsValue)>::new(move |ev: JsValue| {
                let ev: MessageEvent = ev.unchecked_into();
                if let Ok(buf) = ev.data().dyn_into::<js_sys::ArrayBuffer>() {
                    let bytes = js_sys::Uint8Array::new(&buf).to_vec();
                    s.incoming.borrow_mut().extend(bytes);
                    wake(&s);
                }
            });
            socket.ws.set_onmessage(Some(c.as_ref().unchecked_ref()));
            closures.push(c);
        }
        {
            let s = socket.clone();
            let c = Closure::<dyn FnMut(JsValue)>::new(move |_| {
                s.open.set(true);
                wake(&s);
            });
            socket.ws.set_onopen(Some(c.as_ref().unchecked_ref()));
            closures.push(c);
        }
        {
            let s = socket.clone();
            let c = Closure::<dyn FnMut(JsValue)>::new(move |ev: JsValue| {
                let reason = match ev.dyn_into::<web_sys::CloseEvent>() {
                    Ok(close) => format!("closed ({}{})", close.code(), {
                        let r = close.reason();
                        if r.is_empty() { String::new() } else { format!(": {r}") }
                    }),
                    Err(_) => "closed".to_string(),
                };
                *s.close_reason.borrow_mut() = reason;
                s.closed.set(true);
                wake(&s);
            });
            socket.ws.set_onclose(Some(c.as_ref().unchecked_ref()));
            closures.push(c);
        }
        {
            let s = socket.clone();
            let c = Closure::<dyn FnMut(JsValue)>::new(move |_| {
                if s.close_reason.borrow().is_empty() {
                    *s.close_reason.borrow_mut() = "connection error".to_string();
                }
                s.closed.set(true);
                wake(&s);
            });
            socket.ws.set_onerror(Some(c.as_ref().unchecked_ref()));
            closures.push(c);
        }
        *socket._closures.borrow_mut() = closures;

        futures::future::poll_fn(|cx| {
            if socket.open.get() {
                Poll::Ready(Ok(()))
            } else if socket.closed.get() {
                Poll::Ready(Err(anyhow!(
                    "the server refused the connection ({}); the token may be wrong or expired",
                    socket.close_reason.borrow()
                )))
            } else {
                *socket.waker.borrow_mut() = Some(cx.waker().clone());
                Poll::Pending
            }
        })
        .await?;
        if socket.ws.protocol() != SUBPROTOCOL {
            anyhow::bail!("the server did not select the {SUBPROTOCOL} subprotocol");
        }

        let link = Self(Rc::new(Inner {
            socket,
            serials: RefCell::new(SerialTable::new()),
            pushes: RefCell::new(None),
            pending_pushes: RefCell::new(Vec::new()),
            on_close: RefCell::new(None),
            closed_early: RefCell::new(None),
            lease: RefCell::new(Lease::default()),
        }));
        link.spawn_reader();
        Ok(link)
    }

    pub fn lease(&self) -> std::cell::Ref<'_, Lease> {
        self.0.lease.borrow()
    }

    pub fn lease_mut(&self) -> std::cell::RefMut<'_, Lease> {
        self.0.lease.borrow_mut()
    }

    /// Server-initiated PDUs (serial 0) go here, starting with whatever
    /// arrived before the handler did.
    pub fn set_push_handler(&self, handler: impl FnMut(Pdu) + 'static) {
        *self.0.pushes.borrow_mut() = Some(Box::new(handler));
        let pending = std::mem::take(&mut *self.0.pending_pushes.borrow_mut());
        for pdu in pending {
            self.push(pdu);
        }
    }

    /// Told once, when the socket ends -- at once, if it already has.
    pub fn set_close_handler(&self, handler: impl FnOnce(String) + 'static) {
        if let Some(reason) = self.0.closed_early.borrow_mut().take() {
            handler(reason);
            return;
        }
        *self.0.on_close.borrow_mut() = Some(Box::new(handler));
    }

    fn send(&self, pdu: &Pdu, serial: u64) -> Result<(), LinkError> {
        let mut bytes = Vec::new();
        pdu.encode(&mut bytes, serial).map_err(|e| LinkError {
            message: format!("encoding {}: {e:#}", pdu.pdu_name()),
            retryable: false,
        })?;
        self.0
            .socket
            .ws
            .send_with_u8_array(&bytes)
            .map_err(|e| LinkError {
                message: format!("sending {}: {e:?}", pdu.pdu_name()),
                retryable: false,
            })
    }

    fn spawn_reader(&self) {
        let link = self.clone();
        wasm_bindgen_futures::spawn_local(async move {
            let mut reader = SocketReader(link.0.socket.clone());
            loop {
                let DecodedPdu { serial, pdu } = match Pdu::decode_async(&mut reader, None).await {
                    Ok(d) => d,
                    Err(err) => {
                        let reason = {
                            let r = link.0.socket.close_reason.borrow().clone();
                            if r.is_empty() { format!("{err:#}") } else { r }
                        };
                        link.0.socket.closed.set(true);
                        // Every request still waiting fails now; the
                        // senders drop and the receivers see Canceled.
                        link.0.serials.borrow_mut().drain();
                        let on_close = link.0.on_close.borrow_mut().take();
                        match on_close {
                            Some(on_close) => on_close(reason),
                            None => *link.0.closed_early.borrow_mut() = Some(reason),
                        }
                        return;
                    }
                };
                if serial == 0 {
                    match pdu {
                        Pdu::Ping(_) => {
                            let _ = link.send(&Pdu::Pong(codec::Pong {}), 0);
                        }
                        Pdu::FrontendAccessState(access) => {
                            link.0.lease.borrow_mut().apply_access(&access);
                            link.push(Pdu::FrontendAccessState(access));
                        }
                        Pdu::ClientViewportState(state) => {
                            link.0.lease.borrow_mut().apply_viewport(&state);
                            link.push(Pdu::ClientViewportState(state));
                        }
                        other => link.push(other),
                    }
                    continue;
                }
                let answer = link
                    .0
                    .serials
                    .borrow_mut()
                    .answer(serial, pdu, |tx, pdu| tx.send(pdu).is_ok());
                if answer == Answer::Unmatched {
                    log::error!("the server answered serial {serial}, which nothing asked for");
                }
            }
        });
    }

    fn push(&self, pdu: Pdu) {
        // Taken out for the call so a handler that requests something (and
        // so re-enters this link) never finds the slot borrowed. While it
        // is out, or before one exists, pushes queue.
        let handler = self.0.pushes.borrow_mut().take();
        match handler {
            Some(mut handler) => {
                handler(pdu);
                let mut slot = self.0.pushes.borrow_mut();
                if slot.is_none() {
                    *slot = Some(handler);
                }
                drop(slot);
                // Anything that queued while the handler was out.
                let pending = std::mem::take(&mut *self.0.pending_pushes.borrow_mut());
                for pdu in pending {
                    self.push(pdu);
                }
            }
            None => self.0.pending_pushes.borrow_mut().push(pdu),
        }
    }

    /// Become the tab's viewport owner, if this browser is not already.
    /// Called before input goes out, and answered from what the server
    /// says about ownership afterwards.
    pub async fn ensure_owner(&self, tab_id: TabId) -> Result<bool> {
        if self.0.lease.borrow().owns_viewport() {
            return Ok(true);
        }
        let size = {
            let lease = self.0.lease.borrow();
            lease.reported.or(lease.canonical_size)
        }
        .ok_or_else(|| anyhow!("no viewport has been reported yet"))?;
        let state = thinkterm_session::host::request(
            self,
            Pdu::ClaimClientViewport(codec::ClaimClientViewport {
                tab_id,
                viewport: codec::ClientViewport::CellGrid { size },
            }),
            |pdu| match pdu {
                Pdu::ClientViewportState(state) => Ok(state),
                other => Err(other),
            },
        )
        .await?;
        let mut lease = self.0.lease.borrow_mut();
        lease.apply_viewport(&state);
        Ok(lease.owns_viewport())
    }
}

impl PduLink for WsLink {
    type Request = LocalFuture<Result<Pdu, LinkError>>;

    fn request(&self, pdu: Pdu) -> Self::Request {
        let (tx, rx) = oneshot::channel();
        let serial = self.0.serials.borrow_mut().register(tx);
        let sent = self.send(&pdu, serial);
        if sent.is_err() {
            // Nothing went out, so nothing will answer: do not leave a
            // waiter for a serial the server never saw.
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
        false
    }

    fn connection_generation(&self) -> u64 {
        1
    }
}

impl PaneLink for WsLink {
    type Prepare = LocalFuture<Result<bool>>;

    fn prepare(&self, remote_tab_id: TabId) -> Self::Prepare {
        let link = self.clone();
        Box::pin(async move { link.ensure_owner(remote_tab_id).await })
    }
}

