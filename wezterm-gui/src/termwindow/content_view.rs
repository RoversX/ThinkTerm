//! Reusable "content view" framework: a GPU-drawn panel that occupies the
//! terminal content area and behaves like a synthetic tab (switch / close /
//! preserve state). The SSH hosts manager is the first implementer; other
//! native panels can plug in by implementing [`ContentView`] and calling
//! `TermWindow::open_content_view`.

use crate::quad::TripleLayerQuadAllocator;
use crate::termwindow::TermWindow;
use crate::ui::{DrawContext, UiPalette};
use std::path::PathBuf;
use std::rc::Rc;
use std::time::Instant;
use wezterm_font::LoadedFont;
use wezterm_term::{KeyCode, KeyModifiers};
use window::{MouseEventKind as WMEK, RectF};

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
