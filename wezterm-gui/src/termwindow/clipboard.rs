use crate::termwindow::{ContextMenuApplicationAction, TermWindowNotif};
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
        let pane = self
            .pane_state(pane_id)
            .overlay
            .as_ref()
            .map(|overlay| overlay.pane.clone())
            .or_else(|| {
                let mux = Mux::get();
                mux.get_pane(pane_id)
            });
        if let Some(pane) = pane {
            self.paste_text_into(&pane, text);
        }
    }

    /// Paste `text` into `pane`, the terminal or an overlay over it, asking
    /// first, when the setting is on, if its newlines would run what comes
    /// before them.
    pub(crate) fn paste_text_into(&mut self, pane: &Arc<dyn Pane>, text: &str) {
        if text.is_empty() {
            return;
        }
        // An overlay takes a paste in itself; only the program can run it.
        if crate::native_settings::paste_confirmation()
            && paste_needs_confirmation(text, pane.bracketed_paste())
            && self.pane_state(pane.pane_id()).overlay.is_none()
        {
            self.confirm_paste(pane.pane_id(), text.to_string());
            return;
        }
        self.deliver_paste(pane, text);
    }

    fn deliver_paste(&mut self, pane: &Arc<dyn Pane>, text: &str) {
        // Where the paste goes in, read before it does.
        let cursor = pane.get_cursor_position();
        if pane.send_paste(text).is_ok() {
            self.light_up_terminal_paste(pane, text, cursor);
        }
    }

    /// Ask, in a menu at the text cursor, before pasting `text` where its
    /// newlines would run what comes before them.
    fn confirm_paste(&mut self, pane_id: PaneId, text: String) {
        let Some(window) = self.window.clone() else {
            return;
        };
        let anchor = self
            .text_cursor_rect
            .map(|cell| window::Point::new(cell.min_x(), cell.max_y()))
            .or_else(|| self.current_mouse_event.as_ref().map(|event| event.coords))
            .unwrap_or_else(|| window::Point::new(0, 0));
        let header = window::ContextMenuItem::SectionHeader {
            label: crate::i18n::tr("paste-confirm-header"),
        };
        self.begin_context_menu_application_actions();
        let paste = self.context_menu_application_item_with_icon(
            crate::i18n::tr("paste-confirm-anyway"),
            window::ContextMenuIcon::Paste,
            ContextMenuApplicationAction::ResolvePaste(Some((pane_id, text))),
            true,
        );
        let cancel = self.context_menu_application_item_with_icon(
            crate::i18n::tr("paste-confirm-cancel"),
            window::ContextMenuIcon::Close,
            ContextMenuApplicationAction::ResolvePaste(None),
            true,
        );
        self.show_term_context_menu(&window, anchor, vec![header, paste, cancel]);
    }

    /// Paste what the person agreed to paste, should its pane still be there.
    pub(crate) fn paste_confirmed(&mut self, pane_id: PaneId, text: &str) {
        if let Some(pane) = Mux::get().get_pane(pane_id) {
            self.deliver_paste(&pane, text);
        }
    }

    /// Light up `text`, pasted into `pane` at `cursor`, if it went to the
    /// terminal: an overlay over it takes a paste in itself (copy mode's
    /// search line) or drops it (quick select), and the terminal's own text
    /// there is not the paste.
    pub(crate) fn light_up_terminal_paste(
        &mut self,
        pane: &Arc<dyn Pane>,
        text: &str,
        cursor: mux::renderable::StableCursorPosition,
    ) {
        if self.pane_state(pane.pane_id()).overlay.is_none() {
            self.light_up_paste(pane, text, cursor);
        }
    }
}

/// Whether pasting `text` could run commands before the person sees them:
/// it breaks a line, and the program did not ask for pastes bracketed, so
/// each line goes in as if typed and entered. A bracketed paste cannot end
/// early -- the terminal strips the markers from it -- and a pane that
/// cannot tell is not asked about.
fn paste_needs_confirmation(text: &str, bracketed_paste: Option<bool>) -> bool {
    bracketed_paste == Some(false) && text.contains(['\n', '\r'])
}

#[cfg(test)]
mod tests {
    use super::paste_needs_confirmation;

    #[test]
    fn only_a_line_break_into_an_unbracketed_program_asks() {
        assert!(paste_needs_confirmation("ls\nrm -rf build\n", Some(false)));
        assert!(paste_needs_confirmation("make install\n", Some(false)));
        assert!(paste_needs_confirmation("a\rb", Some(false)));
        assert!(!paste_needs_confirmation("git status", Some(false)));
        assert!(!paste_needs_confirmation("ls\nrm -rf build\n", Some(true)));
        assert!(!paste_needs_confirmation("ls\n\x1b[201~rm -rf build\n", Some(true)));
        assert!(!paste_needs_confirmation("ls\nrm -rf build\n", None));
    }
}
