//! Reusable "content view" framework: a GPU-drawn panel that occupies the
//! terminal content area. Some views behave like synthetic tabs (switch /
//! close / preserve state), while thread-owned views can stay driven by the
//! workspace sidebar instead.

use crate::quad::TripleLayerQuadAllocator;
use crate::termwindow::TermWindow;
use crate::ui::{DrawContext, UiPalette};
use std::path::PathBuf;
use std::rc::Rc;
use std::time::Instant;
use wezterm_font::LoadedFont;
use wezterm_term::{KeyCode, KeyModifiers};
use window::{MouseEventKind as WMEK, RectF};

pub(crate) type ContentViewId = u64;

/// Progress of an SSH connection that a content view kicked off, pushed by the
/// owning `TermWindow` while it polls the remote domain. Only the remote-thread
/// view consumes it; `Authenticating`/`Connected` are not pushed because at that
/// point the view is closed and the real terminal is revealed instead.
#[derive(Clone, Debug)]
pub(crate) enum RemoteConnectPhase {
    Connecting,
    Failed { message: String },
}

/// What `TermWindow` should do after a content view handled an input event.
pub(crate) enum ContentViewResponse {
    /// Event not consumed by the view.
    Ignored,
    /// Repaint the window.
    Redraw,
    /// Close the content view (remove its synthetic tab).
    Close,
    /// Run a callback back on the owning `TermWindow` (e.g. connect SSH).
    Run(Box<dyn FnOnce(&mut TermWindow)>),
}

pub(crate) trait ContentView {
    /// Title shown on the synthetic tab.
    fn title(&self) -> String;

    /// Whether this view should appear in the window's top tab bar.
    fn show_in_tab_bar(&self) -> bool {
        true
    }

    /// Stable key used to activate an existing tab instead of opening a duplicate.
    fn tab_key(&self) -> Option<String> {
        None
    }

    /// Space id that owns this synthetic tab. `None` means the view is global
    /// and remains visible while switching Spaces.
    fn space_id(&self) -> Option<&str> {
        None
    }

    /// Called when a deduplicated or background synthetic tab is explicitly
    /// brought back to the foreground by user action.
    fn on_reactivated(&mut self) -> ContentViewResponse {
        ContentViewResponse::Ignored
    }

    /// Push SSH connection progress into a view that initiated a connection.
    /// Default is a no-op; only the remote-thread view reacts.
    fn on_remote_connect_phase(&mut self, _phase: RemoteConnectPhase) {}

    /// Whether the view currently wants the cursor-blink animation running
    /// (true only while a text field is focused, to avoid needless repaints).
    fn wants_cursor_blink(&self) -> bool {
        false
    }

    /// Schedule a follow-up repaint for view-local animations.
    fn next_frame_time(&self) -> Option<Instant> {
        None
    }

    /// Paint the view into `area` (the terminal content rect). `cursor_on` is
    /// the current blink phase for any focused caret.
    fn paint(
        &mut self,
        ctx: &DrawContext,
        layers: &mut TripleLayerQuadAllocator<'_>,
        area: RectF,
        palette: UiPalette,
        font: &Rc<LoadedFont>,
        title_font: &Rc<LoadedFont>,
        section_font: &Rc<LoadedFont>,
        cursor_on: bool,
    ) -> anyhow::Result<()>;

    fn on_mouse(&mut self, x: f32, y: f32, kind: WMEK) -> ContentViewResponse;

    fn on_key(&mut self, key: KeyCode, mods: KeyModifiers) -> ContentViewResponse;

    /// Insert pasted text into the focused field, if any.
    fn on_paste(&mut self, _text: &str) -> ContentViewResponse {
        ContentViewResponse::Redraw
    }

    /// Handle the synthetic tab close button / Escape-style close request.
    fn on_close_requested(&mut self) -> ContentViewResponse {
        ContentViewResponse::Close
    }

    /// Handle a folder selected by a native folder picker that was initiated by
    /// this view.
    fn on_folder_picked(&mut self, _path: PathBuf) -> ContentViewResponse {
        ContentViewResponse::Ignored
    }

    /// Text to copy to the clipboard for ⌘C (e.g. the focused field), if any.
    fn copy_text(&self) -> Option<String> {
        None
    }

    /// Text to cut to the clipboard for ⌘X. Views that only support whole-field
    /// selection should return text only when that selection is active.
    fn cut_text(&mut self) -> Option<String> {
        None
    }
}
