use crate::client::{Client, ClientConnectionPhase, VerifiedConnectSession};
use mux::connect::service::{Bound, Control, Controlled, Hello, RouterRoute};
use mux::connect::{HostStatus, PaneChoice};
use parking_lot::Mutex;
use smol::channel::{bounded, Sender};
use smol::future::FutureExt;
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Weak};
use std::time::Duration;
use thinkterm_reverse_access::budget::{Budgets, RouteBudget};
use thinkterm_reverse_access::channel::{parse, Channel, Deadline, REPLY_MARGIN};
use thinkterm_reverse_access::clock::{Clock, SystemClock, Tick};
use thinkterm_reverse_access::exchange::{
    discard_text, next_block, receive_input, receive_reply, send_payload, Refusable,
};
use thinkterm_reverse_access::ipc::LocalStream;
use thinkterm_reverse_access::protocol::{Command, ExportSummary, Failure, Reply, TimedCommand};
use thinkterm_reverse_access::wire::{Assembly, Frame, Kind, Payload};
use thinkterm_reverse_access::{Error, GrantRef, GrantScope, Id, Result};

#[derive(Clone)]
pub struct Endpoint {
    address: String,
    server_id: String,
}

impl Endpoint {
    pub async fn for_client(client: &Client) -> Result<Self> {
        if !client.is_local {
            return Err(Error::Unavailable);
        }
        let info = Deadline::after(Duration::from_secs(5))?
            .run(async {
                client
                    .get_connect_endpoint(codec::GetConnectEndpoint {})
                    .await
                    .map_err(|_| Error::Unavailable)
            })
            .await?;
        if client.remote_server_id().as_deref() != Some(info.server_id.as_str()) {
            return Err(Error::Unavailable);
        }
        Ok(Self {
            address: info.address,
            server_id: info.server_id,
        })
    }

    pub fn in_process(mux: &Arc<mux::Mux>) -> Result<Self> {
        Ok(Self {
            address: mux.connect_service()?.name().into(),
            server_id: mux.runtime_server_id().into(),
        })
    }

    pub async fn stream(&self) -> Result<LocalStream> {
        let stream = Deadline::after(Duration::from_secs(5))?
            .run(LocalStream::connect(&self.address))
            .await?;
        let gui = if cfg!(windows) {
            "thinkterm-gui.exe"
        } else {
            "thinkterm-gui"
        };
        let host = if cfg!(windows) {
            "thinkterm-mux-server.exe"
        } else {
            "thinkterm-mux-server"
        };
        if !stream.peer().is_packaged(gui) && !stream.peer().is_packaged(host) {
            return Err(Error::Unavailable);
        }
        Ok(stream)
    }

    async fn open(
        &self,
        hello: &Hello,
        budget: &RouteBudget,
    ) -> Result<(Channel<LocalStream>, Bound)> {
        let mut channel = Channel::new(self.stream().await?, budget.acquire(1024 * 1024)?)?;
        let deadline = Deadline::after(Duration::from_secs(5))?;
        channel.write_json(Kind::Hello, 0, hello, &deadline).await?;
        let bound: Bound = parse(&channel.read(Some(&deadline)).await?, Kind::Bound, 0)?;
        if bound.server_id != self.server_id {
            return Err(Error::Unavailable);
        }
        Ok((channel, bound))
    }
}

fn budget() -> Result<RouteBudget> {
    Budgets::process()?.open_route(Id::random()?)
}

pub struct RouterClient {
    endpoint: Endpoint,
    budget: RouteBudget,
}

impl RouterClient {
    pub async fn new(client: &Client) -> Result<Self> {
        Ok(Self {
            endpoint: Endpoint::for_client(client).await?,
            budget: budget()?,
        })
    }

    pub async fn routes(&self) -> Result<Vec<RouterRoute>> {
        let (mut channel, _) = self.endpoint.open(&Hello::Browse, &self.budget).await?;
        let deadline = Deadline::request()?;
        let routes: Vec<RouterRoute> =
            parse(&channel.read(Some(&deadline)).await?, Kind::Reply, 1)?;
        if routes.len() > thinkterm_reverse_access::MAX_ROUTES
            || routes.iter().any(|route| {
                route.name.len() > 64
                    || route.name.is_empty()
                    || route
                        .name
                        .chars()
                        .any(|c| !c.is_ascii_alphanumeric() && c != '-')
            })
        {
            return Err(Error::Protocol);
        }
        channel
            .write(&Frame::new(Kind::Cancel, 1, &[])?, &deadline)
            .await?;
        Ok(routes)
    }

    pub async fn request(
        &self,
        route: Id,
        command: Command,
        input: Option<&str>,
    ) -> Result<(Reply, Option<Payload>)> {
        let deadline = Deadline::request()?;
        command.validate()?;
        match (&command, input) {
            (Command::Text { bytes, .. }, Some(input)) if *bytes == input.len() => {}
            (Command::Text { .. }, _) => return Err(Error::Protocol),
            (_, None) => {}
            _ => return Err(Error::Protocol),
        }
        let (mut channel, bound) = deadline
            .run(self.endpoint.open(&Hello::Request { route }, &self.budget))
            .await?;
        if bound.route != Some(route) {
            return Err(Error::Unavailable);
        }
        let timed = TimedCommand::new(command, &deadline)?;
        channel
            .write_json(Kind::Request, 1, &timed, &deadline)
            .await?;
        if let Some(input) = input {
            send_payload(
                &mut channel,
                input.as_bytes(),
                1,
                Kind::InputBlock,
                &deadline,
            )
            .await?;
        }
        let result = receive_reply(
            &mut channel,
            1,
            |bytes| Assembly::text(bytes, &self.budget),
            &deadline,
        )
        .await??;
        channel
            .write(&Frame::new(Kind::Cancel, 1, &[])?, &deadline)
            .await?;
        Ok(result)
    }

    /// A list or a read changes nothing, so it can simply wait its turn when
    /// the route is at its rate limit. Input is never sent twice this way.
    pub async fn request_read(
        &self,
        route: Id,
        command: Command,
    ) -> Result<(Reply, Option<Payload>)> {
        let deadline = Deadline::request()?;
        loop {
            let result = self.request(route, command.clone(), None).await?;
            if !matches!(result.0, Reply::Failed(Failure::Busy)) {
                return Ok(result);
            }
            deadline
                .run(async {
                    smol::Timer::after(Duration::from_millis(150)).await;
                    Ok(())
                })
                .await?;
        }
    }

