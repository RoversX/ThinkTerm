use crate::terminal::{Alert, Progress};
use crate::terminalstate::{
    default_color_map, CharSet, MouseEncoding, TabStop, UnicodeVersionStackEntry,
};
use crate::{ClipboardSelection, Position, TerminalState, VisibleRowIndex, DCS, ST};
use finl_unicode::grapheme_clusters::Graphemes;
use log::{debug, error};
use num_traits::FromPrimitive;
use ordered_float::NotNan;
use std::fmt::Write;
use std::io::Write as _;
use std::ops::{Deref, DerefMut};
use termwiz::input::KeyboardEncoding;
use unicode_normalization::{is_nfc_quick, IsNormalized, UnicodeNormalization};
use url::Url;
use wezterm_bidi::ParagraphDirectionHint;
use wezterm_cell::{
    grapheme_column_width, is_white_space_grapheme, Cell, CellAttributes, PlainWidths, SemanticType,
};
use wezterm_escape_parser::csi::{
    CharacterPath, EraseInDisplay, Keyboard, KittyKeyboardFlags, KittyKeyboardMode,
};
use wezterm_escape_parser::osc::{
    ChangeColorPair, ColorOrQuery, FinalTermSemanticPrompt, ITermProprietary,
    ITermUnicodeVersionOp, ProgramStatus, Selection,
};
use wezterm_escape_parser::{
    Action, ControlCode, DeviceControlMode, Esc, EscCode, OperatingSystemCommand, CSI,
};

/// A helper struct for implementing `vtparse::VTActor` while compartmentalizing
/// the terminal state and the embedding/host terminal interface
pub(crate) struct Performer<'a> {
    pub state: &'a mut TerminalState,
    print: String,
    #[cfg(test)]
    disable_ascii_batch: bool,
    #[cfg(test)]
    disable_grapheme_runs: bool,
}

impl<'a> Deref for Performer<'a> {
    type Target = TerminalState;

    fn deref(&self) -> &TerminalState {
        self.state
    }
}

impl<'a> DerefMut for Performer<'a> {
    fn deref_mut(&mut self) -> &mut TerminalState {
        &mut self.state
    }
}

impl<'a> Drop for Performer<'a> {
    fn drop(&mut self) {
        self.flush_print();
    }
}

impl<'a> Performer<'a> {
    fn kitty_keyboard_enabled(&self) -> bool {
        // Restored terminals may have negotiated Kitty with another owner.
        // Let that session update its flags until it pops the saved state.
        self.config.enable_kitty_keyboard()
            || matches!(
                self.screen().keyboard_stack.last(),
                Some(KeyboardEncoding::Kitty(_))
            )
    }

    pub fn new(state: &'a mut TerminalState) -> Self {
        Self {
            state,
            print: String::new(),
            #[cfg(test)]
            disable_ascii_batch: false,
            #[cfg(test)]
            disable_grapheme_runs: false,
        }
    }

