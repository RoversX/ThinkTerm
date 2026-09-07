//! The handshake, in the desktop's order, then the lease steps a follower
//! takes: version, identity, the pane list, a viewport report, and the
//! render subscription.

use crate::link::WsLink;
use anyhow::{anyhow, bail, Result};
use codec::Pdu;
use thinkterm_proto::layout::{PaneEntry, PaneNode};
use thinkterm_proto::{ClientId, PaneId, RenderableDimensions, TabId};
use thinkterm_session::host::request;
use wezterm_term::TerminalSize;

pub struct Attached {
    pub pane_id: PaneId,
    pub tab_id: TabId,
    pub title: String,
    pub dims: RenderableDimensions,
    pub alt_screen: bool,
    pub server_version: String,
}

fn active_pane(node: &PaneNode) -> Option<PaneEntry> {
    match node {
        PaneNode::Empty => None,
        PaneNode::Leaf(e) => Some(e.clone()),
        PaneNode::Stack(s) => s.panes.get(s.active).or(s.panes.first()).cloned(),
        PaneNode::Split { left, right, .. } => {
            let (l, r) = (active_pane(left), active_pane(right));
            match (&l, &r) {
                (Some(a), _) if a.is_active_pane => l,
                (_, Some(b)) if b.is_active_pane => r,
                _ => l.or(r),
            }
        }
    }
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
