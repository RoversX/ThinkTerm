//! What the session needs from whoever hosts it. The desktop client wires
//! these to the mux; a browser client to its page.
use crate::clock::Clock;
use codec::Pdu;
use std::future::Future;
use std::pin::Pin;
use wezterm_term::Alert;

/// A future the session hands the host to run to completion. Not `Send`:
/// the desktop's executor spawns local futures and a browser has one
/// thread.
pub type DetachedFuture = Pin<Box<dyn Future<Output = ()> + 'static>>;

/// Keys the image store: the connection an image came from. The desktop
/// uses its mux domain id; the generation an image carries is a counter
/// private to one server-side object, so pictures from two servers, or
/// one server before and after a restart, must not share a copy.
pub type ImageDomainKey = usize;

/// The request could not be answered.
#[derive(Debug, Clone)]
pub struct LinkError {
    pub message: String,
    /// The link is between connections and the request may be answered
    /// after it comes back; false when nothing will ever answer.
    pub retryable: bool,
}

impl std::fmt::Display for LinkError {
    fn fmt(&self, fmt: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        fmt.write_str(&self.message)
    }
}

impl std::error::Error for LinkError {}

/// Request and answer over whatever the transport is. Message-level, not
/// a byte stream: a browser has framed messages. The future is an
/// associated type so a desktop link can hand out `Send` futures and a
/// browser link local ones, with no `Send` bound written here.
pub trait PduLink {
    type Request: Future<Output = Result<Pdu, LinkError>> + 'static;

    /// Put `pdu` on the wire now, behind everything sent before it; the
    /// future is its answer.
    fn request(&self, pdu: Pdu) -> Self::Request;

    /// A timeout on a reconnectable link is not a dead pane.
    fn is_reconnectable(&self) -> bool;

    /// Bumped whenever the transport is replaced.
    fn connection_generation(&self) -> u64;
}

/// Send `pdu` and take the answer of the expected type. An
/// `ErrorResponse` becomes the error it carries.
pub async fn request<L: PduLink, T>(
    link: &L,
    pdu: Pdu,
    extract: impl FnOnce(Pdu) -> Result<T, Pdu>,
) -> anyhow::Result<T> {
    match link.request(pdu).await {
        Ok(Pdu::ErrorResponse(err)) => anyhow::bail!(err.reason),
        Ok(pdu) => extract(pdu).map_err(|other| anyhow::anyhow!("unexpected response {other:?}")),
        Err(err) => Err(err.into()),
    }
}

/// Runs the session's own detached tasks (line fetches, polls, the
/// render-delta drain).
pub trait Spawner {
    fn spawn_detached(&self, fut: DetachedFuture);
}

/// Everything a pane session takes from its host, in one place.
pub trait SessionHost: 'static {
    type Clock: Clock;
    type Spawner: Spawner;
    type Events: SessionEvents;
    type Link: crate::input::PaneLink;
    type Config: HostConfig;

    fn clock(&self) -> &Self::Clock;
    fn spawner(&self) -> &Self::Spawner;
    fn events(&self) -> &Self::Events;
    fn link(&self) -> &Self::Link;
    fn config(&self) -> &Self::Config;
    fn image_domain(&self) -> ImageDomainKey;
}

/// The host's identity for a pane: what its notifications name. The
/// desktop uses its local mux pane id; a browser client can use anything.
pub type HostPaneId = usize;

/// Settings the host may change while a session runs, read where they are
/// used rather than snapshotted, so a reload takes effect.
pub trait HostConfig {
    /// The rules that turn text into hyperlinks as rows arrive. Read per
    /// push and per fetch, so a host that reloads its settings hands out
    /// the new rules; shared, so that costs an `Arc` clone.
    fn hyperlink_rules(&self) -> std::sync::Arc<Vec<termwiz::hyperlink::Rule>>;
    /// How many line prefetch batches a second the pane may ask for.
    fn fetch_rate_per_second(&self) -> u32;
    /// How far past a painted range rows are fetched ahead, on each side,
    /// in viewports. Zero fetches only what is painted.
    fn scrollback_lookahead_screens(&self) -> usize;
    /// Whether a pane's whole scrollback is fetched once it is first shown,
    /// so scrolling never paints a row that is still on its way.
    fn warm_scrollback(&self) -> bool;
}

/// Everything the session tells the host about a pane. Synchronous, and
/// never re-entering the session: several of these are called with the
/// pane's lock held, exactly as the desktop's `Mux::notify` was.
pub trait SessionEvents {
    /// The pane's content changed; a paint is due.
    fn pane_output(&self, pane: HostPaneId);
    /// The remote program raised an alert (bell, progress, user var, ...).
    fn alert(&self, pane: HostPaneId, alert: Alert);
    /// The agent status the server tracks for the pane changed.
    fn agent_status_changed(&self, pane: HostPaneId);
    /// The server removed the pane; its mirror is dead.
    fn pane_removed(&self, pane: HostPaneId);
    /// The server moved focus to the pane.
    fn pane_focused(&self, pane: HostPaneId);
    /// The user gave the pane input; the host's own bookkeeping of "who
    /// is typing" goes here.
    fn input_recorded(&self);
}
