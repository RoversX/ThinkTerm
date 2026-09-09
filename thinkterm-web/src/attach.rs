//! The handshake, in the desktop's order, then the lease steps a follower
//! takes: version, identity, the pane list, a viewport report, and the
//! render subscription.

use crate::link::WsLink;
use anyhow::{anyhow, bail, Result};
use codec::Pdu;
use crate::chrome::active_pane;
use thinkterm_proto::layout::PaneNode;
use thinkterm_proto::{ClientId, PaneId, RenderableDimensions, TabId};
use thinkterm_session::host::request;
use wezterm_term::TerminalSize;

pub struct Attached {
    pub pane_id: PaneId,
    pub tab_id: TabId,
    pub window_id: thinkterm_proto::WindowId,
    pub workspace: String,
    pub title: String,
    pub dims: RenderableDimensions,
    pub alt_screen: bool,
    pub server_version: String,
}

/// `size` is the grid this page can show, or `None` when it cannot show
/// one yet; only a real size is reported, so a later claim never carries
/// a made-up one.
pub async fn attach(link: &WsLink, size: Option<TerminalSize>) -> Result<Attached> {
    let version = request(
        link,
        Pdu::GetCodecVersion(codec::GetCodecVersion {}),
        |pdu| match pdu {
            Pdu::GetCodecVersionResponse(v) => Ok(v),
            other => Err(other),
        },
    )
    .await?;
    if version.codec_vers != codec::CODEC_VERSION {
        bail!(
            "this page speaks protocol {} but the server speaks {}; update the server or the bundle",
            codec::CODEC_VERSION,
            version.codec_vers
        );
    }

    let me = ClientId {
        hostname: "web".into(),
        username: "web".into(),
        pid: 0,
        epoch: js_sys::Date::now() as u64,
        id: (js_sys::Math::random() * u32::MAX as f64) as usize,
        ssh_auth_sock: None,
    };
    link.lease_mut().me = Some(me.clone());
    request(
        link,
        Pdu::SetClientId(codec::SetClientId {
            client_id: me,
            is_proxy: false,
        }),
        |pdu| match pdu {
            Pdu::UnitResponse(_) => Ok(()),
            other => Err(other),
        },
    )
    .await?;

    let panes = request(link, Pdu::ListPanes(codec::ListPanes {}), |pdu| match pdu {
        Pdu::ListPanesResponse(p) => Ok(p),
        other => Err(other),
    })
    .await?;
    // The first tab that has a pane, preferring its active one.
    let entry = panes
        .tabs
        .iter()
        .find_map(active_pane)
        .ok_or_else(|| anyhow!("the server has no panes to show"))?;

    link.lease_mut().tab_id = Some(entry.tab_id);
    if let Some(size) = size {
        let state = request(
            link,
            Pdu::SetClientViewport(codec::SetClientViewport {
                tab_id: entry.tab_id,
                viewport: codec::ClientViewport::CellGrid { size },
            }),
            |pdu| match pdu {
                Pdu::ClientViewportState(s) => Ok(s),
                other => Err(other),
            },
        )
        .await?;
        let mut lease = link.lease_mut();
        lease.reported = Some(size);
        lease.apply_viewport(&state);
    }

    request(
        link,
        Pdu::GetPaneRenderChanges(codec::GetPaneRenderChanges {
            pane_id: entry.pane_id,
        }),
        |pdu| match pdu {
            Pdu::LivenessResponse(_) | Pdu::UnitResponse(_) => Ok(()),
            other => Err(other),
        },
    )
    .await?;

    let rows = entry.size.rows;
    Ok(Attached {
        pane_id: entry.pane_id,
        tab_id: entry.tab_id,
        window_id: entry.window_id,
        workspace: entry.workspace.clone(),
        title: entry.title.clone(),
        dims: RenderableDimensions {
            cols: entry.size.cols,
            viewport_rows: rows,
            scrollback_rows: rows,
            physical_top: entry.physical_top,
            scrollback_top: entry.physical_top,
            dpi: entry.size.dpi,
            pixel_width: entry.size.pixel_width,
            pixel_height: entry.size.pixel_height,
            reverse_video: false,
        },
        alt_screen: entry.alt_screen,
        server_version: version.version_string,
    })
}