    /// Apply character set related remapping to the input glyph if required
    fn remap_grapheme<'b>(&self, g: &'b str) -> &'b str {
        if (self.shift_out && self.g1_charset == CharSet::DecLineDrawing)
            || (!self.shift_out && self.g0_charset == CharSet::DecLineDrawing)
        {
            match g {
                "`" => "◆",
                "a" => "▒",
                "b" => "␉",
                "c" => "␌",
                "d" => "␍",
                "e" => "␊",
                "f" => "°",
                "g" => "±",
                "h" => "␤",
                "i" => "␋",
                "j" => "┘",
                "k" => "┐",
                "l" => "┌",
                "m" => "└",
                "n" => "┼",
                "o" => "⎺",
                "p" => "⎻",
                "q" => "─",
                "r" => "⎼",
                "s" => "⎽",
                "t" => "├",
                "u" => "┤",
                "v" => "┴",
                "w" => "┬",
                "x" => "│",
                "y" => "≤",
                "z" => "≥",
                "{" => "π",
                "|" => "≠",
                "}" => "£",
                "~" => "·",
                _ => g,
            }
        } else if (self.shift_out && self.g1_charset == CharSet::Uk)
            || (!self.shift_out && self.g0_charset == CharSet::Uk)
        {
            match g {
                "#" => "£",
                _ => g,
            }
        } else {
            g
        }
    }

    fn flush_print(&mut self) {
        if self.print.is_empty() {
            return;
        }

        let mut p = std::mem::take(&mut self.print);
        self.print_now(&p, false);
        std::mem::swap(&mut self.print, &mut p);
        self.print.clear();
    }

    /// Write `text` to the screen. `ascii` says it is known to be printable
    /// ASCII, which saves looking.
    fn print_now(&mut self, text: &str, ascii: bool) {
        let seqno = self.seqno;
        let normalized: String;
        let text = if !ascii
            && self.config_normalize_output_to_unicode_nfc()
            && is_nfc_quick(text.chars()) != IsNormalized::Yes
        {
            normalized = text.nfc().collect();
            normalized.as_str()
        } else {
            text
        };

        // Mixed Unicode must still be segmented as a whole: an ASCII base may
        // belong to a combining sequence or emoji. Special character sets and
        // insert mode retain the existing grapheme path as well.
        let charset = if self.shift_out {
            self.g1_charset
        } else {
            self.g0_charset
        };
        let ascii_batch = !self.insert
            && charset == CharSet::Ascii
            && self.unicode_version.cell_widths.is_none()
            && (ascii || wezterm_escape_parser::printable_ascii_len(text.as_bytes()) == text.len());
        #[cfg(test)]
        let ascii_batch = ascii_batch && !self.disable_ascii_batch;
        if ascii_batch {
            self.flush_ascii(text);
        } else {
            // Graphemes written one after another at the end of a clustered
            // row are appended as one run (`Line::append_grapheme_run`). The
            // writes are only deferred: nothing reads the row before the run
            // is flushed. Remapped or custom-width text and insert mode keep
            // writing one grapheme at a time.
            let runs = !self.insert
                && charset == CharSet::Ascii
                && self.unicode_version.cell_widths.is_none();
            #[cfg(test)]
            let runs = runs && !self.disable_grapheme_runs;
            // Writing a few characters is no quicker by way of `print_plain`.
            let plain = if runs && text.len() >= PLAIN_TEXT_MIN_LEN {
                self.unicode_version.plain_widths()
            } else {
                None
            };
            let mut run = GraphemeRun::default();
            // A row found unable to take a run, until the cursor next wraps.
            let mut no_run_row = None;
            let mut offset = 0;
            let mut graphemes = Graphemes::new(text);
            // Graphemes to go before looking for plain text again, and how
            // many to give it next time: text with marks on every letter
            // should not pay for looking at each one. An emoji on its own
            // says nothing about what follows it.
            let mut not_plain = 0;
            let mut back_off = 1;
            loop {
                if not_plain > 0 {
                    not_plain -= 1;
                } else if let Some(plain) = plain {
                    let printed = self.print_plain(text, offset, plain, &mut run, &mut no_run_row);
                    if printed == offset {
                        not_plain = back_off;
                        back_off = (back_off * 2).min(64);
                    } else {
                        back_off = 1;
                        offset = printed;
                        graphemes = Graphemes::new(&text[offset..]);
                    }
                }
                let Some(g) = graphemes.next() else {
                    break;
                };
                if not_plain > 0 && g.len() == 4 && g.as_bytes()[0] >= 0xf0 {
                    not_plain = 0;
                    back_off = 1;
                }
                let start = offset;
                offset += g.len();
                let g = self.remap_grapheme(g);

                let mut print_width = grapheme_column_width(g, Some(&self.unicode_version));
                if print_width == 0 {
                    // We got a zero-width grapheme.

                    // Relevant reading:
                    // <https://github.com/wezterm/wezterm/issues/1422>
                    // <https://github.com/wezterm/wezterm/issues/6637>
                    // <https://github.com/harfbuzz/harfbuzz/issues/4279>
                    // <https://www.unicode.org/faq/unsup_char.html#2>
                    //
                    // For White_Space we want to ensure that we display as a space.
                    // Other non-printing, zero-width characters can be elided
                    // to avoid presentation problems, but may introduce potential
                    // weirdness elsewhere. For example, U+2068 is a BIDI control
                    // character and will be elided by this logic. A consequence
                    // of that is that when the user copies the surrounding text
                    // from the terminal, that BIDI control will not be present.
                    // We do not currently have a solution for that.
                    if is_white_space_grapheme(g) {
                        // Ensure that White_Space shows as a space
                        print_width = 1;
                    } else {
                        log::trace!("Eliding zero-width grapheme {:?}", g);
                        continue;
                    }
                }

                if self.wrap_next {
                    // Wrapping marks the row and may scroll it away.
                    self.flush_grapheme_run(&mut run, text, seqno);
                    no_run_row = None;
                    run.written_to = None;
                }
                self.wrap_if_needed();

                let x = self.cursor.x;
                let y = self.cursor.y;
                let width = self.left_and_right_margins.end;

                let wrappable = x + print_width >= width;

                if self.insert {
                    let margin = self.left_and_right_margins.end;
                    let screen = self.screen_mut();
                    for _ in x..x + print_width as usize {
                        screen.insert_cell(x, y, margin, seqno);
                    }
                }

                // Assign the cell
                log::trace!(
                    "print x={} y={} print_width={} width={} cell={} {:?}",
                    x,
                    y,
                    print_width,
                    width,
                    g,
                    self.pen
                );
                let in_run = runs && no_run_row != Some(y) && {
                    if !run.continues(x, y, start) {
                        self.flush_grapheme_run(&mut run, text, seqno);
                        if !self.can_start_grapheme_run(x, y) {
                            no_run_row = Some(y);
                        }
                    }
                    no_run_row != Some(y)
                };
                if in_run {
                    run.push(x, y, start, offset, print_width);
                } else {
                    let pen = self.pen.clone();
                    let follows = run.written_to == Some(start);
                    run.written_to = Some(offset);
                    let screen = self.screen_mut();
                    let row = screen.phys_row(y);
                    screen.line_mut(row).set_cell_grapheme_after(
                        x,
                        g,
                        print_width,
                        pen,
                        follows,
                        seqno,
                    );
                }

                if !wrappable {
                    self.cursor.x += print_width;
                    self.wrap_next = false;
                } else {
                    self.wrap_next = self.dec_auto_wrap;
                }
            }
            self.flush_grapheme_run(&mut run, text, seqno);
        }
    }

    /// Write the plain text at `offset` of `text` (see `PlainWidths`) that
    /// goes in one row at a time, for as long as it lasts, and return where
    /// it stopped. A character with something else after it is left for the
    /// general path to take with what follows it, and so is the one that
    /// reaches the margin, which has more to do.
    fn print_plain(
        &mut self,
        text: &str,
        offset: usize,
        plain: &PlainWidths,
        run: &mut GraphemeRun,
        no_run_row: &mut Option<VisibleRowIndex>,
    ) -> usize {
        let seqno = self.seqno;
        let bytes = text.as_bytes();
        let mut offset = offset;
        loop {
            // The first grapheme, and whether it can go at all.
            let Some(mut this) = plain.first(&bytes[offset..]) else {
                return offset;
            };
            let Some(mut step) = plain_grapheme(plain, &bytes[offset..], this.0) else {
                return offset;
            };
            // A run waiting is cheaper to go on with than to cut short for a
            // little plain text.
            if run.count > 0 && !plain_lasts(plain, &bytes[offset..]) {
                return offset;
            }
            // What is waiting has to be in the row to tell where this goes.
            self.flush_grapheme_run(run, text, seqno);
            if self.wrap_next {
                *no_run_row = None;
                run.written_to = None;
            }
            self.wrap_if_needed();

            let x = self.cursor.x;
            let y = self.cursor.y;
            let margin = self.left_and_right_margins.end;
            // A row that can't take it all at once takes it a cell at a time.
            let at_once = self.can_start_grapheme_run(x, y);

            let start = offset;
            let mut cells = 0;
            let mut wide = [0; 128];
            let mut num_wide = 0;
            let mut max_len = 0;
            let mut last_width = 0;
            loop {
                let (len, next) = step;
                let width = this.1;
                if x + cells + width >= margin {
                    break;
                }
                if !at_once {
                    let pen = self.pen.clone();
                    let g = &text[offset..offset + len];
                    let follows = run.written_to == Some(offset);
                    run.written_to = Some(offset + len);
                    let screen = self.screen_mut();
                    let row = screen.phys_row(y);
                    screen.line_mut(row).set_cell_grapheme_after(
                        x + cells,
                        g,
                        width,
                        pen,
                        follows,
                        seqno,
                    );
                } else if width > 1 {
                    if num_wide == wide.len() {
                        break;
                    }
                    wide[num_wide] = cells as u16;
                    num_wide += 1;
                }
                let first = cells == 0;
                cells += width;
                offset += len;
                max_len = max_len.max(len);
                last_width = width;
                let Some(next) = next else {
                    break;
                };
                this = next;
                let Some(after) = plain_grapheme(plain, &bytes[offset..], this.0) else {
                    break;
                };
                step = after;
                // Writing past the end of a row fills in the gap, after which
                // the rest can go at once.
                if first && !at_once && self.can_start_grapheme_run(x + cells, y) {
                    break;
                }
            }
            if offset == start {
                return offset;
            }
            if at_once {
                let pen = self.pen.clone();
                let follows = run.written_to == Some(start);
                let appended = {
                    let screen = self.screen_mut();
                    let idx = screen.phys_row(y);
                    screen.line_mut(idx).append_grapheme_run(
                        x,
                        &text[start..offset],
                        max_len,
                        cells,
                        &wide[..num_wide],
                        last_width as u8,
                        &pen,
                        follows,
                        seqno,
                    )
                };
                if !appended {
                    self.write_graphemes(x, y, &text[start..offset], &pen, follows, seqno);
                }
                run.written_to = Some(offset);
            }
            self.cursor.x += cells;
            self.wrap_next = false;
            if offset == bytes.len() {
                return offset;
            }
        }
    }

    fn can_start_grapheme_run(&mut self, x: usize, y: VisibleRowIndex) -> bool {
        let screen = self.screen_mut();
        let idx = screen.phys_row(y);
        screen.line_mut(idx).can_append_grapheme_run(x)
    }

    /// Write the graphemes of `run`, which began where
    /// `can_start_grapheme_run` held and has had nothing written since.
    fn flush_grapheme_run(
        &mut self,
        run: &mut GraphemeRun,
        text: &str,
        seqno: wezterm_surface::SequenceNo,
    ) {
        if std::mem::take(&mut run.count) == 0 {
            return;
        }
        let run_text = &text[run.start..run.end];
        let pen = self.pen.clone();
        let follows = run.written_to == Some(run.start);
        run.written_to = Some(run.end);
        let appended = {
            let screen = self.screen_mut();
            let idx = screen.phys_row(run.y);
            screen.line_mut(idx).append_grapheme_run(
                run.x,
                run_text,
                run.max_len,
                run.cells,
                &run.wide[..run.num_wide],
                run.last_width,
                &pen,
                follows,
                seqno,
            )
        };
        if !appended {
            self.write_graphemes(run.x, run.y, run_text, &pen, follows, seqno);
        }
    }

    /// Write the graphemes of a run the row would not take in one go, for an
    /// overflowing cluster width or a grapheme too long to tell how the
    /// row's text grows, or that would run into what is before it. Zero-width
    /// ones in a run are white space, shown as one cell. `follows` says that
    /// the text is known not to.
    fn write_graphemes(
        &mut self,
        mut x: usize,
        y: VisibleRowIndex,
        text: &str,
        pen: &CellAttributes,
        mut follows: bool,
        seqno: wezterm_surface::SequenceNo,
    ) {
        for g in Graphemes::new(text) {
            let width = grapheme_column_width(g, Some(&self.unicode_version)).max(1);
            let screen = self.screen_mut();
            let row = screen.phys_row(y);
            screen
                .line_mut(row)
                .set_cell_grapheme_after(x, g, width, pen.clone(), follows, seqno);
            x += width;
            follows = true;
        }
    }

    fn wrap_if_needed(&mut self) {
        let seqno = self.seqno;
        if self.wrap_next {
            // Since we're implicitly moving the cursor to the next
            // line, we need to tag the current position as wrapped
            // so that we can correctly reflow it if the window is
            // resized.
            {
                let x = self.cursor.x;
                let y = self.cursor.y;
                let is_conpty = self.state.enable_conpty_quirks;
                let screen = self.screen_mut();
                let y = screen.phys_row(y);

                fn makes_sense_to_wrap(s: &str) -> bool {
                    let len = s.len();
                    match (len, s.chars().next()) {
                        (1, Some(c)) => c.is_alphanumeric() || c.is_ascii_punctuation(),
                        _ => true,
                    }
                }

                let should_mark_wrapped = !is_conpty
                    || screen
                        .line_mut(y)
                        .visible_cells()
                        .last()
                        .map(|cell| makes_sense_to_wrap(cell.str()))
                        .unwrap_or(false);
                if should_mark_wrapped {
                    let line = screen.line_mut(y);
                    line.keep_blanks_to(x + 1);
                    line.set_last_cell_was_wrapped(true, seqno);
                }
            }
            self.new_line(true);
        }
    }

    fn flush_ascii(&mut self, mut text: &str) {
        let seqno = self.seqno;
        while !text.is_empty() {
            self.wrap_if_needed();

            let x = self.cursor.x;
            let y = self.cursor.y;
            let width = self.left_and_right_margins.end;
            let count = text.len().min(width.saturating_sub(x).max(1));
            let pen = self.pen.clone();
            let screen = self.screen_mut();
            let row = screen.phys_row(y);
            screen
                .line_mut(row)
                .set_printable_ascii_cells(x, &text[..count], &pen, seqno);
            if x + count < width {
                self.cursor.x += count;
                self.wrap_next = false;
            } else {
                // The scalar path leaves the cursor on the final cell, even
                // when wrapping is disabled or the cursor is past the margin.
                self.cursor.x += count - 1;
                self.wrap_next = self.dec_auto_wrap;
            }
            text = &text[count..];
        }
    }

    /// ConPTY, at the time of writing, does something horrible to rewrite
    /// `ESC k TITLE ST` into something completely different and out-of-order,
    /// and critically, removes the ST.
    /// The result is that our hack to accumulate the tmux title gets stuck
    /// in a mode where all printable output is accumulated for the title.
    /// To combat this, we pop_tmux_title_state when we're obviously moving
    /// to different escape sequence parsing states.
    /// <https://github.com/wezterm/wezterm/issues/2442>
    fn pop_tmux_title_state(&mut self) {
        if let Some(title) = self.accumulating_title.take() {
            log::debug!("ST never received for pending tmux title escape sequence: {title:?}");
        }
    }

    pub fn print_text(&mut self, text: &str) {
        if let Some(title) = self.accumulating_title.as_mut() {
            title.push_str(text);
        } else {
            self.print.push_str(text);
        }
    }

    /// `print_text` for a parser's run of text, with the tabs and line ends
    /// it may have left in it acted on in place. Text ahead of one of those
    /// goes straight to the screen: nothing can follow it to join it, so
    /// there is no need to gather it up first. What is left at the end can
    /// still be joined by what comes next.
    pub(crate) fn print_run(&mut self, text: &str) {
        let bytes = text.as_bytes();
        let mut start = 0;
        while start < bytes.len() {
            let mut end = start + wezterm_escape_parser::printable_ascii_len(&bytes[start..]);
            let ascii = !matches!(bytes.get(end), Some(0x7f..));
            if !ascii {
                end += wezterm_escape_parser::c0_position(&bytes[end..]);
            }
            let control = match bytes.get(end) {
                None => return self.print_text(&text[start..]),
                Some(b'\t') => ControlCode::HorizontalTab,
                Some(b'\n') => ControlCode::LineFeed,
                Some(b'\r') => ControlCode::CarriageReturn,
                // Anything else is the business of the general path.
                Some(_) => return self.print_text(&text[start..]),
            };
            if self.print.is_empty() && self.accumulating_title.is_none() {
                if end > start {
                    self.print_now(&text[start..end], ascii);
                }
            } else {
                self.print_text(&text[start..end]);
            }
            self.control(control);
            start = end + 1;
        }
    }

    pub fn perform(&mut self, action: Action) {
        debug!("perform {:?}", action);
        if self.suppress_initial_title_change {
            match &action {
                Action::OperatingSystemCommand(osc) => match **osc {
                    OperatingSystemCommand::SetIconNameAndWindowTitle(_) => {
                        debug!("suppressed {:?}", osc);
                        self.suppress_initial_title_change = false;
                        return;
                    }
                    _ => {}
                },
                _ => {}
            }
        }
        match action {
            Action::Print(c) => self.print(c),
            Action::PrintString(s) => self.print_run(&s),
            Action::Control(code) => self.control(code),
            Action::DeviceControl(ctrl) => self.device_control(ctrl),
            Action::OperatingSystemCommand(osc) => self.osc_dispatch(*osc),
            Action::Esc(esc) => self.esc_dispatch(esc),
            Action::CSI(csi) => self.csi_dispatch(csi),
            Action::Sixel(sixel) => self.sixel(sixel),
            Action::XtGetTcap(names) => self.xt_get_tcap(names),
            Action::KittyImage(img) => {
                self.flush_print();
                if let Err(err) = self.kitty_img(*img) {
                    // A frame the parser discarded as superseded carries an
                    // Interrupted io error: expected under a backlog, and
                    // hundreds per second when it happens.
                    let superseded = err
                        .root_cause()
                        .downcast_ref::<std::io::Error>()
                        .is_some_and(|e| e.kind() == std::io::ErrorKind::Interrupted);
                    if superseded {
                        log::debug!("kitty_img: {:#}", err);
                    } else {
                        log::error!("kitty_img: {:#}", err);
                    }
                }
            }
        }
    }

    fn device_control(&mut self, ctrl: DeviceControlMode) {
        self.pop_tmux_title_state();
        match &ctrl {
            DeviceControlMode::ShortDeviceControl(s) => {
                match (s.byte, s.intermediates.as_slice()) {
                    (b'q', &[b'$']) => {
                        // DECRQSS - Request Status String
                        // https://vt100.net/docs/vt510-rm/DECRQSS.html
                        // The response is described here:
                        // https://vt100.net/docs/vt510-rm/DECRPSS.html
                        // but note that *that* text has the validity value
                        // inverted; there's a note about this in the xterm
                        // ctlseqs docs.
                        match s.data.as_slice() {
                            &[b'"', b'p'] => {
                                // DECSCL - select conformance level
                                write!(self.writer, "{}1$r65;1\"p{}", DCS, ST).ok();
                                self.writer.flush().ok();
                            }
                            &[b'r'] => {
                                // DECSTBM - top and bottom margins
                                let margins = self.top_and_bottom_margins.clone();
                                write!(
                                    self.writer,
                                    "{}1$r{};{}r{}",
                                    DCS,
                                    margins.start + 1,
                                    margins.end,
                                    ST
                                )
                                .ok();
                                self.writer.flush().ok();
                            }
                            &[b's'] => {
                                // DECSLRM - left and right margins
                                let margins = self.left_and_right_margins.clone();
                                write!(
                                    self.writer,
                                    "{}1$r{};{}s{}",
                                    DCS,
                                    margins.start + 1,
                                    margins.end,
                                    ST
                                )
                                .ok();
                                self.writer.flush().ok();
                            }
                            _ => {
                                if self.config.log_unknown_escape_sequences() {
                                    log::warn!("unhandled DECRQSS {:?}", s);
                                }
                                // Reply that the request is invalid
                                write!(self.writer, "{}0$r{}", DCS, ST).ok();
                                self.writer.flush().ok();
                            }
                        }
                    }
                    _ => {
                        if self.config.log_unknown_escape_sequences() {
                            log::warn!("unhandled {:?}", s);
                        }
                    }
                }
            }
            _ => match self.device_control_handler.as_mut() {
                Some(handler) => handler.handle_device_control(ctrl),
                None => {
                    if self.config.log_unknown_escape_sequences() {
                        log::warn!("unhandled {:?}", ctrl);
                    }
                }
            },
        }
    }

    /// Draw a character to the screen
    fn print(&mut self, c: char) {
        // We buffer up the chars to increase the chances of correctly grouping graphemes into cells
        if let Some(title) = self.accumulating_title.as_mut() {
            title.push(c);
        } else {
            self.print.push(c);
        }
    }

    fn control(&mut self, control: ControlCode) {
        let seqno = self.seqno;
        self.pop_tmux_title_state();
        self.flush_print();
        match control {
            ControlCode::LineFeed | ControlCode::VerticalTab | ControlCode::FormFeed => {
                if self.left_and_right_margins.contains(&self.cursor.x) {
                    self.new_line(false);
                } else {
                    // Do move down, but don't trigger a scroll when we're
                    // outside of the left/right margins
                    let old_y = self.cursor.y;
                    let y = if old_y == self.top_and_bottom_margins.end - 1 {
                        old_y
                    } else {
                        (old_y + 1).min(self.screen().physical_rows as i64 - 1)
                    };
                    self.screen_mut().dirty_line(old_y, seqno);
                    self.screen_mut().dirty_line(y, seqno);
                    self.cursor.y = y;
                    self.wrap_next = false;
                }
                if self.newline_mode {
                    self.cursor.x = 0;
                    self.clear_semantic_attribute_due_to_movement();
                }
            }
            ControlCode::CarriageReturn => {
                if self.cursor.x >= self.left_and_right_margins.start {
                    self.cursor.x = self.left_and_right_margins.start;
                } else {
                    self.cursor.x = 0;
                }
                let y = self.cursor.y;
                self.wrap_next = false;
                self.clear_semantic_attribute_due_to_movement();
                self.screen_mut().dirty_line(y, seqno);
            }

            ControlCode::Backspace => {
                if self.reverse_wraparound_mode
                    && self.dec_auto_wrap
                    && self.cursor.x == self.left_and_right_margins.start
                    && self.cursor.y == self.top_and_bottom_margins.start
                {
                    // Backspace off the top-left wraps around to the bottom right
                    let x_pos = Position::Absolute(self.left_and_right_margins.end as i64 - 1);
                    let y_pos = Position::Absolute(self.top_and_bottom_margins.end - 1);
                    self.set_cursor_pos(&x_pos, &y_pos);
                } else if self.reverse_wraparound_mode
                    && self.dec_auto_wrap
                    && self.cursor.x <= self.left_and_right_margins.start
                {
                    // Backspace off the left wraps around to the prior line on the right
                    let x_pos = Position::Absolute(self.left_and_right_margins.end as i64 - 1);
                    let y_pos = Position::Relative(-1);
                    self.set_cursor_pos(&x_pos, &y_pos);
                } else if self.reverse_wraparound_mode
                    && self.dec_auto_wrap
                    && self.cursor.x == self.left_and_right_margins.end - 1
                    && self.wrap_next
                {
                    // If the cursor is in the last column and a character was
                    // just output and reverse-wraparound is on then backspace
                    // by 1 cancels the pending wrap.
                    self.wrap_next = false;
                } else if self.cursor.x == self.left_and_right_margins.start {
                    // Respect the left margin and don't BS outside it
                } else {
                    self.set_cursor_pos(&Position::Relative(-1), &Position::Relative(0));
                }
            }
            ControlCode::HorizontalTab => self.c0_horizontal_tab(),
            ControlCode::HTS => self.c1_hts(),
            ControlCode::IND => self.c1_index(),
            ControlCode::NEL => self.c1_nel(),
            ControlCode::Bell => {
                if let Some(handler) = self.alert_handler.as_mut() {
                    handler.alert(Alert::Bell);
                } else {
                    log::info!("Ding! (this is the bell)");
                }
            }
            ControlCode::RI => self.c1_reverse_index(),

            // wezterm only supports UTF-8, so does not support the
            // DEC National Replacement Character Sets.  However, it does
            // support the DEC Special Graphics character set used by
            // numerous ncurses applications.  DEC Special Graphics can be
            // selected by ASCII Shift Out (0x0E, ^N) or by setting G0
            // via ESC ( 0 .
            ControlCode::ShiftIn => {
                self.shift_out = false;
            }
            ControlCode::ShiftOut => {
                self.shift_out = true;
            }

            ControlCode::Enquiry => {
                let response = self.config.enq_answerback();
                if response.len() > 0 {
                    write!(self.writer, "{}", response).ok();
                    self.writer.flush().ok();
                }
            }

            ControlCode::Null => {}

            _ => {
                if self.config.log_unknown_escape_sequences() {
                    log::warn!("unhandled ControlCode {:?}", control);
                }
            }
        }
    }

    fn csi_dispatch(&mut self, csi: CSI) {
        self.pop_tmux_title_state();
        self.flush_print();
        match csi {
            CSI::Sgr(sgr) => self.state.perform_csi_sgr(sgr),
            CSI::Cursor(wezterm_escape_parser::csi::Cursor::Left(n)) => {
                // We treat CUB (Cursor::Left) the same as Backspace as
                // that is what xterm does.
                // <https://github.com/wezterm/wezterm/issues/1273>
                for _ in 0..n {
                    self.control(ControlCode::Backspace);
                }
            }
            CSI::Cursor(cursor) => self.state.perform_csi_cursor(cursor),
            CSI::Edit(edit) => self.state.perform_csi_edit(edit),
            CSI::Mode(mode) => self.state.perform_csi_mode(mode),
            CSI::Device(dev) => self.state.perform_device(*dev),
            CSI::Mouse(mouse) => error!("mouse report sent by app? {:?}", mouse),
            CSI::Window(window) => self.state.perform_csi_window(*window),
            CSI::SelectCharacterPath(CharacterPath::ImplementationDefault, _) => {
                self.state.bidi_hint.take();
            }
            CSI::SelectCharacterPath(CharacterPath::LeftToRightOrTopToBottom, _) => {
                self.state
                    .bidi_hint
                    .replace(ParagraphDirectionHint::LeftToRight);
            }
            CSI::SelectCharacterPath(CharacterPath::RightToLeftOrBottomToTop, _) => {
                self.state
                    .bidi_hint
                    .replace(ParagraphDirectionHint::RightToLeft);
            }
            CSI::Keyboard(Keyboard::SetKittyState { flags, mode }) => {
                if self.kitty_keyboard_enabled() {
                    let current_flags = match self.screen().keyboard_stack.last() {
                        Some(KeyboardEncoding::Kitty(flags)) => *flags,
                        _ => KittyKeyboardFlags::NONE,
                    };
                    let flags = match mode {
                        KittyKeyboardMode::AssignAll => flags,
                        KittyKeyboardMode::SetSpecified => current_flags | flags,
                        KittyKeyboardMode::ClearSpecified => current_flags - flags,
                    };
                    self.screen_mut().keyboard_stack.pop();
                    self.screen_mut()
                        .keyboard_stack
                        .push(KeyboardEncoding::Kitty(flags));
                }
            }
            CSI::Keyboard(Keyboard::PushKittyState { flags, mode }) => {
                if self.kitty_keyboard_enabled() {
                    let current_flags = match self.screen().keyboard_stack.last() {
                        Some(KeyboardEncoding::Kitty(flags)) => *flags,
                        _ => KittyKeyboardFlags::NONE,
                    };
                    let flags = match mode {
                        KittyKeyboardMode::AssignAll => flags,
                        KittyKeyboardMode::SetSpecified => current_flags | flags,
                        KittyKeyboardMode::ClearSpecified => current_flags - flags,
                    };
                    let screen = self.screen_mut();
                    screen.keyboard_stack.push(KeyboardEncoding::Kitty(flags));
                    if screen.keyboard_stack.len() > 128 {
                        screen.keyboard_stack.remove(0);
                    }
                }
            }
            CSI::Keyboard(Keyboard::PopKittyState(n)) => {
                for _ in 0..n {
                    self.screen_mut().keyboard_stack.pop();
                }
            }
            CSI::Keyboard(Keyboard::QueryKittySupport) => {
                if self.kitty_keyboard_enabled() {
                    let flags = match self.screen().keyboard_stack.last() {
                        Some(KeyboardEncoding::Kitty(flags)) => *flags,
                        _ => KittyKeyboardFlags::NONE,
                    };
                    write!(self.writer, "\x1b[?{}u", flags.bits()).ok();
                    self.writer.flush().ok();
                }
            }
            CSI::Keyboard(Keyboard::ReportKittyState(_)) => {
                // This is a response to QueryKittySupport and it is invalid for us
                // to receive it. Just ignore it.
            }
            CSI::Unspecified(unspec) => {
                if self.config.log_unknown_escape_sequences() {
                    log::warn!("unknown unspecified CSI: {:?}", format!("{}", unspec));
                }
            }
        };
    }

    fn esc_dispatch(&mut self, esc: Esc) {
        let seqno = self.seqno;
        self.flush_print();
        if esc != Esc::Code(EscCode::StringTerminator) {
            self.pop_tmux_title_state();
        }
        match esc {
            Esc::Code(EscCode::StringTerminator) => {
                // String Terminator (ST); for the most part has nothing to do here, as its purpose is
                // handled implicitly through a state transition in the vtparse state tables.
                if let Some(title) = self.accumulating_title.take() {
                    self.osc_dispatch(OperatingSystemCommand::SetIconNameAndWindowTitle(title));
                }
            }
            Esc::Code(EscCode::TmuxTitle) => {
                self.accumulating_title.replace(String::new());
            }
            Esc::Code(EscCode::DecApplicationKeyPad) => {
                debug!("DECKPAM on");
                self.application_keypad = true;
            }
            Esc::Code(EscCode::DecNormalKeyPad) => {
                debug!("DECKPAM off");
                self.application_keypad = false;
            }
            Esc::Code(EscCode::ReverseIndex) => self.c1_reverse_index(),
            Esc::Code(EscCode::Index) => self.c1_index(),
            Esc::Code(EscCode::NextLine) => self.c1_nel(),
            Esc::Code(EscCode::HorizontalTabSet) => self.c1_hts(),
            Esc::Code(EscCode::DecLineDrawingG0) => {
                self.g0_charset = CharSet::DecLineDrawing;
            }
            Esc::Code(EscCode::AsciiCharacterSetG0) => {
                self.g0_charset = CharSet::Ascii;
            }
            Esc::Code(EscCode::UkCharacterSetG0) => {
                self.g0_charset = CharSet::Uk;
            }
            Esc::Code(EscCode::DecLineDrawingG1) => {
                self.g1_charset = CharSet::DecLineDrawing;
            }
            Esc::Code(EscCode::AsciiCharacterSetG1) => {
                self.g1_charset = CharSet::Ascii;
            }
            Esc::Code(EscCode::UkCharacterSetG1) => {
                self.g1_charset = CharSet::Uk;
            }
            Esc::Code(EscCode::DecSaveCursorPosition) => self.dec_save_cursor(),
            Esc::Code(EscCode::DecRestoreCursorPosition) => self.dec_restore_cursor(),

            Esc::Code(EscCode::DecDoubleHeightTopHalfLine) => {
                let idx = self.screen.phys_row(self.cursor.y);
                self.screen.line_mut(idx).set_double_height_top(seqno);
            }
            Esc::Code(EscCode::DecDoubleHeightBottomHalfLine) => {
                let idx = self.screen.phys_row(self.cursor.y);
                self.screen.line_mut(idx).set_double_height_bottom(seqno);
            }
            Esc::Code(EscCode::DecDoubleWidthLine) => {
                let idx = self.screen.phys_row(self.cursor.y);
                self.screen.line_mut(idx).set_double_width(seqno);
            }
            Esc::Code(EscCode::DecSingleWidthLine) => {
                let idx = self.screen.phys_row(self.cursor.y);
                self.screen.line_mut(idx).set_single_width(seqno);
            }

            Esc::Code(EscCode::DecScreenAlignmentDisplay) => {
                // This one is just to make vttest happy;
                // its original purpose was for aligning the CRT.
                // https://vt100.net/docs/vt510-rm/DECALN.html

                let screen = self.screen_mut();
                let col_range = 0..screen.physical_cols;
                for y in 0..screen.physical_rows as VisibleRowIndex {
                    let line_idx = screen.phys_row(y);
                    let line = screen.line_mut(line_idx);
                    line.resize(col_range.end, seqno);
                    line.fill_range(
                        col_range.clone(),
                        &Cell::new('E', CellAttributes::default()),
                        seqno,
                    );
                }

                self.top_and_bottom_margins = 0..self.screen().physical_rows as VisibleRowIndex;
                self.left_and_right_margins = 0..self.screen().physical_cols;
                self.cursor = Default::default();
            }

            // RIS resets a device to its initial state, i.e. the state it has after it is switched
            // on. This may imply, if applicable: remove tabulation stops, remove qualified areas,
            // reset graphic rendition, erase all positions, move active position to first
            // character position of first line.
            Esc::Code(EscCode::FullReset) => {
                let seqno = self.seqno;
                self.pen = Default::default();
                self.cursor = Default::default();
                self.wrap_next = false;
                self.clear_semantic_attribute_on_newline = false;
                self.insert = false;
                self.dec_auto_wrap = true;
                self.reverse_wraparound_mode = false;
                self.reverse_video_mode = false;
                self.dec_origin_mode = false;
                self.use_private_color_registers_for_each_graphic = false;
                self.color_map = default_color_map();
                self.application_cursor_keys = false;
                self.sixel_display_mode = false;
                self.dec_ansi_mode = false;
                self.application_keypad = false;
                self.bracketed_paste = false;
                self.focus_tracking = false;
                self.mouse_tracking = false;
                self.mouse_encoding = MouseEncoding::X10;
                self.keyboard_encoding = KeyboardEncoding::Xterm;
                self.sixel_scrolls_right = false;
                self.any_event_mouse = false;
                self.button_event_mouse = false;
                self.current_mouse_buttons.clear();
                self.cursor_visible = true;
                self.g0_charset = CharSet::Ascii;
                self.g1_charset = CharSet::Ascii;
                self.shift_out = false;
                self.newline_mode = false;
                self.tabs = TabStop::new(self.screen().physical_cols, 8);
                self.palette.take();
                self.top_and_bottom_margins = 0..self.screen().physical_rows as VisibleRowIndex;
                self.left_and_right_margins = 0..self.screen().physical_cols;
                self.unicode_version = self.config.unicode_version();
                self.unicode_version_stack.clear();
                self.suppress_initial_title_change = false;
                self.accumulating_title.take();
                self.progress = Progress::default();
                self.agent_osc_title = None;
                self.agent_osc_progress = None;
                self.program_status.clear();

                // Before the screen is torn down, while the placement rows
                // still refer to real lines. The soft reset (DECSTR) already
                // did this; leaving it out of the hard reset meant `reset`
                // wiped the display but kept every image pinned in memory,
                // with their ids still resolvable by a later `a=p`.
                self.kitty_remove_all_placements(true);
                self.kitty_reset_accumulator();

                self.screen.full_reset();
                self.screen.activate_alt_screen(seqno);
                self.erase_in_display(EraseInDisplay::EraseDisplay);
                self.screen.activate_primary_screen(seqno);
                self.erase_in_display(EraseInDisplay::EraseScrollback);
                self.erase_in_display(EraseInDisplay::EraseDisplay);
                self.palette_did_change();
            }

            _ => {
                if self.config.log_unknown_escape_sequences() {
                    log::warn!("ESC: unhandled {:?}", esc);
                }
            }
        }
    }

    fn osc_dispatch(&mut self, osc: OperatingSystemCommand) {
        self.pop_tmux_title_state();
        self.flush_print();
        match osc {
            OperatingSystemCommand::SetIconNameSun(title)
            | OperatingSystemCommand::SetIconName(title) => {
                // Deliberately not recorded as agent title evidence: OSC 1
                // only names the icon, and shells emit it freely -- letting
                // it overwrite (or, when empty, erase) the window-title
                // evidence would wipe what an agent said via OSC 0/2 while
                // the displayed title is untouched.
                if title.is_empty() {
                    self.icon_title = None;
                } else {
                    self.icon_title = Some(title);
                }
                let title = self.icon_title.clone();
                if let Some(handler) = self.alert_handler.as_mut() {
                    handler.alert(Alert::IconTitleChanged(title));
                }
            }
            OperatingSystemCommand::SetIconNameAndWindowTitle(title) => {
                self.agent_osc_title = if title.is_empty() {
                    None
                } else {
                    Some(title.clone())
                };
                self.icon_title.take();
                self.title = title.clone();
                if let Some(handler) = self.alert_handler.as_mut() {
                    handler.alert(Alert::WindowTitleChanged(title.clone()));
                    handler.alert(Alert::IconTitleChanged(Some(title)));
                }
            }

            OperatingSystemCommand::SetWindowTitleSun(title)
            | OperatingSystemCommand::SetWindowTitle(title) => {
                self.agent_osc_title = if title.is_empty() {
                    None
                } else {
                    Some(title.clone())
                };
                self.title = title.clone();
                if let Some(handler) = self.alert_handler.as_mut() {
                    handler.alert(Alert::WindowTitleChanged(title));
                }
            }
            OperatingSystemCommand::SetHyperlink(link) => {
                self.set_hyperlink(link);
            }
            OperatingSystemCommand::Unspecified(unspec) => {
                if self.config.log_unknown_escape_sequences() {
                    let mut output = String::new();
                    write!(&mut output, "Unhandled OSC ").ok();

                    for item in unspec {
                        write!(&mut output, " {}", String::from_utf8_lossy(&item)).ok();
                    }
                    log::warn!("{}", output);
                }
            }

            OperatingSystemCommand::ClearSelection(selection) => {
                let selection = selection_to_selection(selection);
                self.set_clipboard_contents(selection, None).ok();
            }
            OperatingSystemCommand::QuerySelection(_) => {}
            OperatingSystemCommand::SetSelection(selection, selection_data) => {
                let selection = selection_to_selection(selection);
                match self.set_clipboard_contents(selection, Some(selection_data)) {
                    Ok(_) => (),
                    Err(err) => error!("failed to set clipboard in response to OSC 52: {:#?}", err),
                }
            }
            OperatingSystemCommand::ITermProprietary(iterm) => match iterm {
                ITermProprietary::RequestCellSize => {
                    let screen = self.screen();
                    let height = screen.physical_rows;
                    let width = screen.physical_cols;

                    let scale = if screen.dpi == 0 {
                        1.0
                    } else {
                        // Since iTerm2 is a macOS specific piece
                        // of software, it uses the macOS default dpi
                        // if 72 for the basis of its scale, regardless
                        // of the host base dpi.
                        screen.dpi as f32 / 72.
                    };
                    let width = (self.pixel_width as f32 / width as f32) / scale;
                    let height = (self.pixel_height as f32 / height as f32) / scale;

                    let response = OperatingSystemCommand::ITermProprietary(
                        ITermProprietary::ReportCellSize {
                            width_pixels: NotNan::new(width).unwrap(),
                            height_pixels: NotNan::new(height).unwrap(),
                            scale: if screen.dpi == 0 {
                                None
                            } else {
                                Some(NotNan::new(scale).unwrap())
                            },
                        },
                    );
                    write!(self.writer, "{}", response).ok();
                    self.writer.flush().ok();
                }
                ITermProprietary::File(image) => self.set_image(*image),
                ITermProprietary::SetUserVar { name, mut value } => {
                    crate::agent_contract::sanitize_agent_user_var(&name, &mut value);
                    self.user_vars.insert(name.clone(), value.clone());
                    if let Some(handler) = self.alert_handler.as_mut() {
                        handler.alert(Alert::SetUserVar { name, value });
                    }
                }
                ITermProprietary::UnicodeVersion(ITermUnicodeVersionOp::Set(n)) => {
                    self.unicode_version.version = n;
                }
                ITermProprietary::UnicodeVersion(ITermUnicodeVersionOp::Push(label)) => {
                    let vers = self.unicode_version.clone();
                    self.unicode_version_stack
                        .push(UnicodeVersionStackEntry { vers, label });
                }
                ITermProprietary::UnicodeVersion(ITermUnicodeVersionOp::Pop(None)) => {
                    if let Some(entry) = self.unicode_version_stack.pop() {
                        self.unicode_version = entry.vers;
                    }
                }
                ITermProprietary::UnicodeVersion(ITermUnicodeVersionOp::Pop(Some(label))) => {
                    while let Some(entry) = self.unicode_version_stack.pop() {
                        self.unicode_version = entry.vers;
                        if entry.label.as_deref() == Some(&label) {
                            break;
                        }
                    }
                }
                _ => {
                    if self.config.log_unknown_escape_sequences() {
                        log::warn!("unhandled iterm2: {:?}", iterm);
                    }
                }
            },

            OperatingSystemCommand::FinalTermSemanticPrompt(FinalTermSemanticPrompt::FreshLine) => {
                self.fresh_line();
            }
            OperatingSystemCommand::FinalTermSemanticPrompt(
                FinalTermSemanticPrompt::FreshLineAndStartPrompt { aid, .. },
            ) => {
                self.fresh_line();
                self.pen.set_semantic_type(SemanticType::Prompt);
                // A new shell prompt: whatever filed OSC 7501 records has
                // finished, and only its unseen results remain.
                self.program_status.end_of_program();
                self.shell_prompt_started(aid.as_deref());
            }
            OperatingSystemCommand::FinalTermSemanticPrompt(
                FinalTermSemanticPrompt::StartPrompt(_),
            ) => {
                self.pen.set_semantic_type(SemanticType::Prompt);
                // Without an aid it could be a nested shell's; the A or N
                // before it settles that.
                self.shell_commands.reported = true;
            }
            OperatingSystemCommand::FinalTermSemanticPrompt(
                FinalTermSemanticPrompt::MarkEndOfCommandWithFreshLine { aid, .. },
            ) => {
                self.fresh_line();
                self.pen.set_semantic_type(SemanticType::Prompt);
                self.shell_prompt_started(aid.as_deref());
            }
            OperatingSystemCommand::FinalTermSemanticPrompt(
                FinalTermSemanticPrompt::MarkEndOfPromptAndStartOfInputUntilNextMarker { .. },
            ) => {
                self.pen.set_semantic_type(SemanticType::Input);
                self.shell_commands.reported = true;
            }
            OperatingSystemCommand::FinalTermSemanticPrompt(
                FinalTermSemanticPrompt::MarkEndOfPromptAndStartOfInputUntilEndOfLine { .. },
            ) => {
                self.pen.set_semantic_type(SemanticType::Input);
                self.clear_semantic_attribute_on_newline = true;
                self.shell_commands.reported = true;
            }
            OperatingSystemCommand::FinalTermSemanticPrompt(
                FinalTermSemanticPrompt::MarkEndOfInputAndStartOfOutput { .. },
            ) => {
                self.pen.set_semantic_type(SemanticType::Output);
                self.shell_commands.reported = true;
                self.shell_commands.running = true;
            }

            OperatingSystemCommand::FinalTermSemanticPrompt(
                FinalTermSemanticPrompt::CommandStatus { status, aid },
            ) => {
                if self.counts_shell_mark(aid.as_deref()) {
                    self.finish_shell_command(status);
                }
            }

            OperatingSystemCommand::SystemNotification(message) => {
                if let Some(handler) = self.alert_handler.as_mut() {
                    handler.alert(Alert::ToastNotification {
                        title: None,
                        body: message,
                        focus: true,
                    });
                } else {
                    log::info!("Application sends SystemNotification: {}", message);
                }
            }
            OperatingSystemCommand::RxvtExtension(params) => {
                if let Some("notify") = params.get(0).map(String::as_str) {
                    let title = params.get(1);
                    let body = params.get(2);
                    let (title, body) = match (title.cloned(), body.cloned()) {
                        (Some(title), None) => (None, title),
                        (Some(title), Some(body)) => (Some(title), body),
                        _ => {
                            log::warn!("malformed rxvt notify escape: {:?}", params);
                            return;
                        }
                    };
                    if let Some(handler) = self.alert_handler.as_mut() {
                        handler.alert(Alert::ToastNotification {
                            title,
                            body,
                            focus: true,
                        });
                    }
                }
            }
            OperatingSystemCommand::CurrentWorkingDirectory(url) => {
                self.current_dir = Url::parse(&url).ok();
                if let Some(handler) = self.alert_handler.as_mut() {
                    handler.alert(Alert::CurrentWorkingDirectoryChanged);
                }
            }
            OperatingSystemCommand::ChangeColorNumber(specs) => {
                log::trace!("ChangeColorNumber: {:?}", specs);
                for pair in specs {
                    match pair.color {
                        ColorOrQuery::Query => {
                            let response =
                                OperatingSystemCommand::ChangeColorNumber(vec![ChangeColorPair {
                                    palette_index: pair.palette_index,
                                    color: ColorOrQuery::Color(
                                        self.palette().colors.0[pair.palette_index as usize],
                                    ),
                                }]);
                            write!(self.writer, "{}", response).ok();
                            self.writer.flush().ok();
                        }
                        ColorOrQuery::Color(c) => {
                            self.palette_mut().colors.0[pair.palette_index as usize] = c;
                        }
                    }
                }
                self.implicit_palette_reset_if_same_as_configured();
                self.palette_did_change();
            }

            OperatingSystemCommand::ResetColors(colors) => {
                log::trace!("ResetColors: {:?}", colors);
                if colors.is_empty() {
                    // Reset all colors
                    self.palette.take();
                } else {
                    // Reset individual colors
                    if self.palette.is_none() {
                        // Already at the defaults
                    } else {
                        let base = self.config.color_palette();
                        for c in colors {
                            let c = c as usize;
                            self.palette_mut().colors.0[c] = base.colors.0[c];
                        }
                    }
                }
                self.implicit_palette_reset_if_same_as_configured();
                self.palette_did_change();
            }

            OperatingSystemCommand::ChangeDynamicColors(first_color, colors) => {
                log::trace!("ChangeDynamicColors: {:?} {:?}", first_color, colors);
                use wezterm_escape_parser::osc::DynamicColorNumber;
                let mut idx: u8 = first_color as u8;
                for color in colors {
                    let which_color: Option<DynamicColorNumber> = FromPrimitive::from_u8(idx);
                    log::trace!("ChangeDynamicColors item: {:?}", which_color);
                    if let Some(which_color) = which_color {
                        macro_rules! set_or_query {
                            ($name:ident) => {
                                match color {
                                    ColorOrQuery::Query => {
                                        let response = OperatingSystemCommand::ChangeDynamicColors(
                                            which_color,
                                            vec![ColorOrQuery::Color(self.palette().$name.into())],
                                        );
                                        log::trace!("Color Query response {:?}", response);
                                        write!(self.writer, "{}", response).ok();
                                        self.writer.flush().ok();
                                    }
                                    ColorOrQuery::Color(c) => self.palette_mut().$name = c.into(),
                                }
                            };
                        }
                        match which_color {
                            DynamicColorNumber::TextForegroundColor => set_or_query!(foreground),
                            DynamicColorNumber::TextBackgroundColor => set_or_query!(background),
                            DynamicColorNumber::TextCursorColor => {
                                if let ColorOrQuery::Color(c) = color {
                                    // We set the border to the background color; we don't
                                    // have an escape that sets that independently, and this
                                    // way just looks better.
                                    self.palette_mut().cursor_border = c.into();
                                }
                                set_or_query!(cursor_bg)
                            }
                            DynamicColorNumber::HighlightForegroundColor => {
                                set_or_query!(selection_fg)
                            }
                            DynamicColorNumber::HighlightBackgroundColor => {
                                set_or_query!(selection_bg)
                            }
                            DynamicColorNumber::MouseForegroundColor
                            | DynamicColorNumber::MouseBackgroundColor
                            | DynamicColorNumber::TektronixForegroundColor
                            | DynamicColorNumber::TektronixBackgroundColor
                            | DynamicColorNumber::TektronixCursorColor => {}
                        }
                    }
                    idx += 1;
                }
                self.implicit_palette_reset_if_same_as_configured();
                self.palette_did_change();
            }

            OperatingSystemCommand::ResetDynamicColor(color) => {
                log::trace!("ResetDynamicColor: {:?}", color);
                use wezterm_escape_parser::osc::DynamicColorNumber;
                let which_color: Option<DynamicColorNumber> = FromPrimitive::from_u8(color as u8);
                if let Some(which_color) = which_color {
                    macro_rules! reset {
                        ($name:ident) => {
                            if self.palette.is_none() {
                                // Already at the defaults
                            } else {
                                let base = self.config.color_palette();
                                self.palette_mut().$name = base.$name;
                            }
                        };
                    }
                    match which_color {
                        DynamicColorNumber::TextForegroundColor => reset!(foreground),
                        DynamicColorNumber::TextBackgroundColor => reset!(background),
                        DynamicColorNumber::TextCursorColor => {
                            reset!(cursor_bg);
                            // Since we set the border to the bg, we consider it reset
                            // by resetting the bg too!
                            reset!(cursor_border);
                        }
                        DynamicColorNumber::HighlightForegroundColor => reset!(selection_fg),
                        DynamicColorNumber::HighlightBackgroundColor => reset!(selection_bg),
                        DynamicColorNumber::MouseForegroundColor
                        | DynamicColorNumber::MouseBackgroundColor
                        | DynamicColorNumber::TektronixForegroundColor
                        | DynamicColorNumber::TektronixBackgroundColor
                        | DynamicColorNumber::TektronixCursorColor => {}
                    }
                }
                self.implicit_palette_reset_if_same_as_configured();
                self.palette_did_change();
            }
            OperatingSystemCommand::ProgramStatus(ProgramStatus::Query) => {
                // The same body back is the whole answer: nothing about the
                // records is ever echoed.
                write!(
                    self.writer,
                    "{}",
                    OperatingSystemCommand::ProgramStatus(ProgramStatus::Query)
                )
                .ok();
                self.writer.flush().ok();
            }
            OperatingSystemCommand::ProgramStatus(ProgramStatus::Report(report)) => {
                let group = self
                    .program_status_process_group
                    .as_ref()
                    .and_then(|source| source());
                self.program_status.apply_from(report, group);
            }
            OperatingSystemCommand::ConEmuProgress(prog) => {
                use wezterm_escape_parser::osc::Progress as TProg;
                // Retained before the display flattening below (which folds
                // Paused into None) and before the no-change early-out: a
                // repeated identical payload is still evidence that the app
                // is speaking.
                self.agent_osc_progress = Some(match &prog {
                    TProg::None => "4;0".to_string(),
                    TProg::SetPercentage(p) => format!("4;1;{p}"),
                    TProg::SetError(p) => format!("4;2;{p}"),
                    TProg::SetIndeterminate => "4;3".to_string(),
                    TProg::Paused => "4;4".to_string(),
                });
                let prog = match prog {
                    TProg::None => Progress::None,
                    TProg::SetPercentage(p) => Progress::Percentage(p),
                    TProg::SetError(p) => Progress::Error(p),
                    TProg::SetIndeterminate => Progress::Indeterminate,
                    TProg::Paused => Progress::None,
                };
                if prog != self.progress {
                    self.progress = prog.clone();
                    if let Some(handler) = self.alert_handler.as_mut() {
                        handler.alert(Alert::Progress(prog));
                    }
                }
            }
        }
    }
}

