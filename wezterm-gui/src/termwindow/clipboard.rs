use crate::termwindow::TermWindowNotif;
use crate::TermWindow;
use config::keyassignment::{ClipboardCopyDestination, ClipboardPasteSource};
use mux::pane::{Pane, PaneId};
use mux::Mux;
use std::sync::Arc;
use window::{Clipboard, ClipboardContents, WindowOps};

impl TermWindow {
    pub fn copy_to_clipboard(&self, clipboard: ClipboardCopyDestination, text: String) {
        let clipboard = match clipboard {
            ClipboardCopyDestination::Clipboard => [Some(Clipboard::Clipboard), None],
            ClipboardCopyDestination::PrimarySelection => [Some(Clipboard::PrimarySelection), None],
            ClipboardCopyDestination::ClipboardAndPrimarySelection => [
                Some(Clipboard::Clipboard),
                Some(Clipboard::PrimarySelection),
            ],
        };
        for &c in &clipboard {
            if let Some(c) = c {
                self.window.as_ref().unwrap().set_clipboard(c, text.clone());
            }
        }
    }

    pub fn paste_from_clipboard(&mut self, pane: &Arc<dyn Pane>, clipboard: ClipboardPasteSource) {
        let pane_id = pane.pane_id();
        log::trace!(
            "paste_from_clipboard in pane {} {:?}",
            pane.pane_id(),
            clipboard
        );
        let window = self.window.as_ref().unwrap().clone();
        match clipboard {
            ClipboardPasteSource::Clipboard => {
                // The regular clipboard is read typed: files and images can
                // do better than a textual paste when the pane is a remote
                // session (they upload). Text behaves exactly as before.
                let paste_target = self.capture_terminal_paste_target(pane);
                let future = window.get_clipboard_contents(Clipboard::Clipboard);
                promise::spawn::spawn(async move {
                    if let Ok(contents) = future.await {
                        window.notify(TermWindowNotif::Apply(Box::new(move |myself| {
                            myself.dispatch_pasted_clipboard_contents(paste_target, contents);
                        })));
                    }
                })
                .detach();
            }
            ClipboardPasteSource::PrimarySelection => {
                // Middle-click territory: text only, the historical path.
                let future = window.get_clipboard(Clipboard::PrimarySelection);
                promise::spawn::spawn(async move {
                    if let Ok(clip) = future.await {
                        window.notify(TermWindowNotif::Apply(Box::new(move |myself| {
                            myself.paste_text_to_pane(pane_id, &clip);
                        })));
                    }
                })
                .detach();
            }
        }
        self.maybe_scroll_to_bottom_for_input(&pane);
    }

    fn dispatch_pasted_clipboard_contents(
        &mut self,
        paste_target: crate::termwindow::ui::right_sidebar::TerminalPasteTarget,
        contents: ClipboardContents,
    ) {
        let pane_id = paste_target.pane_id();
        match contents {
            ClipboardContents::Text(text) => {
                self.paste_text_to_pane(pane_id, &text);
            }
            ClipboardContents::FilePaths(paths) => {
                if !self.upload_files_to_terminal_paste_target(&paths, None, paste_target) {
                    // Not a remote session (or not this host's pane): paste
                    // the quoted local paths, the historical behavior.
                    let text = ClipboardContents::FilePaths(paths).to_text();
                    self.paste_text_to_pane(pane_id, &text);
                }
            }
            ClipboardContents::Image { format, bytes } => {
                // Pasting an image into a LOCAL shell has never done
                // anything; only remote sessions gain an upload.
                self.upload_pasted_image_to_remote_terminal(paste_target, format, bytes);
            }
        }
    }

    pub(crate) fn paste_text_to_pane(&mut self, pane_id: PaneId, text: &str) {
        if text.is_empty() {
            return;
        }
        if let Some(pane) = self
            .pane_state(pane_id)
            .overlay
            .as_ref()
            .map(|overlay| overlay.pane.clone())
            .or_else(|| {
                let mux = Mux::get();
                mux.get_pane(pane_id)
            })
        {
            pane.send_paste(text).ok();
        }
    }
}
