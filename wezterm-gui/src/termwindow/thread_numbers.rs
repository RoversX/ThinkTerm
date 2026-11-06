//! The numbers on the sidebar's threads: hold the keys that jump to them and
//! the first rows each show the chord that reaches them.
//!
//! Those keys are the ones most shortcuts start with, so nothing shows until
//! a hold has lasted, and anything else pressed on the way ends it until the
//! keys come up: a chord of ours never flashes the numbers. A number pressed
//! once they are up leaves them up, for the next one. The system's own chords
//! are another matter: the window hears nothing of Cmd-Tab, so lingering in
//! the app switcher brings them up until the switch takes the keyboard away.
//!
//! Which keys is the keymap's business. What has to be held is the modifiers
//! of the first number bound, and a row reads what is bound to it, so a
//! shortcut changed in Settings changes what is held and what shows.

use crate::quad::TripleLayerQuadAllocator;
use crate::termwindow::render::split_slide::MIN_FRAMES;
use crate::termwindow::sidebar_hover::HoverFrame;
use crate::termwindow::ui::icons::SvgIcon;
use crate::termwindow::ui::sidebar::over;
use crate::termwindow::ui::tokens::{CAPSULE_BORDER_WIDTH, SIDEBAR_TITLE_FADE_GAP};
use crate::termwindow::TermWindow;
use crate::ui::anim::{self, Easing, Timeline};
use crate::ui::draw::TextFade;
use crate::ui::UiPalette;
use crate::utilsprites::RenderMetrics;
use crate::workspace_threads::{WorkspaceThreadId, WorkspaceThreadsView};
use anyhow::Context;
use config::keyassignment::KeyAssignment;
use mux::Mux;
use std::rc::Rc;
use std::time::{Duration, Instant};
use wezterm_font::LoadedFont;
use window::color::LinearRgba;
use window::{Modifiers, PhysKeyCode, RawKeyEvent, UIKeyCapRendering, Window, WindowOps};

/// How many rows have a number: the digits, 1 to 9.
pub(crate) const MOST_NUMBERS: usize = 9;

/// How long the keys have to stay down before the numbers come.
const HOLD: Duration = Duration::from_millis(500);
/// How long all of them take to come, row by row...
const ARRIVAL: Duration = anim::STANDARD;
/// ...and to go, which nothing should be kept waiting for.
const DEPARTURE: Duration = anim::MICRO;
/// How far through the arrival each row sets off after the one above it.
const STAGGER: f32 = 0.05;
/// How soon a number coming again is its key still held down: past the wait
/// before a key starts to repeat. Nothing is lost to a deliberate second
/// press this soon, which would find the thread already open.
const KEY_STILL_DOWN: Duration = Duration::from_millis(700);

/// A number's text, against the sidebar's own.
const FONT_SCALE: f64 = 0.8;
const FONT_WEIGHT: u16 = 500;
/// The Command key's symbol is drawn as an icon, this much of the text's
/// line tall: the symbol font's stands a third over the digits and runs into
/// them.
const COMMAND_SCALE: f32 = 0.85;
/// The room between it and what follows it.
const COMMAND_GAP: usize = 2;
const PAD_X: usize = 10;
const PAD_Y: usize = 4;
const RADIUS: f32 = 12.0;
/// How far short of its place, towards the row's end, a number sets off.
const TRAVEL: f32 = 12.0;
/// How far a long name takes to go out in front of a number...
const NAME_FADE: f32 = 56.0;
/// ...and the room it leaves there.
const NAME_FADE_GAP: f32 = 4.0;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum Hold {
    #[default]
    Up,
    /// Down since then, and not for long enough yet.
    Counting(Instant),
    /// Down for long enough: the numbers are up.
    Held,
    /// Down, and on the way to something else, until they come up.
    Spent,
}

#[derive(Debug, Default)]
pub(crate) struct ThreadNumbers {
    hold: Hold,
    /// How far through their arrival the numbers are, 0 to 1, for as long
    /// as any of them is on screen.
    reveal: Option<Timeline>,
    /// The number that came last, and when.
    last_number: Option<(usize, Instant)>,
}