fn selection_to_selection(sel: Selection) -> ClipboardSelection {
    match sel {
        Selection::CLIPBOARD => ClipboardSelection::Clipboard,
        Selection::PRIMARY => ClipboardSelection::PrimarySelection,
        // xterm will use a configurable selection in the NONE case
        Selection::NONE => ClipboardSelection::Clipboard,
        // otherwise we just use clipboard.  Could potentially
        // also use the same fallback configuration as NONE,
        // if/when we add it
        _ => ClipboardSelection::Clipboard,
    }
}

/// How far the grapheme goes that starts with the plain character of `len`
/// bytes at the start of `bytes`, and the plain character after it if there
/// is one; None if it takes the general path to tell. The diacritics a letter
/// can have on it are the one thing allowed to follow it, these being how
/// macOS spells accented file names.
#[inline(always)]
fn plain_grapheme(
    plain: &PlainWidths,
    bytes: &[u8],
    len: usize,
) -> Option<(usize, Option<(usize, usize)>)> {
    let rest = &bytes[len..];
    if rest.is_empty() {
        return Some((len, None));
    }
    if let Some(next) = plain.first(rest) {
        return Some((len, Some(next)));
    }
    let marks = if len == 1 {
        wezterm_cell::diacritics_len(rest)
    } else {
        0
    };
    if marks == 0 {
        return None;
    }
    let rest = &rest[marks..];
    if rest.is_empty() {
        return Some((len + marks, None));
    }
    plain.first(rest).map(|next| (len + marks, Some(next)))
}