    pub async fn exports(&self, route: Id) -> Result<Vec<ExportSummary>> {
        let mut after = None;
        let mut all = Vec::new();
        loop {
            let (reply, _) = self.request_read(route, Command::List { after }).await?;
            let Reply::List { exports, more } = reply else {
                return Err(Error::Unavailable);
            };
            if all.len() + exports.len() > thinkterm_reverse_access::MAX_TARGETS {
                return Err(Error::TooLarge);
            }
            if exports.is_empty() && more {
                return Err(Error::Protocol);
            }
            let next = exports.last().map(|item| item.export);
            if next.is_some() && next == after {
                return Err(Error::Replay);
            }
            after = next;
            all.extend(exports);
            if !more {
                return Ok(all);
            }
        }
    }
}

/// The host binding is checked against its live mux connection on every
/// control heartbeat. A configuration flag alone cannot construct one.
pub struct HostBinding {
    endpoint: Endpoint,
    client_id: mux::client::ClientId,
    client: Option<(Client, u64)>,
    mux: Option<Weak<mux::Mux>>,
}

impl HostBinding {
    pub fn in_process(mux: &Arc<mux::Mux>) -> Result<Self> {
        Ok(Self {
            endpoint: Endpoint::in_process(mux)?,
            client_id: mux
                .active_identity()
                .ok_or(Error::Unavailable)?
                .as_ref()
                .clone(),
            client: None,
            mux: Some(Arc::downgrade(mux)),
        })
    }

    pub async fn local_session(client: Client) -> Result<Self> {
        if !client.is_local_session_host() || !client.is_local {
            return Err(Error::Unavailable);
        }
        let generation = client.connection_generation();
        let endpoint = Endpoint::for_client(&client).await?;
        Ok(Self {
            endpoint,
            client_id: client.client_id.clone(),
            client: Some((client, generation)),
            mux: None,
        })
    }

    fn alive(&self) -> bool {
        if let Some((client, generation)) = &self.client {
            client.connection_phase() == ClientConnectionPhase::Ready
                && client.connection_generation() == *generation
                && client.remote_server_id().as_deref() == Some(self.endpoint.server_id.as_str())
        } else {
            self.mux
                .as_ref()
                .and_then(Weak::upgrade)
                .is_some_and(|mux| mux.runtime_server_id() == self.endpoint.server_id)
        }
    }
}

#[derive(Clone, Default)]
pub struct ExportState {
    pub choices: Vec<PaneChoice>,
    pub status: HostStatus,
    pub error: Option<Error>,
    pub closed: bool,
}

pub struct Exporter {
    commands: Sender<Control>,
    state: Arc<Mutex<ExportState>>,
    output: Arc<OutputGuard>,
    _task: smol::Task<()>,
}

impl Exporter {
    pub async fn start(
        session: VerifiedConnectSession,
        host: HostBinding,
        language: String,
    ) -> Result<Self> {
        Self::start_scoped(session, host, GrantScope::SelectedTerminals, language).await
    }

    pub async fn start_all_local(
        session: VerifiedConnectSession,
        host: HostBinding,
        language: String,
    ) -> Result<Self> {
        Self::start_scoped(session, host, GrantScope::AllLocalTerminals, language).await
    }

    async fn start_scoped(
        session: VerifiedConnectSession,
        host: HostBinding,
        scope: GrantScope,
        language: String,
    ) -> Result<Self> {
        if !session.is_alive() || !host.alive() {
            return Err(Error::Unavailable);
        }
        let budget = budget()?;
        let (mut control, bound) = host
            .endpoint
            .open(
                &Hello::Manage {
                    server_id: host.endpoint.server_id.clone(),
                    client: host.client_id.clone(),
                    source: session.source().into(),
                    language,
                },
                &budget,
            )
            .await?;
        let route = bound.route.ok_or(Error::Protocol)?;
        let mut sequence = 0u64;
        let choices = if scope == GrantScope::SelectedTerminals {
            load_choices(&mut control, &mut sequence).await?
        } else {
            Vec::new()
        };
        let state = Arc::new(Mutex::new(ExportState {
            choices,
            ..ExportState::default()
        }));
        let worker_state = Arc::clone(&state);
        let output = Arc::new(OutputGuard::new()?);
        let worker_output = Arc::clone(&output);
        let (commands, receiver) = bounded(4);
        let task = smol::spawn(async move {
            let mut bridge: Option<smol::Task<()>> = None;
            let (failed, failure) = bounded(1);
            let result: Result<()> = async {
                loop {
                    let command = async { receiver.recv().await.map_err(|_| Error::Unavailable) }
                        .or(async {
                            smol::Timer::after(Duration::from_secs(1)).await;
                            Ok(Control::Status)
                        })
                        .or(async {
                            failure.recv().await.map_err(|_| Error::Unavailable)?;
                            Err(Error::Unavailable)
                        })
                        .await?;
                    if !session.is_alive() || !host.alive() {
                        return Err(Error::Unavailable);
                    }
                    if matches!(command, Control::Choices { .. }) {
                        let choices = load_choices(&mut control, &mut sequence).await?;
                        worker_state.lock().choices = choices;
                        continue;
                    }
                    sequence = sequence.checked_add(1).ok_or(Error::Replay)?;
                    let before = worker_output.clock.now()?;
                    let response = control_request(&mut control, sequence, &command).await?;
                    let has_grants = {
                        let mut state = worker_state.lock();
                        match response {
                            Controlled::Status(status) => {
                                worker_output.update(before, &status)?;
                                state.status = status;
                                state.error = None;
                            }
                            Controlled::Choices { .. } => return Err(Error::Protocol),
                            Controlled::Failed(_) => state.error = Some(Error::Unavailable),
                        }
                        !state.status.grants.is_empty()
                    };
                    if has_grants && bridge.is_none() {
                        let (data, data_bound) =
                            host.endpoint.open(&Hello::Data { route }, &budget).await?;
                        if data_bound.route != Some(route) {
                            return Err(Error::Unavailable);
                        }
                        let session = session.clone();
                        let budget = budget.clone();
                        let failed = failed.clone();
                        let output = Arc::clone(&worker_output);
                        bridge = Some(smol::spawn(async move {
                            let _ = reverse_requests(session, data, budget, output).await;
                            let _ = failed.try_send(());
                        }));
                    } else if !has_grants && bridge.is_some() {
                        // Closing the dedicated SSH channel removes the server's
                        // route and cancels all queued requests there.
                        return Ok(());
                    }
                }
            }
            .await;
            worker_output.close();
            drop(bridge);
            let mut state = worker_state.lock();
            state.closed = true;
            state.status = HostStatus::default();
            if !receiver.is_closed() {
                state.error = result.err();
            }
        });
        Ok(Self {
            commands,
            state,
            output,
            _task: task,
        })
    }