/// The pane this page was showing is not on the server any more.
///
/// Its own type so the reconnect loop can tell "come back later" from
/// "there is nothing to come back to" and stop retrying.
#[derive(Debug)]
pub struct PaneGone(pub PaneId);

impl std::fmt::Display for PaneGone {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "pane {} is no longer on the server", self.0)
    }
}

impl std::error::Error for PaneGone {}

/// The handshake again, on a socket that has just been reopened.
///
/// Not `attach`: that picks a pane, and picking again after a reconnect
/// would silently move the page to whatever is active now. This one insists
/// on the pane it was already showing and fails with [`PaneGone`] if the
/// server no longer has it.
///
/// The identity is the one from the first connection. The server tracks the
/// frontend lease by `ClientId`, so coming back under a new one would hand
/// this page a different seat than the one it left.
pub async fn reattach(
    link: &WsLink,
    pane_id: PaneId,
    tab_id: TabId,
    size: Option<TerminalSize>,
) -> Result<()> {
    let version = request(
        link,
        Pdu::GetCodecVersion(codec::GetCodecVersion {}),
        |pdu| match pdu {
            Pdu::GetCodecVersionResponse(v) => Ok(v),
            other => Err(other),
        },
    )
    .await?;
    if version.codec_vers != codec::CODEC_VERSION {
        bail!(
            "this page speaks protocol {} but the server speaks {}; update the server or the bundle",
            codec::CODEC_VERSION,
            version.codec_vers
        );
    }

    let me = link
        .lease()
        .me
        .clone()
        .ok_or_else(|| anyhow!("reconnecting without an identity"))?;
    request(
        link,
        Pdu::SetClientId(codec::SetClientId {
            client_id: me,
            is_proxy: false,
        }),
        |pdu| match pdu {
            Pdu::UnitResponse(_) => Ok(()),
            other => Err(other),
        },
    )
    .await?;

    let panes = request(link, Pdu::ListPanes(codec::ListPanes {}), |pdu| match pdu {
        Pdu::ListPanesResponse(p) => Ok(p),
        other => Err(other),
    })
    .await?;
    if !panes.tabs.iter().any(|tab| contains_pane(tab, pane_id)) {
        return Err(PaneGone(pane_id).into());
    }

    if let Some(size) = size {
        let state = request(
            link,
            Pdu::SetClientViewport(codec::SetClientViewport {
                tab_id,
                viewport: codec::ClientViewport::CellGrid { size },
            }),
            |pdu| match pdu {
                Pdu::ClientViewportState(s) => Ok(s),
                other => Err(other),
            },
        )
        .await?;
        let mut lease = link.lease_mut();
        lease.tab_id = Some(tab_id);
        lease.reported = Some(size);
        lease.apply_viewport(&state);
    }

    // Re-subscribes: the server forgot this page along with the socket.
    request(
        link,
        Pdu::GetPaneRenderChanges(codec::GetPaneRenderChanges { pane_id }),
        |pdu| match pdu {
            Pdu::LivenessResponse(_) | Pdu::UnitResponse(_) => Ok(()),
            other => Err(other),
        },
    )
    .await?;
    Ok(())
}

/// Whether a tab's layout still holds this pane.
fn contains_pane(node: &PaneNode, pane_id: PaneId) -> bool {
    match node {
        PaneNode::Empty => false,
        PaneNode::Leaf(entry) => entry.pane_id == pane_id,
        PaneNode::Stack(stack) => stack.panes.iter().any(|e| e.pane_id == pane_id),
        PaneNode::Split { left, right, .. } => {
            contains_pane(left, pane_id) || contains_pane(right, pane_id)
        }
    }
}