/// How long text has to be for `print_plain` to have a go at it.
const PLAIN_TEXT_MIN_LEN: usize = 12;

/// Whether `bytes` starts with enough plain graphemes to be worth the end of
/// a run of graphemes.
fn plain_lasts(plain: &PlainWidths, mut bytes: &[u8]) -> bool {
    for _ in 0..4 {
        let Some((len, _)) = plain.first(bytes) else {
            return false;
        };
        let Some((len, _)) = plain_grapheme(plain, bytes, len) else {
            return false;
        };
        bytes = &bytes[len..];
    }
    true
}

/// Graphemes waiting to be appended to one row in a single write.
struct GraphemeRun {
    x: usize,
    y: VisibleRowIndex,
    /// Byte range of the run within the text being printed.
    start: usize,
    end: usize,
    cells: usize,
    count: usize,
    /// Byte length of the longest grapheme.
    max_len: usize,
    /// Cell offsets from `x` of the graphemes wider than one cell.
    wide: [u16; GraphemeRun::MAX],
    num_wide: usize,
    last_width: u8,
    /// Where in the text the row was last written up to. Text that goes on
    /// from there has already been cut at a grapheme boundary.
    written_to: Option<usize>,
}

impl Default for GraphemeRun {
    fn default() -> Self {
        Self {
            x: 0,
            y: 0,
            start: 0,
            end: 0,
            cells: 0,
            count: 0,
            max_len: 0,
            wide: [0; GraphemeRun::MAX],
            num_wide: 0,
            last_width: 0,
            written_to: None,
        }
    }
}

