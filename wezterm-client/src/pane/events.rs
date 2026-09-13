//! The desktop's side of the session crate's host traits: the mux for
//! notifications, the system clocks, the live configuration, the promise
//! executor for tasks, and the `ClientInner` connection for the wire.
use crate::domain::ClientInner;
use config::configuration;
use mux::domain::DomainId;
use mux::{Mux, MuxNotification};
use promise::BrokenPromise;
use std::convert::TryInto;
use std::sync::{Arc, Mutex};
use std::time::{Instant, SystemTime};
use thinkterm_proto::TabId;
use thinkterm_session::clock::{Clock, Timestamp};
use thinkterm_session::host::{
    DetachedFuture, HostConfig, HostPaneId, ImageDomainKey, LinkError, PduLink, SessionEvents,
    SessionHost, Spawner,
};
use thinkterm_session::images::ImageStore;
use thinkterm_session::input::{PaneLink, SendFuture};
use thinkterm_session::Lock;
use wezterm_term::Alert;

/// The desktop's clocks. `now` is micros since this process first asked;
/// `wall_millis` is what `InputSerial::now()` always was.
pub(crate) struct SystemClock;

impl Clock for SystemClock {
    fn now(&self) -> Timestamp {
        static ORIGIN: std::sync::OnceLock<Instant> = std::sync::OnceLock::new();
        Timestamp::from_micros(ORIGIN.get_or_init(Instant::now).elapsed().as_micros() as u64)
    }

    fn wall_millis(&self) -> u64 {
        SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .expect("SystemTime before unix epoch?")
            .as_millis()
            .try_into()
            .expect("millisecond count to fit in u64")
    }
}

/// The desktop's live settings, read from the current configuration on
/// every call, as the config-reload-aware code they replace did. The
/// hyperlink rules are copied once per configuration generation, not per
/// push: the session asks for them on every delta.
pub(crate) struct DesktopConfig {
    rules: Mutex<Option<(usize, Arc<Vec<termwiz::hyperlink::Rule>>)>>,
    /// The session server of this machine: a screenful of rows costs a
    /// millisecond over its socket, so the pane fetches well ahead and
    /// keeps its whole scrollback at hand.
    local_session_host: bool,
}

impl DesktopConfig {
    pub(crate) fn new(local_session_host: bool) -> Self {
        Self {
            rules: Mutex::new(None),
            local_session_host,
        }
    }
}

impl HostConfig for DesktopConfig {
    fn hyperlink_rules(&self) -> Arc<Vec<termwiz::hyperlink::Rule>> {
        let config = configuration();
        let generation = config.generation();
        let mut cached = self.rules.lock().unwrap_or_else(|p| p.into_inner());
        match &*cached {
            Some((held, rules)) if *held == generation => Arc::clone(rules),
            _ => {
                let rules = Arc::new(config.hyperlink_rules.clone());
                *cached = Some((generation, Arc::clone(&rules)));
                rules
            }
        }
    }

    fn fetch_rate_per_second(&self) -> u32 {
        configuration().ratelimit_mux_line_prefetches_per_second
    }

    fn scrollback_lookahead_screens(&self) -> usize {
        if self.local_session_host {
            2
        } else {
            1
        }
    }

    fn warm_scrollback(&self) -> bool {
        self.local_session_host
    }
}

/// The session's own tasks run on the promise executor, as they did
/// before the session was a crate: non-`Send`, on this thread.
pub(crate) struct PromiseSpawner;

impl Spawner for PromiseSpawner {
    fn spawn_detached(&self, fut: DetachedFuture) {
        promise::spawn::spawn(fut).detach();
    }
}

/// The connection behind a `ClientInner`, as the session's wire. Its
/// futures are `Send`, so the input drain can run on the main thread
/// executor, which requires that.
pub(crate) struct DesktopLink(Arc<ClientInner>);

impl DesktopLink {
    pub(crate) fn new(client: Arc<ClientInner>) -> Self {
        Self(client)
    }
}

impl PduLink for DesktopLink {
    type Request = SendFuture<Result<codec::Pdu, LinkError>>;

    fn request(&self, pdu: codec::Pdu) -> Self::Request {
        // The same metrics the rpc! wrappers record, keyed the same way
        // (the wrapper names are the snake_case of the PDU names).
        let method: &'static str = match &pdu {
            codec::Pdu::GetLines(_) => "get_lines",
            codec::Pdu::GetPaneRenderChanges(_) => "get_pane_render_changes",
            codec::Pdu::GetImageCell(_) => "get_image_cell",
            other => other.pdu_name(),
        };
        let start = Instant::now();
        let answer = self.0.client.send_pdu_pipelined(pdu);
        Box::pin(async move {
            let answer = answer.await;
            metrics::histogram!("rpc", "method" => method).record(start.elapsed());
            metrics::counter!("rpc.count", "method" => method).increment(1);
            answer.map_err(|err| LinkError {
                // A broken promise is the transport between connections;
                // the request may be answered once it is back.
                retryable: err.downcast_ref::<BrokenPromise>().is_some(),
                message: format!("{err:#}"),
            })
        })
    }

    fn is_reconnectable(&self) -> bool {
        self.0.client.is_reconnectable
    }

    fn connection_generation(&self) -> u64 {
        self.0.client.connection_generation()
    }
}

