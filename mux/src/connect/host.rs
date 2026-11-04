use super::*;
use serde::{Deserialize, Serialize};
use smol::io::{AsyncRead, AsyncWrite};
use std::collections::{HashMap, HashSet};
use std::convert::TryFrom;
use std::task::Poll;
use thinkterm_reverse_access::budget::{Budgets, RouteBudget};
use thinkterm_reverse_access::channel::{json_frame, Channel, Deadline};
use thinkterm_reverse_access::native::{AuthorizedGrant, NativeAttempt, NativeSession};
use thinkterm_reverse_access::protocol::{Command, ExportRef, ExportSummary, Failure, Key, Reply};
use thinkterm_reverse_access::wire::{Frame, Kind, Payload, MAX_BLOCK};
use thinkterm_reverse_access::{
    Authority, Endpoint, GrantRef, GrantScope, ListPermit, Operation, Permission, Request, Route,
};

pub struct Host {
    mux: Weak<Mux>,
    authority: Arc<Authority>,
    endpoint: Endpoint,
    budgets: Budgets,
    state: Mutex<State>,
}

#[derive(Default)]
struct State {
    native: Option<NativeSession>,
    pending: Option<PendingApproval>,
    grants: HashMap<Id, StoredGrant>,
    routes: HashMap<Id, Weak<HostRoute>>,
    errors: HashMap<Id, Failure>,
}

struct PendingApproval {
    route: Id,
    renewal: Option<GrantRef>,
    attempt: NativeAttempt,
    targets: Vec<Arc<ExportedPane>>,
}