impl GraphemeRun {
    const MAX: usize = 64;

    /// Whether a grapheme at `x, y` starting at byte `start` extends the run.
    fn continues(&self, x: usize, y: VisibleRowIndex, start: usize) -> bool {
        self.count > 0
            && self.count < Self::MAX
            && self.y == y
            && self.x + self.cells == x
            && self.end == start
    }

    fn push(&mut self, x: usize, y: VisibleRowIndex, start: usize, end: usize, width: usize) {
        if self.count == 0 {
            self.x = x;
            self.y = y;
            self.start = start;
            self.cells = 0;
            self.max_len = 0;
            self.num_wide = 0;
        }
        if width > 1 {
            self.wide[self.num_wide] = self.cells as u16;
            self.num_wide += 1;
        }
        self.max_len = self.max_len.max(end - start);
        self.cells += width;
        self.end = end;
        self.count += 1;
        self.last_width = width as u8;
    }
}

#[cfg(all(test, feature = "use_serde"))]
mod ascii_tests {
    use super::*;
    use crate::{TerminalConfiguration, TerminalSize};
    use std::sync::Arc;
    use wezterm_cell::UnicodeVersion;
    use wezterm_escape_parser::parser::Parser;

    #[derive(Debug)]
    struct TestConfig {
        normalize: bool,
        custom_widths: bool,
    }
    impl TerminalConfiguration for TestConfig {
        fn scrollback_size(&self) -> usize {
            4
        }
        fn color_palette(&self) -> crate::color::ColorPalette {
            Default::default()
        }
        fn normalize_output_to_unicode_nfc(&self) -> bool {
            self.normalize
        }
        fn unicode_version(&self) -> UnicodeVersion {
            let mut version = UnicodeVersion::new(9);
            if self.custom_widths {
                version.cell_widths = Some(Arc::new(
                    [('a' as u32, 2), ('z' as u32, 0)].iter().copied().collect(),
                ));
            }
            version
        }
    }