    pub fn state(&self) -> ExportState {
        self.state.lock().clone()
    }
    pub fn command(&self, command: Control) -> Result<()> {
        if let Control::Revoke { grant } = &command {
            self.output.revoke(*grant);
        }
        self.commands.try_send(command).map_err(|_| Error::Busy)
    }
    pub fn close(&self) {
        self.output.close();
        self.commands.close();
    }
}

impl Drop for Exporter {
    fn drop(&mut self) {
        self.close();
    }
}

struct OutputState {
    closed: bool,
    last: Tick,
    lease: Tick,
    /// When the newest status was asked for.
    status: Tick,
    grants: HashMap<Id, (GrantRef, GrantScope, Vec<(u64, Id)>, Tick)>,
    denied: HashSet<Id>,
}

/// A conservative replica used only to discard bytes at the last GUI-to-SSH
/// boundary. It cannot approve a grant. Host durations are anchored before
/// the status request, so IPC latency can only shorten them.
struct OutputGuard {
    clock: Arc<dyn Clock>,
    state: Mutex<OutputState>,
}

impl OutputGuard {
    fn new() -> Result<Self> {
        Self::with_clock(SystemClock::shared())
    }

    fn with_clock(clock: Arc<dyn Clock>) -> Result<Self> {
        let now = clock.now()?;
        Ok(Self {
            clock,
            state: Mutex::new(OutputState {
                closed: false,
                last: now,
                lease: now,
                status: now,
                grants: HashMap::new(),
                denied: HashSet::new(),
            }),
        })
    }

    fn update(&self, before: Tick, status: &HostStatus) -> Result<()> {
        if status.grants.len() > thinkterm_reverse_access::MAX_GRANTS {
            return Err(Error::TooLarge);
        }
        let mut state = self.state.lock();
        if state.closed {
            return Err(Error::Revoked);
        }
        let now = self.clock.now()?;
        if now < state.last || now < before {
            state.closed = true;
            return Err(Error::Clock);
        }
        let mut grants = HashMap::new();
        state
            .denied
            .retain(|id| status.grants.iter().any(|grant| grant.grant.id == *id));
        for grant in &status.grants {
            let limit = match grant.scope {
                GrantScope::SelectedTerminals => thinkterm_reverse_access::MAX_TARGETS_PER_GRANT,
                GrantScope::AllLocalTerminals => thinkterm_reverse_access::MAX_TARGETS,
            };
            if grant.targets.len() > limit
                || grant.remaining_ms > thinkterm_reverse_access::MAX_DURATION_SECS * 1000
            {
                return Err(Error::TooLarge);
            }
            if state.denied.contains(&grant.grant.id) {
                continue;
            }
            grants.insert(
                grant.grant.id,
                (
                    grant.grant,
                    grant.scope,
                    grant.targets.clone(),
                    before.after(Duration::from_millis(grant.remaining_ms))?,
                ),
            );
        }
        state.last = now;
        state.lease = before.after(Duration::from_secs(4))?;
        state.status = before;
        state.grants = grants;
        Ok(())
    }

    fn inspect<T>(
        &self,
        refs: &[(GrantRef, (u64, Id))],
        action: impl FnOnce(bool) -> T,
    ) -> Result<T> {
        let mut state = self.state.lock();
        let now = self.clock.now()?;
        if now < state.last {
            state.closed = true;
        }
        if state.closed || now >= state.lease {
            return Err(Error::Revoked);
        }
        state.last = now;
        let mut ready = true;
        for (grant, target) in refs {
            if state.denied.contains(&grant.id) {
                return Err(Error::Revoked);
            }
            match state.grants.get(&grant.id) {
                Some((current, scope, targets, end)) if current == grant => {
                    if now >= *end {
                        return Err(Error::Revoked);
                    }
                    if !targets.contains(target) {
                        if *scope == GrantScope::SelectedTerminals {
                            return Err(Error::Revoked);
                        }
                        // Added or captured again since the last status, or
                        // already gone. `synchronize` finds out which; a
                        // pane id says nothing, recapture keeps it.
                        ready = false;
                    }
                }
                Some((current, _, _, _)) if current.version > grant.version => {
                    return Err(Error::Revoked);
                }
                _ => ready = false,
            }
        }
        Ok(action(ready))
    }

    /// `refs` come with a reply the host has already made. A status asked for
    /// after this began describes the host since then: what it still does not
    /// list is gone, not late.
    async fn synchronize(&self, refs: &[(GrantRef, (u64, Id))], deadline: &Deadline) -> Result<()> {
        let started = self.clock.now()?;
        loop {
            deadline.remaining()?;
            if self.inspect(refs, |ready| ready)? {
                return Ok(());
            }
            if self.state.lock().status > started {
                return Err(Error::Revoked);
            }
            // Authentication can publish a grant between control heartbeats.
            // Only a trusted status update can make its output ready.
            deadline
                .run(async {
                    smol::Timer::after(Duration::from_millis(20)).await;
                    Ok(())
                })
                .await?;
        }
    }

    fn execute<T>(&self, refs: &[(GrantRef, (u64, Id))], write: impl FnOnce() -> T) -> Result<T> {
        self.inspect(refs, |ready| {
            if ready {
                Ok(write())
            } else {
                Err(Error::Revoked)
            }
        })?
    }