impl ThreadNumbers {
    /// The keys are down, or they are not. True when a frame has to follow.
    fn keys(&mut self, down: bool, now: Instant) -> bool {
        match (down, self.hold) {
            (true, Hold::Up) => {
                self.hold = Hold::Counting(now);
                true
            }
            (true, _) | (false, Hold::Up) => false,
            (false, _) => {
                self.hold = Hold::Up;
                self.hide(now);
                true
            }
        }
    }

    /// The modifiers now down are `down`, and `hold` the ones that bring the
    /// numbers, when they may come. One beyond those is a chord on its way,
    /// whichever key of it came first and however they go on to come up; one
    /// of a `hold` of two may just be the first of them. True when a frame
    /// has to follow.
    fn modifiers(&mut self, down: Modifiers, hold: Option<Modifiers>, now: Instant) -> bool {
        if down == Modifiers::NONE {
            self.keys(false, now)
        } else if Some(down) == hold {
            self.keys(true, now)
        } else if self.hold == Hold::Up && hold.is_some_and(|hold| !hold.contains(down)) {
            self.hold = Hold::Spent;
            false
        } else {
            self.interrupt(false, now)
        }
    }

    /// Number `index` came. False when that is its key still held down,
    /// coming round again: a jump can move the rows, and the same number
    /// would then be a different thread.
    fn number_pressed(&mut self, index: usize, now: Instant) -> bool {
        let again = self.last_number.is_some_and(|(last, at)| {
            last == index && now.saturating_duration_since(at) < KEY_STILL_DOWN
        });
        self.last_number = Some((index, now));
        !again
    }

    /// Something other than the keys was pressed, a `number` or not. True
    /// when a frame has to follow.
    fn interrupt(&mut self, number: bool, now: Instant) -> bool {
        match self.hold {
            // Whoever presses a number this soon knows it already.
            Hold::Counting(_) => {
                self.hold = Hold::Spent;
                false
            }
            Hold::Held if !number => {
                self.hold = Hold::Spent;
                self.hide(now);
                true
            }
            _ => false,
        }
    }

    /// No fade: a window that has lost the keyboard hears nothing of them
    /// coming up, and gets few frames to finish one in.
    fn cancel(&mut self) {
        *self = Self::default();
    }

    fn show(&mut self, now: Instant) {
        match self.reveal.as_mut() {
            Some(reveal) => reveal.retarget(now, 1.0, ARRIVAL, Easing::Linear),
            None => self.reveal = Some(Timeline::new(now, 0.0, 1.0, ARRIVAL, Easing::Linear)),
        }
    }

    fn hide(&mut self, now: Instant) {
        if let Some(reveal) = self.reveal.as_mut() {
            if reveal.target() != 0.0 {
                reveal.retarget(now, 0.0, DEPARTURE, Easing::Linear);
            }
        }
    }

    /// Move on to the frame at `now`.
    fn step(&mut self, now: Instant) -> HoverFrame {
        let mut counting = None;
        if let Hold::Counting(since) = self.hold {
            let due = since + HOLD;
            if now >= due {
                self.hold = Hold::Held;
                self.show(now);
            } else {
                counting = Some(due);
            }
        }
        if let Some(reveal) = self.reveal.as_mut() {
            reveal.pace(now, ARRIVAL / MIN_FRAMES);
            if reveal.advance(now) {
                return HoverFrame::Now;
            }
            if reveal.target() == 0.0 {
                self.reveal = None;
            }
        }
        counting.map_or(HoverFrame::None, HoverFrame::At)
    }

    /// How far through their arrival the numbers are, when they are up at
    /// all.
    fn out(&self, now: Instant) -> Option<f32> {
        self.reveal.as_ref().map(|reveal| reveal.value(now))
    }
}

/// How far out the number of row `index` is, 0 to 1, with all of them `out`
/// of the way: a wave down the rows, which going runs back up.
fn row_progress(out: f32, index: usize) -> f32 {
    let start = index as f32 * STAGGER;
    let span = 1.0 - (MOST_NUMBERS - 1) as f32 * STAGGER;
    Easing::OutCubic.apply((out - start) / span)
}

fn is_number_key(key: &RawKeyEvent) -> bool {
    use PhysKeyCode::*;
    matches!(
        key.phys_code,
        Some(K1 | K2 | K3 | K4 | K5 | K6 | K7 | K8 | K9)
    )
}