    #[test]
    fn ascii_batch_matches_grapheme_path_across_modes_and_chunk_boundaries() {
        for (rows, cols) in [(1, 1), (1, 7), (3, 7)] {
            for setup in [
                "",
                "a界",
                "\x1b[?7l",
                "\x1b[4h",
                "\x1b(0",
                "\x1b(A",
                "\x1b)0\x0e",
                "\x1b)A\x0e",
                "\x1b[?1049h",
                "\x1b[44m",
                "\x1b[2;3r\x1b[3;6H",
                "\x1b[?69h\x1b[2;6s\x1b[3;7H",
                "\x1b]8;;https://example.com\x1b\\",
                "\x1b]133;A\x07",
            ] {
                for (normalize, custom_widths, conpty) in [
                    (false, false, false),
                    (true, false, false),
                    (false, true, false),
                    (false, false, true),
                ] {
                    for chunk_size in [1, 3, 17, 4096] {
                        let make = || {
                            let mut state = TerminalState::new(
                                TerminalSize {
                                    rows,
                                    cols,
                                    pixel_width: cols * 8,
                                    pixel_height: rows * 16,
                                    dpi: 96,
                                },
                                Arc::new(TestConfig {
                                    normalize,
                                    custom_widths,
                                }),
                                "test",
                                "test",
                                Box::new(Vec::new()),
                            );
                            state.enable_conpty_quirks = conpty;
                            state
                        };
                        let mut expected = make();
                        let mut actual = make();
                        let mut direct = make();
                        let mut kept = make();
                        let mut parser = Parser::new();
                        let mut direct_parser = Parser::new();
                        let mut kept_parser = Parser::new();
                        kept_parser.set_text_keeps_line_controls(true);
                        let input = format!("{setup}abc #q~0123456789az      next\rOVERWRITE\nmore\te\u{301}1\u{fe0f}\u{20e3}界\x1b[0m\x1bkASCII title界\x1b\\end");
                        for chunk in input.as_bytes().chunks(chunk_size) {
                            let mut actions = Vec::new();
                            parser.parse_with_borrowed_text(chunk, |action| {
                                action.append_to(&mut actions)
                            });
                            let mut kept_actions = Vec::new();
                            kept_parser.parse_with_borrowed_text(chunk, |action| {
                                action.append_to(&mut kept_actions)
                            });
                            actual.increment_seqno();
                            expected.increment_seqno();
                            direct.increment_seqno();
                            kept.increment_seqno();
                            {
                                let mut direct_performer = Performer::new(&mut direct);
                                direct_parser.parse_with_borrowed_text(
                                    chunk,
                                    |event| match event {
                                        wezterm_escape_parser::parser::ParsedAction::Action(
                                            action,
                                        ) => direct_performer.perform(action),
                                        wezterm_escape_parser::parser::ParsedAction::Print(
                                            text,
                                        ) => direct_performer.print_text(text),
                                    },
                                );
                                let mut scalar = Performer::new(&mut expected);
                                scalar.disable_ascii_batch = true;
                                let mut batched = Performer::new(&mut actual);
                                let mut line_controls_kept = Performer::new(&mut kept);
                                for action in kept_actions {
                                    line_controls_kept.perform(action);
                                }
                                for action in actions {
                                    batched.perform(action.clone());
                                    match action {
                                        Action::PrintString(text) => {
                                            for c in text.chars() {
                                                scalar.perform(Action::Print(c));
                                            }
                                        }
                                        other => scalar.perform(other),
                                    }
                                }
                            }
                            let expected_rows = expected.snapshot().with_rows_as_cells();
                            assert_eq!(actual.snapshot().with_rows_as_cells(), expected_rows,
                                "rows={rows}, cols={cols}, setup={setup:?}, normalize={normalize}, custom_widths={custom_widths}, conpty={conpty}, chunk_size={chunk_size}");
                            assert_eq!(direct.snapshot().with_rows_as_cells(), expected_rows,
                                "direct: rows={rows}, cols={cols}, setup={setup:?}, normalize={normalize}, custom_widths={custom_widths}, conpty={conpty}, chunk_size={chunk_size}");
                            assert_eq!(kept.snapshot().with_rows_as_cells(), expected_rows,
                                "kept: rows={rows}, cols={cols}, setup={setup:?}, normalize={normalize}, custom_widths={custom_widths}, conpty={conpty}, chunk_size={chunk_size}");
                        }
                    }
                }
            }
        }
    }