impl PaneLink for DesktopLink {
    type Prepare = SendFuture<anyhow::Result<bool>>;

    fn prepare(&self, remote_tab_id: TabId) -> Self::Prepare {
        let client = Arc::clone(&self.0);
        Box::pin(async move { client.prepare_remote_tab_input(remote_tab_id).await })
    }
}

pub(crate) struct MuxEvents {
    client: Arc<ClientInner>,
}

impl MuxEvents {
    pub(crate) fn new(client: Arc<ClientInner>) -> Self {
        Self { client }
    }
}

impl SessionEvents for MuxEvents {
    fn pane_output(&self, pane: HostPaneId) {
        Mux::get().notify(MuxNotification::PaneOutput(pane));
    }

    fn alert(&self, pane: HostPaneId, alert: Alert) {
        Mux::get().notify(MuxNotification::Alert {
            pane_id: pane,
            alert,
        });
    }

    fn agent_status_changed(&self, pane: HostPaneId) {
        Mux::get().notify(MuxNotification::AgentStatusChanged(pane));
    }

    fn pane_removed(&self, _pane: HostPaneId) {
        let mux = Mux::get();
        mux.prune_dead_windows();
        self.client.expire_stale_mappings();
    }

    fn pane_focused(&self, pane: HostPaneId) {
        let mux = Mux::get();
        if let Err(err) = mux.focus_pane_and_containing_tab(pane) {
            log::error!("Error reconciling remote PaneFocused notification: {err:#}");
        } else if let Some((_domain, window_id, _tab)) = mux.resolve_pane_id(pane) {
            // The reconcile flips tab/stack selection silently (to
            // avoid focus advisory loops); nudge the GUI to repaint.
            mux.notify(MuxNotification::WindowInvalidated(window_id));
        }
    }

    fn input_recorded(&self) {
        Mux::get().record_input_for_current_identity();
    }
}

/// Everything a desktop pane session takes from this process.
pub(crate) struct DesktopHost {
    link: DesktopLink,
    events: MuxEvents,
    clock: SystemClock,
    config: DesktopConfig,
    spawner: PromiseSpawner,
    image_domain: DomainId,
}

impl DesktopHost {
    pub(crate) fn new(client: Arc<ClientInner>) -> Self {
        let config = DesktopConfig::new(client.client.is_local_session_host());
        Self {
            image_domain: client.local_domain_id,
            link: DesktopLink::new(Arc::clone(&client)),
            events: MuxEvents::new(client),
            clock: SystemClock,
            config,
            spawner: PromiseSpawner,
        }
    }
}

impl SessionHost for DesktopHost {
    type Clock = SystemClock;
    type Spawner = PromiseSpawner;
    type Events = MuxEvents;
    type Link = DesktopLink;
    type Config = DesktopConfig;

    fn clock(&self) -> &SystemClock {
        &self.clock
    }

    fn spawner(&self) -> &PromiseSpawner {
        &self.spawner
    }

    fn events(&self) -> &MuxEvents {
        &self.events
    }

    fn link(&self) -> &DesktopLink {
        &self.link
    }

    fn config(&self) -> &DesktopConfig {
        &self.config
    }

    fn image_domain(&self) -> ImageDomainKey {
        self.image_domain
    }
}

/// Images fetched from remote panes, by the domain they came from and
/// their hash, shared by every pane session in this process.
fn image_store() -> &'static Arc<Lock<ImageStore>> {
    static STORE: std::sync::OnceLock<Arc<Lock<ImageStore>>> = std::sync::OnceLock::new();
    STORE.get_or_init(|| Arc::new(Lock::new(ImageStore::default())))
}

pub(crate) fn shared_image_store() -> Arc<Lock<ImageStore>> {
    Arc::clone(image_store())
}

/// What the store of remote images holds right now: a count and bytes.
pub fn remote_image_footprint() -> (usize, usize) {
    image_store().lock().footprint()
}

/// Drop every image held for `domain_id`: on (re)attach the server may be
/// a different process, whose generations start over.
pub(crate) fn forget_images_for_domain(domain_id: DomainId) {
    image_store().lock().forget_domain(domain_id);
}
