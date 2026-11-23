use core::net::{Ipv4Addr, Ipv6Addr};
use core::time::Duration;
use std::sync::Arc;

use futures::StreamExt;
use kameo::{
    actor::{ActorRef, Spawn},
    message::{Context, StreamMessage},
    prelude::{Message, ReplySender},
    reply::DelegatedReply,
    supervision::RestartPolicy,
};
use ts_control::{
    ControlDialer, DialPlan, Endpoint, Error as ControlError, Node, RegistrationError, StateUpdate,
    client::{HttpConn, handle_ping, map_stream, send_map_request},
};

use crate::{
    Task,
    derp_latency::{DerpLatencyMeasurement, DerpLatencyMeasurer},
    direct,
    env::Env,
};

/// Actor responsible for maintaining the connection to control.
///
/// This actor is responsible for proxying the map response stream onto the message bus.
pub struct ControlRunner {
    params: Params,
    state: RegState,

    derp_latency_measurement: Option<DerpLatencyMeasurement>,

    endpoints: Vec<Endpoint>,

    self_node: Option<Node>,
    pending_node_requests: Vec<PendingNodeRequest>,
}

/// The response to [`AuthUrl`]: what the state of control server auth is.
#[derive(Debug, Clone, PartialEq, Eq, kameo::Reply)]
pub enum AuthResponse {
    /// The device is authorized.
    Authorized,
    /// The device is not authorized and an interactive auth URL is provided.
    InteractiveAuthRequired(url::Url),
    /// The device is not authorized because the key was rejected.
    ///
    /// Interactive auth is impossible (control server doesn't supply a URL in this case).
    // ThinkTerm (sign-in state): control answers a machine waiting for an admin's approval the
    // same way, and the runner asks again every 10 seconds (sign-in retry): it may still get in.
    KeyRejected,
}

enum RegState {
    NotRegistered {
        pending_auth_requests: Vec<ReplySender<AuthResponse>>,
    },
    AuthRequired(url::Url),
    Registered(HttpConn),
    AuthKeyRejected,
    // ThinkTerm (sign-in state): added.
    /// The registration failed for the reason given (a machine control knows and has not let
    /// in is `AuthKeyRejected` instead). The runner stays to say why (see
    /// `current_registration_error`) rather than stopping, which would leave every question to
    /// it unanswered and the reason only in the log; the registration is asked again after a
    /// while (sign-in retry, `register_again`).
    Failed(String),
}

impl RegState {
    /// Update this to `new_state`, sending `value_for_pending` to any pending auth
    /// requests.
    fn update(&mut self, new_state: Self, value_for_pending: AuthResponse) {
        let old_state = core::mem::replace(self, new_state);

        if let RegState::NotRegistered {
            pending_auth_requests,
        } = old_state
        {
            for req in pending_auth_requests {
                req.send(value_for_pending.clone());
            }
        }
    }
}

// ThinkTerm (sign-in state): added.
impl RegState {
    /// Why control turned the device away, in this state.
    fn turned_away(&self) -> Option<String> {
        match self {
            RegState::Failed(reason) => Some(reason.clone()),
            // Upstream takes such a machine for one whose auth key was rejected; it may as well
            // be waiting for an admin's approval.
            RegState::AuthKeyRejected => Some(NOT_LET_IN.to_string()),
            RegState::NotRegistered { .. }
            | RegState::AuthRequired(_)
            | RegState::Registered(_) => None,
        }
    }
}

/// Control runner args.
#[derive(Clone)]
pub struct Params {
    /// Control config.
    pub(crate) config: ts_control::Config,

    /// Auth key (if needed).
    pub(crate) auth_key: Option<String>,

    /// The [`Env`] for this actor.
    pub(crate) env: Env,
}