    /// Write `input` with and without grapheme runs, and with the parser as
    /// the mux has it, and compare what each makes of it, whatever each is
    /// stored as.
    fn check_runs(rows: usize, cols: usize, input: &str, chunk_size: usize) {
        let make = || {
            TerminalState::new(
                TerminalSize {
                    rows,
                    cols,
                    pixel_width: cols * 8,
                    pixel_height: rows * 16,
                    dpi: 96,
                },
                Arc::new(TestConfig {
                    normalize: false,
                    custom_widths: false,
                }),
                "test",
                "test",
                Box::new(Vec::new()),
            )
        };
        let mut runs = make();
        let mut single = make();
        let mut parser = Parser::new();
        let mut kept_parser = Parser::new();
        kept_parser.set_text_keeps_line_controls(true);
        for chunk in input.as_bytes().chunks(chunk_size) {
            let mut actions = Vec::new();
            parser.parse_with_borrowed_text(chunk, |action| action.append_to(&mut actions));
            let mut kept_actions = Vec::new();
            kept_parser
                .parse_with_borrowed_text(chunk, |action| action.append_to(&mut kept_actions));
            runs.increment_seqno();
            single.increment_seqno();
            let mut batched = Performer::new(&mut runs);
            for action in kept_actions {
                batched.perform(action);
            }
            drop(batched);
            let mut scalar = Performer::new(&mut single);
            scalar.disable_grapheme_runs = true;
            for action in actions {
                scalar.perform(action);
            }
            drop(scalar);
            // A row of cells keeps the wrap flag of a wide cell in its second
            // half, where the cell doesn't show it.
            let cells = |line: &wezterm_surface::Line| {
                line.visible_cells()
                    .map(|cell| {
                        let mut attrs = cell.attrs().clone();
                        if cell.width() > 1 {
                            attrs.set_wrapped(false);
                        }
                        (
                            cell.cell_index(),
                            cell.str().to_string(),
                            cell.width(),
                            attrs,
                        )
                    })
                    .collect::<Vec<_>>()
            };
            let actual = runs.snapshot();
            let expected = single.snapshot();
            assert_eq!(actual.screen.lines.len(), expected.screen.lines.len());
            for (row, (a, e)) in actual
                .screen
                .lines
                .iter()
                .zip(&expected.screen.lines)
                .enumerate()
            {
                assert_eq!(
                    (a.len(), a.current_seqno(), cells(a)),
                    (e.len(), e.current_seqno(), cells(e)),
                    "row {row}, rows={rows}, cols={cols}, input={input:?}, chunk_size={chunk_size}"
                );
            }
            assert_eq!(runs.cursor_pos(), single.cursor_pos(), "{input:?}");
        }
    }