/// One row's number, as far out as this frame has it.
pub(crate) struct ThreadNumber {
    thread_id: WorkspaceThreadId,
    /// Whether it opens with the Command key's symbol, which `label` then
    /// leaves out.
    command: bool,
    label: String,
    text_width: usize,
    progress: f32,
    font: Rc<LoadedFont>,
    metrics: RenderMetrics,
}

impl ThreadNumber {
    /// How much of the row's end icons is left, as the number takes over.
    pub(crate) fn icons_left(&self) -> f32 {
        1.0 - self.progress
    }
}

/// The numbers one paint of the sidebar shows.
pub(crate) struct ThreadNumberRows(Vec<ThreadNumber>);

impl ThreadNumberRows {
    pub(crate) fn of(&self, thread_id: &str) -> Option<&ThreadNumber> {
        self.0.iter().find(|number| number.thread_id == thread_id)
    }
}

impl TermWindow {
    /// The modifiers the window reports as held now.
    pub(crate) fn note_thread_number_modifiers(&mut self, modifiers: Modifiers) {
        let hold = self
            .thread_numbers_can_show()
            .then(|| self.thread_number_modifiers())
            .flatten();
        let down = modifiers.remove_positional_mods();
        if self.thread_numbers.modifiers(down, hold, Instant::now()) {
            self.invalidate_window();
        }
    }

    /// A key that is not a modifier went down.
    pub(crate) fn note_key_for_thread_numbers(&mut self, key: &RawKeyEvent) {
        self.interrupt_thread_numbers(is_number_key(key));
    }

    /// Something other than the keys was pressed or done, a `number` or not.
    /// Mac menus take their chords before any key event, so an action is as
    /// good as a key here.
    pub(crate) fn interrupt_thread_numbers(&mut self, number: bool) {
        if self.thread_numbers.interrupt(number, Instant::now()) {
            self.invalidate_window();
        }
    }

    /// The window lost the keyboard.
    pub(crate) fn cancel_thread_numbers(&mut self) {
        self.thread_numbers.cancel();
    }

    pub(crate) fn advance_thread_numbers(&mut self, now: Instant) {
        match self.thread_numbers.step(now) {
            HoverFrame::None => {}
            HoverFrame::Now => self.invalidate_window(),
            HoverFrame::At(due) => self.update_next_frame_time(Some(due)),
        }
    }

    /// Nothing is holding the keyboard for itself. Mac menus take the number
    /// keys ahead of whatever is, so that has to be asked here too.
    fn thread_keys_free(&self) -> bool {
        self.command_palette.is_none()
            && !self.menu_or_overlay_open()
            && !self.recording_overlay.owns_keyboard()
            && !self.share_panel_open()
            && !self.library_open()
    }

    /// The window has the keyboard to itself, and the sidebar is there to
    /// show them on.
    fn thread_numbers_can_show(&self) -> bool {
        self.focused.is_some() && self.workspace_sidebar_is_presented() && self.thread_keys_free()
    }

    /// The modifiers of the first number bound to anything.
    fn thread_number_modifiers(&self) -> Option<Modifiers> {
        (0..MOST_NUMBERS)
            .find_map(|index| {
                self.native_chord(&KeyAssignment::ActivateWorkspaceThreadByIndex(index))
            })
            .map(|(_, mods)| mods.remove_positional_mods())
    }

    /// What number `index` reads: whether it opens with the Command key,
    /// and the rest of its chord, set tight, as the Mac writes them. Spelled
    /// out in words a chord would take half the row, and it would say nine
    /// times over what is being held, so elsewhere it is the key alone.
    fn thread_number_label(&self, index: usize) -> Option<(bool, String)> {
        let (key, mods) =
            self.native_chord(&KeyAssignment::ActivateWorkspaceThreadByIndex(index))?;
        let rendering = self.config.ui_key_cap_rendering;
        if rendering != UIKeyCapRendering::AppleSymbols {
            return Some((false, crate::inputmap::ui_key(&key, rendering)));
        }
        let rest = mods - Modifiers::SUPER;
        Some((
            mods.contains(Modifiers::SUPER),
            crate::inputmap::tight_chord_label(&key, rest, rendering),
        ))
    }

