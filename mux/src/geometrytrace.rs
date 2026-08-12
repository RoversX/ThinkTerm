//! Opt-in, one-record-per-transition tracing for the pane zoom / frontend
//! viewport geometry path.
//!
//! Zoom is the only geometry transition that is not atomic across the mux
//! boundary: the GUI sends `SetPaneZoomed` and `SetClientViewport` from two
//! independent detached tasks, and the server keeps its own split tree whose
//! root is back-derived from pane dimensions.  Reasoning about that from unit
//! tests alone has repeatedly proved insufficient, so this module exists to
//! record the *real* order and the *real* numbers on both processes.
//!
//! Enable it on the GUI and on the mux server with:
//!
//! ```text
//! WEZTERM_LOG=info,zoomtrace=debug
//! ```
//!
//! Everything here is inert unless that filter is active: each record is
//! wrapped in [`trace_enabled`] so a disabled build performs no pane-tree
//! walks, takes no extra locks, and formats no strings.

use crate::pane::{Pane, PaneId};
use crate::tab::{PositionedPane, PositionedSplit, SplitDirection};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use wezterm_term::TerminalSize;

/// Log target for every record emitted by [`zoom_trace!`].
pub const TARGET: &str = "zoomtrace";

/// True when the `zoomtrace` target is enabled at debug level.
///
/// Callers must check this before gathering trace data; the `log::debug!`
/// macro only consults the global max level, not the per-target filter, so
/// an unguarded record would still walk the pane tree on any debug-enabled
/// run.
pub fn trace_enabled() -> bool {
    log::log_enabled!(target: TARGET, log::Level::Debug)
}

static SEQ: AtomicU64 = AtomicU64::new(1);

/// Process-local monotonic record number.
///
/// Log timestamps are millisecond resolution and several of these records are
/// emitted back-to-back, so the sequence number is what makes the ordering
/// within one process unambiguous.
pub fn next_seq() -> u64 {
    SEQ.fetch_add(1, Ordering::Relaxed)
}

/// Emit one `zoomtrace` record, doing no work when the target is disabled.
#[macro_export]
macro_rules! zoom_trace {
    ($($arg:tt)*) => {
        if $crate::geometrytrace::trace_enabled() {
            ::log::debug!(
                target: $crate::geometrytrace::TARGET,
                "#{} {}",
                $crate::geometrytrace::next_seq(),
                format_args!($($arg)*)
            );
        }
    };
}

/// `COLSxROWS/PXxPY`
pub fn size(size: &TerminalSize) -> String {
    format!(
        "{}x{}/{}x{}",
        size.cols, size.rows, size.pixel_width, size.pixel_height
    )
}

/// The pane's own view of its grid, in the same shape as [`size`].
pub fn dims(pane: &Arc<dyn Pane>) -> String {
    let dims = pane.get_dimensions();
    format!(
        "{}x{}/{}x{}",
        dims.cols, dims.viewport_rows, dims.pixel_width, dims.pixel_height
    )
}

/// `mirror=.. root=.. zoom=.. splits=[..] panes=[id r=<rect> d=<grid> ..]`
///
/// `r` is the rectangle the split tree assigns to the pane and `d` is the
/// dimensions the pane itself reports.  On the mux server those two must
/// agree once a transition has settled; on a GUI mirror (`mirror=true`) `d`
/// sits below `r` by the per-pane nav bar, so compare the pixel heights
/// rather than expecting equality.
///
/// `panes` deliberately ignores zoom so the preserved split rectangles stay
/// visible across the transition; `splits` does not, so a zoomed tab always
/// reports `splits=[]`.
pub fn geometry(
    mirror: bool,
    root: &TerminalSize,
    zoomed: Option<PaneId>,
    splits: &[PositionedSplit],
    panes: &[PositionedPane],
) -> String {
    let splits = splits
        .iter()
        .map(|split| {
            format!(
                "{}:{}@{},{}+{}",
                split.index,
                match split.direction {
                    SplitDirection::Horizontal => "H",
                    SplitDirection::Vertical => "V",
                },
                split.left,
                split.top,
                split.size
            )
        })
        .collect::<Vec<_>>()
        .join(" ");
    let panes = panes
        .iter()
        .map(|pos| {
            format!(
                "{} r={}x{}/{}x{}@{},{} d={}",
                pos.pane.pane_id(),
                pos.width,
                pos.height,
                pos.pixel_width,
                pos.pixel_height,
                pos.left,
                pos.top,
                dims(&pos.pane),
            )
        })
        .collect::<Vec<_>>()
        .join("; ");
    let zoom = match zoomed {
        Some(pane_id) => pane_id.to_string(),
        None => "-".to_string(),
    };
    format!(
        "mirror={} root={} zoom={} splits=[{}] panes=[{}]",
        mirror,
        size(root),
        zoom,
        splits,
        panes
    )
}