#[doc(hidden)]
#[derive(Debug, Clone, thiserror::Error)]
pub enum ControlRunnerError {
    #[error(transparent)]
    Control(#[from] ControlError),

    #[error(transparent)]
    Crate(#[from] crate::Error),
}

#[derive(Clone)]
struct AuthRequired(url::Url);

struct RegisterResult(Result<HttpConn, RegistrationError>);

impl kameo::Actor for ControlRunner {
    type Args = Params;
    type Error = ControlRunnerError;

    async fn on_start(params: Params, slf: ActorRef<Self>) -> Result<Self, Self::Error> {
        let client = params
            .env
            .ask::<DialerActor, _>(
                None,
                DialNext {
                    url: params.config.server_url.clone(),
                },
                true,
            )
            .await?;

        Task::supervise_with(&slf, {
            let aref = slf.downgrade();
            let client = client.clone();
            let params = params.clone();

            move || {
                let aref = aref.clone();
                let client = client.clone();
                let params = params.clone();

                async move {
                    let mut followup = None;
                    // ThinkTerm (sign-in retry): the connection the next try goes on, the wait
                    // before it, and how many follow-ups in a row were cut short.
                    let mut client = client;
                    let mut wait = REGISTER_RETRY_FIRST;
                    let mut cut = 0;

                    loop {
                        let result = ts_control::register(
                            &params.config,
                            &params.config.server_url,
                            params.auth_key.as_deref(),
                            // ThinkTerm (sign-in retry): kept for a follow-up asked again.
                            followup.clone(),
                            &params.env.keys,
                            &client,
                        )
                        .await;

                        if let Err(RegistrationError::MachineNotAuthorized(Some(u))) = &result {
                            let u = u.clone();
                            tracing::warn!(auth_url = %u, "machine not authorized");
                            followup = Some(u.clone());
                            // ThinkTerm (sign-in retry): a sign-in under way starts afresh.
                            wait = REGISTER_RETRY_FIRST;
                            cut = 0;

                            let Some(aref) = aref.upgrade() else {
                                // if the control runner is dead, we should die shortly, no reason
                                // to keep running.
                                return;
                            };

                            if let Err(e) = aref.tell(AuthRequired(u)).await {
                                tracing::error!(error = %e, "failed to tell control runner required auth");
                            }

                            continue;
                        }

                        // ThinkTerm (sign-in retry): a follow-up cut short -- its long poll
                        // closed under it, by the network or the app put away while its user
                        // signs in -- is asked again with the same address, the sign-in page
                        // still open on it, the device still shown as signing in; cut twice in
                        // a row, the sign-in is asked anew, which gives a new address while it
                        // is not done. A device control has not let in is no cut.
                        if let Err(e) = &result
                            && followup.is_some()
                            && *e != RegistrationError::MachineNotAuthorized(None)
                        {
                            tracing::warn!(error = %e, "sign-in follow-up cut short; asking again");
                            cut += 1;
                            if cut >= 2 {
                                followup = None;
                                cut = 0;
                            }
                            tokio::time::sleep(wait).await;
                            wait = (wait * 2).min(REGISTER_RETRY_MAX);
                            client = redial(&params, client).await;
                            continue;
                        }

                        let Some(aref) = aref.upgrade() else {
                            return;
                        };
                        // ThinkTerm (sign-in retry): a registration that failed is told (the
                        // runner's Failed or AuthKeyRejected state, which says why) and asked
                        // again after a while, as tailscaled does, rather than given up: a
                        // control server out of reach for a moment, a device waiting for an
                        // admin's approval, each gets in once it can, without a new runner.
                        let again = register_again(result.as_ref().err(), wait);
                        let refreshed = matches!(
                            result,
                            Err(RegistrationError::NetworkError | RegistrationError::Internal(_))
                        );
                        drop(aref.tell(RegisterResult(result.map(|_| client.clone()))).await);
                        drop(aref);
                        let Some(again) = again else {
                            break;
                        };
                        followup = None;
                        tokio::time::sleep(again).await;
                        wait = (wait * 2).min(REGISTER_RETRY_MAX);
                        // On a new connection where the last may be what failed.
                        if refreshed {
                            client = redial(&params, client).await;
                        }
                    }
                }
            }
        })
        .restart_policy(RestartPolicy::Transient)
        .spawn()
        .await;

        params.env.subscribe::<DerpLatencyMeasurement>(&slf).await?;
        params.env.subscribe::<direct::NewEndpoints>(&slf).await?;

        DerpLatencyMeasurer::supervise(&slf, params.env.clone())
            .spawn()
            .await;

        params.env.register(None, &slf).await?;

        Ok(Self {
            state: RegState::NotRegistered {
                pending_auth_requests: Default::default(),
            },
            params,
            derp_latency_measurement: None,
            endpoints: Default::default(),
            self_node: None,
            pending_node_requests: Default::default(),
        })
    }
}

#[kameo::messages]
impl ControlRunner {
    /// Fetch the IPv4 address for this tailscale device.
    #[message(ctx)]
    pub fn ipv4(
        &mut self,
        ctx: &mut Context<Self, DelegatedReply<Option<Ipv4Addr>>>,
    ) -> DelegatedReply<Option<Ipv4Addr>> {
        if let Some(node) = &self.self_node {
            return ctx.reply(Some(node.tailnet_address.ipv4.addr()));
        }

        let (deleg, replier) = ctx.reply_sender();
        if let Some(replier) = replier {
            self.pending_node_requests
                .push(PendingNodeRequest::Ipv4(replier));
        }

        deleg
    }

    /// Fetch the IPv6 address for this tailscale device.
    #[message(ctx)]
    pub fn ipv6(
        &mut self,
        ctx: &mut Context<Self, DelegatedReply<Option<Ipv6Addr>>>,
    ) -> DelegatedReply<Option<Ipv6Addr>> {
        if let Some(node) = &self.self_node {
            return ctx.reply(Some(node.tailnet_address.ipv6.addr()));
        }

        let (deleg, replier) = ctx.reply_sender();
        if let Some(replier) = replier {
            self.pending_node_requests
                .push(PendingNodeRequest::Ipv6(replier));
        }

        deleg
    }

    /// Fetch the self node for this tailscale device.
    #[message(ctx)]
    pub fn self_node(
        &mut self,
        ctx: &mut Context<Self, DelegatedReply<Option<Node>>>,
    ) -> DelegatedReply<Option<Node>> {
        if let Some(node) = &self.self_node {
            return ctx.reply(Some(node.clone()));
        }

        let (deleg, replier) = ctx.reply_sender();
        if let Some(replier) = replier {
            self.pending_node_requests
                .push(PendingNodeRequest::SelfNode(replier));
        }

        deleg
    }

    // ThinkTerm (sign-in state): the next three messages are ours.
    /// The node from the latest netmap, without waiting for one to arrive.
    #[message]
    pub fn current_self_node(&mut self) -> Option<Node> {
        self.self_node.clone()
    }

    /// The current registration state: None while connecting, Some(None)
    /// once registered, or Some(Some(url)) while approval is needed.
    /// Unlike auth_url, this does not retain a pending request.
    #[message]
    pub fn current_auth_url(&mut self) -> Option<Option<url::Url>> {
        match &self.state {
            RegState::Registered(..) => Some(None),
            RegState::AuthRequired(url) => Some(Some(url.clone())),
            RegState::NotRegistered { .. } | RegState::AuthKeyRejected | RegState::Failed(..) => {
                None
            }
        }
    }

    /// Why control turned this device away, once it has.
    #[message]
    pub fn current_registration_error(&mut self) -> Option<String> {
        self.state.turned_away()
    }

    /// Wait for a report of whether interactive auth is needed, and if so, what the URL is.
    #[message(ctx)]
    pub fn auth_url(
        &mut self,
        ctx: &mut Context<Self, DelegatedReply<AuthResponse>>,
    ) -> DelegatedReply<AuthResponse> {
        match &mut self.state {
            RegState::Registered(..) => ctx.reply(AuthResponse::Authorized),
            RegState::AuthRequired(u) => {
                ctx.reply(AuthResponse::InteractiveAuthRequired(u.clone()))
            }
            RegState::NotRegistered {
                pending_auth_requests,
            } => {
                let (deleg, replier) = ctx.reply_sender();
                if let Some(replier) = replier {
                    pending_auth_requests.push(replier);
                }

                deleg
            }
            RegState::AuthKeyRejected => ctx.reply(AuthResponse::KeyRejected),
            // ThinkTerm (sign-in state): added.
            // Failed: no answer will come, and the request goes with its reply.
            RegState::Failed(..) => ctx.reply_sender().0,
        }
    }
}

impl ControlRunner {
    /// Call `f` with a map request built from the current control actor state.
    ///
    /// `stream` dictates whether the request is built for a streaming netmap response or as a
    /// request to update to this node's fields in control.
    ///
    /// This takes a closure rather than returning the built request for lifetime reasons.
    async fn with_map_request<T>(
        &self,
        stream: bool,
        f: impl AsyncFnOnce(ts_control::MapRequest) -> T,
    ) -> T {
        let mut mrb = ts_control::MapRequestBuilder::new(&self.params.env.keys);

        mrb = if stream {
            mrb.as_stream()
        } else {
            mrb.as_request()
        };

        mrb = mrb.endpoints(self.endpoints.clone());

        if let Some(hostname) = self.params.config.hostname.as_deref() {
            mrb = mrb.hostname(hostname);
        }

        if let Some(latency) = &self.derp_latency_measurement {
            if let Some(result) = latency.measurement.first() {
                mrb = mrb.preferred_derp(result.id);
            };

            let iter = latency.measurement.iter().map(|result| {
                (
                    result.latency_map_key.as_str(),
                    result.latency.as_secs_f64(),
                )
            });

            mrb = mrb.derp_latencies(iter);
        }

        let client_name = self.params.config.format_client_name();

        let mut request = mrb.build();
        let host_info = request.host_info.get_or_insert_default();
        host_info.app = &client_name;
        host_info.ipn_version = ts_control::PKG_VERSION;

        f(request).await
    }

    async fn update_map_request(&self) {
        let RegState::Registered(conn) = &self.state else {
            tracing::debug!("attempt to update map request while not registered");
            return;
        };

        let update = self.with_map_request(false, async |req| {
            send_map_request(
                req,
                &self.params.config.server_url.join("machine/map").unwrap(),
                conn,
            )
            .await
        });

        // ThinkTerm (map update): was `.await.unwrap()`, with no time limit. A failure panicked
        // the control runner, and with it every question ThinkTerm asks the node, which then
        // made the node anew and dropped every connection; one that hung kept the runner from
        // answering anything else. Since 0.7.0 the address STUN finds is one of the endpoints,
        // and changes with the network, as the connection to control goes down. The endpoints
        // and latencies go with the next map request instead.
        match tokio::time::timeout(MAP_UPDATE_TIMEOUT, update).await {
            Ok(Ok(_)) => {}
            Ok(Err(e)) => tracing::warn!(error = %e, "updating the map request"),
            Err(_) => tracing::warn!("updating the map request timed out"),
        }
    }
}

// ThinkTerm (map update): added.
/// How long an update of the map request -- this node's endpoints, its DERP latencies -- may
/// take before it is given up.
const MAP_UPDATE_TIMEOUT: Duration = Duration::from_secs(15);

impl Message<AuthRequired> for ControlRunner {
    type Reply = ();

    async fn handle(
        &mut self,
        AuthRequired(auth_url): AuthRequired,
        _ctx: &mut Context<Self, Self::Reply>,
    ) {
        let RegState::NotRegistered {
            pending_auth_requests,
        } = core::mem::replace(&mut self.state, RegState::AuthRequired(auth_url.clone()))
        else {
            tracing::warn!("got duplicate authrequired message");
            return;
        };

        for req in pending_auth_requests.into_iter() {
            req.send(AuthResponse::InteractiveAuthRequired(auth_url.clone()));
        }
    }
}

impl Message<RegisterResult> for ControlRunner {
    type Reply = ();

    #[tracing::instrument(skip_all, fields(result = ?msg.0))]
    async fn handle(&mut self, msg: RegisterResult, ctx: &mut Context<Self, Self::Reply>) {
        if matches!(self.state, RegState::Registered(..)) {
            tracing::warn!("got register result after already in registered state");
            return;
        }

        let conn = match msg.0 {
            Ok(conn) => conn,
            Err(RegistrationError::MachineNotAuthorized(None)) => {
                tracing::error!("auth key rejected");
                self.state
                    .update(RegState::AuthKeyRejected, AuthResponse::KeyRejected);

                return;
            }
            Err(e) => {
                tracing::error!(error = %e, "unable to register with control server");
                // ThinkTerm (sign-in state): was `ctx.stop()`.
                // Pending auth requests go unanswered with it.
                self.state = RegState::Failed(registration_failure(&e));
                return;
            }
        };

        self.state
            .update(RegState::Registered(conn.clone()), AuthResponse::Authorized);

        let reader = self
            .with_map_request(true, async |req| {
                let map_url = self.params.config.server_url.join("machine/map").unwrap();

                send_map_request(req, &map_url, &conn).await
            })
            .await
            .unwrap();

        let stream = map_stream(reader).map(Arc::new);

        ctx.actor_ref().attach_stream(stream.boxed(), (), ());
    }
}

// ThinkTerm (sign-in retry): added.
/// The wait before a failed registration is asked again, the first time; it doubles with each
/// failure in a row, up to `REGISTER_RETRY_MAX`.
const REGISTER_RETRY_FIRST: Duration = Duration::from_secs(2);
const REGISTER_RETRY_MAX: Duration = Duration::from_secs(30);
/// How often a device control knows and has not let in asks again: it gets in about this long
/// after an admin approves it, or its auth key is put right.
const REGISTER_APPROVAL_POLL: Duration = Duration::from_secs(10);

// ThinkTerm (sign-in retry): added.
/// When a registration that ended in `failed` (none: it got through) is asked again: never for
/// one that got through; a device control has not let in at a steady pace; any other failure
/// after `wait`.
fn register_again(failed: Option<&RegistrationError>, wait: Duration) -> Option<Duration> {
    match failed? {
        RegistrationError::MachineNotAuthorized(None) => Some(REGISTER_APPROVAL_POLL),
        _ => Some(wait),
    }
}

// ThinkTerm (sign-in retry): added.
/// A new connection to control from the dialer, for a registration asked again; `old` when
/// none can be had, the try after going on it.
async fn redial(params: &Params, old: HttpConn) -> HttpConn {
    match params
        .env
        .ask::<DialerActor, _>(
            None,
            DialNext {
                url: params.config.server_url.clone(),
            },
            true,
        )
        .await
    {
        Ok(fresh) => fresh,
        Err(e) => {
            tracing::warn!(error = %e, "no new connection to control for a registration retry");
            old
        }
    }
}

// ThinkTerm (sign-in state): added.
/// What a registration says when control knows the machine and has not let it in, with no
/// address to sign in at: it waits for an admin of the tailnet to approve it, or the auth key
/// it came with is wrong, used up or expired. Whoever shows the device tells it by this.
pub const NOT_LET_IN: &str = "the control server has not let this machine in: it waits for \
                              an admin's approval, or its auth key is wrong, used up or expired";

// ThinkTerm (sign-in state): added.
/// What a failed registration says to whoever looks at the device.
fn registration_failure(err: &RegistrationError) -> String {
    match err {
        RegistrationError::MachineNotAuthorized(None) => NOT_LET_IN.to_string(),
        RegistrationError::NetworkError => {
            "the control server could not be reached while signing in".to_string()
        }
        other => format!("signing in failed: {other}"),
    }
}

enum PendingNodeRequest {
    Ipv4(ReplySender<Option<Ipv4Addr>>),
    Ipv6(ReplySender<Option<Ipv6Addr>>),
    SelfNode(ReplySender<Option<Node>>),
}

impl Message<StreamMessage<Arc<StateUpdate>, (), ()>> for ControlRunner {
    type Reply = ();

    async fn handle(
        &mut self,
        msg: StreamMessage<Arc<StateUpdate>, (), ()>,
        ctx: &mut Context<Self, Self::Reply>,
    ) {
        let msg = match msg {
            StreamMessage::Started(_) => {
                tracing::trace!("started listening to state updates");
                return;
            }

            StreamMessage::Next(msg) => msg,

            StreamMessage::Finished(_) => {
                tracing::error!("state update stream terminated");
                ctx.stop();
                return;
            }
        };

        if let RegState::Registered(conn) = &self.state {
            let _ = handle_ping(&msg, &self.params.config.server_url, conn).await;
        }

        if let Some(dial_plan) = &msg.dial_plan
            && self
                .params
                .env
                .ask::<DialerActor, _>(
                    None,
                    UpdateDialPlan {
                        dial_plan: dial_plan.clone(),
                    },
                    true,
                )
                .await
                .unwrap()
        {
            tracing::trace!(new_dial_plan = ?dial_plan);
        }

        if let Some(node) = msg.node.as_ref() {
            self.self_node = Some(node.clone());
        }

        if let Err(e) = self.params.env.publish(msg).await {
            tracing::error!(error = %e, "publishing netmap update");
        }

        if let Some(node) = &self.self_node {
            for req in self.pending_node_requests.drain(..) {
                match req {
                    PendingNodeRequest::Ipv4(sender) => {
                        sender.send(Some(node.tailnet_address.ipv4.addr()));
                    }
                    PendingNodeRequest::Ipv6(sender) => {
                        sender.send(Some(node.tailnet_address.ipv6.addr()));
                    }
                    PendingNodeRequest::SelfNode(sender) => {
                        sender.send(Some(node.clone()));
                    }
                }
            }
        }
    }
}

impl Message<direct::NewEndpoints> for ControlRunner {
    type Reply = ();

    async fn handle(&mut self, msg: direct::NewEndpoints, _ctx: &mut Context<Self, Self::Reply>) {
        if self.endpoints == msg.0.as_ref() {
            return;
        }

        self.endpoints = msg.0.to_vec();
        self.update_map_request().await;
    }
}

impl Message<DerpLatencyMeasurement> for ControlRunner {
    type Reply = ();

    async fn handle(&mut self, msg: DerpLatencyMeasurement, _ctx: &mut Context<Self, Self::Reply>) {
        if self.derp_latency_measurement.as_ref() == Some(&msg) {
            return;
        }

        self.derp_latency_measurement = Some(msg);
        self.update_map_request().await;
    }
}

/// Control server dialer.
pub struct DialerActor {
    dialer: ControlDialer,
    env: Env,
}

impl kameo::Actor for DialerActor {
    type Args = Env;
    type Error = crate::Error;

    async fn on_start(env: Env, slf: ActorRef<Self>) -> Result<Self, Self::Error> {
        env.register(None, &slf).await?;

        Ok(Self {
            dialer: Default::default(),
            env,
        })
    }
}

#[kameo::messages]
impl DialerActor {
    #[message]
    async fn dial_next(&mut self, url: url::Url) -> Result<HttpConn, ts_control::Error> {
        self.dialer
            .full_connect_next(&url, &self.env.keys.machine_keys)
            .await
    }

    #[message]
    fn update_dial_plan(&mut self, dial_plan: DialPlan) -> bool {
        self.dialer.update_dial_plan(&dial_plan)
    }
}

// ThinkTerm (sign-in state): the tests are ours.
#[cfg(test)]
mod tests {
    use ts_control::RegistrationError;

    use super::{
        NOT_LET_IN, REGISTER_APPROVAL_POLL, REGISTER_RETRY_FIRST, RegState, register_again,
        registration_failure,
    };

    #[test]
    fn a_failed_registration_is_asked_again() {
        // Through: not again.
        assert_eq!(register_again(None, REGISTER_RETRY_FIRST), None);
        // Waiting for an admin: at a steady pace, so it gets in soon after.
        let refused = RegistrationError::MachineNotAuthorized(None);
        assert_eq!(
            register_again(Some(&refused), core::time::Duration::from_secs(30)),
            Some(REGISTER_APPROVAL_POLL)
        );
        // Anything else: after the wait the caller doubles.
        let unreachable = RegistrationError::NetworkError;
        assert_eq!(
            register_again(Some(&unreachable), core::time::Duration::from_secs(8)),
            Some(core::time::Duration::from_secs(8))
        );
    }

    #[test]
    fn a_sign_in_turned_away_says_why() {
        let refused = registration_failure(&RegistrationError::MachineNotAuthorized(None));
        assert!(refused.contains("auth key"), "{refused}");
        let unreachable = registration_failure(&RegistrationError::NetworkError);
        assert!(unreachable.contains("could not be reached"), "{unreachable}");
    }

    #[test]
    fn a_machine_not_let_in_says_so() {
        // What upstream calls a rejected auth key is how control leaves a machine waiting for
        // an admin's approval too: whoever shows the device tells that wait by NOT_LET_IN.
        assert_eq!(
            RegState::AuthKeyRejected.turned_away().as_deref(),
            Some(NOT_LET_IN)
        );
        let failed = RegState::Failed("signing in failed: no".to_string());
        assert_eq!(
            failed.turned_away().as_deref(),
            Some("signing in failed: no")
        );
        let signing_in = RegState::NotRegistered {
            pending_auth_requests: vec![],
        };
        assert_eq!(signing_in.turned_away(), None);
    }
}