struct StoredGrant {
    route: Id,
    grant: AuthorizedGrant,
    targets: Vec<Arc<ExportedPane>>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PaneChoice {
    pub pane: u64,
    pub title: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ActiveGrant {
    pub grant: GrantRef,
    pub scope: GrantScope,
    pub targets: Vec<(u64, Id)>,
    pub permission: Permission,
    pub remaining_ms: u64,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HostStatus {
    pub grants: Vec<ActiveGrant>,
    pub authenticating: bool,
    pub error: Option<Failure>,
}

pub struct HostRoute {
    host: Arc<Host>,
    route: Route,
    budget: RouteBudget,
    source: String,
    language: String,
    client: ClientId,
}

impl Host {
    pub fn close(&self) {
        self.authority.close();
        *self.state.lock() = State::default();
    }

    pub fn new(mux: &Arc<Mux>) -> Result<Arc<Self>> {
        let (authority, endpoint) = Authority::new();
        Ok(Arc::new(Self {
            mux: Arc::downgrade(mux),
            authority: Arc::new(authority),
            endpoint,
            budgets: Budgets::process()?,
            state: Mutex::new(State::default()),
        }))
    }

    pub fn open(
        self: &Arc<Self>,
        client: ClientId,
        source: &str,
        language: &str,
    ) -> Result<Arc<HostRoute>> {
        if source.is_empty() || source.len() > 256 || source.chars().any(char::is_control) {
            return Err(Error::Protocol);
        }
        let route = self.authority.register_route()?;
        let budget = self.budgets.open_route(route.id())?;
        let route = Arc::new(HostRoute {
            host: Arc::clone(self),
            route,
            budget,
            client,
            source: source.into(),
            language: language.chars().take(16).collect(),
        });
        self.state
            .lock()
            .routes
            .insert(route.id(), Arc::downgrade(&route));
        Ok(route)
    }

    fn poll(&self, state: &mut State) {
        if let Some(pending) = state.pending.take() {
            match pending.attempt.try_result() {
                Ok(None) => state.pending = Some(pending),
                Ok(Some(grant)) => {
                    if state
                        .routes
                        .get(&pending.route)
                        .and_then(Weak::upgrade)
                        .is_some()
                        && pending.targets.iter().all(|target| target.is_current())
                    {
                        state.grants.insert(
                            grant.info().grant.id,
                            StoredGrant {
                                route: pending.route,
                                grant,
                                targets: pending.targets,
                            },
                        );
                    } else {
                        state.errors.insert(pending.route, Failure::Unavailable);
                    }
                }
                // The dialog already told the user; a renewal's old grant
                // simply carries on.
                Err(Error::Cancelled) => {}
                Err(error) => {
                    state.errors.insert(pending.route, error.into());
                }
            }
        }
        let live: HashSet<_> = state
            .routes
            .values()
            .filter_map(Weak::upgrade)
            .flat_map(|route| self.endpoint.list(&route.route).unwrap_or_default())
            .map(|info| info.grant.id)
            .collect();
        state.grants.retain(|id, grant| {
            live.contains(id)
                && (grant.grant.info().scope == GrantScope::AllLocalTerminals
                    || grant.targets.iter().all(|target| target.is_current()))
        });
        let State {
            grants,
            routes,
            errors,
            ..
        } = state;
        for stored in grants.values_mut() {
            if stored.grant.info().scope != GrantScope::AllLocalTerminals {
                continue;
            }
            let result: Result<()> = (|| {
                let route = routes
                    .get(&stored.route)
                    .and_then(Weak::upgrade)
                    .ok_or(Error::Unavailable)?;
                let mux = self.mux.upgrade().ok_or(Error::Unavailable)?;
                let targets = route.all_local_targets(&mux, &stored.targets)?;
                self.authority.replace_all_local_targets(
                    &route.route,
                    stored.grant.info().grant,
                    targets.iter().map(|target| target.target()).collect(),
                )?;
                stored.targets = targets;
                Ok(())
            })();
            if let Err(error) = result {
                if error != Error::Busy {
                    let _ = self.authority.revoke(stored.grant.info().grant);
                    errors.insert(stored.route, error.into());
                }
            }
        }
        if state.grants.is_empty() && state.pending.is_none() {
            state.native = None;
        }
    }

    fn forget(&self, id: Id) {
        let mut state = self.state.lock();
        state.routes.remove(&id);
        state.errors.remove(&id);
        state.grants.retain(|_, grant| grant.route != id);
        if state
            .pending
            .as_ref()
            .is_some_and(|pending| pending.route == id)
        {
            state.pending = None;
        }
        if state.grants.is_empty() && state.pending.is_none() {
            state.native = None;
        }
    }
}

impl HostRoute {
    pub fn id(&self) -> Id {
        self.route.id()
    }
    pub fn budget(&self) -> &RouteBudget {
        &self.budget
    }
    pub fn heartbeat(&self) -> Result<()> {
        self.route.heartbeat()
    }
    pub fn close(&self) {
        self.route.close();
        self.host.forget(self.id());
    }

    pub fn choices(&self, after: Option<u64>) -> Result<(Vec<PaneChoice>, Option<u64>)> {
        let mux = self.host.mux.upgrade().ok_or(Error::Unavailable)?;
        let mut page = std::collections::BTreeSet::new();
        for &id in mux.panes.read().keys() {
            if after.is_some_and(|after| (id as u64) <= after) {
                continue;
            }
            page.insert(id);
            if page.len() > 33 {
                page.pop_last();
            }
        }
        let mut panes: Vec<_> = page.into_iter().collect();
        let more = panes.len() > 32;
        panes.truncate(32);
        let next = more
            .then(|| panes.last().copied().map(|id| id as u64))
            .flatten();
        let mut choices = Vec::new();
        for id in panes {
            if let Ok(pane) = ExportedPane::capture(&mux, id, &self.client) {
                if let Ok(title) = pane.with_current(|local| local.connect_title()) {
                    choices.push(PaneChoice {
                        pane: id as u64,
                        title,
                    });
                }
            }
        }
        choices.sort_by_key(|pane| pane.pane);
        Ok((choices, next))
    }

    fn all_local_targets(
        &self,
        mux: &Arc<Mux>,
        previous: &[Arc<ExportedPane>],
    ) -> Result<Vec<Arc<ExportedPane>>> {
        let panes = mux.panes.try_read().ok_or(Error::Busy)?;
        let ids: Vec<_> = panes
            .values()
            .filter(|pane| pane.downcast_ref::<LocalPane>().is_some())
            .map(|pane| pane.pane_id())
            .collect();
        drop(panes);
        let mut targets = Vec::new();
        for id in ids {
            if let Some(old) = previous
                .iter()
                .find(|target| target.target().pane == id as u64)
            {
                if old.is_current() {
                    targets.push(Arc::clone(old));
                    if targets.len() > thinkterm_reverse_access::MAX_TARGETS {
                        return Err(Error::TooLarge);
                    }
                    continue;
                }
            }
            match ExportedPane::capture(mux, id, &self.client) {
                Ok(target) => targets.push(target),
                Err(Error::Busy) => return Err(Error::Busy),
                Err(Error::Unavailable) => continue,
                Err(error) => return Err(error),
            }
            if targets.len() > thinkterm_reverse_access::MAX_TARGETS {
                return Err(Error::TooLarge);
            }
        }
        targets.sort_by_key(|target| target.target().pane);
        Ok(targets)
    }

    /// Native authentication consumes a frozen intent inside this host.
    /// There is intentionally no method that accepts an approval result.
    pub fn begin_approval(
        &self,
        panes: &[u64],
        permission: Permission,
        seconds: u64,
        renewal: Option<GrantRef>,
    ) -> Result<()> {
        if panes.is_empty() || panes.len() > thinkterm_reverse_access::MAX_TARGETS_PER_GRANT {
            return Err(Error::TooLarge);
        }
        let mux = self.host.mux.upgrade().ok_or(Error::Unavailable)?;
        let targets: Vec<_> = panes
            .iter()
            .map(|id| {
                let id = usize::try_from(*id).map_err(|_| Error::Unavailable)?;
                ExportedPane::capture(&mux, id, &self.client)
            })
            .collect::<Result<_>>()?;
        self.begin_scoped_approval(
            GrantScope::SelectedTerminals,
            targets,
            permission,
            seconds,
            renewal,
        )
    }

    pub fn begin_all_local_approval(
        &self,
        permission: Permission,
        seconds: u64,
        renewal: Option<GrantRef>,
    ) -> Result<()> {
        self.begin_scoped_approval(
            GrantScope::AllLocalTerminals,
            Vec::new(),
            permission,
            seconds,
            renewal,
        )
    }

    fn begin_scoped_approval(
        &self,
        scope: GrantScope,
        targets: Vec<Arc<ExportedPane>>,
        permission: Permission,
        seconds: u64,
        renewal: Option<GrantRef>,
    ) -> Result<()> {
        let mut state = self.host.state.lock();
        self.host.poll(&mut state);
        let result: Result<()> = (|| {
            if let Some(pending) = &state.pending {
                // A second press of this route's button, before the status
                // that hides it arrived, is the approval already running.
                return if pending.route == self.id() {
                    Ok(())
                } else {
                    Err(Error::Busy)
                };
            }
            if state.native.is_none() {
                state.native = Some(NativeSession::start(self.host.authority.clone())?);
            }
            let approval = match scope {
                GrantScope::SelectedTerminals => self.host.authority.prepare(
                    &self.route,
                    targets.iter().map(|target| target.target()).collect(),
                    permission,
                    seconds,
                    renewal,
                )?,
                GrantScope::AllLocalTerminals => self.host.authority.prepare_all_local(
                    &self.route,
                    permission,
                    seconds,
                    renewal,
                )?,
            };
            let attempt = state
                .native
                .as_ref()
                .ok_or(Error::Unavailable)?
                .authenticate(approval, &self.source, &self.language)?;
            state.pending = Some(PendingApproval {
                route: self.id(),
                renewal,
                attempt,
                targets,
            });
            Ok(())
        })();
        match result {
            Ok(()) => {
                state.errors.remove(&self.id());
            }
            // The answer to this command is gone with the next status; this
            // stays until the user moves on from it.
            Err(error) => {
                state.errors.insert(self.id(), error.into());
            }
        }
        if result.is_err() && state.grants.is_empty() && state.pending.is_none() {
            state.native = None;
        }
        result
    }

    pub fn cancel_approval(&self) {
        let mut state = self.host.state.lock();
        state.errors.remove(&self.id());
        if state
            .pending
            .as_ref()
            .is_some_and(|pending| pending.route == self.id())
        {
            state.pending = None;
        }
        self.host.poll(&mut state);
    }

    pub fn revoke(&self, grant: GrantRef) -> Result<()> {
        let mut state = self.host.state.lock();
        self.host.authority.revoke_current(&self.route, grant.id)?;
        if state.pending.as_ref().is_some_and(|pending| {
            pending.route == self.id()
                && pending
                    .renewal
                    .is_some_and(|renewal| renewal.id == grant.id)
        }) {
            state.pending = None;
        }
        if state
            .grants
            .get(&grant.id)
            .is_some_and(|stored| stored.route == self.id())
        {
            state.grants.remove(&grant.id);
        }
        self.host.poll(&mut state);
        Ok(())
    }

    pub fn status(&self) -> HostStatus {
        let mut state = self.host.state.lock();
        self.host.poll(&mut state);
        let grants = self
            .host
            .endpoint
            .list(&self.route)
            .unwrap_or_default()
            .into_iter()
            .map(|info| ActiveGrant {
                grant: info.grant,
                scope: info.scope,
                permission: info.permission,
                targets: info
                    .targets
                    .iter()
                    .map(|target| (target.pane, target.incarnation))
                    .collect(),
                remaining_ms: info.remaining.as_millis().min(u64::MAX as u128) as u64,
            })
            .collect();
        HostStatus {
            grants,
            authenticating: state
                .pending
                .as_ref()
                .is_some_and(|pending| pending.route == self.id()),
            error: state.errors.get(&self.id()).copied(),
        }
    }

    /// The page of a listing after `after`, with what each row needs. By
    /// position in the order rather than by identity, so paging goes on past
    /// a terminal that closed under it.
    fn list_page(&self, after: Option<ExportRef>) -> Result<(Vec<ListRow>, bool)> {
        fn order(export: &ExportRef) -> ([u8; 16], u64, u64) {
            (
                *export.grant.id.as_bytes(),
                export.grant.version,
                export.target.pane,
            )
        }
        let mut state = self.host.state.lock();
        self.host.poll(&mut state);
        let mut items = Vec::new();
        for info in self.host.endpoint.list(&self.route)? {
            for target in &info.targets {
                items.push((
                    ExportRef {
                        grant: info.grant,
                        target: *target,
                    },
                    info.permission,
                    info.remaining.as_millis().min(u64::MAX as u128) as u64,
                ));
            }
        }
        items.sort_by_key(|(export, _, _)| order(export));
        let start = after.map_or(0, |after| {
            items.partition_point(|(export, _, _)| order(export) <= order(&after))
        });
        let more = items.len() > start + 32;
        let page = items
            .into_iter()
            .skip(start)
            .take(32)
            .map(|(export, permission, remaining_ms)| {
                Ok((
                    export,
                    permission,
                    remaining_ms,
                    find_target(&state, self.id(), export)?,
                ))
            })
            .collect::<Result<_>>()?;
        Ok((page, more))
    }

    /// The work ends at `operation`, as in `PreparedReply::send`. List and
    /// read permits also cover the writing of their reply, so they run to
    /// `deadline`: expiring first, they would stop a frame partway through.
    /// Input's is over with the operation.
    pub async fn prepare_reply(
        self: &Arc<Self>,
        sequence: u64,
        command: Command,
        payload: Option<Payload>,
        operation: &Deadline,
        deadline: &Deadline,
    ) -> Result<PreparedReply> {
        command.validate()?;
        let reservation = self.budget.acquire(64 * 1024)?;
        if let Command::List { after } = command {
            let permit =
                self.host
                    .endpoint
                    .accept_list_for(&self.route, sequence, deadline.remaining()?)?;
            let (page, more) = self.list_page(after)?;
            let mut exports = Vec::new();
            let mut targets = Vec::new();
            for (export, permission, remaining_ms, target) in page {
                let title = operation
                    .retry_busy(|_| Poll::Ready(target.with_current(|local| local.connect_title())))
                    .await;
                let title = match title {
                    Ok(title) => title,
                    // Closed since the page was chosen: no longer one of them.
                    Err(Error::Unavailable) => continue,
                    Err(error) => return Err(error),
                };
                exports.push(ExportSummary {
                    export,
                    name: export.target.pane.to_string(),
                    title,
                    permission,
                    remaining_ms,
                });
                targets.push(target);
            }
            return Ok(PreparedReply::List {
                exports,
                more,
                targets,
                permit,
                _reservation: reservation,
            });
        }
        let (export, kind, permitted) = match &command {
            Command::Read { export, .. } => (*export, Operation::Read, deadline),
            Command::Text { export, .. } | Command::Keys { export, .. } => {
                (*export, Operation::Input, operation)
            }
            _ => return Err(Error::Protocol),
        };
        let permit = Arc::new(self.host.endpoint.accept_for(
            &self.route,
            Request {
                grant: export.grant,
                target: export.target,
                operation: kind,
                sequence,
            },
            permitted.remaining()?,
        )?);
        let target = {
            let mut state = self.host.state.lock();
            self.host.poll(&mut state);
            find_target(&state, self.id(), export)?
        };
        match command {
            Command::Read { options, .. } => {
                let snapshot = target.read(permit, options, reservation, operation).await?;
                self.budget.charge_output(snapshot.len())?;
                Ok(PreparedReply::Text(snapshot))
            }
            Command::Text {
                bytes,
                no_paste,
                enter,
                ..
            } => {
                let mut payload = payload.ok_or(Error::Protocol)?;
                if payload.text().len() != bytes {
                    return Err(Error::Protocol);
                }
                payload.reservation().grow(bytes)?;
                let input = Input::Text {
                    text: payload.text().to_string(),
                    no_paste,
                    enter,
                };
                let pending = target.input(permit, input, reservation, operation).await?;
                Ok(PreparedReply::Input {
                    pending,
                    _payload: Some(payload),
                })
            }
            Command::Keys { keys, .. } => {
                let keys = keys.into_iter().map(decode_key).collect::<Result<_>>()?;
                Ok(PreparedReply::Input {
                    pending: target
                        .input(permit, Input::Keys(keys), reservation, operation)
                        .await?,
                    _payload: None,
                })
            }
            _ => Err(Error::Protocol),
        }
    }
}

impl Drop for HostRoute {
    fn drop(&mut self) {
        self.route.close();
        // A temporary Weak upgrade during Host::poll can be the last owner.
        // Cleanup must not recursively lock the state being inspected.
        if let Some(mut state) = self.host.state.try_lock() {
            state.routes.remove(&self.id());
            state.errors.remove(&self.id());
            state.grants.retain(|_, grant| grant.route != self.id());
            if state
                .pending
                .as_ref()
                .is_some_and(|pending| pending.route == self.id())
            {
                state.pending = None;
            }
            if state.grants.is_empty() && state.pending.is_none() {
                state.native = None;
            }
        }
    }
}

fn find_target(state: &State, route: Id, export: ExportRef) -> Result<Arc<ExportedPane>> {
    let grant = state
        .grants
        .get(&export.grant.id)
        .filter(|grant| grant.route == route && grant.grant.info().grant == export.grant)
        .ok_or(Error::Unavailable)?;
    grant
        .targets
        .iter()
        .find(|target| target.target() == export.target)
        .cloned()
        .ok_or(Error::Unavailable)
}

fn decode_key(key: Key) -> Result<(wezterm_term::KeyCode, wezterm_term::KeyModifiers)> {
    use wezterm_term::{KeyCode as K, KeyModifiers as M};
    let allowed = M::CTRL | M::ALT | M::SHIFT | M::SUPER;
    let modifiers = M::from_bits(key.modifiers)
        .filter(|mods| allowed.contains(*mods))
        .ok_or(Error::Protocol)?;
    let mut chars = key.name.chars();
    let code = match (chars.next(), chars.next()) {
        (Some(c), None) if !c.is_control() => K::Char(c),
        _ => match key.name.to_ascii_lowercase().as_str() {
            "enter" => K::Enter,
            "tab" => K::Tab,
            "escape" => K::Escape,
            "backspace" => K::Backspace,
            "delete" => K::Delete,
            "up" => K::UpArrow,
            "down" => K::DownArrow,
            "left" => K::LeftArrow,
            "right" => K::RightArrow,
            "home" => K::Home,
            "end" => K::End,
            "pageup" => K::PageUp,
            "pagedown" => K::PageDown,
            "insert" => K::Insert,
            name => match name
                .strip_prefix('f')
                .and_then(|value| value.parse::<u8>().ok())
            {
                Some(number @ 1..=24) => K::Function(number),
                _ => return Err(Error::Protocol),
            },
        },
    };
    Ok((code, modifiers))
}

type ListRow = (ExportRef, Permission, u64, Arc<ExportedPane>);

pub enum PreparedReply {
    List {
        exports: Vec<ExportSummary>,
        more: bool,
        targets: Vec<Arc<ExportedPane>>,
        permit: ListPermit,
        _reservation: Reservation,
    },
    Text(TextSnapshot),
    Input {
        pending: PendingInput,
        _payload: Option<Payload>,
    },
}

impl PreparedReply {
    /// Waiting and preparing end at `operation`; the reply itself may take
    /// until `deadline`. Whatever stops the work between frames is answered,
    /// so the channel stays in step. An error here leaves part of a frame on
    /// it, or no reader, and closes it.
    pub async fn send<S: AsyncRead + AsyncWrite + Unpin>(
        &self,
        channel: &mut Channel<S>,
        sequence: u64,
        operation: &Deadline,
        deadline: &Deadline,
    ) -> Result<()> {
        match self {
            Self::List {
                exports,
                more,
                targets,
                permit,
                ..
            } => {
                let refs: Vec<_> = exports
                    .iter()
                    .map(|export| (export.export.grant, export.export.target))
                    .collect();
                let allowed = operation
                    .retry_busy(|_| {
                        Poll::Ready(with_current_targets(targets, || {
                            permit.execute(&refs, || ())
                        })?)
                    })
                    .await;
                if let Err(error) = allowed {
                    let reply = Reply::Failed(error.into());
                    return channel
                        .write_json(Kind::Reply, sequence, &reply, deadline)
                        .await;
                }
                let frame = json_frame(
                    Kind::Reply,
                    sequence,
                    &Reply::List {
                        exports: exports.clone(),
                        more: *more,
                    },
                )?;
                channel
                    .write_guarded(&frame, deadline, |write| {
                        with_current_targets(targets, || permit.execute(&refs, write))?
                    })
                    .await
            }
            Self::Text(snapshot) => {
                let allowed = operation
                    .retry_busy(|_| Poll::Ready(snapshot.send(0, 0, |_| ())))
                    .await;
                if let Err(error) = allowed {
                    let reply = Reply::Failed(error.into());
                    return channel
                        .write_json(Kind::Reply, sequence, &reply, deadline)
                        .await;
                }
                let header = json_frame(
                    Kind::Reply,
                    sequence,
                    &Reply::Text {
                        bytes: snapshot.len(),
                    },
                )?;
                channel
                    .write_guarded(&header, deadline, |write| snapshot.send(0, 0, |_| write()))
                    .await?;
                let mut offset = 0;
                while offset < snapshot.len() {
                    let frame = operation
                        .retry_busy(|_| {
                            Poll::Ready(snapshot.send(offset, MAX_BLOCK, |bytes| {
                                let mut block = Vec::with_capacity(bytes.len() + 4);
                                block.extend_from_slice(&(offset as u32).to_be_bytes());
                                block.extend_from_slice(bytes);
                                Frame::new(Kind::TextBlock, sequence, &block)
                            })?)
                        })
                        .await;
                    let Ok(frame) = frame else {
                        // The terminal went, or the time ran out, between two
                        // blocks: this reply ends here, and the route goes on.
                        let abandoned = Frame::new(Kind::Cancel, sequence, &[])?;
                        return channel.write(&abandoned, deadline).await;
                    };
                    channel
                        .write_guarded(&frame, deadline, |write| snapshot.send(0, 0, |_| write()))
                        .await?;
                    offset += frame.body().len() - 4;
                }
                Ok(())
            }
            Self::Input { pending, .. } => {
                let result = loop {
                    if let Some(result) = pending.try_result() {
                        break result;
                    }
                    if operation.remaining().is_err() {
                        break pending.cancel();
                    }
                    smol::Timer::after(Duration::from_millis(2)).await;
                };
                channel
                    .write_json(Kind::Reply, sequence, &Reply::Input(result), deadline)
                    .await
            }
        }
    }
}

fn with_current_targets<T>(targets: &[Arc<ExportedPane>], action: impl FnOnce() -> T) -> Result<T> {
    if targets.is_empty() {
        return Ok(action());
    }
    let mux = targets[0].mux.upgrade().ok_or(Error::Unavailable)?;
    let panes = mux.panes.try_read().ok_or(Error::Busy)?;
    let tabs = mux.tabs.try_read().ok_or(Error::Busy)?;
    let registrations = mux.client_registrations.try_read().ok_or(Error::Busy)?;
    let lease = mux.frontend_lease.try_lock().ok_or(Error::Busy)?;
    let mut groups: Vec<(Arc<Tab>, Vec<(PaneId, &PaneMembership)>)> = Vec::new();
    for target in targets {
        if !target
            .mux
            .upgrade()
            .is_some_and(|owner| Arc::ptr_eq(&owner, &mux))
            || registrations.get(&target.client) != Some(&target.registration)
            || Ownership::capture(&lease, target.tab_id, &target.client)? != target.ownership
        {
            return Err(Error::Unavailable);
        }
        let pane = target.pane.upgrade().ok_or(Error::Unavailable)?;
        if !panes
            .get(&pane.pane_id())
            .is_some_and(|current| Arc::ptr_eq(current, &pane))
        {
            return Err(Error::Unavailable);
        }
        pane.downcast_ref::<LocalPane>()
            .ok_or(Error::Unavailable)?
            .connect_eligible()?;
        let tab = target.tab.upgrade().ok_or(Error::Unavailable)?;
        if !tabs
            .get(&target.tab_id)
            .is_some_and(|current| Arc::ptr_eq(current, &tab))
        {
            return Err(Error::Unavailable);
        }
        if let Some((_, panes)) = groups
            .iter_mut()
            .find(|(existing, _)| Arc::ptr_eq(existing, &tab))
        {
            panes.push((pane.pane_id(), &target.membership));
        } else {
            groups.push((tab, vec![(pane.pane_id(), &target.membership)]));
        }
    }
    fn locked<T, F: FnOnce() -> T>(
        groups: &[(Arc<Tab>, Vec<(PaneId, &PaneMembership)>)],
        action: F,
    ) -> Result<T> {
        match groups.split_first() {
            Some(((tab, panes), remaining)) => {
                tab.with_delegated_panes(panes, || locked(remaining, action))?
            }
            None => Ok(action()),
        }
    }
    locked(&groups, action)
}
