//! A local admission socket, separate from the general mux PDU dispatcher.
//! The only authentication result is the host's own inherited native pipe.

use super::{Host, HostRoute, HostStatus, PaneChoice};
use crate::{client::ClientId, Mux};
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use smol::channel::{bounded, Receiver, Sender};
use smol::future::FutureExt;
use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Weak};
use std::time::Duration;
use thinkterm_reverse_access::budget::{Budgets, Reservation, RouteBudget};
use thinkterm_reverse_access::channel::{parse, Channel, Deadline, REPLY_MARGIN};
use thinkterm_reverse_access::exchange::{receive_input, receive_reply, send_payload};
use thinkterm_reverse_access::ipc::{LocalListener, LocalStream};
use thinkterm_reverse_access::protocol::{Command, Reply, TimedCommand};
use thinkterm_reverse_access::wire::{Assembly, Kind, Payload};
use thinkterm_reverse_access::{Error, GrantRef, Id, Permission, Result};

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub enum Hello {
    Manage {
        server_id: String,
        client: ClientId,
        source: String,
        /// Of the authentication window. Only picks one of its translations.
        language: String,
    },
    Data {
        route: Id,
    },
    Register {
        name: String,
    },
    Browse,
    Request {
        route: Id,
    },
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Bound {
    pub server_id: String,
    pub route: Option<Id>,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub enum Control {
    Status,
    Choices {
        after: Option<u64>,
    },
    Approve {
        panes: Vec<u64>,
        permission: Permission,
        seconds: u64,
        renewal: Option<GrantRef>,
    },
    ApproveAllLocal {
        permission: Permission,
        seconds: u64,
        renewal: Option<GrantRef>,
    },
    Revoke {
        grant: GrantRef,
    },
    Cancel,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub enum Controlled {
    Status(HostStatus),
    Choices {
        panes: Vec<PaneChoice>,
        after: Option<u64>,
    },
    Failed(thinkterm_reverse_access::protocol::Failure),
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RouterRoute {
    pub id: Id,
    pub name: String,
}

struct Managed {
    pid: u32,
    route: Weak<HostRoute>,
}

struct Routed {
    name: String,
    queue: Sender<Forward>,
    budget: RouteBudget,
}

struct Forward {
    command: Command,
    payload: Option<Payload>,
    reply: Sender<(Reply, Option<Payload>)>,
    deadline: Deadline,
    reservation: Reservation,
}

/// How long past a request's own deadline its reply is waited for. The
/// requester has gone by then, but the channel is serial: only that reply, or
/// giving up on the desktop altogether, puts it back in step. A slow
/// connection's buffers can hold this much of a reply the desktop sent in time.
const LATE_REPLY: Duration = Duration::from_secs(15);

pub struct Service {
    host: Arc<Host>,
    mux: Weak<Mux>,
    name: String,
    budgets: Budgets,
    managed: Mutex<HashMap<Id, Managed>>,
    routed: Mutex<HashMap<Id, Routed>>,
    task: Mutex<Option<smol::Task<()>>>,
    stop: Sender<()>,
    stopped: Receiver<()>,
    candidates: Arc<AtomicUsize>,
}

impl Service {
    pub fn start(mux: &Arc<Mux>) -> Result<Arc<Self>> {
        let listener = LocalListener::bind()?;
        let (stop, stopped) = bounded(1);
        let service = Arc::new(Self {
            host: Host::new(mux)?,
            mux: Arc::downgrade(mux),
            name: listener.name().into(),
            budgets: Budgets::process()?,
            managed: Mutex::new(HashMap::new()),
            routed: Mutex::new(HashMap::new()),
            task: Mutex::new(None),
            stop,
            stopped,
            candidates: Arc::new(AtomicUsize::new(0)),
        });
        let weak = Arc::downgrade(&service);
        *service.task.lock() = Some(smol::spawn(async move {
            loop {
                // A peer that could not be identified, say one that has
                // already gone, comes back as an error too: it must not stop
                // the listener for everyone after it.
                let Ok(stream) = listener.accept().await else {
                    smol::Timer::after(Duration::from_millis(100)).await;
                    continue;
                };
                // A burst of clients waits here and in the listen queue, each
                // within its own deadline, rather than being turned away.
                let (service, budget, reservation, admission) = loop {
                    let Some(service) = weak.upgrade() else {
                        return;
                    };
                    if let Some((budget, reservation, admission)) = service.admit() {
                        break (service, budget, reservation, admission);
                    }
                    drop(service);
                    smol::Timer::after(Duration::from_millis(5)).await;
                };
                smol::spawn(async move {
                    let stopped = service.stopped.clone();
                    let _ = service
                        .serve(stream, budget, reservation, admission)
                        .or(async {
                            let _ = stopped.recv().await;
                            Err(Error::Unavailable)
                        })
                        .await;
                })
                .detach();
            }
        }));
        Ok(service)
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    /// Reserving before spawning also caps accepted idle clients.
    fn admit(&self) -> Option<(RouteBudget, Reservation, Admission)> {
        self.candidates
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |count| {
                (count < 8).then_some(count + 1)
            })
            .ok()?;
        let admission = Admission(Arc::clone(&self.candidates));
        let budget = self.budgets.open_route(Id::random().ok()?).ok()?;
        let reservation = budget.acquire(1024 * 1024).ok()?;
        Some((budget, reservation, admission))
    }

    pub(crate) fn close(&self) {
        self.stop.close();
        self.host.close();
        self.task.lock().take();
    }

    async fn serve(
        self: Arc<Self>,
        stream: LocalStream,
        budget: RouteBudget,
        reservation: Reservation,
        admission: Admission,
    ) -> Result<()> {
        let peer = stream.peer().clone();
        let mut channel = Channel::new(stream, reservation)?;
        let handshake = Deadline::after(Duration::from_secs(5))?;
        let hello: Hello = parse(&channel.read(Some(&handshake)).await?, Kind::Hello, 0)?;
        drop(admission);
        let server_id = self
            .mux
            .upgrade()
            .ok_or(Error::Unavailable)?
            .runtime_server_id()
            .to_string();
        match hello {
            Hello::Manage {
                server_id: expected,
                client,
                source,
                language,
            } => {
                let executable = if cfg!(windows) {
                    "thinkterm-gui.exe"
                } else {
                    "thinkterm-gui"
                };
                if !peer.is_packaged(executable)
                    || peer.pid() != client.pid
                    || expected != server_id
                {
                    return Err(Error::Unavailable);
                }
                let route = self.host.open(client, &source, &language)?;
                route.heartbeat()?;
                self.managed.lock().insert(
                    route.id(),
                    Managed {
                        pid: peer.pid(),
                        route: Arc::downgrade(&route),
                    },
                );
                let guard = ManagedGuard {
                    service: Arc::downgrade(&self),
                    route: Arc::clone(&route),
                };
                channel
                    .write_json(
                        Kind::Bound,
                        0,
                        &Bound {
                            server_id,
                            route: Some(route.id()),
                        },
                        &handshake,
                    )
                    .await?;
                let mut sequence = 0;
                loop {
                    let deadline = Deadline::after(Duration::from_secs(4))?;
                    let frame = channel.read(Some(&deadline)).await?;
                    if frame.sequence() <= sequence {
                        return Err(Error::Replay);
                    }
                    sequence = frame.sequence();
                    let command: Control = parse(&frame, Kind::Request, sequence)?;
                    route.heartbeat()?;
                    let response = match command {
                        Control::Status => Ok(Controlled::Status(route.status())),
                        Control::Choices { after } => route
                            .choices(after)
                            .map(|(panes, after)| Controlled::Choices { panes, after }),
                        Control::Approve {
                            panes,
                            permission,
                            seconds,
                            renewal,
                        } => {
                            let route = Arc::clone(&route);
                            smol::unblock(move || {
                                route.begin_approval(&panes, permission, seconds, renewal)?;
                                Ok(Controlled::Status(route.status()))
                            })
                            .await
                        }
                        Control::ApproveAllLocal {
                            permission,
                            seconds,
                            renewal,
                        } => {
                            let route = Arc::clone(&route);
                            smol::unblock(move || {
                                route.begin_all_local_approval(permission, seconds, renewal)?;
                                Ok(Controlled::Status(route.status()))
                            })
                            .await
                        }
                        Control::Revoke { grant } => route
                            .revoke(grant)
                            .map(|_| Controlled::Status(route.status())),
                        Control::Cancel => {
                            route.cancel_approval();
                            Ok(Controlled::Status(route.status()))
                        }
                    }
                    .unwrap_or_else(|error: Error| Controlled::Failed(error.into()));
                    channel
                        .write_json(Kind::Reply, sequence, &response, &Deadline::request()?)
                        .await?;
                    let _ = &guard;
                }
            }
            Hello::Data { route } => {
                let executable = if cfg!(windows) {
                    "thinkterm-gui.exe"
                } else {
                    "thinkterm-gui"
                };
                if !peer.is_packaged(executable) {
                    return Err(Error::Unavailable);
                }
                let route = self
                    .managed
                    .lock()
                    .get(&route)
                    .filter(|managed| managed.pid == peer.pid())
                    .and_then(|managed| managed.route.upgrade())
                    .ok_or(Error::Unavailable)?;
                channel
                    .write_json(
                        Kind::Bound,
                        0,
                        &Bound {
                            server_id,
                            route: Some(route.id()),
                        },
                        &handshake,
                    )
                    .await?;
                loop {
                    let frame = channel.read(None).await?;
                    let sequence = frame.sequence();
                    let timed: TimedCommand = parse(&frame, Kind::Request, sequence)?;
                    let (command, deadline) = timed.receive()?;
                    let payload =
                        receive_input(&mut channel, &command, sequence, &budget, &deadline).await?;
                    let disconnected = channel.watch_peer();
                    let result = async {
                        let prepared = async {
                            let operation = deadline.earlier(REPLY_MARGIN)?;
                            let reply = route
                                .prepare_reply(sequence, command, payload?, &operation, &deadline)
                                .await?;
                            Ok::<_, Error>((reply, operation))
                        }
                        .await;
                        match prepared {
                            Ok((reply, operation)) => {
                                reply
                                    .send(&mut channel, sequence, &operation, &deadline)
                                    .await
                            }
                            Err(error) => {
                                channel
                                    .write_json(
                                        Kind::Reply,
                                        sequence,
                                        &Reply::Failed(error.into()),
                                        &deadline,
                                    )
                                    .await
                            }
                        }
                    }
                    .or(disconnected)
                    .await;
                    if result.is_err() {
                        route.close();
                        return result;
                    }
                }
            }
            Hello::Register { name } => {
                if name.is_empty()
                    || name.len() > 48
                    || name.chars().any(|c| !c.is_ascii_alphanumeric() && c != '-')
                {
                    return Err(Error::Protocol);
                }
                let id = Id::random()?;
                let (sender, receiver) = bounded(8);
                {
                    let mut routes = self.routed.lock();
                    if routes.len() >= thinkterm_reverse_access::MAX_ROUTES {
                        return Err(Error::Busy);
                    }
                    let suffix: String = id.as_bytes()[..4]
                        .iter()
                        .map(|b| format!("{b:02x}"))
                        .collect();
                    routes.insert(
                        id,
                        Routed {
                            name: format!("{name}-{suffix}"),
                            queue: sender,
                            budget: budget.clone(),
                        },
                    );
                }
                let _guard = RoutedGuard {
                    service: Arc::downgrade(&self),
                    id,
                };
                channel
                    .write_json(
                        Kind::Bound,
                        0,
                        &Bound {
                            server_id,
                            route: Some(id),
                        },
                        &handshake,
                    )
                    .await?;
                route_requests(&mut channel, receiver).await
            }
            Hello::Browse => {
                channel
                    .write_json(
                        Kind::Bound,
                        0,
                        &Bound {
                            server_id,
                            route: None,
                        },
                        &handshake,
                    )
                    .await?;
                let routes: Vec<_> = self
                    .routed
                    .lock()
                    .iter()
                    .map(|(id, route)| RouterRoute {
                        id: *id,
                        name: route.name.clone(),
                    })
                    .collect();
                channel
                    .write_json(Kind::Reply, 1, &routes, &handshake)
                    .await?;
                acknowledge(&mut channel, 1, &handshake).await
            }
            Hello::Request { route } => {
                let (queue, route_budget) = self
                    .routed
                    .lock()
                    .get(&route)
                    .map(|route| (route.queue.clone(), route.budget.clone()))
                    .ok_or(Error::Unavailable)?;
                channel
                    .write_json(
                        Kind::Bound,
                        0,
                        &Bound {
                            server_id,
                            route: Some(route),
                        },
                        &handshake,
                    )
                    .await?;
                let deadline = Deadline::request()?;
                let frame = channel.read(Some(&deadline)).await?;
                let timed: TimedCommand = parse(&frame, Kind::Request, 1)?;
                let (command, remote_deadline) = timed.receive()?;
                let deadline =
                    Deadline::after(deadline.remaining()?.min(remote_deadline.remaining()?))?;
                let payload = receive_input(&mut channel, &command, 1, &budget, &deadline).await?;
                let (reply, receiver) = bounded(1);
                // Over the limit is an answer the requester can act on.
                let queued = payload.and_then(|payload| {
                    let forward = Forward {
                        command,
                        payload,
                        reply,
                        deadline: deadline.clone(),
                        reservation: route_budget.acquire(64 * 1024)?,
                    };
                    queue.try_send(forward).map_err(|error| {
                        if error.is_full() {
                            Error::Busy
                        } else {
                            Error::Unavailable
                        }
                    })
                });
                if let Err(error) = queued {
                    channel
                        .write_json(Kind::Reply, 1, &Reply::Failed(error.into()), &deadline)
                        .await?;
                    return acknowledge(&mut channel, 1, &deadline).await;
                }
                let disconnected = channel.watch_peer();
                let result = async {
                    let (reply, payload) = deadline
                        .run(async { receiver.recv().await.map_err(|_| Error::Unavailable) })
                        .await?;
                    channel
                        .write_json(Kind::Reply, 1, &reply, &deadline)
                        .await?;
                    if let Some(payload) = payload {
                        send_payload(
                            &mut channel,
                            payload.text().as_bytes(),
                            1,
                            Kind::TextBlock,
                            &deadline,
                        )
                        .await?;
                    }
                    Ok(())
                }
                .or(disconnected)
                .await;
                receiver.close();
                result?;
                acknowledge(&mut channel, 1, &deadline).await
            }
        }
    }
}

struct Admission(Arc<AtomicUsize>);
impl Drop for Admission {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::AcqRel);
    }
}

struct ManagedGuard {
    service: Weak<Service>,
    route: Arc<HostRoute>,
}
impl Drop for ManagedGuard {
    fn drop(&mut self) {
        self.route.close();
        if let Some(service) = self.service.upgrade() {
            service.managed.lock().remove(&self.route.id());
        }
    }
}

struct RoutedGuard {
    service: Weak<Service>,
    id: Id,
}
impl Drop for RoutedGuard {
    fn drop(&mut self) {
        if let Some(service) = self.service.upgrade() {
            service.routed.lock().remove(&self.id);
        }
    }
}

/// One exchange at a time over the desktop's channel. Once a request is on
/// it, only its reply puts the channel back in step, so the exchange always
/// runs to that reply, even if nobody is left to pass it to. Its requester
/// going, a limit, or a refusal ends that request. This returns, and the
/// desktop's authorization with it, only for a channel that no longer works.
async fn route_requests(
    channel: &mut Channel<LocalStream>,
    queue: Receiver<Forward>,
) -> Result<()> {
    let mut sequence = 0u64;
    loop {
        let disconnected = channel.watch_peer();
        let request = async { queue.recv().await.map_err(|_| Error::Unavailable).map(Some) }
            .or(async {
                disconnected.await?;
                Ok(None)
            })
            .await?
            .ok_or(Error::Unavailable)?;
        let Forward {
            command,
            payload,
            reply,
            deadline,
            reservation,
        } = request;
        if reply.is_closed() || deadline.remaining().is_err() {
            continue;
        }
        sequence = sequence
            .checked_add(1)
            .filter(|n| *n != u64::MAX)
            .ok_or(Error::Replay)?;
        let timed = deadline
            .earlier(REPLY_MARGIN)
            .and_then(|forwarded| TimedCommand::new(command, &forwarded));
        let timed = match timed {
            Ok(timed) => timed,
            Err(error) => {
                let _ = reply.try_send((Reply::Failed(error.into()), None));
                continue;
            }
        };
        channel
            .write_json(Kind::Request, sequence, &timed, &deadline)
            .await?;
        if let Some(payload) = &payload {
            send_payload(
                channel,
                payload.text().as_bytes(),
                sequence,
                Kind::InputBlock,
                &deadline,
            )
            .await?;
        }
        let late = Deadline::after(deadline.remaining().unwrap_or_default() + LATE_REPLY)?;
        let received = receive_reply(
            channel,
            sequence,
            |bytes| Assembly::reply_text(bytes, reservation),
            &late,
        )
        .await?;
        let _ =
            reply.try_send(received.unwrap_or_else(|error| (Reply::Failed(error.into()), None)));
    }
}

async fn acknowledge(
    channel: &mut Channel<LocalStream>,
    sequence: u64,
    deadline: &Deadline,
) -> Result<()> {
    let frame = channel.read(Some(deadline)).await?;
    if frame.kind() != Kind::Cancel || frame.sequence() != sequence || !frame.body().is_empty() {
        return Err(Error::Protocol);
    }
    Ok(())
}

#[cfg(all(test, any(target_os = "macos", target_os = "linux", windows)))]
mod tests {
    use super::*;
    use thinkterm_reverse_access::budget::ROUTE_BYTES;
    use thinkterm_reverse_access::protocol::{
        ExportRef, Failure, InputResult, InputStatus, ReadOptions,
    };
    use thinkterm_reverse_access::wire::Frame;
    use thinkterm_reverse_access::Target;

    async fn open(service: &Service, hello: Hello, budget: &RouteBudget) -> Channel<LocalStream> {
        let stream = LocalStream::connect(service.name()).await.unwrap();
        let mut channel = Channel::new(stream, budget.acquire(1024 * 1024).unwrap()).unwrap();
        channel
            .write_json(Kind::Hello, 0, &hello, &Deadline::request().unwrap())
            .await
            .unwrap();
        channel
    }

    async fn bind(channel: &mut Channel<LocalStream>) -> Bound {
        parse(
            &channel
                .read(Some(&Deadline::request().unwrap()))
                .await
                .unwrap(),
            Kind::Bound,
            0,
        )
        .unwrap()
    }

    async fn receive(
        request: &mut Channel<LocalStream>,
        budget: &RouteBudget,
        deadline: &Deadline,
    ) -> (Reply, Option<Payload>) {
        receive_reply(request, 1, |bytes| Assembly::text(bytes, budget), deadline)
            .await
            .unwrap()
            .unwrap()
    }

    enum DesktopSays {
        Reply(Reply),
        Text(String),
        Abandoned,
    }

    /// A request through the route, answered as `answer` says.
    async fn ask(
        service: &Service,
        route: Id,
        budget: &RouteBudget,
        desktop: &mut Channel<LocalStream>,
        sequence: u64,
        command: Command,
        answer: DesktopSays,
    ) -> (Reply, Option<Payload>) {
        let mut request = open(service, Hello::Request { route }, budget).await;
        bind(&mut request).await;
        let deadline = Deadline::request().unwrap();
        request
            .write_json(
                Kind::Request,
                1,
                &TimedCommand::new(command, &deadline).unwrap(),
                &deadline,
            )
            .await
            .unwrap();
        let on_the_desktop = async {
            let frame = desktop.read(Some(&deadline)).await.unwrap();
            assert_eq!(frame.sequence(), sequence);
            match answer {
                DesktopSays::Reply(reply) => {
                    desktop
                        .write_json(Kind::Reply, sequence, &reply, &deadline)
                        .await
                        .unwrap();
                }
                DesktopSays::Text(text) => {
                    let reply = Reply::Text { bytes: text.len() };
                    desktop
                        .write_json(Kind::Reply, sequence, &reply, &deadline)
                        .await
                        .unwrap();
                    send_payload(
                        desktop,
                        text.as_bytes(),
                        sequence,
                        Kind::TextBlock,
                        &deadline,
                    )
                    .await
                    .unwrap();
                }
                DesktopSays::Abandoned => {
                    let reply = Reply::Text { bytes: 100_000 };
                    desktop
                        .write_json(Kind::Reply, sequence, &reply, &deadline)
                        .await
                        .unwrap();
                    send_payload(desktop, b"example", sequence, Kind::TextBlock, &deadline)
                        .await
                        .unwrap();
                    desktop
                        .write(&Frame::new(Kind::Cancel, sequence, &[]).unwrap(), &deadline)
                        .await
                        .unwrap();
                }
            }
        };
        let (_, received) =
            smol::future::zip(on_the_desktop, receive(&mut request, budget, &deadline)).await;
        request
            .write(&Frame::new(Kind::Cancel, 1, &[]).unwrap(), &deadline)
            .await
            .unwrap();
        received
    }

    #[test]
    fn local_router_preserves_payloads_and_outlives_what_goes_wrong_with_a_request() {
        smol::block_on(async {
            let mux = Arc::new(Mux::new(None));
            let service = mux.connect_service().unwrap();
            let budgets = Budgets::new().unwrap();
            let budget = budgets.open_route(Id::random().unwrap()).unwrap();

            // An ordinary process cannot become the authentication controller.
            let mut denied = open(
                &service,
                Hello::Manage {
                    server_id: mux.runtime_server_id().into(),
                    source: "server-a".into(),
                    language: "en-US".into(),
                    client: ClientId {
                        hostname: "devbox".into(),
                        username: "user".into(),
                        pid: std::process::id(),
                        epoch: 0,
                        id: 0,
                        ssh_auth_sock: None,
                    },
                },
                &budget,
            )
            .await;
            assert!(denied
                .read(Some(&Deadline::request().unwrap()))
                .await
                .is_err());
            drop(denied);
            assert!(service.managed.lock().is_empty());

            let mut desktop = open(
                &service,
                Hello::Register {
                    name: "desktop".into(),
                },
                &budget,
            )
            .await;
            let registered = bind(&mut desktop).await;
            let route = registered.route.unwrap();
            let mut browse = open(&service, Hello::Browse, &budget).await;
            assert_eq!(bind(&mut browse).await.server_id, mux.runtime_server_id());
            let routes: Vec<RouterRoute> = parse(
                &browse
                    .read(Some(&Deadline::request().unwrap()))
                    .await
                    .unwrap(),
                Kind::Reply,
                1,
            )
            .unwrap();
            assert_eq!(routes.len(), 1);
            assert_eq!(routes[0].id, route);
            assert!(routes[0].name.starts_with("desktop-"));
            browse
                .write(
                    &Frame::new(Kind::Cancel, 1, &[]).unwrap(),
                    &Deadline::request().unwrap(),
                )
                .await
                .unwrap();
            drop(browse);

            let export = ExportRef {
                grant: GrantRef {
                    id: Id::random().unwrap(),
                    version: 1,
                },
                target: Target::new(1, Id::random().unwrap()).unwrap(),
            };
            let output = "example text\n".repeat(10_000);
            let mut request = open(&service, Hello::Request { route }, &budget).await;
            assert_eq!(bind(&mut request).await.route, Some(route));
            let deadline = Deadline::request().unwrap();
            request
                .write_json(
                    Kind::Request,
                    1,
                    &TimedCommand::new(
                        Command::Read {
                            export,
                            options: ReadOptions::default(),
                        },
                        &deadline,
                    )
                    .unwrap(),
                    &deadline,
                )
                .await
                .unwrap();
            let desktop_reply = async {
                let frame = desktop.read(Some(&deadline)).await.unwrap();
                assert_eq!(frame.sequence(), 1);
                let timed: TimedCommand = parse(&frame, Kind::Request, 1).unwrap();
                assert!(matches!(timed.receive().unwrap().0, Command::Read { .. }));
                desktop
                    .write_json(
                        Kind::Reply,
                        1,
                        &Reply::Text {
                            bytes: output.len(),
                        },
                        &deadline,
                    )
                    .await
                    .unwrap();
                send_payload(
                    &mut desktop,
                    output.as_bytes(),
                    1,
                    Kind::TextBlock,
                    &deadline,
                )
                .await
                .unwrap();
            };
            let client_reply = async {
                let (reply, payload) = receive(&mut request, &budget, &deadline).await;
                assert!(matches!(reply, Reply::Text { bytes } if bytes == output.len()));
                assert_eq!(payload.unwrap().text(), output);
                request
                    .write(&Frame::new(Kind::Cancel, 1, &[]).unwrap(), &deadline)
                    .await
                    .unwrap();
            };
            smol::future::zip(desktop_reply, client_reply).await;
            drop(request);

            let input = "example input\n".repeat(2000);
            let command = Command::Text {
                export,
                bytes: input.len(),
                no_paste: false,
                enter: true,
            };
            let mut request = open(&service, Hello::Request { route }, &budget).await;
            bind(&mut request).await;
            let deadline = Deadline::request().unwrap();
            request
                .write_json(
                    Kind::Request,
                    1,
                    &TimedCommand::new(command.clone(), &deadline).unwrap(),
                    &deadline,
                )
                .await
                .unwrap();
            send_payload(
                &mut request,
                input.as_bytes(),
                1,
                Kind::InputBlock,
                &deadline,
            )
            .await
            .unwrap();
            let desktop_reply = async {
                let frame = desktop.read(Some(&deadline)).await.unwrap();
                assert_eq!(frame.sequence(), 2);
                let timed: TimedCommand = parse(&frame, Kind::Request, 2).unwrap();
                let (command, _) = timed.receive().unwrap();
                assert!(matches!(command, Command::Text { enter: true, .. }));
                let payload = receive_input(&mut desktop, &command, 2, &budget, &deadline)
                    .await
                    .unwrap()
                    .unwrap()
                    .unwrap();
                assert_eq!(payload.text(), input);
                desktop
                    .write_json(
                        Kind::Reply,
                        2,
                        &Reply::Input(InputResult {
                            status: InputStatus::Submitted,
                            bytes: input.len(),
                            enter_submitted: true,
                            paste_may_be_open: false,
                        }),
                        &deadline,
                    )
                    .await
                    .unwrap();
            };
            let client_reply = async {
                let (reply, _) = receive(&mut request, &budget, &deadline).await;
                assert!(matches!(
                    reply,
                    Reply::Input(InputResult {
                        status: InputStatus::Submitted,
                        enter_submitted: true,
                        ..
                    })
                ));
                request
                    .write(&Frame::new(Kind::Cancel, 1, &[]).unwrap(), &deadline)
                    .await
                    .unwrap();
            };
            smol::future::zip(desktop_reply, client_reply).await;
            drop(request);

            // A requester that goes takes its result with it, and the route
            // stays: nobody else receives that result or repeats its request.
            let mut request = open(&service, Hello::Request { route }, &budget).await;
            bind(&mut request).await;
            let deadline = Deadline::request().unwrap();
            let list = Command::List { after: None };
            request
                .write_json(
                    Kind::Request,
                    1,
                    &TimedCommand::new(list.clone(), &deadline).unwrap(),
                    &deadline,
                )
                .await
                .unwrap();
            assert_eq!(desktop.read(Some(&deadline)).await.unwrap().sequence(), 3);
            drop(request);
            desktop
                .write_json(
                    Kind::Reply,
                    3,
                    &Reply::List {
                        exports: vec![],
                        more: false,
                    },
                    &deadline,
                )
                .await
                .unwrap();
            let (reply, _) = ask(
                &service,
                route,
                &budget,
                &mut desktop,
                4,
                list.clone(),
                DesktopSays::Reply(Reply::Failed(Failure::Timeout)),
            )
            .await;
            assert!(matches!(reply, Reply::Failed(Failure::Timeout)));

            // Text there is no room for is read off the channel and refused.
            let room = service.routed.lock().get(&route).unwrap().budget.clone();
            let held = room.acquire(ROUTE_BYTES - 1024 * 1024 - 64 * 1024).unwrap();
            let (reply, _) = ask(
                &service,
                route,
                &budget,
                &mut desktop,
                5,
                list.clone(),
                DesktopSays::Text("example".into()),
            )
            .await;
            assert!(matches!(reply, Reply::Failed(Failure::Busy)));
            drop(held);

            // A desktop can abandon a reply between two blocks.
            let (reply, _) = ask(
                &service,
                route,
                &budget,
                &mut desktop,
                6,
                list.clone(),
                DesktopSays::Abandoned,
            )
            .await;
            assert!(matches!(reply, Reply::Failed(Failure::Unavailable)));

            // Over the rate limit the requester is told, and the desktop
            // never hears of it.
            while room.acquire(0).is_ok() {}
            let mut request = open(&service, Hello::Request { route }, &budget).await;
            bind(&mut request).await;
            let deadline = Deadline::request().unwrap();
            request
                .write_json(
                    Kind::Request,
                    1,
                    &TimedCommand::new(list.clone(), &deadline).unwrap(),
                    &deadline,
                )
                .await
                .unwrap();
            let (reply, _) = receive(&mut request, &budget, &deadline).await;
            assert!(matches!(reply, Reply::Failed(Failure::Busy)));
            drop(request);
            assert_eq!(service.routed.lock().len(), 1);
            drop(room);

            // Only a channel that no longer works ends the route.
            drop(desktop);
            let gone = Deadline::after(Duration::from_secs(1)).unwrap();
            while !service.routed.lock().is_empty() {
                gone.remaining().unwrap();
                smol::Timer::after(Duration::from_millis(10)).await;
            }
            service.close();
            let shutdown = Deadline::after(Duration::from_secs(1)).unwrap();
            while LocalStream::connect(service.name()).await.is_ok() {
                shutdown.remaining().unwrap();
                smol::Timer::after(Duration::from_millis(10)).await;
            }
            assert_eq!(budgets.retained().unwrap(), (0, 0));
        });
    }

    #[test]
    fn a_peer_that_vanishes_does_not_stop_the_listener() {
        smol::block_on(async {
            let mux = Arc::new(Mux::new(None));
            let service = mux.connect_service().unwrap();
            let budgets = Budgets::new().unwrap();
            let budget = budgets.open_route(Id::random().unwrap()).unwrap();
            // A client gone before it is accepted has no peer to identify.
            for _ in 0..20 {
                drop(std::os::unix::net::UnixStream::connect(service.name()).unwrap());
            }
            smol::Timer::after(Duration::from_millis(250)).await;
            let mut browse = open(&service, Hello::Browse, &budget).await;
            assert_eq!(bind(&mut browse).await.server_id, mux.runtime_server_id());
            drop(browse);
            service.close();
        });
    }
}
