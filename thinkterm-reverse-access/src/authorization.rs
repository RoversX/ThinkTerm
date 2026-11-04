use crate::clock::{Clock, SystemClock, Tick};
use crate::{
    Error, Result, AUTH_TIMEOUT_SECS, MAX_DURATION_SECS, MAX_GRANTS, MAX_ROUTES, MAX_TARGETS,
    MAX_TARGETS_PER_GRANT, REQUEST_TIMEOUT_SECS,
};
use std::collections::HashMap;
use std::fmt;
use std::sync::{Arc, Mutex, MutexGuard, Weak};
use std::time::Duration;

#[derive(Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub struct Id([u8; 16]);

impl Id {
    pub fn random() -> Result<Self> {
        let mut bytes = [0; 16];
        getrandom::fill(&mut bytes).map_err(|_| Error::Random)?;
        Ok(Self(bytes))
    }

    pub fn from_bytes(bytes: [u8; 16]) -> Self {
        Self(bytes)
    }

    pub fn as_bytes(&self) -> &[u8; 16] {
        &self.0
    }
}

impl fmt::Debug for Id {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Id(..)")
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum Permission {
    Read,
    ReadAndInput,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum GrantScope {
    SelectedTerminals,
    AllLocalTerminals,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Operation {
    Read,
    Input,
}

impl Permission {
    fn allows(self, operation: Operation) -> bool {
        operation == Operation::Read || self == Self::ReadAndInput
    }
}

/// Captured by the actual host, not copied from a request's pane label.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Target {
    pub pane: u64,
    pub incarnation: Id,
    pub owner: Id,
}

impl Target {
    pub fn new(pane: u64, owner: Id) -> Result<Self> {
        Ok(Self {
            pane,
            incarnation: Id::random()?,
            owner,
        })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GrantRef {
    pub id: Id,
    pub version: u64,
}

#[derive(Debug, Clone)]
pub struct GrantInfo {
    pub grant: GrantRef,
    pub scope: GrantScope,
    pub permission: Permission,
    pub targets: Vec<Target>,
    pub remaining: Duration,
}

struct Grant {
    approval: Id,
    reference: GrantRef,
    route: Id,
    scope: GrantScope,
    permission: Permission,
    targets: Vec<Target>,
    deadline: Tick,
}

struct RouteState {
    // Retained after revocation and renewal, until this route closes.
    last_request: u64,
    control_deadline: Option<Tick>,
}

struct Intent {
    id: Id,
    route: Id,
    scope: GrantScope,
    permission: Permission,
    targets: Vec<Target>,
    duration: Duration,
    renewal: Option<GrantRef>,
    deadline: Tick,
    session: u64,
}

struct State {
    closed: bool,
    routes: HashMap<Id, RouteState>,
    grants: HashMap<Id, Grant>,
    pending: Option<Intent>,
    session: u64,
    session_available: bool,
    monitor_deadline: Option<Tick>,
    last_tick: Option<Tick>,
}

impl State {
    fn revoke_all(&mut self) {
        self.grants.clear();
        self.pending = None;
    }

    fn observe(&mut self, value: Result<Tick>) -> Result<Tick> {
        if self.closed {
            return Err(Error::Unavailable);
        }
        let now = match value {
            Ok(now) if self.last_tick.is_none_or(|last| now >= last) => now,
            _ => {
                self.revoke_all();
                self.session_available = false;
                return Err(Error::Clock);
            }
        };
        self.last_tick = Some(now);
        self.routes
            .retain(|_, route| route.control_deadline.is_none_or(|deadline| now < deadline));
        self.grants
            .retain(|_, grant| self.routes.contains_key(&grant.route));
        if self
            .pending
            .as_ref()
            .is_some_and(|intent| !self.routes.contains_key(&intent.route))
        {
            self.pending = None;
        }
        if self
            .monitor_deadline
            .is_some_and(|deadline| now >= deadline)
        {
            self.revoke_all();
            self.session_available = false;
            self.monitor_deadline = None;
        }
        self.grants.retain(|_, grant| now < grant.deadline);
        if self
            .pending
            .as_ref()
            .is_some_and(|intent| now >= intent.deadline)
        {
            self.pending = None;
        }
        Ok(now)
    }
}

struct Shared {
    state: Mutex<State>,
    clock: Arc<dyn Clock>,
}

impl Shared {
    fn lock(&self) -> Result<(MutexGuard<'_, State>, Tick)> {
        let mut state = self.state.lock().map_err(|_| Error::Unavailable)?;
        let now = state.observe(self.clock.now())?;
        Ok((state, now))
    }
}

/// Kept by the native approval UI in the actual terminal host. Never put
/// this handle in a Lua value, configuration object, RPC, or plugin API.
pub struct Authority {
    shared: Arc<Shared>,
}

/// Data-plane access has no grant creation or authentication entry point.
#[derive(Clone)]
pub struct Endpoint {
    shared: Arc<Shared>,
}

impl Authority {
    pub fn new() -> (Self, Endpoint) {
        Self::with_clock(SystemClock::shared())
    }

    fn with_clock(clock: Arc<dyn Clock>) -> (Self, Endpoint) {
        let shared = Arc::new(Shared {
            clock,
            state: Mutex::new(State {
                closed: false,
                routes: HashMap::new(),
                grants: HashMap::new(),
                pending: None,
                session: 0,
                session_available: false,
                monitor_deadline: None,
                last_tick: None,
            }),
        });
        (
            Self {
                shared: shared.clone(),
            },
            Endpoint { shared },
        )
    }

    #[cfg(test)]
    fn session_changed(&self, available: bool) -> Result<()> {
        let (mut state, _) = self.shared.lock()?;
        state.revoke_all();
        state.session = state.session.checked_add(1).ok_or(Error::Unavailable)?;
        state.session_available = available;
        state.monitor_deadline = None;
        Ok(())
    }

    /// Created only after the packaged native monitor has connected through
    /// its inherited pipe. A missed heartbeat invalidates access at execution,
    /// even when the UI or the monitor reader is no longer being scheduled.
    pub(crate) fn start_monitor(&self) -> Result<Monitor> {
        let (mut state, now) = self.shared.lock()?;
        state.revoke_all();
        state.session = state.session.checked_add(1).ok_or(Error::Unavailable)?;
        state.session_available = true;
        state.monitor_deadline = Some(now.after(Duration::from_secs(5))?);
        Ok(Monitor {
            shared: Arc::downgrade(&self.shared),
            session: state.session,
        })
    }

    /// Only the owner of a verified Connect session may hold this route.
    /// Dropping it removes grants and the route's replay watermark.
    pub fn register_route(&self) -> Result<Route> {
        let id = Id::random()?;
        let (mut state, _) = self.shared.lock()?;
        if state.routes.len() >= MAX_ROUTES {
            return Err(Error::Busy);
        }
        state.routes.insert(
            id,
            RouteState {
                last_request: 0,
                control_deadline: None,
            },
        );
        Ok(Route {
            id,
            shared: Arc::downgrade(&self.shared),
        })
    }

    pub fn prepare(
        &self,
        route: &Route,
        targets: Vec<Target>,
        permission: Permission,
        seconds: u64,
        renewal: Option<GrantRef>,
    ) -> Result<Approval> {
        self.prepare_scope(
            route,
            GrantScope::SelectedTerminals,
            targets,
            permission,
            seconds,
            renewal,
        )
    }

    pub fn prepare_all_local(
        &self,
        route: &Route,
        permission: Permission,
        seconds: u64,
        renewal: Option<GrantRef>,
    ) -> Result<Approval> {
        self.prepare_scope(
            route,
            GrantScope::AllLocalTerminals,
            Vec::new(),
            permission,
            seconds,
            renewal,
        )
    }

    fn prepare_scope(
        &self,
        route: &Route,
        scope: GrantScope,
        targets: Vec<Target>,
        permission: Permission,
        seconds: u64,
        renewal: Option<GrantRef>,
    ) -> Result<Approval> {
        if seconds == 0 || seconds > MAX_DURATION_SECS {
            return Err(Error::Duration);
        }
        if scope == GrantScope::SelectedTerminals
            && (targets.is_empty() || targets.len() > MAX_TARGETS_PER_GRANT)
        {
            return Err(Error::TooLarge);
        }
        let mut unique = std::collections::HashSet::new();
        if targets.iter().any(|target| !unique.insert(target.pane)) {
            return Err(Error::Protocol);
        }
        if !route.belongs_to(&self.shared) {
            return Err(Error::Unavailable);
        }
        let id = Id::random()?;
        let (mut state, now) = self.shared.lock()?;
        if !state.session_available || !state.routes.contains_key(&route.id) {
            return Err(Error::Unavailable);
        }
        if state.pending.is_some() {
            return Err(Error::Busy);
        }
        if let Some(reference) = renewal {
            let old = state.grants.get(&reference.id).ok_or(Error::Revoked)?;
            if old.reference != reference || old.route != route.id || old.scope != scope {
                return Err(Error::Revoked);
            }
        }
        if state.grants.values().any(|grant| {
            grant.route == route.id
                && Some(grant.reference) != renewal
                && (scope == GrantScope::AllLocalTerminals
                    || grant.scope == GrantScope::AllLocalTerminals
                    || grant
                        .targets
                        .iter()
                        .any(|target| unique.contains(&target.pane)))
        }) {
            return Err(Error::Busy);
        }
        state.pending = Some(Intent {
            id,
            route: route.id,
            scope,
            permission,
            targets,
            duration: Duration::from_secs(seconds),
            renewal,
            deadline: now.after(Duration::from_secs(AUTH_TIMEOUT_SECS))?,
            session: state.session,
        });
        Ok(Approval {
            id,
            shared: Arc::downgrade(&self.shared),
        })
    }

    /// Only the actual host can refresh the eligible terminals covered by a
    /// native approval that explicitly included current and future terminals.
    /// Updating membership never renews the grant or its replay watermark.
    pub fn replace_all_local_targets(
        &self,
        route: &Route,
        reference: GrantRef,
        targets: Vec<Target>,
    ) -> Result<()> {
        if !route.belongs_to(&self.shared) {
            return Err(Error::Unavailable);
        }
        if targets.len() > MAX_TARGETS {
            return Err(Error::TooLarge);
        }
        let mut unique = std::collections::HashSet::new();
        if targets.iter().any(|target| !unique.insert(target.pane)) {
            return Err(Error::Protocol);
        }
        let (mut state, _) = self.shared.lock()?;
        let grant = state.grants.get_mut(&reference.id).ok_or(Error::Revoked)?;
        if grant.route != route.id
            || grant.reference != reference
            || grant.scope != GrantScope::AllLocalTerminals
        {
            return Err(Error::Revoked);
        }
        grant.targets = targets;
        Ok(())
    }

    pub fn close(&self) {
        if let Ok(mut state) = self.shared.state.lock() {
            state.closed = true;
            state.revoke_all();
            state.routes.clear();
            state.session_available = false;
            state.monitor_deadline = None;
        }
    }

    /// Version-specific cleanup must not revoke a successful replacement.
    pub fn revoke(&self, reference: GrantRef) -> Result<()> {
        let (mut state, _) = self.shared.lock()?;
        if state
            .grants
            .get(&reference.id)
            .is_some_and(|grant| grant.reference == reference)
        {
            state.grants.remove(&reference.id);
        }
        if state
            .pending
            .as_ref()
            .is_some_and(|intent| intent.renewal == Some(reference))
        {
            state.pending = None;
        }
        Ok(())
    }

    /// Explicit user revocation covers the route's current grant and pending
    /// renewal even when the UI still holds an older version of the grant.
    pub fn revoke_current(&self, route: &Route, id: Id) -> Result<()> {
        if !route.belongs_to(&self.shared) {
            return Err(Error::Unavailable);
        }
        let (mut state, _) = self.shared.lock()?;
        if state
            .grants
            .get(&id)
            .is_some_and(|grant| grant.route == route.id)
        {
            state.grants.remove(&id);
        }
        if state.pending.as_ref().is_some_and(|intent| {
            intent.route == route.id && intent.renewal.is_some_and(|grant| grant.id == id)
        }) {
            state.pending = None;
        }
        Ok(())
    }

    /// Called before removal, ownership changes, or host/domain replacement.
    pub fn invalidate_target(&self, target: Target) -> Result<()> {
        let (mut state, _) = self.shared.lock()?;
        state.grants.retain(|_, grant| {
            grant.scope == GrantScope::AllLocalTerminals || !grant.targets.contains(&target)
        });
        for grant in state
            .grants
            .values_mut()
            .filter(|grant| grant.scope == GrantScope::AllLocalTerminals)
        {
            grant.targets.retain(|current| *current != target);
        }
        if state
            .pending
            .as_ref()
            .is_some_and(|intent| intent.targets.contains(&target))
        {
            state.pending = None;
        }
        Ok(())
    }
}

impl Drop for Authority {
    fn drop(&mut self) {
        if let Ok(mut state) = self.shared.state.lock() {
            state.revoke_all();
            state.session_available = false;
        }
    }
}

/// A single-use, in-process completion capability. Native authentication
/// owns it while its dialog is open. There is no wire representation.
pub struct Approval {
    id: Id,
    shared: Weak<Shared>,
}

pub(crate) struct ApprovalDetails {
    pub id: Id,
    pub scope: GrantScope,
    pub targets: Vec<Target>,
    pub permission: Permission,
    pub seconds: u64,
}

impl Approval {
    pub(crate) fn cancellation(&self) -> ApprovalCancellation {
        ApprovalCancellation {
            id: self.id,
            shared: self.shared.clone(),
        }
    }

    pub(crate) fn details(&self) -> Result<ApprovalDetails> {
        let shared = self.shared.upgrade().ok_or(Error::StaleApproval)?;
        let (state, _) = shared.lock()?;
        let intent = state
            .pending
            .as_ref()
            .filter(|intent| intent.id == self.id)
            .ok_or(Error::StaleApproval)?;
        Ok(ApprovalDetails {
            id: self.id,
            scope: intent.scope,
            targets: intent.targets.clone(),
            permission: intent.permission,
            seconds: intent.duration.as_secs(),
        })
    }

    pub fn reject(self) {}

    /// Invoke only from the native provider's successful completion. The
    /// handle freezes the intent; the callback cannot supply a new scope.
    pub(crate) fn authenticated(self) -> Result<GrantInfo> {
        let shared = self.shared.upgrade().ok_or(Error::StaleApproval)?;
        let (mut state, now) = shared.lock()?;
        if !state
            .pending
            .as_ref()
            .is_some_and(|intent| intent.id == self.id)
        {
            return Err(Error::StaleApproval);
        }
        let intent = state.pending.take().ok_or(Error::StaleApproval)?;
        if !state.session_available
            || state.session != intent.session
            || !state.routes.contains_key(&intent.route)
        {
            return Err(Error::StaleApproval);
        }
        let reference = if let Some(reference) = intent.renewal {
            let old = state
                .grants
                .get(&reference.id)
                .ok_or(Error::StaleApproval)?;
            if old.reference != reference || old.route != intent.route {
                return Err(Error::StaleApproval);
            }
            GrantRef {
                id: reference.id,
                version: reference
                    .version
                    .checked_add(1)
                    .ok_or(Error::StaleApproval)?,
            }
        } else {
            if state.grants.len() >= MAX_GRANTS {
                return Err(Error::Busy);
            }
            GrantRef {
                id: Id::random()?,
                version: 1,
            }
        };
        let retained = state
            .grants
            .values()
            .filter(|grant| {
                grant.reference.id != reference.id && grant.scope == GrantScope::SelectedTerminals
            })
            .map(|grant| grant.targets.len())
            .sum::<usize>();
        if intent.scope == GrantScope::SelectedTerminals
            && retained + intent.targets.len() > MAX_TARGETS
        {
            return Err(Error::Busy);
        }
        let info = GrantInfo {
            grant: reference,
            scope: intent.scope,
            permission: intent.permission,
            targets: intent.targets.clone(),
            remaining: intent.duration,
        };
        state.grants.insert(
            reference.id,
            Grant {
                approval: self.id,
                reference,
                route: intent.route,
                scope: intent.scope,
                permission: intent.permission,
                targets: intent.targets,
                deadline: now.after(intent.duration)?,
            },
        );
        Ok(info)
    }
}

pub(crate) struct ApprovalCancellation {
    id: Id,
    shared: Weak<Shared>,
}

impl ApprovalCancellation {
    pub(crate) fn cancel(&self) {
        if let Some(shared) = self.shared.upgrade() {
            if let Ok(mut state) = shared.state.lock() {
                if state
                    .pending
                    .as_ref()
                    .is_some_and(|intent| intent.id == self.id)
                {
                    state.pending = None;
                }
                state.grants.retain(|_, grant| grant.approval != self.id);
            }
        }
    }
}

pub(crate) struct Monitor {
    shared: Weak<Shared>,
    session: u64,
}

impl Monitor {
    pub(crate) fn check(&self) -> Result<()> {
        let shared = self.shared.upgrade().ok_or(Error::Unavailable)?;
        let (state, _) = shared.lock()?;
        if state.session_available && state.session == self.session {
            Ok(())
        } else {
            Err(Error::Unavailable)
        }
    }

    pub(crate) fn heartbeat(&self) -> Result<()> {
        let shared = self.shared.upgrade().ok_or(Error::Unavailable)?;
        let (mut state, now) = shared.lock()?;
        if !state.session_available || state.session != self.session {
            return Err(Error::Unavailable);
        }
        state.monitor_deadline = Some(now.after(Duration::from_secs(5))?);
        Ok(())
    }

    pub(crate) fn close(&self) {
        if let Some(shared) = self.shared.upgrade() {
            if let Ok(mut state) = shared.state.lock() {
                if state.session == self.session {
                    state.revoke_all();
                    state.session_available = false;
                    state.monitor_deadline = None;
                }
            }
        }
    }
}

impl Drop for Monitor {
    fn drop(&mut self) {
        self.close();
    }
}

impl Drop for Approval {
    fn drop(&mut self) {
        if let Some(shared) = self.shared.upgrade() {
            if let Ok(mut state) = shared.state.lock() {
                if state
                    .pending
                    .as_ref()
                    .is_some_and(|intent| intent.id == self.id)
                {
                    state.pending = None;
                }
            }
        }
    }
}

pub struct Route {
    id: Id,
    shared: Weak<Shared>,
}

impl Route {
    pub fn id(&self) -> Id {
        self.id
    }

    /// Only the trusted local management connection calls this. Business
    /// traffic has an Endpoint and cannot renew a control lease.
    pub fn heartbeat(&self) -> Result<()> {
        let shared = self.shared.upgrade().ok_or(Error::Unavailable)?;
        let (mut state, now) = shared.lock()?;
        state
            .routes
            .get_mut(&self.id)
            .ok_or(Error::Unavailable)?
            .control_deadline = Some(now.after(Duration::from_secs(5))?);
        Ok(())
    }

    pub fn close(&self) {
        if let Some(shared) = self.shared.upgrade() {
            if let Ok(mut state) = shared.state.lock() {
                state.routes.remove(&self.id);
                state.grants.retain(|_, grant| grant.route != self.id);
                if state
                    .pending
                    .as_ref()
                    .is_some_and(|intent| intent.route == self.id)
                {
                    state.pending = None;
                }
            }
        }
    }

    fn belongs_to(&self, shared: &Arc<Shared>) -> bool {
        self.shared
            .upgrade()
            .is_some_and(|owner| Arc::ptr_eq(&owner, shared))
    }
}

impl Drop for Route {
    fn drop(&mut self) {
        self.close();
    }
}

#[derive(Clone, Copy, Debug)]
pub struct Request {
    pub grant: GrantRef,
    pub target: Target,
    pub operation: Operation,
    pub sequence: u64,
}

fn validate(state: &State, route: Id, request: Request) -> Result<()> {
    if !state.session_available || !state.routes.contains_key(&route) {
        return Err(Error::Unavailable);
    }
    let grant = state
        .grants
        .get(&request.grant.id)
        .ok_or(Error::Unavailable)?;
    if grant.route != route
        || grant.reference != request.grant
        || !grant.permission.allows(request.operation)
        || !grant.targets.contains(&request.target)
    {
        return Err(Error::Unavailable);
    }
    Ok(())
}

impl Endpoint {
    pub fn accept_list(&self, route: &Route, sequence: u64) -> Result<ListPermit> {
        self.accept_list_for(route, sequence, Duration::from_secs(REQUEST_TIMEOUT_SECS))
    }

    pub fn accept_list_for(
        &self,
        route: &Route,
        sequence: u64,
        remaining: Duration,
    ) -> Result<ListPermit> {
        validate_remaining(remaining)?;
        if !route.belongs_to(&self.shared) {
            return Err(Error::Unavailable);
        }
        let (mut state, now) = self.shared.lock()?;
        if !state.session_available {
            return Err(Error::Unavailable);
        }
        let route_state = state.routes.get_mut(&route.id).ok_or(Error::Unavailable)?;
        if sequence == 0 || sequence <= route_state.last_request {
            return Err(Error::Replay);
        }
        if sequence == u64::MAX {
            state.routes.remove(&route.id);
            state.grants.retain(|_, grant| grant.route != route.id);
            return Err(Error::Protocol);
        }
        route_state.last_request = sequence;
        Ok(ListPermit {
            shared: self.shared.clone(),
            route: route.id,
            deadline: now.after(remaining)?,
        })
    }

    pub fn list(&self, route: &Route) -> Result<Vec<GrantInfo>> {
        if !route.belongs_to(&self.shared) {
            return Err(Error::Unavailable);
        }
        let (state, now) = self.shared.lock()?;
        if !state.session_available || !state.routes.contains_key(&route.id) {
            return Err(Error::Unavailable);
        }
        state
            .grants
            .values()
            .filter(|grant| grant.route == route.id)
            .map(|grant| {
                Ok(GrantInfo {
                    grant: grant.reference,
                    scope: grant.scope,
                    permission: grant.permission,
                    targets: grant.targets.clone(),
                    remaining: grant.deadline.elapsed_since(now)?,
                })
            })
            .collect()
    }

    /// Reserve the sequence before any terminal access. Dropping a permit
    /// never makes its sequence eligible for retry.
    pub fn accept(&self, route: &Route, request: Request) -> Result<RequestPermit> {
        self.accept_for(route, request, Duration::from_secs(REQUEST_TIMEOUT_SECS))
    }

    pub fn accept_for(
        &self,
        route: &Route,
        request: Request,
        remaining: Duration,
    ) -> Result<RequestPermit> {
        validate_remaining(remaining)?;
        if !route.belongs_to(&self.shared) {
            return Err(Error::Unavailable);
        }
        let (mut state, now) = self.shared.lock()?;
        validate(&state, route.id, request)?;
        let route_state = state.routes.get_mut(&route.id).ok_or(Error::Unavailable)?;
        if request.sequence == 0 || request.sequence <= route_state.last_request {
            return Err(Error::Replay);
        }
        if request.sequence == u64::MAX {
            state.routes.remove(&route.id);
            state.grants.retain(|_, grant| grant.route != route.id);
            return Err(Error::Protocol);
        }
        route_state.last_request = request.sequence;
        Ok(RequestPermit {
            shared: self.shared.clone(),
            route: route.id,
            request,
            deadline: now.after(remaining)?,
        })
    }
}

fn validate_remaining(remaining: Duration) -> Result<()> {
    if remaining.is_zero() {
        return Err(Error::Timeout);
    }
    if remaining > Duration::from_secs(REQUEST_TIMEOUT_SECS) {
        return Err(Error::Protocol);
    }
    Ok(())
}

pub struct RequestPermit {
    shared: Arc<Shared>,
    route: Id,
    request: Request,
    deadline: Tick,
}

pub struct ListPermit {
    shared: Arc<Shared>,
    route: Id,
    deadline: Tick,
}

impl ListPermit {
    pub fn execute<T>(
        &self,
        exports: &[(GrantRef, Target)],
        send: impl FnOnce() -> T,
    ) -> Result<T> {
        let (state, now) = self.shared.lock()?;
        if now >= self.deadline {
            return Err(Error::Timeout);
        }
        if !state.session_available || !state.routes.contains_key(&self.route) {
            return Err(Error::Unavailable);
        }
        for &(grant, target) in exports {
            validate(
                &state,
                self.route,
                Request {
                    grant,
                    target,
                    operation: Operation::Read,
                    sequence: 1,
                },
            )?;
        }
        Ok(send())
    }
}

impl RequestPermit {
    /// Serializes a final check with one nonblocking host submission or
    /// transport write. Never enqueue unchecked work from this closure.
    pub fn execute<T>(&self, current_target: Target, submit: impl FnOnce() -> T) -> Result<T> {
        let (state, now) = self.shared.lock()?;
        if now >= self.deadline {
            return Err(Error::Timeout);
        }
        if current_target != self.request.target {
            return Err(Error::Unavailable);
        }
        validate(&state, self.route, self.request)?;
        let result = submit();
        drop(state);
        Ok(result)
    }
}

#[cfg(test)]
mod tests;