    #[test]
    fn grapheme_runs_over_erased_rows_match_one_grapheme_at_a_time() {
        let texts = [
            "\u{4e2d}\u{6587}\u{5b57}\u{7b26}\u{4e2d}\u{6587}",
            "abc\u{4e2d}def",
            "\u{1f600}\u{4e2d}\u{6587}\u{1f600} hello",
            "\u{2764}\u{fe0f}\u{4e2d}",
            "e\u{301}xe\u{301}",
            "\u{e01}\u{e33}x",
            "\u{1f1fa}\u{1f1f8}",
        ];
        for (rows, cols) in [(3, 7), (4, 12), (4, 80)] {
            for setup in [
                "",
                "abcdef\x1b[2K",
                "abcdef\x1b[2J\x1b[H",
                "\x1b[2K\x1b[3G",
                "ab\x1b[K\x1b[5G",
                "\u{4e2d}\u{6587}\x1b[2K",
                "abcdef\r\x1b[2K\x1b[32m",
                "\x1b[44m\x1b[2K\x1b[0m",
                "ab\x1b[1b\r\x1b[3C",
            ] {
                for text in texts {
                    for chunk_size in [1, 5, 4096] {
                        let input =
                            format!("{setup}{text}\x1b[31m{text}\x1b[0m\x1b[K{text}\r\n{text}");
                        check_runs(rows, cols, &input, chunk_size);
                    }
                }
            }
        }
    }

    /// `print_plain` starts as soon as what is waiting has been written.
    #[test]
    fn plain_text_follows_what_is_not() {
        for text in [
            "\u{1f600}\u{4e2d}\u{6587}\u{5b57}\u{7b26}\r\n",
            "\u{2764}\u{fe0f}\u{4e2d}\u{6587}\u{5b57}\u{7b26}\r\n",
            "\u{1f600} hello world\r\n",
            "\u{4e2d}\u{1f600}\u{4e2d}\u{1f600}\u{4e2d}\r\n",
        ] {
            for chunk_size in [1, 7, 4096] {
                check_runs(4, 20, &text.repeat(9), chunk_size);
            }
        }
    }

    /// A row a run can't go on keeps it for the rest of the row, plain or not.
    #[test]
    fn plain_text_over_text_matches_one_grapheme_at_a_time() {
        for chunk_size in [1, 4096] {
            check_runs(
                3,
                20,
                "\u{4e2d}\u{6587}abc\x1b[H\u{5b57}\u{7b26}\x1b[H\u{6587}x",
                chunk_size,
            );
        }
    }

    #[test]
    fn a_line_control_after_a_c1_control_is_a_control() {
        for control in ['\n', '\r', '\t'] {
            for tail in ["cd", "\x1b[32mcd"] {
                let input = format!("ab\u{94}{control}{tail}");
                check_runs(4, 20, &input, 4);
                check_runs(4, 20, &input, 4096);
            }
        }
    }

    /// What a row wraps from has blanks in it that were written, whatever
    /// the row is stored as.
    #[test]
    fn blanks_before_a_wrap_survive_a_resize() {
        for input in [
            "abc\x1b[10G next",
            "qqqqq\r\x1b[Kabc\x1b[K\x1b[10G next",
            "xxxxxxxxxx\x1b[2K\x1b[Khello",
        ] {
            let mut term = TerminalState::new(
                TerminalSize {
                    rows: 4,
                    cols: 10,
                    pixel_width: 80,
                    pixel_height: 64,
                    dpi: 96,
                },
                Arc::new(TestConfig {
                    normalize: false,
                    custom_widths: false,
                }),
                "test",
                "test",
                Box::new(Vec::new()),
            );
            let mut parser = Parser::new();
            let mut actions = Vec::new();
            parser.parse_with_borrowed_text(input.as_bytes(), |action| {
                action.append_to(&mut actions)
            });
            term.increment_seqno();
            let mut performer = Performer::new(&mut term);
            for action in actions {
                performer.perform(action);
            }
            drop(performer);
            let (_, lines) = term.screen().lines_in_phys_range(0..2).into_iter().fold(
                (0, Vec::new()),
                |(n, mut lines), line| {
                    lines.push(line);
                    (n + 1, lines)
                },
            );
            assert_eq!(lines[0].len(), 10, "{input:?}");
            assert!(lines[0].last_cell_was_wrapped(), "{:?}", input);
        }
    }

    #[test]
    fn deleting_on_a_row_of_cells_written_apart_does_not_panic() {
        for cols in [10, 11] {
            for input in [
                "abbbb\u{e01}\x1b[1m\u{e33}xy\x1b[P",
                "ab\x1b[3b\u{e01}\x1b[1m\u{e33}xy\x1b[P",
                "ab\x1b[3b\u{1f1fa}\x1b[1m\u{1f1f8}\x1b[P",
            ] {
                check_runs(3, cols, input, 4096);
                check_runs(3, cols, input, 1);
            }
        }
    }

    #[test]
    fn grapheme_runs_match_one_grapheme_at_a_time() {
        let long = format!("{}e\u{301}{}", "界".repeat(45), "字".repeat(40));
        // Graphemes longer than a new row's text capacity: runs holding
        // them are refused and written one at a time.
        let marked = format!("ab界e{}界x{}y", "\u{301}".repeat(45), "\u{301}".repeat(150));
        let texts = [
            "中文，全角！ＡＢＣ测试".to_string(),
            "ab界e\u{301}x\u{200b}y\u{2028}z\u{3000} 🙂👍🏽 1\u{fe0f}\u{20e3}é".to_string(),
            long,
            marked,
        ];
        for (rows, cols) in [(1, 1), (1, 2), (3, 7), (4, 23)] {
            for setup in [
                "",
                "a界",
                "\x1b[?7l",
                "\x1b[4h",
                "\x1b(0",
                "\x1b)0\x0e",
                "\x1b[?1049h",
                "\x1b[44m",
                "\x1b[2;3r\x1b[3;6H",
                "\x1b[?69h\x1b[2;6s\x1b[3;7H",
                "\x1b]8;;https://example.com\x1b\\",
                "\x1b[3G",
                "\x1b[1;2H界界",
            ] {
                for custom_widths in [false, true] {
                    for text in &texts {
                        for chunk_size in [1, 5, 4096] {
                            let make = || {
                                TerminalState::new(
                                    TerminalSize {
                                        rows,
                                        cols,
                                        pixel_width: cols * 8,
                                        pixel_height: rows * 16,
                                        dpi: 96,
                                    },
                                    Arc::new(TestConfig {
                                        normalize: false,
                                        custom_widths,
                                    }),
                                    "test",
                                    "test",
                                    Box::new(Vec::new()),
                                )
                            };
                            let mut runs = make();
                            let mut single = make();
                            let mut parser = Parser::new();
                            let input = format!("{setup}{text}\r\n{text}\x1b[A\x1b[3G{text}end");
                            for chunk in input.as_bytes().chunks(chunk_size) {
                                let mut actions = Vec::new();
                                parser.parse_with_borrowed_text(chunk, |action| {
                                    action.append_to(&mut actions)
                                });
                                runs.increment_seqno();
                                single.increment_seqno();
                                let mut batched = Performer::new(&mut runs);
                                let mut scalar = Performer::new(&mut single);
                                scalar.disable_grapheme_runs = true;
                                for action in actions {
                                    batched.perform(action.clone());
                                    scalar.perform(action);
                                }
                                drop(batched);
                                drop(scalar);
                                assert_eq!(runs.snapshot(), single.snapshot(),
                                    "rows={rows}, cols={cols}, setup={setup:?}, custom_widths={custom_widths}, text={text:?}, chunk_size={chunk_size}");
                                assert_eq!(runs.screen().all_lines(), single.screen().all_lines(),
                                    "rows={rows}, cols={cols}, setup={setup:?}, custom_widths={custom_widths}, text={text:?}, chunk_size={chunk_size}");
                            }
                        }
                    }
                }
            }
        }
    }
}