    /// The numbers to show on the rows of `view` in this frame, when any
    /// are up.
    pub(crate) fn thread_number_rows(
        &self,
        view: &WorkspaceThreadsView,
    ) -> anyhow::Result<Option<ThreadNumberRows>> {
        let Some(out) = self
            .thread_numbers
            .out(Instant::now())
            .filter(|_| self.thread_numbers_can_show())
        else {
            return Ok(None);
        };
        let font = self
            .fonts
            .title_font_with_size_and_weight(
                crate::native_settings::sidebar_font_size(&crate::native_settings::load_shared())
                    * FONT_SCALE,
                FONT_WEIGHT,
            )
            .context("sidebar thread number font")?;
        let metrics = RenderMetrics::with_font_metrics(&font.metrics());
        let mut rows = Vec::new();
        for (index, thread) in view.first_threads(MOST_NUMBERS).into_iter().enumerate() {
            let Some((command, label)) = self.thread_number_label(index) else {
                continue;
            };
            let text_width = self.sidebar_text_width(&font, &label)?.ceil() as usize;
            rows.push(ThreadNumber {
                thread_id: thread.id.clone(),
                command,
                label,
                text_width,
                progress: row_progress(out, index),
                font: Rc::clone(&font),
                metrics,
            });
        }
        Ok(Some(ThreadNumberRows(rows)))
    }

    /// How tall the Command key's symbol stands in `number`.
    fn thread_number_command_size(&self, number: &ThreadNumber) -> usize {
        (number.metrics.cell_size.height as f32 * COMMAND_SCALE).round() as usize
    }

    /// How wide `number` is, padding and all.
    fn thread_number_width(&self, number: &ThreadNumber) -> usize {
        let command = if number.command {
            self.thread_number_command_size(number) + self.ui_px(COMMAND_GAP)
        } else {
            0
        };
        command + number.text_width + self.ui_px(PAD_X) * 2
    }

    /// Where a row's number starts in this frame, when it ends at `right`
    /// once it has landed.
    fn thread_number_left(&self, number: &ThreadNumber, right: usize) -> usize {
        let width = self.thread_number_width(number);
        let shift = (self.ui_f32(TRAVEL) * (1.0 - number.progress)).round() as usize;
        right.saturating_sub(width) + shift
    }

    /// What a row's name does in front of its number: it thins out to
    /// nothing short of it, so a long name ends softly and not at an edge,
    /// and it travels with the number, coming and going as the number does.
    fn thread_number_fade(&self, number: &ThreadNumber, right: usize) -> TextFade {
        let to = self.thread_number_left(number, right) as f32 - self.ui_f32(NAME_FADE_GAP);
        TextFade {
            from: to - self.ui_f32(NAME_FADE),
            to,
            amount: number.progress,
        }
    }