    fn revoke(&self, grant: GrantRef) {
        let mut state = self.state.lock();
        state.grants.remove(&grant.id);
        if state.denied.len() >= thinkterm_reverse_access::MAX_GRANTS {
            state.closed = true;
        } else {
            state.denied.insert(grant.id);
        }
    }
    fn close(&self) {
        let mut state = self.state.lock();
        state.closed = true;
        state.grants.clear();
    }
}

async fn control_request(
    channel: &mut Channel<LocalStream>,
    sequence: u64,
    command: &Control,
) -> Result<Controlled> {
    let deadline = Deadline::request()?;
    channel
        .write_json(Kind::Request, sequence, command, &deadline)
        .await?;
    parse(&channel.read(Some(&deadline)).await?, Kind::Reply, sequence)
}

async fn load_choices(
    channel: &mut Channel<LocalStream>,
    sequence: &mut u64,
) -> Result<Vec<PaneChoice>> {
    let mut after = None;
    let mut all = Vec::new();
    // Bounded even if a compromised host supplies an endless cursor.
    for _ in 0..32 {
        *sequence = sequence.checked_add(1).ok_or(Error::Replay)?;
        let Controlled::Choices { panes, after: next } =
            control_request(channel, *sequence, &Control::Choices { after }).await?
        else {
            return Err(Error::Unavailable);
        };
        if panes.len() > 32
            || all.len() + panes.len() > thinkterm_reverse_access::MAX_TARGETS
            || panes.iter().any(|pane| pane.title.len() > 512)
            || next.is_some_and(|next| after.is_some_and(|after| next <= after))
        {
            return Err(Error::TooLarge);
        }
        all.extend(panes);
        if next.is_none() {
            return Ok(all);
        }
        after = next;
    }
    Err(Error::TooLarge)
}

async fn reverse_requests(
    session: VerifiedConnectSession,
    mut host: Channel<LocalStream>,
    budget: RouteBudget,
    output: Arc<OutputGuard>,
) -> Result<()> {
    let stream = session
        .open_reverse_channel()
        .await
        .map_err(|_| Error::Unavailable)?;
    let mut channel = Channel::new(stream, budget.acquire(1024 * 1024)?)?;
    let handshake = Deadline::after(Duration::from_secs(5))?;
    channel
        .write_json(
            Kind::Hello,
            0,
            &Hello::Register {
                name: "desktop".into(),
            },
            &handshake,
        )
        .await?;
    let _: Bound = parse(&channel.read(Some(&handshake)).await?, Kind::Bound, 0)?;
    let mut previous = 0;
    loop {
        let frame = channel.read(None).await?;
        let sequence = frame.sequence();
        if sequence <= previous || sequence == u64::MAX || !session.is_alive() {
            return Err(Error::Replay);
        }
        previous = sequence;
        let timed: TimedCommand = parse(&frame, Kind::Request, sequence)?;
        let (command, deadline) = timed.receive()?;
        let payload = receive_input(&mut channel, &command, sequence, &budget, &deadline).await?;
        let disconnected = channel.watch_peer();
        forward_request(
            &mut channel,
            &mut host,
            &output,
            command,
            payload,
            sequence,
            &deadline,
        )
        .or(disconnected)
        .await?;
    }
}

/// Carry one request to the host and its reply back. A reply refused here
/// holds no terminal data, so it needs no guard. An error means a channel is
/// no longer in step and the bridge ends.
async fn forward_request<S, H>(
    channel: &mut Channel<S>,
    host: &mut Channel<H>,
    output: &OutputGuard,
    command: Command,
    payload: Refusable<Option<Payload>>,
    sequence: u64,
    deadline: &Deadline,
) -> Result<()>
where
    S: futures::io::AsyncRead + futures::io::AsyncWrite + Unpin,
    H: futures::io::AsyncRead + futures::io::AsyncWrite + Unpin,
{
    let ready = payload.and_then(|payload| {
        let forwarded = deadline.earlier(REPLY_MARGIN)?;
        let timed = TimedCommand::new(command.clone(), &forwarded)?;
        Ok((payload, forwarded, timed))
    });
    let (payload, forwarded, timed) = match ready {
        Ok(ready) => ready,
        Err(error) => return refuse(channel, sequence, error, deadline).await,
    };
    host.write_json(Kind::Request, sequence, &timed, &forwarded)
        .await?;
    if let Some(payload) = payload {
        send_payload(
            host,
            payload.text().as_bytes(),
            sequence,
            Kind::InputBlock,
            &forwarded,
        )
        .await?;
    }
    // Forward each already bounded frame without retaining terminal objects
    // or assembling an extra copy of a read result in the GUI.
    let frame = host.read(Some(deadline)).await?;
    let reply: Reply = parse(&frame, Kind::Reply, sequence)?;
    let refs = match &reply {
        Reply::List { exports, .. } if exports.len() <= 32 => exports
            .iter()
            .map(|item| {
                (
                    item.export.grant,
                    (item.export.target.pane, item.export.target.incarnation),
                )
            })
            .collect::<Vec<_>>(),
        Reply::Text { .. } => match &command {
            Command::Read { export, .. } => vec![(
                export.grant,
                (export.target.pane, export.target.incarnation),
            )],
            _ => return Err(Error::Protocol),
        },
        Reply::List { .. } => return Err(Error::TooLarge),
        _ => Vec::new(),
    };
    let expected = match reply {
        Reply::Text { bytes } if bytes <= thinkterm_reverse_access::MAX_READ_BYTES => bytes,
        Reply::Text { .. } => return Err(Error::TooLarge),
        _ => 0,
    };
    if let Err(error) = output.synchronize(&refs, &forwarded).await {
        refuse(channel, sequence, error, deadline).await?;
        return discard_text(host, 0, expected, sequence, deadline).await;
    }
    channel
        .write_guarded(&frame, deadline, |write| output.execute(&refs, write))
        .await?;
    let mut received = 0;
    while received < expected {
        let frame = host.read(Some(deadline)).await?;
        let Some(bytes) = next_block(&frame, Kind::TextBlock, sequence, received, expected)? else {
            return channel.write(&frame, deadline).await;
        };
        received += bytes.len();
        // The host starts no block in the last REPLY_MARGIN of `forwarded`.
        // What it sent before can still be queued here, and on a slow
        // connection forwarding that would run out of `deadline` partway
        // through a frame, which ends the bridge. Stopping with the host also
        // lets it finish the frame it is writing.
        let host_stopped = !forwarded.remaining().is_ok_and(|left| left > REPLY_MARGIN);
        if host_stopped || output.execute(&refs, || ()).is_err() {
            channel
                .write(&Frame::new(Kind::Cancel, sequence, &[])?, deadline)
                .await?;
            return discard_text(host, received, expected, sequence, deadline).await;
        }
        channel
            .write_guarded(&frame, deadline, |write| output.execute(&refs, write))
            .await?;
    }
    Ok(())
}

async fn refuse<S: futures::io::AsyncRead + futures::io::AsyncWrite + Unpin>(
    channel: &mut Channel<S>,
    sequence: u64,
    error: Error,
    deadline: &Deadline,
) -> Result<()> {
    let reply = Reply::Failed(error.into());
    channel
        .write_json(Kind::Reply, sequence, &reply, deadline)
        .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::io::{AsyncRead, AsyncWrite};
    use futures::FutureExt as _;
    use std::io;
    use std::pin::Pin;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::task::{Context, Poll};
    use thinkterm_reverse_access::Permission;

    struct TestClock {
        origin: Tick,
        millis: AtomicU64,
    }
    impl Clock for TestClock {
        fn now(&self) -> Result<Tick> {
            self.origin
                .after(Duration::from_millis(self.millis.load(Ordering::Acquire)))
        }
    }
    impl TestClock {
        fn set(&self, millis: u64) {
            self.millis.store(millis, Ordering::Release);
        }
    }

    fn target(pane: u64) -> (u64, Id) {
        (pane, Id::from_bytes([3; 16]))
    }

    async fn write_reply<S: AsyncRead + AsyncWrite + Unpin>(
        output: &OutputGuard,
        channel: &mut Channel<S>,
        frame: &Frame,
        refs: &[(GrantRef, (u64, Id))],
        deadline: &Deadline,
    ) -> Result<()> {
        output.synchronize(refs, deadline).await?;
        channel
            .write_guarded(frame, deadline, |write| output.execute(refs, write))
            .await
    }

    fn fixture() -> (OutputGuard, Arc<TestClock>, HostStatus, GrantRef) {
        let clock = Arc::new(TestClock {
            origin: SystemClock::shared().now().unwrap(),
            millis: AtomicU64::new(0),
        });
        let output = OutputGuard::with_clock(clock.clone()).unwrap();
        let grant = GrantRef {
            id: Id::from_bytes([1; 16]),
            version: 1,
        };
        let status = HostStatus {
            grants: vec![mux::connect::ActiveGrant {
                grant,
                scope: GrantScope::SelectedTerminals,
                targets: vec![target(42)],
                permission: Permission::ReadAndInput,
                remaining_ms: 2000,
            }],
            ..HostStatus::default()
        };
        (output, clock, status, grant)
    }

    struct RecordingStream {
        bytes: Arc<Mutex<Vec<u8>>>,
        pause: bool,
    }

    impl AsyncRead for RecordingStream {
        fn poll_read(
            self: Pin<&mut Self>,
            _: &mut Context<'_>,
            _: &mut [u8],
        ) -> Poll<io::Result<usize>> {
            Poll::Pending
        }
    }

    impl AsyncWrite for RecordingStream {
        fn poll_write(
            self: Pin<&mut Self>,
            _: &mut Context<'_>,
            bytes: &[u8],
        ) -> Poll<io::Result<usize>> {
            if self.pause && !self.bytes.lock().is_empty() {
                return Poll::Pending;
            }
            let count = bytes.len().min(3);
            self.bytes.lock().extend_from_slice(&bytes[..count]);
            Poll::Ready(Ok(count))
        }
        fn poll_flush(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<io::Result<()>> {
            Poll::Ready(Ok(()))
        }
        fn poll_close(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<io::Result<()>> {
            Poll::Ready(Ok(()))
        }
    }

    fn recording_channel(pause: bool) -> (Channel<RecordingStream>, Arc<Mutex<Vec<u8>>>) {
        let bytes = Arc::new(Mutex::new(Vec::new()));
        let budgets = Budgets::new().unwrap();
        let route = budgets.open_route(Id::from_bytes([9; 16])).unwrap();
        let channel = Channel::new(
            RecordingStream {
                bytes: bytes.clone(),
                pause,
            },
            route.acquire(1024 * 1024).unwrap(),
        )
        .unwrap();
        (channel, bytes)
    }

    #[test]
    fn all_local_output_waits_for_new_targets_and_rejects_replaced_incarnations() {
        let (output, clock, mut status, grant) = fixture();
        status.grants[0].scope = GrantScope::AllLocalTerminals;
        output.update(clock.now().unwrap(), &status).unwrap();
        let (mut channel, bytes) = recording_channel(false);
        let frame = Frame::new(Kind::Reply, 1, b"example").unwrap();
        let refs = [(grant, target(43))];
        let deadline = Deadline::request().unwrap();
        smol::block_on(async {
            let mut pending =
                Box::pin(write_reply(&output, &mut channel, &frame, &refs, &deadline));
            assert!(futures::poll!(&mut pending).is_pending());
            assert!(bytes.lock().is_empty());
            status.grants[0].targets.push(target(43));
            output.update(clock.now().unwrap(), &status).unwrap();
            assert_eq!(pending.await, Ok(()));
        });
        status.grants[0].targets.retain(|target| target.0 != 43);
        output.update(clock.now().unwrap(), &status).unwrap();
        assert_eq!(output.execute(&refs, || ()), Err(Error::Revoked));
        status.grants[0].targets.push((43, Id::from_bytes([4; 16])));
        output.update(clock.now().unwrap(), &status).unwrap();
        // A newer status than this reply's turn settles it as gone.
        let deadline = Deadline::request().unwrap();
        smol::block_on(async {
            let mut pending =
                Box::pin(write_reply(&output, &mut channel, &frame, &refs, &deadline));
            assert!(futures::poll!(&mut pending).is_pending());
            clock.set(1000);
            output.update(clock.now().unwrap(), &status).unwrap();
            assert_eq!(pending.await, Err(Error::Revoked));
        });
        assert_eq!(output.execute(&refs, || ()), Err(Error::Revoked));
        assert!(output
            .execute(&[(grant, (43, Id::from_bytes([4; 16])))], || ())
            .is_ok());
    }

    #[test]
    fn a_recaptured_pane_waits_for_status_instead_of_ending_the_bridge() {
        let (output, clock, mut status, grant) = fixture();
        status.grants[0].scope = GrantScope::AllLocalTerminals;
        output.update(clock.now().unwrap(), &status).unwrap();
        let (mut channel, bytes) = recording_channel(false);
        let frame = Frame::new(Kind::Reply, 1, b"example").unwrap();
        // The host moved on from incarnation [3; 16] before this status.
        let recaptured = (42, Id::from_bytes([4; 16]));
        let refs = [(grant, recaptured)];
        let deadline = Deadline::request().unwrap();
        smol::block_on(async {
            let mut pending =
                Box::pin(write_reply(&output, &mut channel, &frame, &refs, &deadline));
            assert!(futures::poll!(&mut pending).is_pending());
            assert!(bytes.lock().is_empty());
            status.grants[0].targets = vec![recaptured];
            clock.set(500);
            output.update(clock.now().unwrap(), &status).unwrap();
            assert_eq!(pending.await, Ok(()));
        });
    }

    #[test]
    fn all_local_output_does_not_extend_expiry_while_waiting_for_membership() {
        let (output, clock, mut status, grant) = fixture();
        status.grants[0].scope = GrantScope::AllLocalTerminals;
        output.update(clock.now().unwrap(), &status).unwrap();
        let (mut channel, bytes) = recording_channel(false);
        let frame = Frame::new(Kind::Reply, 1, b"example").unwrap();
        let refs = [(grant, target(43))];
        let deadline = Deadline::request().unwrap();
        smol::block_on(async {
            let mut pending =
                Box::pin(write_reply(&output, &mut channel, &frame, &refs, &deadline));
            assert!(futures::poll!(&mut pending).is_pending());
            clock.set(2000);
            assert_eq!(pending.await, Err(Error::Revoked));
        });
        assert!(bytes.lock().is_empty());
    }

    #[test]
    fn maximum_all_local_status_fits_a_bounded_control_frame() {
        let (output, clock, mut status, _) = fixture();
        let grant = &mut status.grants[0];
        grant.scope = GrantScope::AllLocalTerminals;
        grant.targets = (0..thinkterm_reverse_access::MAX_TARGETS)
            .map(|index| (u64::MAX - index as u64, Id::from_bytes([255; 16])))
            .collect();
        output.update(clock.now().unwrap(), &status).unwrap();
        let response = Controlled::Status(status.clone());
        thinkterm_reverse_access::channel::json_frame(Kind::Reply, 1, &response).unwrap();
        status.grants[0].targets.push(target(42));
        assert_eq!(
            output.update(clock.now().unwrap(), &status),
            Err(Error::TooLarge)
        );
    }

    #[test]
    fn new_and_renewed_replies_wait_for_trusted_status_before_writing() {
        for renewal in [false, true] {
            let (output, clock, mut status, grant) = fixture();
            output.update(clock.now().unwrap(), &status).unwrap();
            let next = if renewal {
                GrantRef {
                    version: 2,
                    ..grant
                }
            } else {
                GrantRef {
                    id: Id::from_bytes([2; 16]),
                    ..grant
                }
            };
            let (mut channel, bytes) = recording_channel(false);
            let frame = Frame::new(Kind::Reply, 1, b"example").unwrap();
            let refs = [(next, target(42))];
            let deadline = Deadline::request().unwrap();
            smol::block_on(async {
                let mut pending =
                    Box::pin(write_reply(&output, &mut channel, &frame, &refs, &deadline));
                assert!(futures::poll!(&mut pending).is_pending());
                assert!(bytes.lock().is_empty());
                if renewal {
                    status.grants[0].grant = next;
                } else {
                    let mut added = status.grants[0].clone();
                    added.grant = next;
                    status.grants.push(added);
                }
                output.update(clock.now().unwrap(), &status).unwrap();
                assert_eq!(pending.await, Ok(()));
            });
            let mut expected = frame.header().to_vec();
            expected.extend_from_slice(frame.body());
            assert_eq!(*bytes.lock(), expected);
            if renewal {
                assert_eq!(
                    output.execute(&[(grant, target(42))], || ()),
                    Err(Error::Revoked)
                );
            } else {
                assert!(output.execute(&[(grant, target(42))], || ()).is_ok());
            }
        }
    }

    #[test]
    fn output_synchronization_keeps_revocation_expiry_and_lease_failures_closed() {
        for failure in 0..5 {
            let (output, clock, mut status, grant) = fixture();
            clock.set(100);
            output.update(clock.now().unwrap(), &status).unwrap();
            let next = GrantRef {
                id: Id::from_bytes([2; 16]),
                ..grant
            };
            let refs = [(next, target(42)), (grant, target(42))];
            let (mut channel, bytes) = recording_channel(false);
            let frame = Frame::new(Kind::Reply, 1, b"example").unwrap();
            let deadline = Deadline::request().unwrap();
            smol::block_on(async {
                let mut pending =
                    Box::pin(write_reply(&output, &mut channel, &frame, &refs, &deadline));
                assert!(futures::poll!(&mut pending).is_pending());
                assert!(bytes.lock().is_empty());
                match failure {
                    0 => {
                        output.revoke(next);
                        let mut added = status.grants[0].clone();
                        added.grant = next;
                        status.grants.push(added);
                        output.update(clock.now().unwrap(), &status).unwrap();
                    }
                    1 => output.close(),
                    2 => clock.set(99),
                    3 => clock.set(2100),
                    4 => clock.set(4100),
                    _ => unreachable!(),
                }
                assert_eq!(pending.await, Err(Error::Revoked));
            });
            assert!(bytes.lock().is_empty());
        }
    }

    #[test]
    fn output_synchronization_keeps_the_original_request_deadline() {
        let (output, clock, status, grant) = fixture();
        output.update(clock.now().unwrap(), &status).unwrap();
        let next = GrantRef {
            version: 2,
            ..grant
        };
        let (mut channel, bytes) = recording_channel(false);
        let frame = Frame::new(Kind::Reply, 1, b"example").unwrap();
        let result = smol::block_on(write_reply(
            &output,
            &mut channel,
            &frame,
            &[(next, target(42))],
            &Deadline::after(Duration::from_millis(10)).unwrap(),
        ));
        assert_eq!(result, Err(Error::Timeout));
        assert!(bytes.lock().is_empty());
    }

    #[test]
    fn synchronized_output_still_checks_every_partial_write() {
        let (output, clock, status, grant) = fixture();
        output.update(clock.now().unwrap(), &status).unwrap();
        let (mut channel, bytes) = recording_channel(true);
        let frame = Frame::new(Kind::Reply, 1, b"example").unwrap();
        let refs = [(grant, target(42))];
        let deadline = Deadline::request().unwrap();
        smol::block_on(async {
            let mut pending =
                Box::pin(write_reply(&output, &mut channel, &frame, &refs, &deadline));
            assert!(futures::poll!(&mut pending).is_pending());
            assert_eq!(bytes.lock().len(), 3);
            output.revoke(grant);
            assert_eq!(pending.await, Err(Error::Revoked));
        });
        assert_eq!(bytes.lock().len(), 3);
    }

    #[test]
    fn output_replica_includes_status_latency_in_expiration() {
        let (output, clock, status, grant) = fixture();
        let before = clock.now().unwrap();
        clock.set(1500);
        output.update(before, &status).unwrap();
        assert!(output.execute(&[(grant, target(42))], || ()).is_ok());
        clock.set(2000);
        assert!(output
            .execute(&[(grant, target(42))], || panic!("expired output was sent"))
            .is_err());
    }

    #[test]
    fn pending_status_cannot_restore_a_locally_revoked_grant() {
        let (output, clock, status, grant) = fixture();
        output.update(clock.now().unwrap(), &status).unwrap();
        output.revoke(grant);
        clock.set(500);
        output.update(clock.now().unwrap(), &status).unwrap();
        assert!(output
            .execute(&[(grant, target(42))], || panic!("revoked output was sent"))
            .is_err());
        assert!(output
            .execute(&[(grant, target(43))], || panic!("wrong pane was sent"))
            .is_err());
    }

    #[test]
    fn output_replica_needs_a_live_control_lease_and_exact_version() {
        let (output, clock, mut status, grant) = fixture();
        status.grants[0].remaining_ms = 60000;
        output.update(clock.now().unwrap(), &status).unwrap();
        let stale = GrantRef {
            version: 2,
            ..grant
        };
        assert!(output.execute(&[(stale, target(42))], || ()).is_err());
        clock.set(4000);
        assert!(output.execute(&[(grant, target(42))], || ()).is_err());
        output.close();
        assert!(output.update(clock.now().unwrap(), &status).is_err());
    }

    /// The router's end and the host's end of one bridge, over real sockets.
    #[cfg(unix)]
    mod bridge {
        use super::*;
        use std::os::unix::net::UnixStream;
        use thinkterm_reverse_access::protocol::{ExportRef, ReadOptions};
        use thinkterm_reverse_access::Target;

        type Socket = smol::Async<UnixStream>;
        const SEQUENCE: u64 = 7;

        struct Ends {
            near_router: Channel<Socket>,
            near_host: Channel<Socket>,
            router: Channel<Socket>,
            host: Channel<Socket>,
            _budgets: Budgets,
        }

        fn ends() -> Ends {
            let budgets = Budgets::new().unwrap();
            let budget = budgets.open_route(Id::from_bytes([9; 16])).unwrap();
            let channel = |socket| Channel::new(socket, budget.acquire(1024 * 1024).unwrap());
            let (near_router, router) = Socket::pair().unwrap();
            let (near_host, host) = Socket::pair().unwrap();
            Ends {
                near_router: channel(near_router).unwrap(),
                near_host: channel(near_host).unwrap(),
                router: channel(router).unwrap(),
                host: channel(host).unwrap(),
                _budgets: budgets,
            }
        }

        fn read_command(grant: GrantRef, pane: u64) -> Command {
            Command::Read {
                export: ExportRef {
                    grant,
                    target: Target {
                        pane,
                        incarnation: Id::from_bytes([3; 16]),
                        owner: Id::from_bytes([5; 16]),
                    },
                },
                options: ReadOptions::default(),
            }
        }

        async fn serve_text(host: &mut Channel<Socket>, text: &[u8], deadline: &Deadline) {
            let frame = host.read(Some(deadline)).await.unwrap();
            assert_eq!(frame.sequence(), SEQUENCE);
            let reply = Reply::Text { bytes: text.len() };
            host.write_json(Kind::Reply, SEQUENCE, &reply, deadline)
                .await
                .unwrap();
            send_payload(host, text, SEQUENCE, Kind::TextBlock, deadline)
                .await
                .unwrap();
        }

        #[test]
        fn a_refused_reply_is_answered_and_its_text_consumed() {
            let (output, clock, status, grant) = fixture();
            output.update(clock.now().unwrap(), &status).unwrap();
            let mut ends = ends();
            let deadline = Deadline::request().unwrap();
            let text = "example".repeat(20_000);
            smol::block_on(async {
                let (forwarded, _) = smol::future::zip(
                    forward_request(
                        &mut ends.near_router,
                        &mut ends.near_host,
                        &output,
                        // A selected-terminal grant that never had this pane.
                        read_command(grant, 43),
                        Ok(None),
                        SEQUENCE,
                        &deadline,
                    ),
                    serve_text(&mut ends.host, text.as_bytes(), &deadline),
                )
                .await;
                forwarded.unwrap();
                let frame = ends.router.read(Some(&deadline)).await.unwrap();
                let reply: Reply = parse(&frame, Kind::Reply, SEQUENCE).unwrap();
                assert!(matches!(reply, Reply::Failed(_)));
                // Both channels are at a frame boundary.
                assert!(ends.near_host.watch_peer().now_or_never().is_none());
                assert!(ends.near_router.watch_peer().now_or_never().is_none());
            });
        }

        #[test]
        fn a_reply_refused_partway_is_abandoned_between_blocks() {
            let (output, clock, status, grant) = fixture();
            output.update(clock.now().unwrap(), &status).unwrap();
            let mut ends = ends();
            let deadline = Deadline::request().unwrap();
            let text = "example".repeat(20_000);
            smol::block_on(async {
                let router = &mut ends.router;
                let (forwarded, _) = smol::future::zip(
                    forward_request(
                        &mut ends.near_router,
                        &mut ends.near_host,
                        &output,
                        read_command(grant, 42),
                        Ok(None),
                        SEQUENCE,
                        &deadline,
                    ),
                    smol::future::zip(
                        serve_text(&mut ends.host, text.as_bytes(), &deadline),
                        async {
                            let frame = router.read(Some(&deadline)).await.unwrap();
                            let reply: Reply = parse(&frame, Kind::Reply, SEQUENCE).unwrap();
                            assert!(matches!(reply, Reply::Text { .. }));
                            let frame = router.read(Some(&deadline)).await.unwrap();
                            assert_eq!(frame.kind(), Kind::TextBlock);
                            output.revoke(grant);
                            loop {
                                let frame = router.read(Some(&deadline)).await.unwrap();
                                if frame.kind() == Kind::Cancel {
                                    assert_eq!(frame.sequence(), SEQUENCE);
                                    break;
                                }
                                assert_eq!(frame.kind(), Kind::TextBlock);
                            }
                        },
                    ),
                )
                .await;
                forwarded.unwrap();
                assert!(ends.near_host.watch_peer().now_or_never().is_none());
            });
        }

        #[test]
        fn a_reply_past_the_hosts_time_is_abandoned_between_blocks() {
            let (output, clock, status, grant) = fixture();
            output.update(clock.now().unwrap(), &status).unwrap();
            let mut ends = ends();
            // The host stops starting blocks after the first 300 ms of these.
            let deadline = Deadline::after(2 * REPLY_MARGIN + Duration::from_millis(300)).unwrap();
            let block = |offset: u32, text: &[u8]| {
                let body = [&offset.to_be_bytes()[..], text].concat();
                Frame::new(Kind::TextBlock, SEQUENCE, &body).unwrap()
            };
            smol::block_on(async {
                let host = &mut ends.host;
                let (forwarded, _) = smol::future::zip(
                    forward_request(
                        &mut ends.near_router,
                        &mut ends.near_host,
                        &output,
                        read_command(grant, 42),
                        Ok(None),
                        SEQUENCE,
                        &deadline,
                    ),
                    async {
                        host.read(Some(&deadline)).await.unwrap();
                        let reply = Reply::Text { bytes: 100_000 };
                        host.write_json(Kind::Reply, SEQUENCE, &reply, &deadline)
                            .await
                            .unwrap();
                        host.write(&block(0, b"example"), &deadline).await.unwrap();
                        smol::Timer::after(Duration::from_millis(350)).await;
                        host.write(&block(7, b"example"), &deadline).await.unwrap();
                        host.write(&Frame::new(Kind::Cancel, SEQUENCE, &[]).unwrap(), &deadline)
                            .await
                            .unwrap();
                    },
                )
                .await;
                forwarded.unwrap();
                let read = Deadline::request().unwrap();
                let frame = ends.router.read(Some(&read)).await.unwrap();
                let reply: Reply = parse(&frame, Kind::Reply, SEQUENCE).unwrap();
                assert!(matches!(reply, Reply::Text { .. }));
                let frame = ends.router.read(Some(&read)).await.unwrap();
                assert_eq!(frame.kind(), Kind::TextBlock);
                let frame = ends.router.read(Some(&read)).await.unwrap();
                assert_eq!(frame.kind(), Kind::Cancel);
                assert!(ends.near_host.watch_peer().now_or_never().is_none());
            });
        }

        #[test]
        fn a_host_that_abandons_its_text_is_passed_on() {
            let (output, clock, status, grant) = fixture();
            output.update(clock.now().unwrap(), &status).unwrap();
            let mut ends = ends();
            let deadline = Deadline::request().unwrap();
            smol::block_on(async {
                let host = &mut ends.host;
                let (forwarded, _) = smol::future::zip(
                    forward_request(
                        &mut ends.near_router,
                        &mut ends.near_host,
                        &output,
                        read_command(grant, 42),
                        Ok(None),
                        SEQUENCE,
                        &deadline,
                    ),
                    async {
                        host.read(Some(&deadline)).await.unwrap();
                        let reply = Reply::Text { bytes: 100_000 };
                        host.write_json(Kind::Reply, SEQUENCE, &reply, &deadline)
                            .await
                            .unwrap();
                        send_payload(host, b"example", SEQUENCE, Kind::TextBlock, &deadline)
                            .await
                            .unwrap();
                        host.write(&Frame::new(Kind::Cancel, SEQUENCE, &[]).unwrap(), &deadline)
                            .await
                            .unwrap();
                    },
                )
                .await;
                forwarded.unwrap();
                ends.router.read(Some(&deadline)).await.unwrap();
                ends.router.read(Some(&deadline)).await.unwrap();
                let frame = ends.router.read(Some(&deadline)).await.unwrap();
                assert_eq!(frame.kind(), Kind::Cancel);
            });
        }

        #[test]
        fn input_refused_on_arrival_never_reaches_the_host() {
            let (output, clock, status, grant) = fixture();
            output.update(clock.now().unwrap(), &status).unwrap();
            let mut ends = ends();
            let deadline = Deadline::request().unwrap();
            smol::block_on(async {
                forward_request(
                    &mut ends.near_router,
                    &mut ends.near_host,
                    &output,
                    read_command(grant, 42),
                    Err(Error::Busy),
                    SEQUENCE,
                    &deadline,
                )
                .await
                .unwrap();
                let frame = ends.router.read(Some(&deadline)).await.unwrap();
                let reply: Reply = parse(&frame, Kind::Reply, SEQUENCE).unwrap();
                assert!(matches!(reply, Reply::Failed(Failure::Busy)));
                assert!(ends.host.watch_peer().now_or_never().is_none());
            });
        }
    }

    #[test]
    fn a_backwards_clock_permanently_closes_the_output_guard() {
        let (output, clock, status, grant) = fixture();
        clock.set(1000);
        output.update(clock.now().unwrap(), &status).unwrap();
        clock.set(999);
        assert!(output.execute(&[(grant, target(42))], || ()).is_err());
        clock.set(1001);
        assert!(output.execute(&[(grant, target(42))], || ()).is_err());
    }
}