    /// A thread row's `name`, and its number when it has one, ending at
    /// `row_end`.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn paint_thread_row_name(
        &self,
        layers: &mut TripleLayerQuadAllocator,
        font: &Rc<LoadedFont>,
        metrics: RenderMetrics,
        name: &str,
        x: usize,
        text_y: usize,
        width: usize,
        color: LinearRgba,
        number: Option<&ThreadNumber>,
        row_end: usize,
        row_y: usize,
        row_height: usize,
        current: bool,
        chrome: &UiPalette,
    ) -> anyhow::Result<()> {
        self.paint_title_fading_with(
            layers,
            font,
            &metrics,
            name,
            x,
            text_y,
            width,
            color,
            SIDEBAR_TITLE_FADE_GAP,
            |own| {
                let Some(number) = number else {
                    return own;
                };
                let fade = self.thread_number_fade(number, row_end);
                // A name already going out goes out in front of the number
                // instead, by the time it has landed.
                Some(own.map_or(fade, |own| own.towards(fade, number.progress)))
            },
        )?;
        let Some(number) = number else {
            return Ok(());
        };
        self.paint_thread_number(layers, number, row_end, row_y, row_height, current, chrome)
    }

    /// A row's number, ending at `right`. It is solid, on the sidebar's own
    /// colour whatever shows through the sidebar, so as it fades in it takes
    /// the place of the row's mark and nothing on the row has to move.
    #[allow(clippy::too_many_arguments)]
    fn paint_thread_number(
        &self,
        layers: &mut TripleLayerQuadAllocator,
        number: &ThreadNumber,
        right: usize,
        y: usize,
        row_height: usize,
        current: bool,
        chrome: &UiPalette,
    ) -> anyhow::Result<()> {
        let progress = number.progress;
        let cell_height = number.metrics.cell_size.height as usize;
        let pad_x = self.ui_px(PAD_X);
        let width = self.thread_number_width(number);
        let height = (cell_height + self.ui_px(PAD_Y) * 2).min(row_height);
        let x = self.thread_number_left(number, right);
        let top = y + row_height.saturating_sub(height) / 2;
        self.fill_rounded_rectangle_with_border(
            layers,
            2,
            euclid::rect(x as f32, top as f32, width as f32, height as f32),
            over(chrome.sidebar_row_active_bg, chrome.workspace_sidebar_bg).mul_alpha(progress),
            chrome.sidebar_row_active_border.mul_alpha(progress),
            self.ui_f32(RADIUS),
            CAPSULE_BORDER_WIDTH,
        )
        .context("sidebar thread number")?;
        let color = if current {
            chrome.text
        } else {
            chrome.secondary_text
        }
        .mul_alpha(progress);
        let mut text_x = x + pad_x;
        if number.command {
            let size = self.thread_number_command_size(number);
            let icon_y = top + height.saturating_sub(size) / 2;
            self.paint_sidebar_icon(layers, SvgIcon::Command, text_x, icon_y, size, color)?;
            text_x += size + self.ui_px(COMMAND_GAP);
        }
        self.paint_sidebar_text(
            layers,
            &number.font,
            number.metrics,
            &number.label,
            text_x,
            top + height.saturating_sub(cell_height) / 2,
            number.text_width + pad_x,
            color,
        )
    }

    /// Open the thread number `index` reaches, as a click on its row does,
    /// unless something else has the keyboard: with the Library up, say, its
    /// list is not there to count down.
    pub(crate) fn activate_numbered_thread(&mut self, index: usize, window: &Window) {
        if !self.thread_keys_free() || !self.thread_numbers.number_pressed(index, Instant::now()) {
            return;
        }
        let mux = Mux::get();
        let active_workspace = self
            .current_mux_workspace()
            .unwrap_or_else(|| mux.active_workspace());
        let workspaces = mux.iter_workspaces();
        let view = self.workspace_sidebar_view(&active_workspace, &workspaces);
        let threads = view.first_threads(index + 1);
        let Some(thread_id) = threads.get(index).map(|thread| thread.id.clone()) else {
            return;
        };
        if !self.open_remote_workspace_thread_without_connecting(&thread_id, window) {
            self.activate_workspace_thread(thread_id, window);
        }
        window.invalidate();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(start: Instant, ms: u64) -> Instant {
        start + Duration::from_millis(ms)
    }

    /// A frame every 10ms from `from` to `to`, each one painted.
    fn frames(numbers: &mut ThreadNumbers, start: Instant, from: u64, to: u64) {
        for ms in (from..=to).step_by(10) {
            numbers.step(at(start, ms));
            numbers.out(at(start, ms));
        }
    }

    /// Up for good, `at` 1000ms past `start`.
    fn held(start: Instant) -> ThreadNumbers {
        let mut numbers = ThreadNumbers::default();
        numbers.keys(true, start);
        frames(&mut numbers, start, 500, 1000);
        assert_eq!(numbers.out(at(start, 1000)), Some(1.0));
        numbers
    }

    #[test]
    fn a_chord_that_lets_go_of_its_extra_key_first_stays_spent() {
        let start = Instant::now();
        let hold = Some(Modifiers::SUPER);
        let mut numbers = ThreadNumbers::default();
        numbers.modifiers(Modifiers::SUPER, hold, start);
        numbers.modifiers(Modifiers::SUPER | Modifiers::SHIFT, hold, at(start, 50));
        numbers.modifiers(Modifiers::SUPER, hold, at(start, 100));
        frames(&mut numbers, start, 100, 1000);
        assert_eq!(numbers.out(at(start, 1000)), None);
        numbers.modifiers(Modifiers::NONE, hold, at(start, 1010));
        numbers.modifiers(Modifiers::SUPER, hold, at(start, 1020));
        assert!(matches!(numbers.hold, Hold::Counting(_)));
    }

    #[test]
    fn a_chord_that_starts_with_its_extra_key_is_spent_from_the_start() {
        let start = Instant::now();
        let hold = Some(Modifiers::SUPER);
        let mut numbers = ThreadNumbers::default();
        numbers.modifiers(Modifiers::SHIFT, hold, start);
        numbers.modifiers(Modifiers::SHIFT | Modifiers::SUPER, hold, at(start, 50));
        numbers.modifiers(Modifiers::SUPER, hold, at(start, 100));
        frames(&mut numbers, start, 100, 1000);
        assert_eq!(numbers.out(at(start, 1000)), None);
        numbers.modifiers(Modifiers::NONE, hold, at(start, 1010));
        numbers.modifiers(Modifiers::SUPER, hold, at(start, 1020));
        assert!(matches!(numbers.hold, Hold::Counting(_)));
    }

    #[test]
    fn the_keys_of_a_hold_of_two_can_come_one_at_a_time() {
        let start = Instant::now();
        let hold = Some(Modifiers::CTRL | Modifiers::SHIFT);
        let mut numbers = ThreadNumbers::default();
        numbers.modifiers(Modifiers::CTRL, hold, start);
        assert_eq!(numbers.hold, Hold::Up);
        numbers.modifiers(Modifiers::CTRL | Modifiers::SHIFT, hold, at(start, 50));
        assert!(matches!(numbers.hold, Hold::Counting(_)));
    }

    #[test]
    fn a_number_key_held_down_counts_once() {
        let start = Instant::now();
        let mut numbers = ThreadNumbers::default();
        assert!(numbers.number_pressed(1, start));
        // The wait before it starts to repeat, and the repeats that follow.
        assert!(!numbers.number_pressed(1, at(start, 500)));
        assert!(!numbers.number_pressed(1, at(start, 530)));
        assert!(!numbers.number_pressed(1, at(start, 1100)));
        // Let go, and pressed again.
        assert!(numbers.number_pressed(1, at(start, 2000)));
    }

    #[test]
    fn another_number_is_never_the_last_one_coming_round() {
        let start = Instant::now();
        let mut numbers = ThreadNumbers::default();
        assert!(numbers.number_pressed(1, start));
        assert!(numbers.number_pressed(2, at(start, 50)));
        assert!(numbers.number_pressed(1, at(start, 100)));
    }

    #[test]
    fn keys_that_may_not_bring_them_leave_them_away() {
        let start = Instant::now();
        let mut numbers = ThreadNumbers::default();
        numbers.modifiers(Modifiers::SUPER, None, start);
        frames(&mut numbers, start, 0, 1000);
        assert_eq!(numbers.out(at(start, 1000)), None);
        assert_eq!(numbers.hold, Hold::Up);
    }

    #[test]
    fn a_short_press_shows_nothing() {
        let start = Instant::now();
        let mut numbers = ThreadNumbers::default();
        assert!(numbers.keys(true, start));
        assert_eq!(numbers.step(at(start, 100)), HoverFrame::At(at(start, 500)));
        assert_eq!(numbers.out(at(start, 100)), None);
        assert!(numbers.keys(false, at(start, 200)));
        assert_eq!(numbers.step(at(start, 500)), HoverFrame::None);
        assert_eq!(numbers.out(at(start, 500)), None);
    }

    #[test]
    fn a_hold_brings_them_up_and_then_asks_for_nothing() {
        let start = Instant::now();
        let mut numbers = ThreadNumbers::default();
        numbers.keys(true, start);
        assert_eq!(numbers.step(at(start, 500)), HoverFrame::Now);
        // Not a moment of the arrival goes on the frame that first draws it.
        assert_eq!(numbers.out(at(start, 500)), Some(0.0));
        frames(&mut numbers, start, 510, 1000);
        assert_eq!(numbers.out(at(start, 1000)), Some(1.0));
        assert_eq!(numbers.step(at(start, 1010)), HoverFrame::None);
    }

    #[test]
    fn letting_go_takes_them_away() {
        let start = Instant::now();
        let mut numbers = held(start);
        assert!(numbers.keys(false, at(start, 1000)));
        assert_eq!(numbers.step(at(start, 1000)), HoverFrame::Now);
        frames(&mut numbers, start, 1000, 1300);
        assert_eq!(numbers.out(at(start, 1300)), None);
        assert_eq!(numbers.step(at(start, 1310)), HoverFrame::None);
    }

    #[test]
    fn going_again_while_they_leave_waits_out_a_whole_hold() {
        let start = Instant::now();
        let mut numbers = held(start);
        numbers.keys(false, at(start, 1000));
        frames(&mut numbers, start, 1000, 1040);
        assert!(numbers.keys(true, at(start, 1050)));
        frames(&mut numbers, start, 1050, 1300);
        assert_eq!(numbers.out(at(start, 1300)), None);
        frames(&mut numbers, start, 1300, 2000);
        assert_eq!(numbers.out(at(start, 2000)), Some(1.0));
    }

    #[test]
    fn a_chord_on_the_way_shows_nothing_until_the_keys_come_up() {
        let start = Instant::now();
        let mut numbers = ThreadNumbers::default();
        numbers.keys(true, start);
        assert!(!numbers.interrupt(false, at(start, 100)));
        frames(&mut numbers, start, 100, 1000);
        assert_eq!(numbers.out(at(start, 1000)), None);

        // Still down, as far as the window hears of it again.
        assert!(!numbers.keys(true, at(start, 1000)));
        frames(&mut numbers, start, 1000, 2000);
        assert_eq!(numbers.out(at(start, 2000)), None);

        numbers.keys(false, at(start, 2000));
        numbers.keys(true, at(start, 2010));
        frames(&mut numbers, start, 2510, 3000);
        assert_eq!(numbers.out(at(start, 3000)), Some(1.0));
    }

    #[test]
    fn a_number_pressed_before_they_are_up_keeps_them_away() {
        let start = Instant::now();
        let mut numbers = ThreadNumbers::default();
        numbers.keys(true, start);
        numbers.interrupt(true, at(start, 100));
        frames(&mut numbers, start, 100, 1000);
        assert_eq!(numbers.out(at(start, 1000)), None);
    }

    #[test]
    fn a_number_leaves_them_up_and_anything_else_takes_them_away() {
        let start = Instant::now();
        let mut numbers = held(start);
        assert!(!numbers.interrupt(true, at(start, 1000)));
        frames(&mut numbers, start, 1000, 1200);
        assert_eq!(numbers.out(at(start, 1200)), Some(1.0));

        assert!(numbers.interrupt(false, at(start, 1200)));
        frames(&mut numbers, start, 1200, 1500);
        assert_eq!(numbers.out(at(start, 1500)), None);
    }

    #[test]
    fn losing_the_keyboard_leaves_nothing_up_and_nothing_counting() {
        let start = Instant::now();
        let mut numbers = held(start);
        numbers.cancel();
        assert_eq!(numbers.out(at(start, 1000)), None);
        assert_eq!(numbers.step(at(start, 1000)), HoverFrame::None);

        // Keys that come back down are a new hold, not the rest of the old.
        assert!(numbers.keys(true, at(start, 1010)));
        assert_eq!(
            numbers.step(at(start, 1020)),
            HoverFrame::At(at(start, 1510))
        );
    }

    #[test]
    fn the_rows_go_out_in_a_wave_that_lands_together() {
        for index in 0..MOST_NUMBERS {
            assert_eq!(row_progress(0.0, index), 0.0);
            assert!(row_progress(1.0, index) > 0.999);
        }
        let halfway: Vec<f32> = (0..MOST_NUMBERS)
            .map(|index| row_progress(0.5, index))
            .collect();
        assert!(halfway.windows(2).all(|rows| rows[0] > rows[1]));
        assert!(halfway[0] > 0.9);
        assert!(halfway[MOST_NUMBERS - 1] > 0.0);
    }

    #[test]
    fn only_the_digits_are_numbers() {
        let key = |phys_code| RawKeyEvent {
            key: window::KeyCode::Char('x'),
            modifiers: Modifiers::NONE,
            leds: window::KeyboardLedStatus::empty(),
            phys_code,
            raw_code: 0,
            #[cfg(windows)]
            scan_code: 0,
            repeat_count: 1,
            key_is_down: true,
            handled: window::Handled::new(),
        };
        assert!(is_number_key(&key(Some(PhysKeyCode::K1))));
        assert!(is_number_key(&key(Some(PhysKeyCode::K9))));
        assert!(!is_number_key(&key(Some(PhysKeyCode::K0))));
        assert!(!is_number_key(&key(Some(PhysKeyCode::A))));
        assert!(!is_number_key(&key(None)));
    }
}
