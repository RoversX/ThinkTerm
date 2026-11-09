//! Dynamic Status: what is under way or waiting, in the terminal area's
//! bottom-right corner -- the live few of what the Status panel lists.
//!
//! It gathers remote downloads and uploads, programs that wait on the user
//! or report how far they have got (agent detection and OSC 7501), and panes
//! that report their own progress (OSC 9;4). Programs in the panes on screen
//! are left out: their own screen already says it.
//!
//! Folded, it is a black pill naming the most pressing thing, with a row of
//! discs beside it for the next few. Under the pointer the pill grows into
//! that thing's card, the discs run into it, and the cards for the next few
//! rise out of it one after another. Something new arriving grows the pill
//! into its card for a moment, then folds it again. It is black in every
//! theme: the one surface meant to be told apart from whatever is behind it.

use super::transfer_card::{card_content, CardAction, CardView, TransferToastOutcome};
use crate::agent_status::{AgentPaneStatus, AgentState, ProgramReport, ShownState};
use crate::settings_window::UpdateCapsuleStatus;
use crate::termwindow::render::update_capsule::{self, paint_ring};
use crate::termwindow::ui::icons::SvgIcon;
use crate::termwindow::{UIItem, UIItemType};
use crate::ui::anim::{Easing, Timeline};
use crate::ui::draw::DrawContext;
use crate::utilsprites::RenderMetrics;
use anyhow::Context;
use mux::pane::PaneId;
use mux::task_status::{TaskReading, TaskState};
use mux::Mux;
use std::collections::{HashMap, HashSet};
use std::hash::{Hash, Hasher};
use std::rc::Rc;
use std::sync::Arc;
use std::time::{Duration, Instant};
use wezterm_font::LoadedFont;
use wezterm_term::Progress;
use window::color::LinearRgba;

type RectF = euclid::Rect<f32, window::PixelUnit>;

/// Above the chrome and the right sidebar, below content-view flights,
/// tooltips and menus. A layer of its own: two surfaces sharing one draw each
/// other's text through their backgrounds.
const ZINDEX: i8 = 6;

/// Points: the unit everything here is measured in.
const POINT: f32 = 2.0;
const MARGIN: f32 = 10.0;

/// The folded pill.
const PILL_HEIGHT: f32 = 36.0;
const PILL_PAD_LEFT: f32 = 8.0;
const PILL_PAD_RIGHT: f32 = 14.0;
const PILL_MARK: f32 = 20.0;
const PILL_GAP: f32 = 9.0;
const PILL_NAME_MAX: f32 = 190.0;
const PILL_RING: f32 = 16.0;
const PILL_RING_STROKE: f32 = 2.4;
const WAIT_DOT: f32 = 8.0;
/// The disc the second thing splits off into, and the count of the rest.
const SATELLITE_GAP: f32 = 6.0;
const SATELLITE_RING: f32 = 26.0;
const SATELLITE_MARK: f32 = 13.0;
const BADGE: f32 = 16.0;
/// Discs beside the pill at most; the last of them counts the rest.
const MAX_DISCS: usize = 3;

/// One thing's card, open.
const CARD_WIDTH: f32 = 380.0;
const CARD_HEIGHT: f32 = 60.0;
const CARD_PAD_LEFT: f32 = 12.0;
const CARD_PAD_RIGHT: f32 = 12.0;
const CARD_MARK: f32 = 36.0;
const CARD_GAP: f32 = 11.0;
const CARD_RING: f32 = 34.0;
const CARD_RING_STROKE: f32 = 3.0;
const BUTTON: f32 = 36.0;
const BUTTON_GLYPH: f32 = 15.0;
const BUTTON_GAP: f32 = 7.0;
const STACK_GAP: f32 = 8.0;
/// The "all of it in Status" pill on top of the stack.
const MORE_HEIGHT: f32 = 30.0;
const MORE_PAD: f32 = 13.0;
const MORE_ICON: f32 = 13.0;
/// Cards that rise above the front one.
const MAX_ABOVE: usize = 3;

/// The pointer has to rest this long before the dynamic status opens, so passing
/// over the corner does not set it off...
const HOVER_OPEN: Duration = Duration::from_millis(120);
/// ...and be gone this long before it folds, so a slip off an edge does not.
const HOVER_CLOSE: Duration = Duration::from_millis(320);
const GROW: Duration = Duration::from_millis(640);
const RISE: Duration = Duration::from_millis(640);
const RISE_DELAY: Duration = Duration::from_millis(130);
const RISE_STAGGER: Duration = Duration::from_millis(65);
/// Folding: the cards drop back into the front one, the top one first, and
/// then the front one shrinks into the pill, a little past it and back.
const DROP: Duration = Duration::from_millis(320);
const DROP_STAGGER: Duration = Duration::from_millis(45);
const FOLD: Duration = Duration::from_millis(600);
const FOLD_OVERSHOOT: f32 = 0.1;
/// How far past its size the pill grows before it settles.
const GROW_OVERSHOOT: f32 = 0.08;
/// The dynamic status arriving out of a disc at the corner, and going back into it.
const APPEAR: Duration = Duration::from_millis(640);
const VANISH: Duration = Duration::from_millis(480);
/// One thing's content giving way to another's in the same place.
const SWAP: Duration = Duration::from_millis(420);
/// The pill easing to the width of a new name.
const PILL_RESIZE: Duration = Duration::from_millis(480);
/// How often a turning ring on the pill or a disc is drawn: smooth enough
/// to read as turning, rather than the sidebar spinners' dozen steps a
/// second. A card's, twice the size, takes every frame the display gives.
const RING_FRAME: Duration = Duration::from_millis(33);
/// A progress ring easing to a new reading: readings come a second or two
/// apart, and the ring would otherwise jump from one to the next.
const BAR_EASE: Duration = Duration::from_millis(600);
/// A reading that falls back by more than this -- a download done, the
/// install starting from nothing -- is jumped to, not wound back.
const BAR_FALLBACK: f32 = 0.05;
const RESIZE_OVERSHOOT: f32 = 0.05;
/// A disc beside the pill popping up in its place, and shrinking away.
const SATELLITE_IN: Duration = Duration::from_millis(600);
const SATELLITE_OUT: Duration = Duration::from_millis(380);
/// A card arriving in the open stack, going from it, and the rest closing up.
const CARD_IN: Duration = Duration::from_millis(480);
const CARD_OUT: Duration = Duration::from_millis(380);
const SLIDE: Duration = Duration::from_millis(460);
const SLIDE_OVERSHOOT: f32 = 0.04;
/// How far past its size something popping in goes before it settles.
const POP_OVERSHOOT: f32 = 0.12;
/// The most of an animation's time one frame may use up: a frame that
/// takes longer slows the animation rather than skip part of it.
const PACE: Duration = Duration::from_millis(34);
/// How recently something must have happened to be shown as news.
const NEWS_FOR: Duration = Duration::from_secs(5);
/// How long an agent that has just finished stays on, as a finished
/// transfer does: counted only while it can be seen.
const FINISHED_FOR: Duration = Duration::from_secs(5);
/// Agents that have just finished kept on at once.
const FINISHED_MAX: usize = 3;
/// Work shorter than this is not news when it ends -- a one-line answer --
/// as the finished sound has it.
const FINISHED_AFTER: Duration = Duration::from_secs(5);
/// A command that ran shorter than this is not news when it ends.
const COMMAND_AFTER: Duration = Duration::from_secs(10);
/// How long before the shell is seen back its last output may be, to be
/// taken as the prompt that ended the command: a pane out of sight is looked
/// at again some six seconds late at worst (a five-second floor, a batch and
/// a confirmation), while a window of a mux client hears of such a pane's
/// output only now and then -- an older print is not the end.
const END_LOOKBACK: Duration = Duration::from_secs(8);
/// How late an exit report may update a notice after the shell was observed.
const EXIT_MATCH: Duration = Duration::from_secs(3);
/// How long an agent has to stay stopped to have finished: its state can
/// flicker to idle for a moment mid-task, as a thread's running does
/// (the sidebar holds it through 800 ms for the same reason).
const FINISHED_SETTLE: Duration = Duration::from_millis(800);
/// How long something new shows as its card before the dynamic status folds again.
const BANNER: Duration = Duration::from_millis(3000);
/// The waiting dot breathes for a while after the wait begins, then rests:
/// a wait can last hours, and a window that never stops drawing costs
/// power for nothing.
const PULSE: Duration = Duration::from_millis(1600);
const PULSE_FOR: Duration = Duration::from_secs(10);
/// A breath a second and a half long reads as smooth at fifteen frames a
/// second; there is no more to see at sixty.
const PULSE_FRAME: Duration = Duration::from_millis(66);
/// The steps a surface's corner radius is drawn on, in points: every radius
/// at rest -- the pill's, a card's, the Status pill's -- is a whole number of
/// them.
const RADIUS_STEP: f32 = 3.0;
/// The panes' own progress reports (OSC 9;4) are read again on a change
/// the pane announces; this only bounds how stale a title on the dynamic status can
/// get, and asks for no frames of its own.
const PANE_STALE: Duration = Duration::from_secs(2);
/// A transfer's numbers move on their own, with nothing announcing it.
const COUNT_EVERY: Duration = Duration::from_millis(250);
/// How stale the dynamic status's agent snapshot may get, for what changes without
/// a status change: a title, the thread a pane is in.
const AGENTS_STALE: Duration = Duration::from_secs(2);

/// What the dynamic status answers to, steady across frames: a transfer by its id,
/// a program by the pane it runs in.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub(crate) enum StatusKey {
    Pane(PaneId),
    Transfer(u64),
    /// A tool running in a pane, as its task rules read its screen.
    Task(PaneId),
    /// An agent that has just finished, by its pane: apart from the same
    /// agent's own card, which it never shows beside.
    Finished(PaneId),
    /// The update notice: one at a time, whatever state it is in.
    Update,
}

/// A pane that reported its progress itself (OSC 9;4).
#[derive(Clone)]
pub(crate) struct PaneProgress {
    pane_id: PaneId,
    /// In a tab of this window, rather than another window's.
    here: bool,
    title: String,
    place: String,
    progress: Progress,
}

/// One thing on the dynamic status.
#[derive(Clone)]
pub(crate) enum StatusItem {
    Transfer(CardView),
    /// A coding agent, or any program that reports its state (OSC 7501),
    /// that needs the user or says how far it has got: one entry of the
    /// shared status snapshot, not a copy of it.
    Agent {
        agents: Arc<Vec<AgentPaneStatus>>,
        index: usize,
        here: bool,
    },
    Pane(PaneProgress),
    /// A newer release, followed through its install to the restart.
    Update(UpdateCapsuleStatus),
    /// An agent that has just finished: an entry of the shared status
    /// snapshot, as `Agent` is, and since when it has been on.
    Finished {
        agent: FinishedAgent,
        agents: Arc<Vec<AgentPaneStatus>>,
        index: usize,
        here: bool,
    },
    /// A long command that has just ended in a pane out of sight.
    Command {
        notice: FinishedAgent,
        command: FinishedCommand,
        here: bool,
    },
    /// A tool at work in a pane out of sight -- a package manager, say --
    /// as its task rules read its screen.
    Task {
        pane_id: PaneId,
        name: String,
        reading: TaskReading,
        /// When it came to be in its state, as this window heard it.
        since: Instant,
        here: bool,
    },
}

/// A command that ran a while and ended: what it was, for how long, how
/// it ended when the shell said, and the thread it ran in.
#[derive(Clone)]
pub(crate) struct FinishedCommand {
    label: String,
    took: Duration,
    exit: Option<i32>,
    /// Its task rules last read an error off its screen.
    saw_error: bool,
    place: String,
}

impl FinishedCommand {
    /// It said it failed: a non-zero status, or, without the shell's word,
    /// an error its screen showed last. Otherwise it is taken to have
    /// finished.
    fn failed(&self) -> bool {
        match self.exit {
            Some(code) => code != 0,
            None => self.saw_error,
        }
    }
}

/// A command running in a pane, as its foreground program shows it.
struct Run {
    label: String,
    since: Instant,
    /// When the pane last printed while it ran: the shell's prompt coming
    /// back is when it ended, which a pane out of sight is only looked at
    /// again some seconds after.
    printed: Option<Instant>,
    /// An agent ran in it: the agent's own card says when it is done.
    agent: bool,
    /// What it is doing, when it is a tool with task rules: read off its
    /// screen where the pane lives (`mux::task_status`) and pushed with its
    /// foreground program; and when it came to be in that state.
    reading: Option<TaskReading>,
    state_since: Instant,
    /// The shell's exit report, retained until this run is removed.
    exit: Option<i32>,
}

/// An agent that has just finished, or a command, as one window shows it.
#[derive(Clone)]
pub(crate) struct FinishedAgent {
    pane_id: PaneId,
    shown_at: Instant,
    paused_at: Option<Instant>,
    /// What ended, when it was a command rather than an agent.
    command: Option<FinishedCommand>,
}

/// What one window knows of agents finishing: since when each one at work
/// has been, and those that have just finished.
#[derive(Default)]
pub(crate) struct FinishedAgents {
    working: HashMap<PaneId, Instant>,
    /// Stopped, and since when: finished once that has held for
    /// [`FINISHED_SETTLE`].
    stopping: HashMap<PaneId, Instant>,
    shown: Vec<FinishedAgent>,
    /// Commands running in panes, and how the last one in each ended, as
    /// the shell said, while the pane has yet to show its prompt again.
    runs: HashMap<PaneId, Run>,
    seeded: bool,
}

impl FinishedAgents {
    /// Read existing panes once; unchanged mux readings need not be republished.
    fn seed(
        &mut self,
        now: Instant,
        panes: impl IntoIterator<Item = (PaneId, Option<thinkterm_proto::ForegroundProgram>)>,
    ) {
        for (pane_id, program) in panes {
            if !self.runs.contains_key(&pane_id) {
                self.observe_program(now, pane_id, program.as_ref(), true, String::new);
            }
        }
        self.seeded = true;
    }

    fn observe_program(
        &mut self,
        now: Instant,
        pane_id: PaneId,
        program: Option<&thinkterm_proto::ForegroundProgram>,
        in_sight: bool,
        place: impl FnOnce() -> String,
    ) -> bool {
        // An SSH-domain prompt has no local leader and clears its reported
        // program. With no previous run, an unknown pane is simply a no-op.
        self.program(
            now,
            pane_id,
            program.and_then(running_command),
            program.and_then(|program| program.task.clone()),
            in_sight,
            place,
        )
    }

    /// An agent's state, as it changes. Stopping after work starts the wait
    /// for it to stay stopped; working again, waiting, failing or ending
    /// takes it off, or out of the wait. More news of it still idle -- a new
    /// title, its result seen elsewhere -- leaves it as it is. True when it
    /// started to wait, and so wants a frame to see it through.
    fn note(&mut self, now: Instant, pane_id: PaneId, state: Option<AgentState>) -> bool {
        // An agent at work in the pane: whatever ran there was the agent.
        // Every pane hears of its agent status, an idle or unknown one in
        // a plain shell too, so only work says an agent was there.
        if matches!(
            state,
            Some(AgentState::Working | AgentState::Blocked | AgentState::Error)
        ) {
            if let Some(run) = self.runs.get_mut(&pane_id) {
                run.agent = true;
            }
        }
        match state {
            Some(AgentState::Working) => {
                self.drop_agent(pane_id);
                // A flicker of idle: the same run, still counted from its start.
                self.stopping.remove(&pane_id);
                self.working.entry(pane_id).or_insert(now);
                false
            }
            Some(AgentState::Idle) => {
                if !self.working.contains_key(&pane_id) || self.stopping.contains_key(&pane_id) {
                    return false;
                }
                self.stopping.insert(pane_id, now);
                true
            }
            // Waiting, failed, ended or unknown -- or no agent at all, as
            // in a shell: not an agent's finish. A command's notice is the
            // command's own.
            _ => {
                self.working.remove(&pane_id);
                self.stopping.remove(&pane_id);
                self.drop_agent(pane_id);
                false
            }
        }
    }

    /// Put on those that have stayed stopped long enough after working
    /// long enough -- unless the Status panel lists them. True when one went
    /// on.
    fn settle(&mut self, now: Instant, listed: bool) -> bool {
        if listed {
            self.shown.retain(|notice| notice.command.is_some());
        }
        let settled: Vec<(PaneId, Instant)> = self
            .stopping
            .iter()
            .filter(|(_, stopped)| now.duration_since(**stopped) >= FINISHED_SETTLE)
            .map(|(pane_id, stopped)| (*pane_id, *stopped))
            .collect();
        let mut on = false;
        for (pane_id, stopped) in settled {
            self.stopping.remove(&pane_id);
            let worked = self
                .working
                .remove(&pane_id)
                .is_some_and(|since| stopped.duration_since(since) >= FINISHED_AFTER);
            if !worked || listed {
                continue;
            }
            self.drop_agent(pane_id);
            self.shown.push(FinishedAgent {
                pane_id,
                shown_at: now,
                paused_at: None,
                command: None,
            });
            on = true;
        }
        let excess = self.shown.len().saturating_sub(FINISHED_MAX);
        self.shown.drain(..excess);
        on
    }

    /// Take an agent's notice off; a command's in the same pane stays.
    fn drop_agent(&mut self, pane_id: PaneId) {
        self.shown
            .retain(|notice| notice.pane_id != pane_id || notice.command.is_some());
    }

    fn forget(&mut self, pane_id: PaneId) {
        self.runs.remove(&pane_id);
        self.working.remove(&pane_id);
        self.stopping.remove(&pane_id);
        self.shown.retain(|agent| agent.pane_id != pane_id);
    }

    /// A pane's foreground program, as it changes: `running` names the
    /// command when one runs, `None` is the shell at its prompt. A command
    /// that ran long enough and ends out of sight puts its notice on, with
    /// how it ended if the shell said. True when it went on.
    fn program(
        &mut self,
        now: Instant,
        pane_id: PaneId,
        running: Option<String>,
        task: Option<TaskReading>,
        in_sight: bool,
        place: impl FnOnce() -> String,
    ) -> bool {
        let Some(label) = running else {
            let Some(run) = self.runs.remove(&pane_id) else {
                return false;
            };
            let exit = run.exit;
            let ended = run
                .printed
                .filter(|printed| *printed > run.since && now.duration_since(*printed) <= END_LOOKBACK)
                .unwrap_or(now);
            let took = ended.duration_since(run.since);
            if run.agent || in_sight || took < COMMAND_AFTER {
                return !in_sight && run.reading.is_some();
            }
            let saw_error = run
                .reading
                .as_ref()
                .is_some_and(|reading| reading.state == TaskState::Error);
            self.shown.retain(|notice| notice.pane_id != pane_id);
            self.shown.push(FinishedAgent {
                pane_id,
                shown_at: now,
                paused_at: None,
                command: Some(FinishedCommand {
                    label: run.reading.map_or(run.label, |reading| reading.name),
                    took,
                    exit,
                    saw_error,
                    place: place(),
                }),
            });
            let excess = self.shown.len().saturating_sub(FINISHED_MAX);
            self.shown.drain(..excess);
            return true;
        };
        if let Some(run) = self.runs.get_mut(&pane_id) {
            // What the tool is doing now; a handover to another program of
            // the same tool -- `brew` to Ruby running `brew.rb` -- is the
            // same run. A new reading of one out of sight wants a paint.
            if run.reading == task {
                return false;
            }
            let state = |reading: &Option<TaskReading>| reading.as_ref().map(|reading| reading.state);
            if state(&run.reading) != state(&task) {
                run.state_since = now;
            }
            run.reading = task;
            return !in_sight;
        } else {
            // Another command: what the last one left behind is over.
            self.shown
                .retain(|notice| notice.pane_id != pane_id || notice.command.is_none());
            self.runs.insert(
                pane_id,
                Run {
                    label,
                    since: now,
                    printed: None,
                    agent: false,
                    reading: task,
                    state_since: now,
                    exit: None,
                },
            );
        }
        false
    }

    /// A pane printed. Only a pane running a command is noted; nothing is
    /// looked up for the rest, however much they print.
    fn printed(&mut self, now: Instant, pane_id: PaneId) {
        if self.runs.is_empty() {
            return;
        }
        if let Some(run) = self.runs.get_mut(&pane_id) {
            run.printed = Some(now);
        }
    }

    /// The shell said how a command ended (OSC 133). Usually before it takes
    /// the foreground back, so kept for that; just after, it goes on the
    /// notice already up. True when a notice changed.
    fn exit(&mut self, now: Instant, pane_id: PaneId, code: Option<i32>) -> bool {
        if let Some(command) = self
            .shown
            .iter_mut()
            .filter(|notice| notice.pane_id == pane_id && now.duration_since(notice.shown_at) <= EXIT_MATCH)
            .find_map(|notice| notice.command.as_mut())
        {
            command.exit = code;
            return true;
        }
        if let Some(run) = self.runs.get_mut(&pane_id) {
            run.exit = code;
        }
        false
    }

    /// Run each one's clock while it is in `visible` and nothing holds it;
    /// drop those whose time is up. True when one went.
    fn advance(&mut self, now: Instant, visible: &HashSet<PaneId>, held: bool) -> bool {
        let before = self.shown.len();
        self.shown.retain_mut(|agent| {
            if held || !visible.contains(&agent.pane_id) {
                agent.paused_at.get_or_insert(now);
                return true;
            }
            if let Some(paused) = agent.paused_at.take() {
                agent.shown_at += now.duration_since(paused);
            }
            now < agent.shown_at + FINISHED_FOR
        });
        self.shown.len() != before
    }

    fn pause(&mut self, now: Instant) {
        for agent in &mut self.shown {
            agent.paused_at.get_or_insert(now);
        }
    }

    /// When the next one is due to go on or off: one stopped long enough,
    /// or one whose clock runs out.
    pub(crate) fn deadline(&self) -> Option<Instant> {
        let settling = self.stopping.values().map(|stopped| *stopped + FINISHED_SETTLE);
        self.shown
            .iter()
            .filter(|agent| agent.paused_at.is_none())
            .map(|agent| agent.shown_at + FINISHED_FOR)
            .chain(settling)
            .min()
    }
}

impl StatusItem {
    fn key(&self) -> StatusKey {
        match self {
            Self::Transfer(view) => StatusKey::Transfer(view.id()),
            Self::Agent { agents, index, .. } => StatusKey::Pane(agents[*index].pane_id),
            Self::Pane(pane) => StatusKey::Pane(pane.pane_id),
            Self::Update(_) => StatusKey::Update,
            Self::Finished { agent, .. } => StatusKey::Finished(agent.pane_id),
            Self::Command { notice, .. } => StatusKey::Finished(notice.pane_id),
            Self::Task { pane_id, .. } => StatusKey::Task(*pane_id),
        }
    }

    /// Lower comes first: whatever waits on the user, then whatever
    /// stopped on an error, then what has just finished -- news, with
    /// something to open -- and last what is still under way.
    fn rank(&self) -> u8 {
        match self {
            Self::Agent { agents, index, .. } => match agents[*index].shown_state() {
                ShownState::Blocked => 0,
                ShownState::Error => 1,
                _ => 3,
            },
            Self::Pane(pane) => match pane.progress {
                Progress::Error(_) => 1,
                _ => 3,
            },
            Self::Transfer(CardView::Toast(toast)) => match toast.outcome {
                TransferToastOutcome::Failed(_) => 1,
                TransferToastOutcome::Done => 2,
            },
            Self::Transfer(CardView::Running { .. }) => 3,
            Self::Update(status) => update_capsule::rank(status),
            // With what has just finished.
            Self::Finished { .. } => 2,
            Self::Command { command, .. } => {
                if command.failed() {
                    1
                } else {
                    2
                }
            }
            Self::Task { reading, .. } => match reading.state {
                TaskState::Waiting => 0,
                TaskState::Error => 1,
                TaskState::Running => 3,
            },
        }
    }

    /// When it got to where it is: an agent when its state last changed, a
    /// finished transfer when it finished, anything else when the dynamic status
    /// first showed it. The newest of a kind comes first.
    fn since(
        &self,
        now: Instant,
        first_seen: &HashMap<StatusKey, Instant>,
        agent_since: &HashMap<PaneId, (u64, Instant)>,
    ) -> Instant {
        match self {
            Self::Agent { agents, index, .. } => agent_since
                .get(&agents[*index].pane_id)
                .map_or(now, |(_, at)| *at),
            Self::Transfer(CardView::Toast(toast)) => toast.shown_at,
            Self::Finished { agent, .. } => agent.shown_at,
            Self::Command { notice, .. } => notice.shown_at,
            _ => first_seen.get(&self.key()).copied().unwrap_or(now),
        }
    }

    /// Which state it is in, as far as its arrival goes: moving into another
    /// -- a transfer ending, an agent starting to wait -- it arrives again.
    fn phase(&self) -> u8 {
        match self {
            Self::Agent { agents, index, .. } => match agents[*index].state {
                AgentState::Blocked => 0,
                AgentState::Error => 1,
                _ => 2,
            },
            Self::Pane(pane) => match pane.progress {
                Progress::Error(_) => 4,
                _ => 3,
            },
            Self::Transfer(CardView::Running { .. }) => 5,
            Self::Transfer(CardView::Toast(toast)) => match toast.outcome {
                TransferToastOutcome::Done => 6,
                TransferToastOutcome::Failed(_) => 7,
            },
            Self::Update(status) => update_capsule::phase(status),
            Self::Finished { .. } => 13,
            Self::Command { command, .. } => {
                if command.failed() {
                    15
                } else {
                    14
                }
            }
            Self::Task { reading, .. } => match reading.state {
                TaskState::Running => 16,
                TaskState::Waiting => 17,
                TaskState::Error => 18,
            },
        }
    }

    /// Whether it arriving is an event worth its moment as a card: an agent
    /// this machine has just heard start waiting or fail (`agent_news`, by
    /// its own clock, whatever the host's says), a transfer that has just
    /// started or just ended. A pane's progress bar is not one, and nothing
    /// is that only turned up because it went out of view -- a thread
    /// switched away from, say.
    fn is_news(&self, now: Instant, agent_news: &HashMap<PaneId, Instant>) -> bool {
        match self {
            Self::Agent { agents, index, .. } => {
                matches!(agents[*index].state, AgentState::Blocked | AgentState::Error)
                    && agent_news
                        .get(&agents[*index].pane_id)
                        .is_some_and(|at| now.duration_since(*at) <= NEWS_FOR)
            }
            Self::Transfer(CardView::Toast(toast)) => now.duration_since(toast.shown_at) <= NEWS_FOR,
            Self::Transfer(CardView::Running { .. }) => true,
            Self::Pane(_) => false,
            Self::Update(status) => update_capsule::is_news(status),
            Self::Finished { .. } | Self::Command { .. } => true,
            // A tool that has just stopped to ask, or just failed, is news;
            // working is not, and nor is one that only came into view
            // because its tab was switched away from.
            Self::Task { reading, since, .. } => {
                reading.state != TaskState::Running && now.duration_since(*since) <= NEWS_FOR
            }
        }
    }

    /// What a click on the card itself does, where it does anything.
    fn reveal(&self) -> Option<UIItemType> {
        match self {
            Self::Agent {
                agents,
                index,
                here,
            } => Some(UIItemType::DynamicStatusReveal(agents[*index].pane_id, *here)),
            Self::Pane(pane) => Some(UIItemType::DynamicStatusReveal(pane.pane_id, pane.here)),
            Self::Finished { agent, here, .. } => {
                Some(UIItemType::DynamicStatusReveal(agent.pane_id, *here))
            }
            Self::Command { notice, here, .. } => {
                Some(UIItemType::DynamicStatusReveal(notice.pane_id, *here))
            }
            Self::Task { pane_id, here, .. } => Some(UIItemType::DynamicStatusReveal(*pane_id, *here)),
            Self::Transfer(_) | Self::Update(_) => None,
        }
    }
}

/// Content giving way to other content in the same place: what was there,
/// and how far the change has got, 0 to 1.
struct Swap {
    from: StatusItem,
    at: Timeline,
}

/// A card gone from the open stack, or a disc from the row beside the pill,
/// drawn as it goes: what it was, the place it had, and its going, from 1
/// to 0.
struct Leaving {
    item: StatusItem,
    slot: f32,
    here: Timeline,
}

/// The dynamic status's memory between frames.
#[derive(Default)]
pub(crate) struct DynamicStatus {
    /// Where it was painted: the pointer over it keeps it open.
    bounds: Option<RectF>,
    /// When each thing on it was first shown.
    first_seen: HashMap<StatusKey, Instant>,
    /// The panes' own progress reports, as last read: dropped when a pane
    /// announces a change, and at most [`PANE_STALE`] old.
    panes: Option<(Instant, Vec<PaneProgress>)>,
    /// What the dynamic status last showed of each agent on it, so a status change
    /// that does not change that repaints nothing.
    agent_marks: HashMap<PaneId, u64>,
    /// The agent status snapshot the dynamic status reads: replaced when an agent's
    /// status changes, and at most [`AGENTS_STALE`] old. Building one walks
    /// every pane and asks each for its foreground program; the shared one
    /// is only kept a fifth of a second, which painting every frame would
    /// rebuild five times a second in the middle of an animation.
    agents: Option<(Instant, Arc<Vec<AgentPaneStatus>>)>,
    /// 0 folded, 1 open: the pill growing into the front card.
    grow: Option<Timeline>,
    /// Each card above the front one (and last, the Status pill) rising out
    /// of it, on clocks of their own.
    rise: [Option<Timeline>; MAX_ABOVE + 1],
    /// Opened by the pointer, with the whole stack; otherwise any opening
    /// is a banner for one new thing.
    full: bool,
    hover_since: Option<Instant>,
    away_since: Option<Instant>,
    /// Something new, shown as its card until then.
    banner: Option<(StatusKey, Instant)>,
    /// The dynamic status as a whole arriving, 0 to 1, or going, back to 0.
    presence: Option<Timeline>,
    /// Last frame's things, most pressing first: what a going dynamic status still
    /// draws, and what a thing that went had been.
    last: Vec<StatusItem>,
    /// The folded pill's width, eased from one name to the next.
    pill_width: Option<Timeline>,
    /// What the pill named last frame, and what the front card did; and
    /// either one's old content giving way to its new.
    pill_key: Option<StatusKey>,
    card_key: Option<StatusKey>,
    pill_swap: Option<Swap>,
    card_swap: Option<Swap>,
    /// The discs beside the pill, one for each of the next few things after
    /// it: each one's place in the row, counted out from the pill and
    /// easing as the row closes up or reorders, and its coming.
    discs: HashMap<StatusKey, (Timeline, Timeline)>,
    /// Discs gone from the row, still going.
    leaving_discs: Vec<Leaving>,
    /// Each card above the front one: its place in the stack, easing as the
    /// stack closes up or reorders, and its arrival.
    slots: HashMap<StatusKey, (Timeline, Timeline)>,
    /// The Status pill on top of the stack: its place and its presence.
    more: Option<(Timeline, Timeline)>,
    /// Cards gone from the stack, still going.
    leaving: Vec<Leaving>,
    /// Each progress ring's fraction, easing from one reading to the next.
    bars: HashMap<StatusKey, Timeline>,
    /// Each thing's state as the dynamic status last saw it (see
    /// [`StatusItem::phase`]).
    phases: HashMap<StatusKey, u8>,
    /// When this machine heard each agent start waiting or fail: the news a
    /// banner is for. Kept [`NEWS_FOR`].
    agent_news: HashMap<PaneId, Instant>,
    /// When each agent's state began, on this machine's clock, worked out
    /// once from the host's whole seconds: worked out every frame it would
    /// jitter by a second, and the order with it.
    agent_since: HashMap<PaneId, (u64, Instant)>,
    /// Agents that have just finished, each on for a moment.
    pub(crate) finished: FinishedAgents,
    /// Where the pointer was when a banner came up under it. Until it
    /// moves, it is resting there, not hovering: the banner folds when its
    /// time is up rather than opening into the whole stack.
    still_pointer: Option<(isize, isize)>,
    /// Where the pointer was when typing hid it. Until it moves, it is out
    /// of sight and over nothing: a notice under it runs down as any other.
    hidden_pointer: Option<(isize, isize)>,
}

impl DynamicStatus {
    fn expire_agent_news(&mut self, now: Instant) {
        self.agent_news
            .retain(|_, at| now.duration_since(*at) < NEWS_FOR);
    }

    fn grow(&self, now: Instant) -> f32 {
        self.grow.as_ref().map_or(0.0, |timeline| timeline.value(now))
    }

    fn rise(&self, index: usize, now: Instant) -> f32 {
        self.rise[index]
            .as_ref()
            .map_or(0.0, |timeline| timeline.value(now))
    }

    fn presence(&self, now: Instant) -> f32 {
        self.presence
            .as_ref()
            .map_or(0.0, |timeline| timeline.value(now))
    }

    /// Coming, rather than going.
    fn arriving(&self) -> bool {
        self.presence
            .as_ref()
            .is_none_or(|timeline| timeline.target() > 0.5)
    }

    /// All the way there, and not on its way anywhere.
    fn settled(&self, now: Instant) -> bool {
        self.presence.as_ref().is_some_and(|timeline| {
            timeline.target() > 0.5 && !timeline.is_running() && timeline.value(now) >= 0.999
        })
    }

    /// Shown or not: out of a disc at the corner, or back into one.
    fn set_present(&mut self, now: Instant, present: bool) {
        let to = if present { 1.0 } else { 0.0 };
        if self
            .presence
            .as_ref()
            .is_some_and(|timeline| timeline.target() == to)
        {
            return;
        }
        let from = self.presence(now);
        let duration = if present { APPEAR } else { VANISH };
        self.presence = Some(Timeline::new(now, from, to, duration, Easing::Linear));
    }

    /// Open or fold: the pill grows with a little overshoot and folds
    /// back the same way; the cards above rise one after another and drop
    /// back the top one first.
    fn set_open(&mut self, now: Instant, open: bool, full: bool) {
        let grow_to = if open { 1.0 } else { 0.0 };
        let current = self.grow(now);
        // Cards above the front one have to be back inside it before it
        // shrinks: they take the first stretch of the fold.
        let cards_out = self.full
            && self
                .rise
                .iter()
                .flatten()
                .any(|timeline| timeline.value(now) > 0.01);
        if self.grow.as_ref().is_none_or(|t| t.target() != grow_to) {
            self.grow = Some(if open {
                Timeline::new(now, current, 1.0, GROW, Easing::settling(GROW_OVERSHOOT))
            } else {
                Timeline::delayed(
                    now,
                    current,
                    0.0,
                    if cards_out { DROP / 2 } else { Duration::ZERO },
                    FOLD,
                    Easing::settling(FOLD_OVERSHOOT),
                )
            });
        }
        let rise_to = if open && full { 1.0 } else { 0.0 };
        let last = self.rise.len() - 1;
        for index in 0..self.rise.len() {
            let current = self.rise(index, now);
            if self.rise[index]
                .as_ref()
                .map_or(rise_to == 0.0, |t| t.target() == rise_to)
            {
                continue;
            }
            self.rise[index] = Some(if rise_to > 0.0 {
                Timeline::delayed(
                    now,
                    current,
                    1.0,
                    RISE_DELAY + RISE_STAGGER * index as u32,
                    RISE,
                    Easing::settling(GROW_OVERSHOOT),
                )
            } else {
                Timeline::delayed(
                    now,
                    current,
                    0.0,
                    DROP_STAGGER * (last - index) as u32,
                    DROP,
                    Easing::OutCubic,
                )
            });
        }
        self.full = open && full;
    }

    /// What `key` was last frame, to draw as it goes.
    fn ghost(&self, key: StatusKey) -> Option<StatusItem> {
        self.last.iter().find(|item| item.key() == key).cloned()
    }

    /// What the pill and the front card name now -- the card only while the
    /// dynamic status is open, as a folding one keeps what it showed. Something
    /// else, and the old content fades as the new comes in, once the dynamic status
    /// is all there; arriving, it brings its content with it.
    fn note_front(
        &mut self,
        now: Instant,
        pill: StatusKey,
        card: Option<StatusKey>,
        settled: bool,
    ) {
        let swap = |from: StatusItem| Swap {
            from,
            at: Timeline::new(now, 0.0, 1.0, SWAP, Easing::Linear),
        };
        if let Some(old) = self.pill_key.filter(|old| *old != pill && settled) {
            self.pill_swap = self.ghost(old).map(swap);
        }
        if let Some(card) = card {
            if let Some(old) = self.card_key.filter(|old| *old != card && settled) {
                self.card_swap = self.ghost(old).map(swap);
            }
            self.card_key = Some(card);
        }
        self.pill_key = Some(pill);
    }

    /// Whose discs sit beside the pill now, nearest first. Once the dynamic status
    /// is all there, one that comes pops up in its place, one that goes
    /// shrinks away in its place, and the rest move along the row to theirs;
    /// arriving or going as a whole, the dynamic status brings its discs with it.
    fn note_discs(&mut self, now: Instant, keys: &[StatusKey], animate: bool) {
        let gone: Vec<StatusKey> = self
            .discs
            .keys()
            .filter(|key| !keys.contains(key))
            .copied()
            .collect();
        for key in gone {
            let Some((slot, here)) = self.discs.remove(&key) else {
                continue;
            };
            if !animate {
                continue;
            }
            if let Some(item) = self.ghost(key) {
                let from = here.value(now).min(1.0);
                self.leaving_discs.push(Leaving {
                    item,
                    slot: slot.value(now),
                    here: Timeline::new(now, from, 0.0, SATELLITE_OUT, Easing::OutCubic),
                });
            }
        }
        for (index, key) in keys.iter().enumerate() {
            let target = index as f32;
            match self.discs.get_mut(key) {
                Some((slot, _)) => {
                    if slot.target() != target {
                        if animate {
                            slot.retarget(now, target, SLIDE, Easing::settling(SLIDE_OVERSHOOT));
                        } else {
                            *slot = Timeline::settled(now, target);
                        }
                    }
                }
                None => {
                    let here = if animate {
                        Timeline::new(now, 0.0, 1.0, SATELLITE_IN, Easing::settling(RESIZE_OVERSHOOT))
                    } else {
                        Timeline::settled(now, 1.0)
                    };
                    self.discs
                        .insert(*key, (Timeline::settled(now, target), here));
                }
            }
        }
    }

    /// The cards above the front one now, nearest first, and whether the
    /// Status pill tops them. While the stack stands open, a card that goes
    /// shrinks away where it was and the rest close up; one that comes
    /// pops in. Folded, or on the way, the stack's own rise and drop say it.
    fn note_stack(&mut self, now: Instant, above: &[StatusKey], more: bool) {
        let open = self.full && self.rise(0, now) >= 0.99;
        let gone: Vec<StatusKey> = self
            .slots
            .keys()
            .filter(|key| !above.contains(key))
            .copied()
            .collect();
        for key in gone {
            let Some((slot, _)) = self.slots.remove(&key) else {
                continue;
            };
            let at = slot.value(now);
            if self.rise(slot_index(at), now) <= 0.5 {
                continue;
            }
            if let Some(item) = self.ghost(key) {
                self.leaving.push(Leaving {
                    item,
                    slot: at,
                    here: Timeline::new(now, 1.0, 0.0, CARD_OUT, Easing::OutCubic),
                });
            }
        }
        let arrival = |now: Instant| {
            if open {
                Timeline::new(now, 0.0, 1.0, CARD_IN, Easing::settling(POP_OVERSHOOT))
            } else {
                Timeline::settled(now, 1.0)
            }
        };
        for (index, key) in above.iter().enumerate() {
            let target = index as f32;
            match self.slots.get_mut(key) {
                Some((slot, _)) => {
                    if slot.target() != target {
                        // Folded, no one sees it go: it is just there.
                        if open {
                            slot.retarget(now, target, SLIDE, Easing::settling(SLIDE_OVERSHOOT));
                        } else {
                            *slot = Timeline::settled(now, target);
                        }
                    }
                }
                None => {
                    self.slots
                        .insert(*key, (Timeline::settled(now, target), arrival(now)));
                }
            }
        }
        let target = above.len() as f32;
        match (&mut self.more, more) {
            (Some((slot, here)), true) => {
                if slot.target() != target {
                    if open {
                        slot.retarget(now, target, SLIDE, Easing::settling(SLIDE_OVERSHOOT));
                    } else {
                        *slot = Timeline::settled(now, target);
                    }
                }
                if here.target() < 1.0 {
                    here.retarget(now, 1.0, CARD_IN, Easing::settling(POP_OVERSHOOT));
                }
            }
            (None, true) => {
                self.more = Some((Timeline::settled(now, target), arrival(now)));
            }
            (Some((_, here)), false) => {
                if here.target() > 0.0 {
                    if open {
                        here.retarget(now, 0.0, CARD_OUT, Easing::OutCubic);
                    } else {
                        *here = Timeline::settled(now, 0.0);
                    }
                }
            }
            (None, false) => {}
        }
    }

    /// Ease the pill to `width`; or, with `animate` false, just be it.
    fn set_pill_width(&mut self, now: Instant, width: f32, animate: bool) {
        match self.pill_width.as_mut() {
            Some(timeline) if (timeline.target() - width).abs() > 0.5 => {
                if animate {
                    timeline.retarget(
                        now,
                        width,
                        PILL_RESIZE,
                        Easing::settling(RESIZE_OVERSHOOT),
                    );
                } else {
                    *timeline = Timeline::settled(now, width);
                }
            }
            Some(_) => {}
            None => self.pill_width = Some(Timeline::settled(now, width)),
        }
    }

    /// A menu or dialog took the corner: nothing shown, and nothing to see go.
    /// It comes back as it was, without arriving all over again.
    fn hide(&mut self, now: Instant) {
        let shown = self
            .presence
            .as_ref()
            .is_some_and(|timeline| timeline.target() > 0.5);
        *self = Self {
            first_seen: std::mem::take(&mut self.first_seen),
            panes: self.panes.take(),
            agent_marks: std::mem::take(&mut self.agent_marks),
            agents: self.agents.take(),
            presence: shown.then(|| Timeline::settled(now, 1.0)),
            // Where it was going, at once: a clock left running here would
            // never be stepped, and would keep the window owing frames.
            pill_width: self
                .pill_width
                .take()
                .map(|timeline| Timeline::settled(now, timeline.target())),
            phases: std::mem::take(&mut self.phases),
            agent_news: std::mem::take(&mut self.agent_news),
            agent_since: std::mem::take(&mut self.agent_since),
            finished: std::mem::take(&mut self.finished),
            pill_key: self.pill_key,
            card_key: self.card_key,
            discs: self
                .discs
                .drain()
                .map(|(key, (slot, _))| {
                    (key, (Timeline::settled(now, slot.target()), Timeline::settled(now, 1.0)))
                })
                .collect(),
            ..Self::default()
        };
    }

    /// Nothing on it, and nothing still going.
    fn clear(&mut self) {
        *self = Self {
            panes: self.panes.take(),
            agents: self.agents.take(),
            agent_news: std::mem::take(&mut self.agent_news),
            finished: std::mem::take(&mut self.finished),
            ..Self::default()
        };
    }

    /// Whether one of its transitions is under way: arriving, going,
    /// opening, folding, one thing giving way to another. Those finish even
    /// in a window without focus -- the dynamic status in the corner of a window
    /// next to the one being typed in is in plain view, and an animation
    /// left half done there would stay half done. So does a fold it has
    /// coming: a banner's time running out, the pointer about to open it or
    /// gone from it. What only keeps time (the waiting dot, a turning ring,
    /// a transfer's numbers) waits for focus.
    pub(crate) fn owes_frames(&self) -> bool {
        let running = |timeline: &Option<Timeline>| timeline.as_ref().is_some_and(Timeline::is_running);
        self.banner.is_some()
            || (self.hover_since.is_some() && !self.full)
            || (self.away_since.is_some() && self.full)
            || running(&self.grow)
            || self.rise.iter().any(running)
            || running(&self.presence)
            || running(&self.pill_width)
            || self.pill_swap.is_some()
            || self.card_swap.is_some()
            || !self.leaving.is_empty()
            || !self.leaving_discs.is_empty()
            || self
                .discs
                .values()
                .any(|(slot, here)| slot.is_running() || here.is_running())
            || self
                .slots
                .values()
                .any(|(slot, here)| slot.is_running() || here.is_running())
            || self
                .more
                .as_ref()
                .is_some_and(|(slot, here)| slot.is_running() || here.is_running())
            || self.bars.values().any(Timeline::is_running)
    }

    /// Step every clock; let go of what has finished going. Says whether
    /// anything is still moving. A frame that took long holds the clocks
    /// back rather than let them skip -- a busy window slows an animation
    /// down instead of cutting it short.
    fn advance(&mut self, now: Instant) -> bool {
        let mut moving = false;
        let mut step = |timeline: &mut Timeline| {
            timeline.pace(now, PACE);
            moving |= timeline.advance(now);
        };
        self.grow.iter_mut().for_each(&mut step);
        self.rise.iter_mut().flatten().for_each(&mut step);
        self.presence.iter_mut().for_each(&mut step);
        self.pill_width.iter_mut().for_each(&mut step);
        for swap in self
            .pill_swap
            .iter_mut()
            .chain(self.card_swap.iter_mut())
        {
            step(&mut swap.at);
        }
        for (slot, here) in self.slots.values_mut() {
            step(slot);
            step(here);
        }
        if let Some((slot, here)) = self.more.as_mut() {
            step(slot);
            step(here);
        }
        for (slot, here) in self.discs.values_mut() {
            step(slot);
            step(here);
        }
        self.bars.values_mut().for_each(&mut step);
        for gone in self.leaving.iter_mut().chain(self.leaving_discs.iter_mut()) {
            step(&mut gone.here);
        }

        self.leaving.retain(|card| card.here.is_running());
        self.leaving_discs.retain(|disc| disc.here.is_running());
        for swap in IntoIterator::into_iter([&mut self.pill_swap, &mut self.card_swap]) {
            if swap.as_ref().is_some_and(|swap| !swap.at.is_running()) {
                *swap = None;
            }
        }
        if self
            .more
            .as_ref()
            .is_some_and(|(_, here)| here.target() == 0.0 && !here.is_running())
        {
            self.more = None;
        }
        moving
    }
}

/// The dynamic status's fixed colours: black, whatever the theme.
struct Ink {
    fill: LinearRgba,
    shadow: LinearRgba,
    text: LinearRgba,
    muted: LinearRgba,
    button: LinearRgba,
    button_hover: LinearRgba,
    /// What is drawn on a plain button.
    glyph: LinearRgba,
    badge: LinearRgba,
    accent: LinearRgba,
    danger: LinearRgba,
}

/// The fonts the dynamic status sets text in, and their line heights.
struct Fonts {
    title: Rc<LoadedFont>,
    title_metrics: RenderMetrics,
    title_line: f32,
    body: Rc<LoadedFont>,
    body_metrics: RenderMetrics,
    body_line: f32,
    big: Rc<LoadedFont>,
    big_metrics: RenderMetrics,
    big_line: f32,
    small: Rc<LoadedFont>,
    small_metrics: RenderMetrics,
    small_line: f32,
}

/// Where a thing's mark and name sit, on the pill or on its card: the
/// mark's centre and size, and the name's top left and room.
#[derive(Clone, Copy)]
struct Head {
    mark_x: f32,
    mark: f32,
    name_x: f32,
    name_y: f32,
    name_width: f32,
}

impl Head {
    fn lerp(self, to: Head, t: f32) -> Head {
        let mix = |a: f32, b: f32| a + (b - a) * t;
        Head {
            mark_x: mix(self.mark_x, to.mark_x),
            mark: mix(self.mark, to.mark),
            name_x: mix(self.name_x, to.name_x),
            name_y: mix(self.name_y, to.name_y),
            name_width: mix(self.name_width, to.name_width),
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Tone {
    Progress,
    Done,
    Failed,
    /// Waiting on the user.
    Waiting,
}

/// What stands for a thing: a transfer's icon, or a program's own mark.
pub(crate) enum Mark {
    Icon(SvgIcon),
    /// The pane's tab icon, as its tab and its thread's row in the sidebar
    /// show it -- for an agent too, so one pane looks the same everywhere.
    Pane(PaneId),
}

/// What a card says and offers, whatever its state.
pub(crate) struct CardContent {
    pub mark: Mark,
    pub tone: Tone,
    pub name: String,
    pub detail: String,
    /// The word or number after the name on the folded pill.
    pub short: String,
    /// How far it has got: `Some(None)` when there is no telling.
    pub bar: Option<Option<f32>>,
    pub actions: Vec<CardAction>,
}

impl crate::TermWindow {
    /// Paint the dynamic status, in the terminal area's bottom-right corner.
    pub(crate) fn paint_dynamic_status(&mut self) -> anyhow::Result<()> {
        let now = Instant::now();
        self.dynamic_status.expire_agent_news(now);
        if let Some(deadline) = self
            .dynamic_status
            .agent_news
            .values()
            .map(|at| *at + NEWS_FOR)
            .min()
        {
            self.update_next_frame_time(Some(deadline));
        }
        // This window's own focus: someone looking at this one, not at any
        // window of the app.
        let focused = self.focused.is_some();
        // Out of the way while a menu or dialog is up, and on the welcome
        // page, whose buttons sit there. A right sidebar coming out over
        // the corner -- docking, going, revealed by the pointer -- moves it
        // along instead (see `right_sidebar_on_screen_width`).
        let hidden = self.transfer_cards_hidden()
            || self
                .active_content_view()
                .and_then(|view| view.tab_key())
                .is_some_and(|key| key.starts_with("onboarding:"));
        if hidden {
            self.pause_status_notices(now);
            self.dynamic_status.hide(now);
            return Ok(());
        }
        let mut items = self.dynamic_status_items(now);

        // Nothing on it now: it goes back into the corner, drawn as it last
        // was -- unless it has gone already.
        let present = !items.is_empty();
        self.dynamic_status.set_present(now, present);
        let going = !present;
        if going {
            self.pause_status_notices(now);
            let dynamic = &mut self.dynamic_status;
            let still_going = dynamic
                .presence
                .as_ref()
                .is_some_and(|timeline| timeline.is_running() || timeline.value(now) > 0.0);
            if !still_going || dynamic.last.is_empty() {
                dynamic.clear();
                return Ok(());
            }
            items = std::mem::take(&mut dynamic.last);
        }

        // The pointer anywhere over it, resting or not: what holds a
        // notice's clock, as `held` (below) is what opens it.
        let mut over = false;
        let mut opening = false;
        let front_open;
        if going {
            // Drawn as it was: the card it had open, if it had one.
            front_open = self
                .dynamic_status
                .card_key
                .and_then(|key| items.iter().position(|item| item.key() == key))
                .unwrap_or(0);
        } else {
            // Where the pointer is, and for how long -- unless it is resting
            // where a banner came up under it, which is not a hover.
            let pointer = self
                .current_mouse_event
                .as_ref()
                .map(|event| (event.coords.x, event.coords.y));
            if self.dynamic_status.still_pointer.is_some() && self.dynamic_status.still_pointer != pointer {
                self.dynamic_status.still_pointer = None;
            }
            if self.dynamic_status.hidden_pointer.is_some() && self.dynamic_status.hidden_pointer != pointer {
                self.dynamic_status.hidden_pointer = None;
            }
            over = self.dynamic_status.hidden_pointer.is_none()
                && self
                    .dynamic_status
                    .bounds
                    .is_some_and(|rect| pointer_over(self, rect));
            let held = self.dynamic_status.still_pointer.is_none() && over;
            let dynamic = &mut self.dynamic_status;
            if held {
                dynamic.away_since = None;
                dynamic.hover_since.get_or_insert(now);
            } else {
                dynamic.hover_since = None;
                if dynamic.full {
                    dynamic.away_since.get_or_insert(now);
                }
            }

            // Something that has just happened, while someone is looking,
            // gets a moment as its card. What turned up while the window was
            // in the background just takes its place: looking again is not
            // news, and the dynamic status must not leap out at every return to it.
            let keys: HashSet<StatusKey> = items.iter().map(StatusItem::key).collect();
            dynamic.first_seen.retain(|key, _| keys.contains(key));
            dynamic.phases.retain(|key, _| keys.contains(key));
            let mut arrived = None;
            for item in &items {
                let key = item.key();
                let phase = item.phase();
                let changed = dynamic
                    .phases
                    .insert(key, phase)
                    .is_some_and(|was| was != phase);
                if changed || !dynamic.first_seen.contains_key(&key) {
                    dynamic.first_seen.insert(key, now);
                    if arrived.is_none() && item.is_news(now, &dynamic.agent_news) {
                        arrived = Some(key);
                    }
                }
            }
            if let Some(key) = arrived.filter(|_| focused && !dynamic.full) {
                dynamic.banner = Some((key, now + BANNER));
                dynamic.still_pointer = pointer;
            }
            if dynamic
                .banner
                .is_some_and(|(key, until)| until <= now || !keys.contains(&key))
            {
                dynamic.banner = None;
            }

            let rested = dynamic
                .hover_since
                .is_some_and(|since| now.duration_since(since) >= HOVER_OPEN);
            let lingering = dynamic
                .away_since
                .is_some_and(|since| now.duration_since(since) < HOVER_CLOSE);
            let full = rested || (dynamic.full && (held || lingering));
            if full {
                dynamic.banner = None;
            }
            let banner = dynamic.banner.map(|(key, _)| key);
            opening = full || banner.is_some();
            dynamic.set_open(now, opening, full);
            // Open, the front card is the banner's thing or the most pressing;
            // folding, it stays what it was until it is the pill again.
            let front = if opening { banner } else { dynamic.card_key };
            front_open = front
                .and_then(|key| items.iter().position(|item| item.key() == key))
                .unwrap_or(0);

            let mut wake = Vec::new();
            if let Some(since) = dynamic.hover_since.filter(|_| !full) {
                wake.push(since + HOVER_OPEN);
            }
            if let Some(since) = dynamic.away_since.filter(|_| full) {
                wake.push(since + HOVER_CLOSE);
            }
            if let Some((_, until)) = dynamic.banner {
                wake.push(until);
            }
            for at in wake {
                self.update_next_frame_time(Some(at));
            }
        }

        // Which thing is where: the pill, the disc beside it, the cards above
        // the front one.
        let above: Vec<usize> = (0..items.len())
            .filter(|index| *index != front_open)
            .take(MAX_ABOVE)
            .collect();
        if !going {
            let dynamic = &mut self.dynamic_status;
            let settled = dynamic.settled(now);
            dynamic.note_front(
                now,
                items[0].key(),
                opening.then(|| items[front_open].key()),
                settled,
            );
            let above_keys: Vec<StatusKey> = above.iter().map(|index| items[*index].key()).collect();
            let has_agents = items
                .iter()
                .any(|item| matches!(item, StatusItem::Agent { .. }));
            // On top of the stack: Status to see the rest in, when it would
            // list them; else how many there are that have no card.
            let unseen = items.len().saturating_sub(1 + above.len());
            dynamic.note_stack(now, &above_keys, has_agents || unseen > 0);
        }

        let settings = crate::native_settings::load_shared();
        let font_size = crate::native_settings::home_font_size(&settings);
        let font = |size: f64, weight: u16| -> anyhow::Result<(Rc<LoadedFont>, RenderMetrics, f32)> {
            let font = self
                .fonts
                .title_font_with_size_and_weight(size, weight)
                .context("dynamic status font")?;
            let metrics = RenderMetrics::with_font_metrics(&font.metrics());
            let line = metrics.cell_size.height as f32;
            Ok((font, metrics, line))
        };
        let (title, title_metrics, title_line) = font(font_size, 600)?;
        let (body, body_metrics, body_line) = font((font_size - 1.5).max(9.0), 400)?;
        let (big, big_metrics, big_line) = font(font_size + 7.0, 600)?;
        let (small, small_metrics, small_line) = font((font_size - 3.0).max(8.0), 700)?;
        let fonts = Fonts {
            title,
            title_metrics,
            title_line,
            body,
            body_metrics,
            body_line,
            big,
            big_metrics,
            big_line,
            small,
            small_metrics,
            small_line,
        };

        let scale = self.ui_f32(1.0);
        let px = move |value: f32| value * POINT * scale;

        let border = self.get_os_border();
        let (_, padding_top) = self.padding_left_top();
        let (top_tab_height, bottom_tab_height) = self.pane_area_insets();
        let content_bottom =
            self.dimensions.pixel_height as f32 - border.bottom.get() as f32 - bottom_tab_height;
        let grid_top = border.top.get() as f32 + top_tab_height + padding_top;
        let grid_bottom = grid_top + self.terminal_size.pixel_height as f32;
        let floor = if settings.terminal.bottom_quote_enabled {
            grid_bottom
        } else {
            content_bottom
        };
        // The terminal area's own corner, left of an open right sidebar.
        // With the sidebar sliding in or out, as much of it as is out this
        // frame: the dynamic status moves with its edge.
        let right = self.dimensions.pixel_width as f32
            - border.right.get() as f32
            - self.right_sidebar_on_screen_width(now)
            - px(MARGIN);
        let bottom = floor - px(MARGIN);
        let top_limit = border.top.get() as f32 + top_tab_height + px(20.0);
        // No wider than the terminal area left of it, as far as there is one.
        let left = border.left.get() as f32 + self.workspace_sidebar_width() as f32 + px(MARGIN);
        let card_width = px(CARD_WIDTH).min((right - left).max(px(120.0)));

        // What each thing drawn says, worked out once for the frame however
        // many places it is drawn in -- and only for what is drawn: the pill,
        // the disc beside it, the front card and the few above it, however
        // many more there are.
        // A disc for each of the next few after the pill, as many as fit
        // left of it; the last of them counts the rest.
        let pill_reach = right - px(PILL_PAD_LEFT + PILL_MARK + PILL_GAP + PILL_PAD_RIGHT);
        let fit = ((pill_reach - px(PILL_NAME_MAX) - left).max(0.0)
            / px(PILL_HEIGHT + SATELLITE_GAP))
            .floor() as usize;
        let disc_count = items.len().saturating_sub(1).min(MAX_DISCS).min(fit);
        let disc_items = 1..1 + disc_count;
        let drawn_items: HashSet<usize> = [0usize, front_open]
            .iter()
            .copied()
            .chain(disc_items.clone())
            .chain(above.iter().copied())
            .filter(|index| *index < items.len())
            .collect();
        let mut contents: Vec<Option<CardContent>> = items
            .iter()
            .enumerate()
            .map(|(index, item)| drawn_items.contains(&index).then(|| status_content(item)))
            .collect();
        // A ring eases to each new reading rather than jumping to it.
        let bars = &mut self.dynamic_status.bars;
        bars.retain(|key, _| items.iter().any(|item| item.key() == *key));
        for (item, content) in items.iter().zip(contents.iter_mut()) {
            let Some(content) = content else {
                continue;
            };
            let key = item.key();
            let Some(Some(reading)) = content.bar else {
                bars.remove(&key);
                continue;
            };
            let bar = bars
                .entry(key)
                .or_insert_with(|| Timeline::settled(now, reading));
            if reading < bar.value(now) - BAR_FALLBACK {
                *bar = Timeline::settled(now, reading);
            } else if (bar.target() - reading).abs() > f32::EPSILON {
                bar.retarget(now, reading, BAR_EASE, Easing::OutCubic);
            }
            content.bar = Some(Some(bar.value(now).clamp(0.0, 1.0)));
        }
        let content = |index: usize| {
            contents[index]
                .as_ref()
                .expect("what is said is worked out for everything drawn")
        };
        let pill_target = self.pill_width(&fonts, content(0), &px)?;
        let dynamic = &mut self.dynamic_status;
        let animate = !going && dynamic.settled(now);
        dynamic.set_pill_width(now, pill_target, animate);
        if !going {
            let disc_keys: Vec<StatusKey> = disc_items.clone().map(|index| items[index].key()).collect();
            dynamic.note_discs(now, &disc_keys, animate);
        }
        if dynamic.advance(now) {
            // The very next frame the display gives, as a sidebar sliding
            // asks for it: a deadline even a frame ahead goes the long way
            // round through a timer, and arrives every other frame.
            self.update_next_frame_time(Some(now));
        }

        let chrome = self.chrome();
        let ink = Ink {
            fill: LinearRgba::with_srgba(5, 5, 6, 255),
            shadow: LinearRgba::with_components(0.0, 0.0, 0.0, 0.5),
            text: LinearRgba::with_srgba(245, 245, 247, 255),
            muted: LinearRgba::with_srgba(235, 235, 245, 158),
            // Opaque greys, not white over the black: blending is done in
            // linear light, where a little white comes out a lot lighter
            // than it looks in a mockup.
            button: LinearRgba::with_srgba(40, 40, 43, 255),
            button_hover: LinearRgba::with_srgba(56, 56, 60, 255),
            glyph: LinearRgba::with_srgba(205, 205, 212, 255),
            badge: LinearRgba::with_srgba(58, 58, 60, 255),
            accent: chrome.accent,
            danger: chrome.danger,
        };

        let dynamic = &self.dynamic_status;
        let grow = dynamic.grow(now);
        let open_amount = grow.clamp(0.0, 1.0);
        let (disc, stretch, content_in, surface_in) =
            presence_phases(dynamic.presence(now), dynamic.arriving());

        // The pill, folded, and the front card, open; what is drawn is
        // between the two, anchored at the corner -- or, arriving or going,
        // between that and a disc at the pill's end.
        let pill_width = dynamic
            .pill_width
            .as_ref()
            .map_or(pill_target, |timeline| timeline.value(now));
        let pill = euclid::rect(
            right - pill_width,
            bottom - px(PILL_HEIGHT),
            pill_width,
            px(PILL_HEIGHT),
        );
        let card = euclid::rect(
            right - card_width,
            bottom - px(CARD_HEIGHT),
            card_width,
            px(CARD_HEIGHT),
        );
        let end = px(PILL_HEIGHT) * disc;
        let end_center = (right - px(PILL_HEIGHT) / 2.0, bottom - px(PILL_HEIGHT) / 2.0);
        let front_rect = lerp_rect(
            euclid::rect(end_center.0 - end / 2.0, end_center.1 - end / 2.0, end, end),
            lerp_rect(pill, card, grow),
            stretch,
        );

        // A card's place in the stack, counted up from the front card; and
        // where it is when tucked into the pill below.
        let slot_rect = |slot: f32, width: f32, height: f32| {
            let slot_bottom = card.min_y() - px(STACK_GAP) - slot * px(CARD_HEIGHT + STACK_GAP);
            euclid::rect(right - width, slot_bottom - height, width, height)
        };
        let tucked = |index: usize, target: RectF| {
            let drop = (index + 1) as f32 * px(CARD_HEIGHT + STACK_GAP);
            euclid::rect(
                right - target.size.width * 0.5,
                target.min_y() + drop + target.size.height * 0.4,
                target.size.width * 0.5,
                target.size.height * 0.6,
            )
        };

        // Old content giving way in the pill, the front card and the disc.
        let pill_from = dynamic
            .pill_swap
            .as_ref()
            .map(|swap| (status_content(&swap.from), swap.at.value(now)));
        let card_from = dynamic
            .card_swap
            .as_ref()
            .map(|swap| (status_content(&swap.from), swap.at.value(now)));
        let leaving_discs: Vec<(CardContent, f32, f32)> = dynamic
            .leaving_discs
            .iter()
            .map(|disc| (status_content(&disc.item), disc.slot, disc.here.value(now)))
            .collect();
        let leaving: Vec<(CardContent, f32, f32)> = dynamic
            .leaving
            .iter()
            .map(|card| (status_content(&card.item), card.slot, card.here.value(now)))
            .collect();

        let gl_state = self
            .render_state
            .as_ref()
            .context("dynamic status render state")?;
        let layer = gl_state
            .layer_for_zindex(ZINDEX)
            .context("dynamic status layer")?;
        let mut layers = layer.quad_allocator();
        let ctx = DrawContext::new(gl_state, self.dimensions, &fonts.title_metrics);

        let mut drawn: Vec<RectF> = vec![front_rect];
        let mut turning = false;
        let mut turning_open = false;
        let mut hits: Vec<UIItem> = Vec::new();
        let mut shown_keys = Vec::new();
        let interactive = !going && content_in > 0.9;

        // Cards that have left the stack, shrinking away where they were;
        // under everything else.
        for (content, slot, here) in &leaving {
            let index = slot_index(*slot);
            let rise = dynamic.rise(index, now);
            let target = slot_rect(*slot, card_width, px(CARD_HEIGHT));
            let rect = scale_rect(lerp_rect(tucked(index, target), target, rise), 0.9 + 0.1 * here);
            let surface = smoothstep(rise * 2.5) * smoothstep(here * 1.6) * content_in;
            let shown = smoothstep((rise - 0.55) / 0.45) * smoothstep((here - 0.35) / 0.65) * content_in;
            self.paint_status_surface(&mut layers, &ctx, &ink, rect, surface)?;
            self.paint_status_card(&mut layers, &ctx, &fonts, &ink, rect, content, shown, &mut Vec::new(), true, true, &px)?;
        }

        // The cards above the front one, rising out of it, each in its place.
        for (index, item_index) in above.iter().enumerate() {
            let item = &items[*item_index];
            let Some((slot, here)) = dynamic.slots.get(&item.key()) else {
                continue;
            };
            let rise = dynamic.rise(index, now);
            if rise <= 0.001 {
                continue;
            }
            let here = here.value(now);
            let target = slot_rect(slot.value(now), card_width, px(CARD_HEIGHT));
            if target.min_y() < top_limit {
                continue;
            }
            let rect = scale_rect(lerp_rect(tucked(index, target), target, rise), 0.9 + 0.1 * here);
            let surface = smoothstep(rise * 2.5) * smoothstep(here * 1.6) * content_in;
            let shown = smoothstep((rise - 0.55) / 0.45) * smoothstep((here - 0.35) / 0.65) * content_in;
            self.paint_status_surface(&mut layers, &ctx, &ink, rect, surface)?;
            drawn.push(rect);
            shown_keys.push(item.key());
            turning_open |= content(*item_index).bar == Some(None);
            let mut buttons = Vec::new();
            self.paint_status_card(
                &mut layers,
                &ctx,
                &fonts,
                &ink,
                rect,
                content(*item_index),
                shown,
                &mut buttons,
                true,
                true,
                &px,
            )?;
            if interactive && rise > 0.9 && here > 0.9 {
                hits.push(ui_item(
                    rect,
                    item.reveal().unwrap_or(UIItemType::TransferCardBackground),
                ));
                hits.extend(buttons);
            }
        }

        // The Status pill on top of them.
        if let Some((slot, here)) = dynamic.more.as_ref() {
            let index = above.len().min(MAX_ABOVE);
            let rise = dynamic.rise(index, now);
            let here = here.value(now);
            let has_agents = items
                .iter()
                .any(|item| matches!(item, StatusItem::Agent { .. }));
            if rise > 0.001 && here > 0.001 {
                let unseen = items.len().saturating_sub(1 + above.len());
                let label = if has_agents {
                    crate::i18n::tr("dynamic-status-show-all")
                } else {
                    let mut args = fluent_bundle::FluentArgs::new();
                    args.set("count", unseen as i64);
                    crate::i18n::tr_args("dynamic-status-more", &args)
                };
                let width = self.sidebar_text_width(&fonts.body, &label)?.ceil()
                    + px(MORE_PAD * 2.0 + MORE_ICON + 6.0);
                let target = slot_rect(slot.value(now), width, px(MORE_HEIGHT));
                if target.min_y() >= top_limit {
                    let rect = scale_rect(lerp_rect(tucked(index, target), target, rise), 0.9 + 0.1 * here);
                    let surface = smoothstep(rise * 2.5) * smoothstep(here * 1.6) * content_in;
                    let shown = smoothstep((rise - 0.55) / 0.45) * smoothstep((here - 0.35) / 0.65) * content_in;
                    self.paint_status_surface(&mut layers, &ctx, &ink, rect, surface)?;
                    self.paint_status_more(&mut layers, &ctx, &fonts, &ink, rect, &label, has_agents, shown)?;
                    drawn.push(rect);
                    if interactive && has_agents && rise > 0.9 && here > 0.9 {
                        hits.push(ui_item(rect, UIItemType::DynamicStatusShowAll));
                    }
                }
            }
        }

        // The discs beside the pill, each in its place in the row: popping
        // up as one comes, shrinking away as one goes, and all running into
        // the front card as the pill grows.
        let disc_at = |slot: f32, here: f32| {
            let size = px(PILL_HEIGHT) * (1.0 - 0.6 * open_amount) * (0.6 + 0.4 * here);
            let rest_cx = pill.min_x()
                - px(SATELLITE_GAP)
                - px(PILL_HEIGHT) / 2.0
                - slot * px(PILL_HEIGHT + SATELLITE_GAP);
            let cx = rest_cx + (front_rect.min_x() + px(PILL_HEIGHT) - rest_cx) * open_amount;
            let cy = bottom - px(PILL_HEIGHT) / 2.0;
            let fade = (1.0 - smoothstep(open_amount * 1.6)) * smoothstep(here * 1.2) * content_in;
            (euclid::rect(cx - size / 2.0, cy - size / 2.0, size, size), fade)
        };
        if open_amount < 0.99 {
            for (content, slot, here) in &leaving_discs {
                let (rect, fade) = disc_at(*slot, *here);
                self.paint_status_satellite(&mut layers, &ctx, &fonts, &ink, rect, content, 0, fade, fade, &px)?;
            }
            for (position, index) in disc_items.clone().enumerate() {
                let item = &items[index];
                let Some((slot, here)) = dynamic.discs.get(&item.key()) else {
                    continue;
                };
                let (rect, fade) = disc_at(slot.value(now), here.value(now));
                // The last disc counts what has no disc of its own.
                let rest = if position + 1 == disc_count {
                    items.len() - 1 - disc_count
                } else {
                    0
                };
                self.paint_status_satellite(
                    &mut layers, &ctx, &fonts, &ink, rect, content(index), rest, fade, fade, &px,
                )?;
                turning |= content(index).bar == Some(None);
                shown_keys.push(item.key());
                drawn.push(rect);
            }
        }

        // The front: the pill growing into its card, the one's content
        // giving way to the other's, and either one's old content to its new.
        self.paint_status_surface(&mut layers, &ctx, &ink, front_rect, surface_in)?;
        let folded_alpha = (1.0 - smoothstep(open_amount / 0.4)) * content_in;
        // The pill opening into its own card -- not into another thing's
        // (a banner), and not while either gives way to new content.
        let travel = front_open == 0 && pill_from.is_none() && card_from.is_none();
        // Its ring goes with it when the card has one where the pill does:
        // a card with buttons in its place has none.
        let travel_ring = travel
            && content(0).bar.is_some()
            && content(0).tone != Tone::Waiting
            && content(0).actions.is_empty();
        let open_alpha = smoothstep((open_amount - 0.45) / 0.45) * content_in;
        if folded_alpha > 0.001 {
            let first_seen = dynamic.first_seen.get(&items[0].key()).copied();
            shown_keys.push(items[0].key());
            turning |= content(0).bar == Some(None);
            if content(0).tone == Tone::Waiting
                && first_seen.is_some_and(|at| now.duration_since(at) < PULSE_FOR)
            {
                self.update_next_frame_time(Some(now + PULSE_FRAME));
            }
            let (was, is) = swap_mix(pill_from.as_ref().map(|(_, at)| *at));
            if let Some((from, _)) = pill_from.as_ref() {
                self.paint_status_pill(&mut layers, &ctx, &fonts, &ink, front_rect, from, folded_alpha * was, None, true, true, &px)?;
            }
            self.paint_status_pill(
                &mut layers,
                &ctx,
                &fonts,
                &ink,
                front_rect,
                content(0),
                folded_alpha * is,
                first_seen.map(|at| now.duration_since(at)),
                !travel,
                !travel_ring,
                &px,
            )?;
        }
        let mut front_buttons = Vec::new();
        if open_alpha > 0.001 {
            shown_keys.push(items[front_open].key());
            turning_open |= content(front_open).bar == Some(None);
            let (was, is) = swap_mix(card_from.as_ref().map(|(_, at)| *at));
            if let Some((from, _)) = card_from.as_ref() {
                self.paint_status_card(&mut layers, &ctx, &fonts, &ink, front_rect, from, open_alpha * was, &mut Vec::new(), true, true, &px)?;
            }
            self.paint_status_card(
                &mut layers,
                &ctx,
                &fonts,
                &ink,
                front_rect,
                content(front_open),
                open_alpha * is,
                &mut front_buttons,
                !travel,
                !travel_ring,
                &px,
            )?;
        }
        // The same thing folded and open: its mark and name go from the one
        // to the other themselves.
        if travel {
            self.paint_status_head(&mut layers, &ctx, &fonts, &ink, front_rect, content(0), open_amount, content_in, travel_ring, &px)?;
        }
        if interactive && grow > 0.9 {
            hits.push(ui_item(
                front_rect,
                items[front_open]
                    .reveal()
                    .unwrap_or(UIItemType::TransferCardBackground),
            ));
            hits.extend(front_buttons);
        } else if !going {
            hits.push(ui_item(front_rect, UIItemType::TransferCardBackground));
        }

        drop(layers);

        // Only what is on screen and moves on its own asks for frames. A
        // ring with no fraction turns some thirty frames a second folded,
        // and every frame on an open card. A transfer's numbers are read a
        // few times a second.
        // A program's own progress is announced when it changes, and paints
        // then.
        let counting = items.iter().any(|item| {
            matches!(item, StatusItem::Transfer(CardView::Running { .. }))
                && shown_keys.contains(&item.key())
        });

        if going {
            // Nothing to point at while it goes.
            self.dynamic_status.bounds = None;
        } else {
            // The whole of it, gaps included, is one surface to the pointer:
            // leaving it is what folds it. Pushed first so that
            // every card and button above wins the hit test.
            // The folded pill and its discs stay part of it while it is open:
            // the discs run into the card as it opens, and a pointer resting
            // where one was would otherwise fold it again, and open it, and
            // fold it.
            let mut footprint = pill;
            if disc_count > 0 {
                let row = disc_count as f32 * px(PILL_HEIGHT + SATELLITE_GAP);
                footprint = footprint.union(&euclid::rect(
                    pill.min_x() - row,
                    pill.min_y(),
                    row,
                    px(PILL_HEIGHT),
                ));
            }
            let bounds = drawn
                .iter()
                .copied()
                .fold(footprint, |a, b| a.union(&b));
            self.ui_items
                .push(ui_item(bounds, UIItemType::TransferCardBackground));
            self.ui_items.extend(hits);
            self.dynamic_status.bounds = Some(bounds);

            // A notice burns its time only where it can be seen, and not
            // while the pointer holds it open.
            let visible: HashSet<u64> = shown_keys
                .iter()
                .filter_map(|key| match key {
                    StatusKey::Transfer(id) => Some(*id),
                    StatusKey::Pane(_)
                    | StatusKey::Update
                    | StatusKey::Finished(_)
                    | StatusKey::Task(_) => None,
                })
                .collect();
            // Nor while the window is out of sight -- minimised, covered,
            // on another desktop: a notice must not run out before anyone
            // could have looked. A window seen but not in front counts. A
            // pointer it came up under holds it too: that is someone
            // reading it, even before they move.
            let hold = over || self.occluded.is_some();
            self.advance_transfer_toasts(now, &visible, hold);
            if let Some(deadline) = self.next_transfer_toast_deadline() {
                self.update_next_frame_time(Some(deadline));
            }
            let finished: HashSet<PaneId> = shown_keys
                .iter()
                .filter_map(|key| match key {
                    StatusKey::Finished(pane_id) => Some(*pane_id),
                    _ => None,
                })
                .collect();
            if self.dynamic_status.finished.advance(now, &finished, hold) {
                // This frame already painted the one that went.
                self.update_next_frame_time(Some(now));
            }
            if let Some(deadline) = self.dynamic_status.finished.deadline() {
                self.update_next_frame_time(Some(deadline));
            }
            if turning_open {
                // The next frame the display gives: a deadline goes through
                // a timer, and comes a frame late.
                self.update_next_frame_time(Some(now));
            } else if turning {
                self.update_next_frame_time(Some(now + RING_FRAME));
            }
            if counting {
                self.update_next_frame_time(Some(now + COUNT_EVERY));
            }
        }
        self.drop_closed_transfer_toasts(now);
        // Kept for next frame: what a thing that goes had been.
        self.dynamic_status.last = items;
        Ok(())
    }

    /// Typing hid the pointer: until it moves, it is over nothing here.
    pub(crate) fn dynamic_status_pointer_hidden(&mut self) {
        self.dynamic_status.hidden_pointer = self
            .current_mouse_event
            .as_ref()
            .map(|event| (event.coords.x, event.coords.y));
    }

    /// Hold every finished notice's clock -- a transfer's, a thread's --
    /// while the dynamic status is not there to show it.
    pub(crate) fn pause_status_notices(&mut self, now: Instant) {
        for toast in &mut self.transfer_toasts {
            if toast.closing_at.is_none() && toast.paused_at.is_none() {
                toast.paused_at = Some(now);
            }
        }
        self.dynamic_status.finished.pause(now);
    }

    /// The folded pill's width for `content`.
    fn pill_width(
        &self,
        fonts: &Fonts,
        content: &CardContent,
        px: &impl Fn(f32) -> f32,
    ) -> anyhow::Result<f32> {
        let name = self
            .sidebar_text_width(&fonts.title, &content.name)?
            .ceil()
            .min(px(PILL_NAME_MAX));
        Ok(px(PILL_PAD_LEFT + PILL_MARK + PILL_GAP)
            + name
            + self.pill_trail_width(fonts, content, px)?
            + px(PILL_PAD_RIGHT))
    }

    /// The width of what follows the name on the pill, with its gap.
    /// How far right a card's text may reach in `rect`: left of its
    /// buttons, or of its ring and number. As `paint_status_card` lays it out.
    fn card_text_end(&self, fonts: &Fonts, rect: RectF, content: &CardContent, px: &impl Fn(f32) -> f32) -> anyhow::Result<f32> {
        let mut end = rect.max_x() - px(CARD_PAD_RIGHT);
        end -= content.actions.len() as f32 * px(BUTTON + BUTTON_GAP);
        if let Some(fraction) = content.bar {
            if content.actions.is_empty() {
                end -= px(CARD_RING) + px(BUTTON_GAP);
            }
            if let Some(fraction) = fraction {
                let number = format!("{}", (fraction * 100.0).floor() as u32);
                let percent_width = self.sidebar_text_width(&fonts.body, "%")?.ceil();
                let number_width = self.sidebar_text_width(&fonts.big, &number)?.ceil();
                end -= percent_width + px(1.0) + number_width + px(BUTTON_GAP);
            }
        }
        Ok(end)
    }

    /// The mark and name of the thing the pill opens into, travelling from
    /// where the pill has them to where its card does as it opens (`t`, 0 to
    /// 1): one name moving, rather than one fading as another comes. The
    /// pill's square mark gives way to the card's round one on the way.
    #[allow(clippy::too_many_arguments)]
    fn paint_status_head(
        &self,
        layers: &mut crate::quad::TripleLayerQuadAllocator,
        ctx: &DrawContext,
        fonts: &Fonts,
        ink: &Ink,
        rect: RectF,
        content: &CardContent,
        t: f32,
        alpha: f32,
        ring: bool,
        px: &impl Fn(f32) -> f32,
    ) -> anyhow::Result<()> {
        if alpha <= 0.001 {
            return Ok(());
        }
        let t = t.clamp(0.0, 1.0);
        let mix = |a: f32, b: f32| a + (b - a) * t;
        let mid = rect.min_y() + rect.size.height / 2.0;
        let folded = self.pill_head(fonts, rect, content, px)?;
        let open = card_head(fonts, rect, self.card_text_end(fonts, rect, content, px)?, px);
        let place = folded.lerp(open, t);
        // The ring too: from the pill's end, small, to the card's, large.
        if let (true, Some(fraction)) = (ring, content.bar) {
            let pill_cx = folded.name_x + folded.name_width + px(PILL_GAP) + px(PILL_RING) / 2.0;
            let card_cx = rect.max_x() - px(CARD_PAD_RIGHT) - px(CARD_RING) / 2.0;
            self.paint_status_ring(
                layers,
                ctx,
                ink,
                (mix(pill_cx, card_cx), mid),
                mix(px(PILL_RING), px(CARD_RING)),
                mix(px(PILL_RING_STROKE), px(CARD_RING_STROKE)),
                fraction,
                ink.accent,
                alpha,
            )?;
        }
        if t < 0.999 {
            self.paint_status_tile(layers, ctx, ink, content, place.mark_x - place.mark / 2.0, mid, place.mark, alpha * (1.0 - t))?;
        }
        if t > 0.001 {
            self.paint_status_disc_mark(layers, ctx, ink, content, place.mark_x, mid, place.mark, alpha * t)?;
        }
        self.paint_status_line(
            layers,
            &fonts.title,
            fonts.title_metrics,
            &content.name,
            place.name_x.max(0.0) as usize,
            place.name_y.max(0.0) as usize,
            place.name_width as usize,
            ink.text.mul_alpha(alpha),
        )
    }

    fn pill_trail_width(
        &self,
        fonts: &Fonts,
        content: &CardContent,
        px: &impl Fn(f32) -> f32,
    ) -> anyhow::Result<f32> {
        // A ring says how far on its own; the number is the card's.
        let text = if content.short.is_empty() || content.bar.is_some() {
            0.0
        } else {
            self.sidebar_text_width(&fonts.body, &content.short)?.ceil()
        };
        let lead = match (content.tone, content.bar) {
            (Tone::Waiting, _) => px(WAIT_DOT),
            (_, Some(_)) => px(PILL_RING),
            (Tone::Done, None) => px(14.0),
            _ => 0.0,
        };
        let width = if lead > 0.0 && text > 0.0 {
            lead + px(6.0) + text
        } else {
            lead + text
        };
        Ok(if width > 0.0 { px(PILL_GAP) + width } else { 0.0 })
    }

    /// A black capsule, as wide and tall as `rect`, at `alpha`.
    fn paint_status_surface(
        &self,
        layers: &mut crate::quad::TripleLayerQuadAllocator,
        ctx: &DrawContext,
        ink: &Ink,
        rect: RectF,
        alpha: f32,
    ) -> anyhow::Result<()> {
        if alpha <= 0.001 || rect.size.width <= 0.0 || rect.size.height <= 0.0 {
            return Ok(());
        }
        // Every surface here is a capsule, its corners half its height, and
        // the height changes all through an animation. Corners and shadows
        // are pictures, made once per radius and kept: radii on steps, so an
        // animation goes through a handful of them rather than making one
        // every frame. At rest every surface's radius is on a step.
        let radius = rect.size.height.min(rect.size.width) / 2.0;
        let step = RADIUS_STEP * POINT * self.ui_f32(1.0);
        let radius = if radius > step {
            (radius / step + 0.001).floor() * step
        } else {
            radius
        };
        // A wide, faint shadow and a tight, darker one close under the edge,
        // as a lit surface casts. No rim: black on the window is edge
        // enough.
        ctx.draw_shadow(
            layers,
            0,
            rect,
            radius,
            ctx.px(9.0),
            ctx.px(6.0),
            ink.shadow.mul_alpha(0.42 * alpha),
        )
        .context("dynamic status shadow")?;
        ctx.draw_shadow(
            layers,
            0,
            rect,
            radius,
            ctx.px(2.0),
            ctx.px(1.5),
            ink.shadow.mul_alpha(0.28 * alpha),
        )
        .context("dynamic status shadow")?;
        ctx.draw_rounded_rect(
            layers,
            0,
            rect.origin.x,
            rect.origin.y,
            rect.size.width,
            rect.size.height,
            ink.fill.mul_alpha(alpha),
            radius,
        )
        .context("dynamic status surface")
    }

    /// Where the pill has its mark and name, in `rect`.
    fn pill_head(&self, fonts: &Fonts, rect: RectF, content: &CardContent, px: &impl Fn(f32) -> f32) -> anyhow::Result<Head> {
        let mid = rect.min_y() + rect.size.height / 2.0;
        let x = rect.min_x() + px(PILL_PAD_LEFT);
        let name_x = x + px(PILL_MARK + PILL_GAP);
        let trail = self.pill_trail_width(fonts, content, px)?;
        Ok(Head {
            mark_x: x + px(PILL_MARK) / 2.0,
            mark: px(PILL_MARK),
            name_x,
            name_y: mid - fonts.title_line / 2.0,
            name_width: (rect.max_x() - px(PILL_PAD_RIGHT) - trail - name_x).max(0.0),
        })
    }

    /// The folded pill's content: the mark, the name, and its state. Without
    /// `head`, the mark and name are left to be drawn on their way to the
    /// card (`paint_status_head`).
    #[allow(clippy::too_many_arguments)]
    fn paint_status_pill(
        &self,
        layers: &mut crate::quad::TripleLayerQuadAllocator,
        ctx: &DrawContext,
        fonts: &Fonts,
        ink: &Ink,
        rect: RectF,
        content: &CardContent,
        alpha: f32,
        waited: Option<Duration>,
        head: bool,
        ring: bool,
        px: &impl Fn(f32) -> f32,
    ) -> anyhow::Result<()> {
        let mid = rect.min_y() + rect.size.height / 2.0;
        let place = self.pill_head(fonts, rect, content, px)?;
        if head {
            self.paint_status_tile(layers, ctx, ink, content, place.mark_x - place.mark / 2.0, mid, place.mark, alpha)?;
            self.paint_status_line(
                layers,
                &fonts.title,
                fonts.title_metrics,
                &content.name,
                place.name_x.max(0.0) as usize,
                place.name_y.max(0.0) as usize,
                place.name_width as usize,
                ink.text.mul_alpha(alpha),
            )?;
        }
        let name_x = place.name_x;
        let name_width = place.name_width;
        let trail = self.pill_trail_width(fonts, content, px)?;
        if trail <= 0.0 {
            return Ok(());
        }

        let mut x = name_x + name_width + px(PILL_GAP);
        let color = tone_color(content.tone, ink);
        match (content.tone, content.bar) {
            (Tone::Waiting, _) => {
                let cx = x + px(WAIT_DOT) / 2.0;
                // A halo round the dot breathing, for the first few seconds
                // of the wait. One size, its light coming and going: a ring
                // of a new size every frame would be a new picture every
                // frame.
                if let Some(waited) = waited.filter(|waited| *waited < PULSE_FOR) {
                    let phase = (waited.as_secs_f32() % PULSE.as_secs_f32()) / PULSE.as_secs_f32();
                    let breath = 0.5 - 0.5 * (phase * std::f32::consts::TAU).cos();
                    ctx.draw_disc(
                        layers,
                        1,
                        cx,
                        mid,
                        px(WAIT_DOT * 2.4),
                        color.mul_alpha(0.32 * breath * alpha),
                    )?;
                }
                ctx.draw_disc(layers, 1, cx, mid, px(WAIT_DOT), color.mul_alpha(alpha))?;
                x += px(WAIT_DOT + 6.0);
            }
            (_, Some(fraction)) => {
                if ring {
                    self.paint_status_ring(
                        layers,
                        ctx,
                        ink,
                        (x + px(PILL_RING) / 2.0, mid),
                        px(PILL_RING),
                        px(PILL_RING_STROKE),
                        fraction,
                        ink.accent,
                        alpha,
                    )?;
                }
                x += px(PILL_RING + 6.0);
            }
            (Tone::Done, None) => {
                ctx.draw_svg_icon(layers, SvgIcon::Check, x, mid - px(7.0), px(14.0), color.mul_alpha(alpha))
                    .context("dynamic status check")?;
                x += px(14.0);
            }
            _ => {}
        }
        if !content.short.is_empty() && content.bar.is_none() {
            let text_color = match content.tone {
                Tone::Waiting | Tone::Failed => color,
                Tone::Progress | Tone::Done => ink.muted,
            };
            let width = self.sidebar_text_width(&fonts.body, &content.short)?.ceil();
            self.paint_status_line(
                layers,
                &fonts.body,
                fonts.body_metrics,
                &content.short,
                x.max(0.0) as usize,
                (mid - fonts.body_line / 2.0).max(0.0) as usize,
                width as usize,
                text_color.mul_alpha(alpha),
            )?;
        }
        Ok(())
    }

    /// One thing's card: its mark, its name over a line of detail, and at
    /// the end how far it has got and what can be done about it.
    #[allow(clippy::too_many_arguments)]
    fn paint_status_card(
        &self,
        layers: &mut crate::quad::TripleLayerQuadAllocator,
        ctx: &DrawContext,
        fonts: &Fonts,
        ink: &Ink,
        rect: RectF,
        content: &CardContent,
        alpha: f32,
        buttons: &mut Vec<UIItem>,
        head: bool,
        ring: bool,
        px: &impl Fn(f32) -> f32,
    ) -> anyhow::Result<()> {
        if alpha <= 0.001 {
            return Ok(());
        }
        let mid = rect.min_y() + rect.size.height / 2.0;

        // The end, laid out from the right edge in.
        let mut end = rect.max_x() - px(CARD_PAD_RIGHT);
        for action in content.actions.iter().rev() {
            let button = euclid::rect(end - px(BUTTON), mid - px(BUTTON) / 2.0, px(BUTTON), px(BUTTON));
            let hovered = pointer_over(self, button);
            let (fill, glyph) = if action.primary {
                let fill = match content.tone {
                    Tone::Waiting => tone_color(Tone::Waiting, ink),
                    _ => ink.accent,
                };
                (fill, LinearRgba::with_srgba(20, 14, 0, 255))
            } else if hovered {
                (ink.button_hover, ink.text)
            } else {
                (ink.button, ink.glyph)
            };
            ctx.draw_disc(
                layers,
                1,
                button.min_x() + px(BUTTON) / 2.0,
                mid,
                px(BUTTON),
                fill.mul_alpha(alpha),
            )?;
            let glyph = if action.primary && content.tone != Tone::Waiting {
                ink.text
            } else {
                glyph
            };
            ctx.draw_svg_icon(
                layers,
                action.icon,
                button.min_x() + (px(BUTTON) - px(BUTTON_GLYPH)) / 2.0,
                mid - px(BUTTON_GLYPH) / 2.0,
                px(BUTTON_GLYPH),
                glyph.mul_alpha(alpha),
            )
            .context("dynamic status button")?;
            buttons.push(ui_item(button, action.item.clone()));
            end -= px(BUTTON + BUTTON_GAP);
        }
        if let Some(fraction) = content.bar {
            if content.actions.is_empty() {
                let size = px(CARD_RING);
                if ring {
                    self.paint_status_ring(
                        layers,
                        ctx,
                        ink,
                        (end - size / 2.0, mid),
                        size,
                        px(CARD_RING_STROKE),
                        fraction,
                        ink.accent,
                        alpha,
                    )?;
                }
                end -= size + px(BUTTON_GAP);
            }
            if let Some(fraction) = fraction {
                let number = format!("{}", (fraction * 100.0).floor() as u32);
                let percent_width = self.sidebar_text_width(&fonts.body, "%")?.ceil();
                let number_width = self.sidebar_text_width(&fonts.big, &number)?.ceil();
                let baseline = mid + fonts.big_line / 2.0;
                self.paint_status_line(
                    layers,
                    &fonts.body,
                    fonts.body_metrics,
                    "%",
                    (end - percent_width).max(0.0) as usize,
                    (baseline - fonts.body_line - px(3.0)).max(0.0) as usize,
                    percent_width as usize,
                    ink.muted.mul_alpha(alpha),
                )?;
                end -= percent_width + px(1.0);
                self.paint_status_line(
                    layers,
                    &fonts.big,
                    fonts.big_metrics,
                    &number,
                    (end - number_width).max(0.0) as usize,
                    (mid - fonts.big_line / 2.0).max(0.0) as usize,
                    number_width as usize,
                    ink.text.mul_alpha(alpha),
                )?;
                end -= number_width + px(BUTTON_GAP);
            }
        }

        let place = card_head(fonts, rect, end, px);
        if head {
            self.paint_status_disc_mark(layers, ctx, ink, content, place.mark_x, mid, place.mark, alpha)?;
            self.paint_status_line(
                layers,
                &fonts.title,
                fonts.title_metrics,
                &content.name,
                place.name_x.max(0.0) as usize,
                place.name_y.max(0.0) as usize,
                place.name_width as usize,
                ink.text.mul_alpha(alpha),
            )?;
        }
        let text_x = place.name_x;
        let text_width = place.name_width;
        let name_y = place.name_y;
        let detail_color = match content.tone {
            Tone::Waiting | Tone::Failed => tone_color(content.tone, ink),
            Tone::Progress | Tone::Done => ink.muted,
        };
        self.paint_status_line(
            layers,
            &fonts.body,
            fonts.body_metrics,
            &content.detail,
            text_x.max(0.0) as usize,
            (name_y + fonts.title_line + px(2.0)).max(0.0) as usize,
            text_width as usize,
            detail_color.mul_alpha(alpha),
        )?;
        Ok(())
    }

    /// The disc the second thing sits in beside the pill: a ring round its
    /// mark while it runs, and a badge counting whatever comes after it.
    /// The disc's surface and what is on it fade apart, so one thing can
    /// give way to another on the same disc.
    #[allow(clippy::too_many_arguments)]
    fn paint_status_satellite(
        &self,
        layers: &mut crate::quad::TripleLayerQuadAllocator,
        ctx: &DrawContext,
        fonts: &Fonts,
        ink: &Ink,
        disc: RectF,
        content: &CardContent,
        rest: usize,
        surface: f32,
        alpha: f32,
        px: &impl Fn(f32) -> f32,
    ) -> anyhow::Result<()> {
        self.paint_status_surface(layers, ctx, ink, disc, surface)?;
        // What is on the disc keeps its size and fades as the disc shrinks
        // under it: drawn smaller, every mark and ring would be a new
        // picture each frame.
        let alpha = alpha * smoothstep((disc.size.width / px(PILL_HEIGHT) - 0.7) / 0.3);
        if alpha <= 0.001 {
            return Ok(());
        }
        let cx = disc.min_x() + disc.size.width / 2.0;
        let cy = disc.min_y() + disc.size.height / 2.0;
        match content.bar {
            Some(fraction) => {
                self.paint_status_ring(
                    layers,
                    ctx,
                    ink,
                    (cx, cy),
                    px(SATELLITE_RING),
                    px(2.4),
                    fraction,
                    tone_color(content.tone, ink),
                    alpha,
                )?;
                self.paint_status_mark(layers, ctx, content, cx, cy, px(SATELLITE_MARK), tone_color(content.tone, ink).mul_alpha(alpha))?;
            }
            None => {
                let size = px(PILL_MARK);
                self.paint_status_tile(layers, ctx, ink, content, cx - size / 2.0, cy, size, alpha)?;
            }
        }
        if rest > 0 {
            let badge = px(BADGE);
            let bx = disc.min_x() + badge / 2.0 - px(3.0);
            let by = disc.min_y() + badge / 2.0 - px(3.0);
            ctx.draw_disc(layers, 1, bx, by, badge + px(4.0), ink.fill.mul_alpha(alpha))?;
            ctx.draw_disc(layers, 1, bx, by, badge, ink.badge.mul_alpha(alpha))?;
            let label = format!("+{rest}");
            let width = self.sidebar_text_width(&fonts.small, &label)?.ceil();
            self.paint_status_line(
                layers,
                &fonts.small,
                fonts.small_metrics,
                &label,
                (bx - width / 2.0).max(0.0) as usize,
                (by - fonts.small_line / 2.0).max(0.0) as usize,
                width as usize,
                ink.text.mul_alpha(alpha),
            )?;
        }
        Ok(())
    }

    /// The pill on top of the open stack: one that opens Status, or one that
    /// only says how many more there are.
    #[allow(clippy::too_many_arguments)]
    fn paint_status_more(
        &self,
        layers: &mut crate::quad::TripleLayerQuadAllocator,
        ctx: &DrawContext,
        fonts: &Fonts,
        ink: &Ink,
        rect: RectF,
        label: &str,
        opens_status: bool,
        alpha: f32,
    ) -> anyhow::Result<()> {
        if alpha <= 0.001 {
            return Ok(());
        }
        let scale = self.ui_f32(1.0);
        let px = move |value: f32| value * POINT * scale;
        let hovered = opens_status && pointer_over(self, rect);
        let color = if hovered { ink.text } else { ink.muted }.mul_alpha(alpha);
        let mid = rect.min_y() + rect.size.height / 2.0;
        let x = rect.min_x() + px(MORE_PAD);
        let icon = if opens_status {
            SvgIcon::ListChecks
        } else {
            SvgIcon::Ellipsis
        };
        ctx.draw_svg_icon(layers, icon, x, mid - px(MORE_ICON) / 2.0, px(MORE_ICON), color)
            .context("dynamic status list icon")?;
        let text_x = x + px(MORE_ICON + 6.0);
        self.paint_status_line(
            layers,
            &fonts.body,
            fonts.body_metrics,
            label,
            text_x.max(0.0) as usize,
            (mid - fonts.body_line / 2.0).max(0.0) as usize,
            (rect.max_x() - text_x).max(0.0) as usize,
            color,
        )
    }

    /// One line of the dynamic status's text. Too long, it goes out towards the end
    /// of its space, as a tab's title does, rather than stopping at "...".
    #[allow(clippy::too_many_arguments)]
    fn paint_status_line(
        &self,
        layers: &mut crate::quad::TripleLayerQuadAllocator,
        font: &Rc<LoadedFont>,
        metrics: RenderMetrics,
        text: &str,
        x: usize,
        y: usize,
        width: usize,
        color: LinearRgba,
    ) -> anyhow::Result<()> {
        if text.is_empty() {
            return Ok(());
        }
        self.paint_title_fading_with(layers, font, &metrics, text, x, y, width, color, 0.0, |fade| fade)
    }

    /// A progress ring centred on `center`, `size` across: as far as it has
    /// got, or, when there is no telling, a quarter of it turning -- on the
    /// pill and its discs as on the cards.
    #[allow(clippy::too_many_arguments)]
    fn paint_status_ring(
        &self,
        layers: &mut crate::quad::TripleLayerQuadAllocator,
        ctx: &DrawContext,
        ink: &Ink,
        center: (f32, f32),
        size: f32,
        stroke: f32,
        fraction: Option<f32>,
        color: LinearRgba,
        alpha: f32,
    ) -> anyhow::Result<()> {
        // A turning ring goes round smoothly, by the wall clock, so every
        // ring in every window is at the same angle.
        let turn = match fraction {
            Some(_) => 0.0,
            None => {
                let millis = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_millis();
                (millis % RING_TURN_MS) as f32 / RING_TURN_MS as f32
            }
        };
        paint_ring(
            ctx,
            layers,
            center,
            (size - stroke) / 2.0,
            stroke,
            fraction,
            turn,
            LinearRgba::with_srgba(255, 255, 255, 20).mul_alpha(alpha),
            color.mul_alpha(alpha),
            ink.fill.mul_alpha(alpha),
        )
    }

    /// A thing's mark on the pill: a transfer's icon on a tinted tile, or a
    /// program's own mark at the tile's size. `x` is the tile's left edge.
    #[allow(clippy::too_many_arguments)]
    fn paint_status_tile(
        &self,
        layers: &mut crate::quad::TripleLayerQuadAllocator,
        ctx: &DrawContext,
        ink: &Ink,
        content: &CardContent,
        x: f32,
        mid: f32,
        size: f32,
        alpha: f32,
    ) -> anyhow::Result<()> {
        let color = tone_color(content.tone, ink);
        if let Mark::Icon(_) = content.mark {
            ctx.draw_rounded_rect(
                layers,
                1,
                x,
                mid - size / 2.0,
                size,
                size,
                color.mul_alpha(0.13 * alpha),
                size * 0.3,
            )
            .context("dynamic status tile")?;
            return self.paint_status_mark(layers, ctx, content, x + size / 2.0, mid, size * 0.65, color.mul_alpha(alpha));
        }
        self.paint_status_mark(layers, ctx, content, x + size / 2.0, mid, size, color.mul_alpha(alpha))
    }

    /// A thing's mark on its card: in a disc, tinted by its state for a
    /// transfer, dark behind a program's own.
    #[allow(clippy::too_many_arguments)]
    fn paint_status_disc_mark(
        &self,
        layers: &mut crate::quad::TripleLayerQuadAllocator,
        ctx: &DrawContext,
        ink: &Ink,
        content: &CardContent,
        cx: f32,
        cy: f32,
        size: f32,
        alpha: f32,
    ) -> anyhow::Result<()> {
        let color = tone_color(content.tone, ink);
        let (disc, mark) = match content.mark {
            Mark::Icon(_) => (color.mul_alpha(0.13), 0.5),
            // A pane's tab card is a disc already: drawn at the full size,
            // with nothing behind to show as a rim.
            Mark::Pane(_) => return self.paint_status_mark(layers, ctx, content, cx, cy, size, color.mul_alpha(alpha)),
        };
        ctx.draw_disc(layers, 1, cx, cy, size, disc.mul_alpha(alpha))?;
        self.paint_status_mark(layers, ctx, content, cx, cy, size * mark, color.mul_alpha(alpha))
    }

    /// A thing's mark alone, centred on (`cx`, `cy`).
    #[allow(clippy::too_many_arguments)]
    fn paint_status_mark(
        &self,
        layers: &mut crate::quad::TripleLayerQuadAllocator,
        ctx: &DrawContext,
        content: &CardContent,
        cx: f32,
        cy: f32,
        size: f32,
        color: LinearRgba,
    ) -> anyhow::Result<()> {
        let x = (cx - size / 2.0).round().max(0.0);
        let y = (cy - size / 2.0).round().max(0.0);
        match content.mark {
            Mark::Icon(icon) => ctx
                .draw_svg_icon(layers, icon, cx - size / 2.0, cy - size / 2.0, size, color)
                .context("dynamic status icon"),
            // Program marks fade with what they stand beside, as content
            // giving way to other content does.
            Mark::Pane(pane_id) => {
                self.paint_pane_card_faded(
                    layers,
                    pane_id,
                    x as usize,
                    y as usize,
                    size.round() as usize,
                    color.3,
                )
            }
        }
    }

    /// Everything that could be on the dynamic status now, most pressing first.
    /// Programs in the panes on screen are left out -- their own screen
    /// already says it -- and so are agents while the Status panel lists
    /// them.
    ///
    /// Every frame of every window comes through here, flooding terminals
    /// included, and almost always there is nothing to show: that answer
    /// costs no allocation. The panes on screen are only listed once there
    /// is something to filter.
    fn dynamic_status_items(&mut self, now: Instant) -> Vec<StatusItem> {
        let update = if update_capsule::dismissed() {
            None
        } else {
            crate::settings_window::update_capsule_status()
        };
        self.update_capsule.note(now, update.as_ref());
        // The Status panel lists them: none of its own then.
        let listed = self.right_sidebar_width() > 0
            && self.right_sidebar_mode == crate::termwindow::RightSidebarMode::Agents;
        if !self.dynamic_status.finished.seeded {
            let mux = Mux::get();
            self.dynamic_status.finished.seed(
                now,
                mux.iter_panes()
                    .into_iter()
                    .map(|pane| (pane.pane_id(), pane.foreground_program())),
            );
        }
        self.dynamic_status.finished.settle(now, listed);
        if let Some(due) = self.dynamic_status.finished.deadline() {
            self.update_next_frame_time(Some(due));
        }
        let any_finished = !self.dynamic_status.finished.shown.is_empty();
        let any_task = self
            .dynamic_status
            .finished
            .runs
            .values()
            .any(|run| run.reading.is_some());
        let has_transfers = self.has_transfer_cards();
        let agents_listed = self.right_sidebar_width() > 0
            && self.right_sidebar_mode == crate::termwindow::RightSidebarMode::Agents;
        let show = crate::native_settings::agent_status_display();
        let agents = (crate::agent_status::enabled() && !agents_listed)
            .then(|| self.status_agents(now));
        let any_agent = agents.as_ref().is_some_and(|agents| {
            agents
                .iter()
                .any(|agent| status_wants(agent.state, agent.report.as_ref(), show))
        });
        self.refresh_status_panes(now);
        let any_pane = self
            .dynamic_status
            .panes
            .as_ref()
            .is_some_and(|(_, panes)| !panes.is_empty());
        if !has_transfers && !any_agent && !any_pane && update.is_none() && !any_finished && !any_task {
            self.dynamic_status.agent_marks.clear();
            return Vec::new();
        }

        let mut items: Vec<StatusItem> = if has_transfers {
            self.transfer_card_views()
                .into_iter()
                .map(StatusItem::Transfer)
                .collect()
        } else {
            Vec::new()
        };
        items.extend(update.map(StatusItem::Update));
        let on_screen: HashSet<PaneId> = if any_agent || any_pane || any_finished || any_task {
            self.get_panes_to_render()
                .iter()
                .map(|positioned| positioned.pane.pane_id())
                .collect()
        } else {
            HashSet::new()
        };
        self.dynamic_status.agent_marks.clear();
        let all_agents = agents.clone().unwrap_or_else(|| self.status_agents(now));
        if let Some(agents) = agents.filter(|_| any_agent) {
            for (index, agent) in agents.iter().enumerate() {
                if on_screen.contains(&agent.pane_id) {
                    continue;
                }
                let Some(mark) = status_mark(agent.state, agent.report.as_ref(), &agent.title, show)
                else {
                    continue;
                };
                self.dynamic_status.agent_marks.insert(agent.pane_id, mark);
                items.push(StatusItem::Agent {
                    agents: Arc::clone(&agents),
                    index,
                    here: agent.window_id == Some(self.mux_window_id),
                });
            }
        }
        if any_pane {
            // A pane an agent runs in is the agent's to show, whatever it
            // says on the dynamic status or not. So is one a tool with task
            // rules runs in, once its rules read anything, whatever progress
            // it reports too: one thing, said once.
            let agent_panes: HashSet<PaneId> = all_agents.iter().map(|agent| agent.pane_id).collect();
            let runs = &self.dynamic_status.finished.runs;
            let task_pane = |pane_id: &PaneId| {
                runs.get(pane_id)
                    .and_then(|run| run.reading.as_ref())
                    .is_some_and(reading_says_anything)
            };
            if let Some((_, panes)) = &self.dynamic_status.panes {
                items.extend(
                    panes
                        .iter()
                        .filter(|pane| {
                            !agent_panes.contains(&pane.pane_id)
                                && !task_pane(&pane.pane_id)
                                && !on_screen.contains(&pane.pane_id)
                        })
                        .cloned()
                        .map(StatusItem::Pane),
                );
            }
        }
        if any_finished {
            // One in sight has been seen, and one no longer in the snapshot
            // has gone with its pane: either is done with.
            let mux = Mux::get();
            self.dynamic_status.finished.shown.retain(|notice| {
                !on_screen.contains(&notice.pane_id)
                    && match notice.command {
                        Some(_) => mux.get_pane(notice.pane_id).is_some(),
                        None => all_agents.iter().any(|status| status.pane_id == notice.pane_id),
                    }
            });
            for agent in &self.dynamic_status.finished.shown {
                if let Some(command) = &agent.command {
                    let here = mux
                        .resolve_pane_id(agent.pane_id)
                        .is_some_and(|(_, window_id, _)| window_id == self.mux_window_id);
                    items.push(StatusItem::Command {
                        notice: agent.clone(),
                        command: command.clone(),
                        here,
                    });
                    continue;
                }
                let Some(index) = all_agents
                    .iter()
                    .position(|status| status.pane_id == agent.pane_id)
                else {
                    continue;
                };
                items.push(StatusItem::Finished {
                    agent: agent.clone(),
                    agents: Arc::clone(&all_agents),
                    index,
                    here: all_agents[index].window_id == Some(self.mux_window_id),
                });
            }
        }
        if any_task {
            // A tool out of sight, as its task rules last read it; one on
            // screen tells the user itself. Rules that have read nothing
            // give way to the progress the tool reports, if it does.
            let mux = Mux::get();
            for (pane_id, run) in &self.dynamic_status.finished.runs {
                let Some(reading) = &run.reading else {
                    continue;
                };
                if on_screen.contains(pane_id) {
                    continue;
                }
                if !reading_says_anything(reading)
                    && items.iter().any(|item| matches!(item, StatusItem::Pane(pane) if pane.pane_id == *pane_id))
                {
                    continue;
                }
                let here = mux
                    .resolve_pane_id(*pane_id)
                    .is_some_and(|(_, window_id, _)| window_id == self.mux_window_id);
                items.push(StatusItem::Task {
                    pane_id: *pane_id,
                    name: reading.name.clone(),
                    reading: reading.clone(),
                    since: run.state_since,
                    here,
                });
            }
        }
        // An agent's state began at a whole second on its host's clock:
        // placed on this machine's once, while it stays that state.
        let unix_now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |since| since.as_secs());
        let dynamic = &mut self.dynamic_status;
        dynamic
            .agent_since
            .retain(|pane_id, _| keys_contain_pane(&items, *pane_id));
        for item in &items {
            if let StatusItem::Agent { agents, index, .. } = item {
                let agent = &agents[*index];
                let placed = dynamic
                    .agent_since
                    .get(&agent.pane_id)
                    .is_some_and(|(since, _)| *since == agent.since_unix);
                if !placed {
                    let age = Duration::from_secs(unix_now.saturating_sub(agent.since_unix));
                    let at = now.checked_sub(age).unwrap_or(now);
                    dynamic.agent_since.insert(agent.pane_id, (agent.since_unix, at));
                }
            }
        }
        let first_seen = &dynamic.first_seen;
        let agent_since = &dynamic.agent_since;
        items.sort_by(|a, b| {
            a.rank()
                .cmp(&b.rank())
                .then_with(|| {
                    b.since(now, first_seen, agent_since)
                        .cmp(&a.since(now, first_seen, agent_since))
                })
                .then_with(|| a.key().cmp(&b.key()))
        });
        items
    }

    /// The dynamic status's agent status snapshot, read again when it is stale.
    fn status_agents(&mut self, now: Instant) -> Arc<Vec<AgentPaneStatus>> {
        if let Some((at, agents)) = &self.dynamic_status.agents {
            if now.duration_since(*at) < AGENTS_STALE {
                return Arc::clone(agents);
            }
        }
        let agents = crate::agent_status::agent_panes();
        self.dynamic_status.agents = Some((now, Arc::clone(&agents)));
        agents
    }

    /// Read the panes' own progress reports again, if a pane announced a
    /// change since or the last reading is old.
    fn refresh_status_panes(&mut self, now: Instant) {
        if let Some((at, _)) = &self.dynamic_status.panes {
            if now.duration_since(*at) < PANE_STALE {
                return;
            }
        }
        let mux = Mux::get();
        let mut panes = Vec::new();
        for pane in mux.iter_panes() {
            let progress = pane.get_progress();
            if progress == Progress::None {
                continue;
            }
            let Some((_, window_id, _)) = mux.resolve_pane_id(pane.pane_id()) else {
                continue;
            };
            let workspace = mux
                .get_window(window_id)
                .map(|window| window.get_workspace().to_string())
                .unwrap_or_default();
            let place = crate::workspace_threads::thread_place_and_machine_for_workspace(&workspace)
                .map(|(place, _)| place)
                .unwrap_or(workspace);
            let raw_title = pane.get_title();
            let title =
                crate::termwindow::ui::status_icon::split_leading_legacy_progress_marker(
                    &raw_title,
                )
                .unwrap_or(&raw_title)
                .to_string();
            panes.push(PaneProgress {
                pane_id: pane.pane_id(),
                here: window_id == self.mux_window_id,
                title,
                place,
                progress,
            });
        }
        self.dynamic_status.panes = Some((now, panes));
    }

    /// A pane announced its progress (OSC 9;4). Read again on the next
    /// frame, and ask for that frame only when the dynamic status would show the
    /// change: a program announcing every step of the same percentage, or a
    /// pane on screen, costs no repaint.
    pub(crate) fn dynamic_status_progress_changed(&mut self, pane_id: PaneId, progress: &Progress) {
        let known = self
            .dynamic_status
            .panes
            .as_ref()
            .map(|(_, panes)| {
                panes
                    .iter()
                    .find(|pane| pane.pane_id == pane_id)
                    .map(|pane| &pane.progress)
            });
        // Known as it is: nothing to read again.
        match known {
            Some(Some(known)) if known == progress => return,
            Some(None) if *progress == Progress::None => return,
            _ => {}
        }
        self.dynamic_status.panes = None;
        let on_screen = self
            .get_panes_to_render()
            .iter()
            .any(|positioned| positioned.pane.pane_id() == pane_id);
        if !on_screen {
            self.invalidate_window();
        }
    }

    /// Whether an agent status change changes what the dynamic status shows: it
    /// appears, goes, or says something else. An agent reporting every step
    /// of its work repaints nothing while what the dynamic status shows of it stays
    /// the same.
    ///
    /// Every window hears of every change, and an agent at work reports
    /// several times a second: this reads only the pane that changed. When
    /// the answer is yes the dynamic status's snapshot is let go, and the next frame
    /// reads a new one -- once, from the cache all windows share.
    pub(crate) fn dynamic_status_agent_changed(&mut self, pane_id: PaneId) -> bool {
        if !crate::agent_status::enabled() {
            return false;
        }
        let now = Instant::now();
        self.dynamic_status.expire_agent_news(now);
        let shown = self.dynamic_status.agent_marks.get(&pane_id).copied();
        let status = Mux::get().get_pane(pane_id).and_then(|pane| {
            let status = pane.agent_status().filter(|status| !status.ended)?;
            Some((pane, status))
        });
        // Heard now, on this machine's clock: starting to wait, or failing,
        // is news for a while.
        if status
            .as_ref()
            .is_some_and(|(_, status)| matches!(status.state, AgentState::Blocked | AgentState::Error))
        {
            self.dynamic_status.agent_news.insert(pane_id, now);
        }
        let agents_listed = self.right_sidebar_width() > 0
            && self.right_sidebar_mode == crate::termwindow::RightSidebarMode::Agents;
        let finished = &mut self.dynamic_status.finished;
        let is_finished =
            |finished: &FinishedAgents| finished.shown.iter().any(|agent| agent.pane_id == pane_id);
        let was_finished = is_finished(finished);
        let state = status.as_ref().map(|(_, status)| status.state);
        if finished.note(now, pane_id, state) || was_finished != is_finished(finished) {
            // Just finished, or no longer: on or off the dynamic status now,
            // with a snapshot that knows the agent as it stands.
            self.dynamic_status.agents = None;
            return true;
        }
        let show = crate::native_settings::agent_status_display();
        let mark = status.filter(|_| !agents_listed).and_then(|(pane, status)| {
            if !status_wants(status.state, status.report.as_ref(), show) {
                return None;
            }
            let raw_title = pane.get_title();
            let title = crate::termwindow::ui::status_icon::split_leading_legacy_progress_marker(
                &raw_title,
            )
            .unwrap_or(&raw_title);
            status_mark(status.state, status.report.as_ref(), title, show)
        });
        if mark == shown {
            return false;
        }
        // What the dynamic status would show of it changed: the snapshot is out of
        // date even for an agent on screen now, which a thread switch can
        // put on the dynamic status any moment.
        self.dynamic_status.agents = None;
        // On screen, it is not on the dynamic status, unless it was a moment ago.
        shown.is_some()
            || !self
                .get_panes_to_render()
                .iter()
                .any(|positioned| positioned.pane.pane_id() == pane_id)
    }

    /// A pane's foreground program changed: a command started, or ended and
    /// gave the shell back. True when the window should paint.
    pub(crate) fn dynamic_status_program_changed(&mut self, pane_id: PaneId) -> bool {
        let mux = Mux::get();
        let Some(pane) = mux.get_pane(pane_id) else {
            return false;
        };
        let program = pane.foreground_program();
        let in_sight = self
            .get_panes_to_render()
            .iter()
            .any(|positioned| positioned.pane.pane_id() == pane_id);
        let place = || {
            mux.resolve_pane_id(pane_id)
                .and_then(|(_, window_id, _)| {
                    let workspace = mux.get_window(window_id)?.get_workspace().to_string();
                    crate::workspace_threads::thread_place_and_machine_for_workspace(&workspace)
                })
                .map(|(place, _)| place)
                .unwrap_or_default()
        };
        self.dynamic_status
            .finished
            .observe_program(Instant::now(), pane_id, program.as_ref(), in_sight, place)
    }

    /// A pane printed: the last output of a command running there marks
    /// when it ended.
    pub(crate) fn dynamic_status_pane_printed(&mut self, pane_id: PaneId) {
        self.dynamic_status.finished.printed(Instant::now(), pane_id);
    }

    /// The shell said how a command ended (OSC 133). True when the window
    /// should paint.
    pub(crate) fn dynamic_status_command_exit(&mut self, pane_id: PaneId, code: Option<i32>) -> bool {
        self.dynamic_status.finished.exit(Instant::now(), pane_id, code)
    }

    /// A pane went away: what the dynamic status read about panes and agents is out
    /// of date, and if it was on the dynamic status, it goes now rather than at the
    /// next repaint.
    pub(crate) fn dynamic_status_pane_removed(&mut self, pane_id: PaneId) {
        self.dynamic_status.finished.forget(pane_id);
        self.dynamic_status.agent_news.remove(&pane_id);
        self.dynamic_status.agent_marks.remove(&pane_id);
        self.dynamic_status.agent_since.remove(&pane_id);
        self.dynamic_status.agents = None;
        self.dynamic_status.panes = None;
        if self
            .dynamic_status
            .first_seen
            .contains_key(&StatusKey::Pane(pane_id))
        {
            self.invalidate_window();
        }
    }

    /// Open the Status panel, from the dynamic status's pill on top of the stack:
    /// switched to as its own tab switches to it, and docked as the sidebar
    /// button docks it, the terminal laid out again beside it.
    pub(crate) fn show_status_from_dynamic_status(&mut self, context: &dyn window::WindowOps) {
        self.set_right_sidebar_mode(crate::termwindow::RightSidebarMode::Agents, context);
        if self.right_sidebar_collapsed {
            self.toggle_right_sidebar();
            self.reflow_right_sidebar(context);
        }
    }
}

/// The state colours the sidebars and the Status panel use, so a colour
/// means the same thing everywhere.
fn tone_color(tone: Tone, ink: &Ink) -> LinearRgba {
    match tone {
        Tone::Progress => ink.accent,
        Tone::Done => crate::thread_marks::FINISHED_COLOR,
        Tone::Failed => ink.danger,
        Tone::Waiting => crate::thread_marks::WAITING_COLOR,
    }
}

fn status_content(item: &StatusItem) -> CardContent {
    let go_to = |pane_id: PaneId, here: bool, primary: bool| CardAction {
        icon: SvgIcon::ArrowRight,
        primary,
        item: UIItemType::DynamicStatusReveal(pane_id, here),
    };
    match item {
        StatusItem::Transfer(view) => card_content(view),
        StatusItem::Update(status) => update_capsule::content(status),
        StatusItem::Finished {
            agents,
            index,
            here,
            ..
        } => {
            let agent = &agents[*index];
            let done = crate::i18n::tr("right-agents-state-done");
            let name = crate::agent_status::display_name(&agent.agent_id);
            // Named as its agent card was -- by what it worked on -- with
            // the thread said under it when that is another window's.
            let mut detail = vec![name.clone(), done.clone()];
            if !*here && !agent.place.is_empty() {
                detail.push(agent.place.clone());
            }
            CardContent {
                mark: Mark::Pane(agent.pane_id),
                tone: Tone::Done,
                name: if agent.title.is_empty() {
                    name
                } else {
                    agent.title.clone()
                },
                detail: detail.join(" · "),
                short: done,
                bar: None,
                actions: vec![go_to(agent.pane_id, *here, true)],
            }
        }
        StatusItem::Command {
            notice,
            command,
            here,
        } => {
            let failed = command.failed();
            let state = crate::i18n::tr(if failed {
                "right-agents-state-error"
            } else {
                "right-agents-state-done"
            });
            let mut detail = vec![state.clone()];
            if let Some(code) = command.exit.filter(|_| failed) {
                let mut args = fluent_bundle::FluentArgs::new();
                args.set("code", code);
                detail.push(crate::i18n::tr_args("dynamic-status-exit", &args));
            }
            detail.push(took_label(command.took));
            if !*here && !command.place.is_empty() {
                detail.push(command.place.clone());
            }
            CardContent {
                mark: Mark::Pane(notice.pane_id),
                tone: if failed { Tone::Failed } else { Tone::Done },
                name: command.label.clone(),
                detail: detail.join(" · "),
                short: state,
                bar: None,
                actions: vec![go_to(notice.pane_id, *here, true)],
            }
        }
        StatusItem::Task {
            pane_id,
            name,
            reading,
            here,
            ..
        } => {
            let (tone, state) = match reading.state {
                TaskState::Running => (Tone::Progress, "right-agents-state-working"),
                TaskState::Waiting => (Tone::Waiting, "right-agents-state-blocked"),
                TaskState::Error => (Tone::Failed, "right-agents-state-error"),
            };
            let state = crate::i18n::tr(state);
            let fraction = reading
                .percent
                .map(|percent| f32::from(percent) / 100.0)
                .or(reading.count.map(|(done, total)| done as f32 / total as f32));
            let short = match (reading.percent, reading.count) {
                _ if tone != Tone::Progress => state.clone(),
                (Some(percent), _) => format!("{percent}%"),
                (None, Some((done, total))) => format!("{done}/{total}"),
                (None, None) => state.clone(),
            };
            CardContent {
                mark: Mark::Pane(*pane_id),
                tone,
                name: name.clone(),
                detail: reading.detail.clone().unwrap_or_else(|| state.clone()),
                short,
                bar: (tone == Tone::Progress).then_some(fraction),
                actions: if tone == Tone::Progress {
                    Vec::new()
                } else {
                    vec![go_to(*pane_id, *here, true)]
                },
            }
        }
        StatusItem::Agent {
            agents,
            index,
            here,
        } => {
            let agent = &agents[*index];
            let show = crate::native_settings::agent_status_display();
            let (tone, state) = match agent.shown_state() {
                ShownState::Error => (Tone::Failed, "right-agents-state-error"),
                ShownState::Blocked => (Tone::Waiting, "right-agents-state-blocked"),
                _ => (Tone::Progress, "right-agents-state-working"),
            };
            let state = crate::i18n::tr(state);
            let progress = match tone {
                Tone::Progress => crate::agent_status::report_progress(agent, show),
                _ => None,
            };
            // The thread is named only when it is another window's; in this
            // one its tab is a click away and its name says nothing new.
            let mut detail = Vec::new();
            if !*here && !agent.place.is_empty() {
                detail.push(agent.place.clone());
            }
            match crate::agent_status::reason_line(agent) {
                Some(reason) if tone != Tone::Progress => {
                    detail.push(state.clone());
                    detail.push(reason);
                }
                Some(reason) => detail.push(reason),
                None => detail.push(state.clone()),
            }
            CardContent {
                mark: Mark::Pane(agent.pane_id),
                tone,
                name: if agent.title.is_empty() {
                    crate::agent_status::display_name(&agent.agent_id)
                } else {
                    agent.title.clone()
                },
                detail: detail.join(" · "),
                short: match progress {
                    Some(percent) => format!("{percent}%"),
                    None => state,
                },
                bar: progress.map(|percent| Some(f32::from(percent) / 100.0)),
                actions: match tone {
                    Tone::Progress => Vec::new(),
                    _ => vec![go_to(agent.pane_id, *here, tone == Tone::Waiting)],
                },
            }
        }
        StatusItem::Pane(pane) => {
            let (tone, short, bar) = match pane.progress {
                Progress::Percentage(percent) => (
                    Tone::Progress,
                    format!("{percent}%"),
                    Some(Some(f32::from(percent) / 100.0)),
                ),
                Progress::Error(_) => (
                    Tone::Failed,
                    crate::i18n::tr("right-agents-state-error"),
                    None,
                ),
                Progress::Indeterminate | Progress::None => (
                    Tone::Progress,
                    crate::i18n::tr("right-agents-state-working"),
                    Some(None),
                ),
            };
            let name = if pane.title.is_empty() {
                pane.place.clone()
            } else {
                pane.title.clone()
            };
            // The card's end already says how far; the line under the name
            // says where, or, in this window, what state it is in.
            let detail = if !pane.here && !pane.place.is_empty() && pane.place != name {
                pane.place.clone()
            } else {
                match pane.progress {
                    Progress::Error(_) => short.clone(),
                    _ => crate::i18n::tr("right-agents-state-working"),
                }
            };
            CardContent {
                mark: Mark::Pane(pane.pane_id),
                tone,
                name,
                detail,
                short,
                bar,
                actions: match tone {
                    Tone::Failed => vec![go_to(pane.pane_id, pane.here, false)],
                    _ => Vec::new(),
                },
            }
        }
    }
}

/// Whether a tool's rules have read anything off its screen: a state, a
/// number or what it is doing, rather than only that it is running.
fn reading_says_anything(reading: &TaskReading) -> bool {
    reading.state != TaskState::Running
        || reading.percent.is_some()
        || reading.count.is_some()
        || reading.detail.is_some()
}

/// Where a card in `rect` has its mark and name, its text reaching to `end`.
fn card_head(fonts: &Fonts, rect: RectF, end: f32, px: &impl Fn(f32) -> f32) -> Head {
    let mid = rect.min_y() + rect.size.height / 2.0;
    let mark_left = rect.min_x() + px(CARD_PAD_LEFT);
    let name_x = mark_left + px(CARD_MARK + CARD_GAP);
    let block = fonts.title_line + px(2.0) + fonts.body_line;
    Head {
        mark_x: mark_left + px(CARD_MARK) / 2.0,
        mark: px(CARD_MARK),
        name_x,
        name_y: mid - block / 2.0,
        name_width: (end - px(CARD_GAP) + px(BUTTON_GAP) - name_x).max(0.0),
    }
}

fn lerp_rect(from: RectF, to: RectF, t: f32) -> RectF {
    let lerp = |a: f32, b: f32| a + (b - a) * t;
    let width = lerp(from.size.width, to.size.width).max(0.0);
    let height = lerp(from.size.height, to.size.height).max(0.0);
    // Anchored at the bottom-right corner both share, so an overshoot grows
    // up and to the left, away from the corner.
    let right = lerp(from.max_x(), to.max_x());
    let bottom = lerp(from.max_y(), to.max_y());
    euclid::rect(right - width, bottom - height, width, height)
}

/// Where a dynamic status arriving (or, `arriving` false, going) is at presence
/// `t`: how big the disc it comes out of is, how far it has stretched from
/// that into its shape, how much of its content shows, and how much of its
/// surface. Arriving, the disc pops a little past its size and the stretch
/// a little past the pill's; going runs the other way round, its content
/// first.
fn presence_phases(t: f32, arriving: bool) -> (f32, f32, f32, f32) {
    if arriving {
        let disc = Easing::settling(POP_OVERSHOOT).apply(t / 0.4);
        let stretch = Easing::settling(RESIZE_OVERSHOOT).apply((t - 0.3) / 0.5);
        let content = smoothstep((t - 0.6) / 0.4);
        let surface = smoothstep(t / 0.15);
        (disc, stretch, content, surface)
    } else {
        let gone = 1.0 - t;
        let content = 1.0 - smoothstep(gone / 0.3);
        let stretch = 1.0 - ease_in((gone - 0.15) / 0.45);
        let disc = 1.0 - ease_in((gone - 0.5) / 0.5);
        let surface = 1.0 - smoothstep((gone - 0.75) / 0.25);
        (disc, stretch, content, surface)
    }
}

/// How much of the old content and of the new show, `at` of the way
/// through one giving way to the other: the old goes over the first half,
/// the new comes in over the last two thirds.
fn swap_mix(at: Option<f32>) -> (f32, f32) {
    match at {
        None => (0.0, 1.0),
        Some(at) => (1.0 - smoothstep(at / 0.5), smoothstep((at - 0.35) / 0.65)),
    }
}

/// `rect` scaled by `by` about its centre.
fn scale_rect(rect: RectF, by: f32) -> RectF {
    let width = (rect.size.width * by).max(0.0);
    let height = (rect.size.height * by).max(0.0);
    let center = rect.center();
    euclid::rect(center.x - width / 2.0, center.y - height / 2.0, width, height)
}

/// Whether an agent in `pane_id` is among `items`.
fn keys_contain_pane(items: &[StatusItem], pane_id: PaneId) -> bool {
    items
        .iter()
        .any(|item| matches!(item, StatusItem::Agent { agents, index, .. } if agents[*index].pane_id == pane_id))
}

/// Which of the stack's rising clocks a card at `slot` goes by.
fn slot_index(slot: f32) -> usize {
    (slot.round().max(0.0) as usize).min(MAX_ABOVE)
}

fn ease_in(t: f32) -> f32 {
    let t = t.clamp(0.0, 1.0);
    t * t * t
}

fn smoothstep(t: f32) -> f32 {
    let t = t.clamp(0.0, 1.0);
    t * t * (3.0 - 2.0 * t)
}

fn pointer_over(window: &crate::TermWindow, rect: RectF) -> bool {
    window.is_pointer_over_ui_rect(
        rect.origin.x.max(0.0) as usize,
        rect.origin.y.max(0.0) as usize,
        rect.size.width as usize,
        rect.size.height as usize,
    )
}

/// A hit area for `rect`, cut where it runs off the window's top or left
/// edge rather than moved inside it.
fn ui_item(rect: RectF, item_type: UIItemType) -> UIItem {
    let x = rect.origin.x.max(0.0);
    let y = rect.origin.y.max(0.0);
    UIItem {
        x: x as usize,
        y: y as usize,
        width: (rect.max_x() - x).max(0.0) as usize,
        height: (rect.max_y() - y).max(0.0) as usize,
        item_type,
    }
}

/// One turn of an indeterminate ring, in milliseconds.
const RING_TURN_MS: u128 = 1000;

/// Whether the dynamic status shows an agent in `state`: waiting on the user,
/// stopped on an error, or saying how far it has got. From the parts a pane
/// reports, so a status change can be judged without the whole snapshot.
fn status_wants(
    state: AgentState,
    report: Option<&ProgramReport>,
    show: crate::native_settings::NativeAgentStatusDisplay,
) -> bool {
    match state {
        AgentState::Blocked | AgentState::Error => true,
        AgentState::Working => {
            show.progress && report.and_then(|report| report.progress).is_some()
        }
        AgentState::Idle | AgentState::Unknown => false,
    }
}

/// What the dynamic status shows of an agent, as a number that changes when that
/// does: `None` when the dynamic status would not show it at all. The line under
/// the name is left out while it works -- it changes with every step of the
/// work, and the dynamic status is not where that is followed. Worked out alike from
/// the snapshot, when painting, and from the pane, when it reports a change.
/// How long a command took, to the second under an hour.
fn took_label(took: Duration) -> String {
    let seconds = took.as_secs();
    let mut args = fluent_bundle::FluentArgs::new();
    let key = if seconds < 60 {
        args.set("seconds", seconds);
        "dynamic-status-took-seconds"
    } else if seconds < 3600 {
        args.set("minutes", seconds / 60);
        args.set("seconds", seconds % 60);
        "dynamic-status-took-minutes"
    } else {
        args.set("hours", seconds / 3600);
        args.set("minutes", seconds % 3600 / 60);
        "dynamic-status-took-hours"
    };
    crate::i18n::tr_args(key, &args)
}

/// What a pane's foreground program says runs in it: the command, or
/// `None` for a shell at its prompt -- a shell running nothing of its own.
fn running_command(program: &thinkterm_proto::ForegroundProgram) -> Option<String> {
    let shell = matches!(
        program.executable.as_str(),
        "sh" | "bash" | "zsh" | "fish" | "dash" | "ash" | "busybox" | "ksh" | "mksh" | "tcsh"
            | "csh" | "nu" | "elvish" | "xonsh" | "pwsh" | "-zsh" | "-bash" | "-sh" | "-fish"
    );
    match (&program.runs, shell) {
        (None, true) => None,
        (Some(runs), _) => Some(runs.clone()),
        (None, false) => Some(program.executable.clone()),
    }
}

fn status_mark(
    state: AgentState,
    report: Option<&ProgramReport>,
    title: &str,
    show: crate::native_settings::NativeAgentStatusDisplay,
) -> Option<u64> {
    if !status_wants(state, report, show) {
        return None;
    }
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    std::mem::discriminant(&state).hash(&mut hasher);
    match state {
        AgentState::Working => report.and_then(|report| report.progress).hash(&mut hasher),
        _ => {
            report
                .and_then(|report| report.kind)
                .filter(|_| show.reason)
                .map(crate::agent_status::blocked_kind_key)
                .hash(&mut hasher);
            report
                .and_then(|report| report.msg.as_deref())
                .filter(|_| show.description)
                .hash(&mut hasher);
        }
    }
    title.hash(&mut hasher);
    Some(hasher.finish())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rules_that_read_nothing_leave_a_tool_to_its_own_progress() {
        let mut reading = TaskReading {
            name: "apt".into(),
            ..TaskReading::default()
        };
        assert!(!reading_says_anything(&reading));
        reading.detail = Some("Setting up libssl3".into());
        assert!(reading_says_anything(&reading));
        let waiting = TaskReading {
            state: TaskState::Waiting,
            ..TaskReading::default()
        };
        assert!(reading_says_anything(&waiting));
    }

    #[test]
    fn an_agent_that_worked_a_while_and_stayed_stopped_is_finished() {
        let now = Instant::now();
        let at = |secs: f32| now + Duration::from_secs_f32(secs);
        let mut finished = FinishedAgents::default();
        assert!(!finished.note(at(0.0), 1, Some(AgentState::Working)));
        // A quick answer is not news.
        assert!(finished.note(at(2.0), 1, Some(AgentState::Idle)));
        assert!(!finished.settle(at(3.0), false));
        assert!(!finished.note(at(10.0), 1, Some(AgentState::Working)));
        // Flickering to idle mid-task: the same run, nothing shown.
        assert!(finished.note(at(12.0), 1, Some(AgentState::Idle)));
        assert!(!finished.settle(at(12.5), false));
        assert!(!finished.note(at(12.6), 1, Some(AgentState::Working)));
        assert!(!finished.settle(at(14.0), false));
        assert!(finished.shown.is_empty());
        // Stopping for good, counted from the start of the run.
        assert!(finished.note(at(16.0), 1, Some(AgentState::Idle)));
        assert_eq!(finished.deadline(), Some(at(16.0) + FINISHED_SETTLE));
        assert!(!finished.settle(at(16.5), false));
        assert!(finished.settle(at(16.9), false));
        assert_eq!(finished.shown.len(), 1);
        // More news of it, still idle, leaves it on.
        assert!(!finished.note(at(17.0), 1, Some(AgentState::Idle)));
        assert_eq!(finished.shown.len(), 1);
        // At work again, it is not finished any more.
        assert!(!finished.note(at(18.0), 1, Some(AgentState::Working)));
        assert!(finished.shown.is_empty());
        // Nor is one that stopped to wait.
        assert!(!finished.note(at(30.0), 1, Some(AgentState::Blocked)));
        assert!(finished.stopping.is_empty() && finished.working.is_empty());
        // And while the Status panel lists them, none of its own.
        assert!(!finished.note(at(31.0), 1, Some(AgentState::Working)));
        assert!(finished.note(at(40.0), 1, Some(AgentState::Idle)));
        assert!(!finished.settle(at(41.0), true));
        assert!(finished.shown.is_empty());
    }

    #[test]
    fn a_long_command_that_ends_out_of_sight_is_finished() {
        let now = Instant::now();
        let at = |secs: u64| now + Duration::from_secs(secs);
        let place = || "acme · Launch".to_string();
        let mut finished = FinishedAgents::default();
        // A quick one is not news.
        assert!(!finished.program(at(0), 1, Some("ls".into()), None, false, place));
        // (A plain shell's agent status says nothing about what runs.)
        assert!(!finished.note(at(1), 1, Some(AgentState::Unknown)));
        assert!(!finished.program(at(2), 1, None, None, false, place));
        // Nor is one that ends in front of the user.
        assert!(!finished.program(at(10), 1, Some("cargo".into()), None, false, place));
        assert!(!finished.program(at(40), 1, None, None, true, place));
        // Out of sight, with the shell's word on how it went first...
        assert!(!finished.program(at(50), 1, Some("cargo".into()), None, false, place));
        // ...the program it hands over to does not restart the clock, and
        // its last output -- the prompt -- is when it ended, however late
        // the shell is seen back.
        assert!(!finished.program(at(55), 1, Some("rustc".into()), None, false, place));
        finished.printed(at(79), 1);
        assert!(!finished.exit(at(80), 1, Some(101)));
        assert!(finished.program(at(81), 1, None, None, false, place));
        let command = finished.shown[0].command.clone().unwrap();
        assert_eq!(command.label, "cargo");
        assert_eq!(command.took, Duration::from_secs(29));
        assert!(command.failed());
        // ...or just after.
        assert!(!finished.program(at(90), 2, Some("npm".into()), None, false, place));
        // A print from long before the end is not the prompt that ended it.
        finished.printed(at(91), 2);
        assert!(finished.program(at(120), 2, None, None, false, place));
        assert!(finished.exit(at(121), 2, Some(0)));
        assert!(!finished.shown[1].command.as_ref().unwrap().failed());
        // News that no agent runs there leaves a command's notice alone.
        assert!(!finished.note(at(122), 2, None));
        assert!(!finished.note(at(122), 2, Some(AgentState::Unknown)));
        assert_eq!(finished.shown.len(), 2);
        // The next command in that pane takes its notice off.
        assert!(!finished.program(at(130), 2, Some("npm".into()), None, false, place));
        assert_eq!(finished.shown.len(), 1);
    }

    #[test]
    fn an_exit_survives_a_delayed_background_probe_and_stays_with_its_run() {
        let now = Instant::now();
        let mut finished = FinishedAgents::default();
        finished.program(now, 1, Some("cargo".into()), None, false, String::new);
        finished.exit(now + Duration::from_secs(30), 1, Some(101));
        assert!(finished.program(now + Duration::from_secs(36), 1, None, None, false, String::new));
        let command = finished.shown[0].command.as_ref().unwrap();
        assert_eq!(command.exit, Some(101));
        assert!(command.failed());
        finished.program(now + Duration::from_secs(40), 1, Some("cargo".into()), None, false, String::new);
        finished.program(now + Duration::from_secs(60), 1, None, None, false, String::new);
        assert_eq!(finished.shown[0].command.as_ref().unwrap().exit, None);
    }

    fn waiting_program() -> thinkterm_proto::ForegroundProgram {
        thinkterm_proto::ForegroundProgram {
            executable: "apt".into(),
            runs: None,
            task: Some(TaskReading {
                name: "apt".into(),
                state: TaskState::Waiting,
                ..TaskReading::default()
            }),
        }
    }

    #[test]
    fn an_ssh_prompt_clears_the_task_and_the_next_run_starts_fresh() {
        let now = Instant::now();
        let mut finished = FinishedAgents::default();
        let program = waiting_program();
        finished.observe_program(now, 1, Some(&program), false, String::new);
        // Even a short run must repaint to remove its waiting card.
        assert!(finished.observe_program(now + Duration::from_secs(2), 1, None, false, String::new));
        assert!(finished.runs.is_empty());
        assert!(finished.shown.is_empty());
        let later = now + Duration::from_secs(3);
        finished.observe_program(later, 1, Some(&program), false, String::new);
        assert_eq!(finished.runs[&1].since, later);
        assert!(finished.observe_program(now + Duration::from_secs(30), 1, None, false, String::new));
        assert!(finished.runs.is_empty());
        assert_eq!(finished.shown.len(), 1);
    }

    #[test]
    fn a_new_window_seeds_existing_tasks_without_restarting_known_runs() {
        let now = Instant::now();
        let mut finished = FinishedAgents::default();
        let program = waiting_program();
        finished.observe_program(now, 1, Some(&program), false, String::new);
        let later = now + Duration::from_secs(5);
        finished.seed(later, [(1, Some(program.clone())), (2, Some(program.clone())), (3, None)]);
        assert!(finished.seeded);
        assert_eq!(finished.runs.len(), 2);
        assert_eq!(finished.runs[&1].since, now);
        assert_eq!(finished.runs[&2].reading, program.task);
        assert_eq!(finished.runs[&2].state_since, later);
        assert!(finished.shown.is_empty());
    }

    #[test]
    fn the_status_sidebar_preserves_command_notices() {
        let now = Instant::now();
        let mut finished = FinishedAgents::default();
        finished.program(now, 1, Some("cargo".into()), None, false, String::new);
        finished.note(now, 2, Some(AgentState::Working));
        let later = now + Duration::from_secs(30);
        finished.program(later, 1, None, None, false, String::new);
        finished.note(later, 2, Some(AgentState::Idle));
        assert!(finished.settle(later + FINISHED_SETTLE, false));
        assert_eq!(finished.shown.len(), 2);
        finished.settle(later + FINISHED_SETTLE, true);
        assert_eq!(finished.shown.len(), 1);
        assert!(finished.shown[0].command.is_some());
        assert_eq!(finished.shown[0].pane_id, 1);
    }

    #[test]
    fn a_tool_that_ends_goes_by_its_rules_name_and_its_last_error() {
        let now = Instant::now();
        let mut finished = FinishedAgents::default();
        finished.program(now, 1, Some("brew".into()), None, false, String::new);
        // Handed over to Ruby running `brew.rb`, read by brew's rules: a
        // new reading out of sight wants a paint.
        let failing = TaskReading {
            name: "brew".to_string(),
            state: TaskState::Error,
            ..TaskReading::default()
        };
        assert!(finished.program(
            now + Duration::from_secs(1),
            1,
            Some("brew.rb".into()),
            Some(failing),
            false,
            String::new
        ));
        assert!(finished.program(now + Duration::from_secs(60), 1, None, None, false, String::new));
        let command = finished.shown[0].command.clone().unwrap();
        assert_eq!(command.label, "brew");
        assert!(command.failed());
    }

    #[test]
    fn a_tool_waiting_is_news_only_just_after_it_began_to() {
        let now = Instant::now();
        let task = |state: TaskState, since: Instant| StatusItem::Task {
            pane_id: 1,
            name: "apt".to_string(),
            reading: TaskReading {
                name: "apt".to_string(),
                state,
                ..TaskReading::default()
            },
            since,
            here: true,
        };
        let news = HashMap::new();
        assert!(task(TaskState::Waiting, now).is_news(now, &news));
        // Waiting a while, and only now in view: not news.
        let earlier = now - Duration::from_secs(60);
        assert!(!task(TaskState::Waiting, earlier).is_news(now, &news));
        assert!(!task(TaskState::Running, now).is_news(now, &news));
    }

    #[test]
    fn an_agent_run_is_not_a_command_finishing() {
        let now = Instant::now();
        let mut finished = FinishedAgents::default();
        finished.program(now, 1, Some("claude".into()), None, false, String::new);
        finished.note(now + Duration::from_secs(1), 1, Some(AgentState::Working));
        let later = now + Duration::from_secs(600);
        assert!(!finished.program(later, 1, None, None, false, String::new));
        assert!(finished.shown.iter().all(|notice| notice.command.is_none()));
    }

    #[test]
    fn a_shell_at_its_prompt_runs_nothing() {
        let program = |executable: &str, runs: Option<&str>| thinkterm_proto::ForegroundProgram {
            executable: executable.to_string(),
            runs: runs.map(str::to_string),
            task: None,
        };
        assert_eq!(running_command(&program("zsh", None)), None);
        assert_eq!(running_command(&program("bash", Some("deploy.sh"))).as_deref(), Some("deploy.sh"));
        assert_eq!(running_command(&program("node", Some("npm"))).as_deref(), Some("npm"));
        assert_eq!(running_command(&program("cargo", None)).as_deref(), Some("cargo"));
    }

    #[test]
    fn a_command_took_reads_in_its_largest_units() {
        assert!(took_label(Duration::from_secs(42)).contains("42"));
        let minutes = took_label(Duration::from_secs(192));
        assert!(minutes.contains('3') && minutes.contains("12"));
        let hours = took_label(Duration::from_secs(3 * 3600 + 5 * 60));
        assert!(hours.contains('3') && hours.contains('5'));
    }

    #[test]
    fn a_finished_agent_goes_after_its_time_on_screen() {
        let now = Instant::now();
        let mut finished = FinishedAgents::default();
        finished.working.insert(1, now);
        assert!(finished.note(now + FINISHED_AFTER, 1, Some(AgentState::Idle)));
        let start = now + FINISHED_AFTER + FINISHED_SETTLE;
        assert!(finished.settle(start, false));
        let seen: HashSet<PaneId> = [1].iter().copied().collect();
        assert!(!finished.advance(start + Duration::from_secs(4), &seen, false));
        assert_eq!(finished.deadline(), Some(start + FINISHED_FOR));
        assert!(finished.advance(start + FINISHED_FOR, &seen, false));
        assert!(finished.shown.is_empty());
    }

    #[test]
    fn a_finished_agent_waits_while_held_or_unseen() {
        let now = Instant::now();
        let mut finished = FinishedAgents::default();
        finished.shown.push(FinishedAgent {
            pane_id: 1,
            shown_at: now,
            paused_at: None,
            command: None,
        });
        let seen: HashSet<PaneId> = [1].iter().copied().collect();
        // Under the pointer for ten seconds, then left alone.
        assert!(!finished.advance(now + Duration::from_secs(1), &seen, true));
        assert_eq!(finished.deadline(), None);
        assert!(!finished.advance(now + Duration::from_secs(11), &seen, false));
        assert!(!finished.advance(now + Duration::from_secs(14), &seen, false));
        // Not on the status at all -- folded into "+N" -- holds it too.
        assert!(!finished.advance(now + Duration::from_secs(30), &HashSet::new(), false));
        assert_eq!(finished.shown.len(), 1);
    }

    #[test]
    fn agent_news_expires_even_after_an_empty_or_hidden_status() {
        let now = Instant::now();
        let mut dynamic = DynamicStatus::default();
        dynamic.agent_news.insert(1, now);
        dynamic.agent_news.insert(2, now + Duration::from_secs(1));
        dynamic.clear();
        dynamic.hide(now);
        dynamic.expire_agent_news(now + NEWS_FOR);
        assert!(!dynamic.agent_news.contains_key(&1));
        assert!(dynamic.agent_news.contains_key(&2));
        dynamic.expire_agent_news(now + NEWS_FOR + Duration::from_secs(1));
        assert!(dynamic.agent_news.is_empty());
    }

    fn rect(x: f32, y: f32, w: f32, h: f32) -> RectF {
        euclid::rect(x, y, w, h)
    }

    #[test]
    fn the_pill_grows_from_the_corner_it_shares_with_the_card() {
        let pill = rect(300.0, 164.0, 100.0, 36.0);
        let card = rect(20.0, 140.0, 380.0, 60.0);
        let half = lerp_rect(pill, card, 0.5);
        assert_eq!(half.max_x(), 400.0);
        assert_eq!(half.max_y(), 200.0);
        assert_eq!(half.size.width, 240.0);
        // Past the end it keeps growing away from the corner.
        let over = lerp_rect(pill, card, 1.05);
        assert_eq!(over.max_x(), 400.0);
        assert!(over.size.width > 380.0);
    }

    #[test]
    fn opening_staggers_the_cards_and_folding_drops_them_together() {
        let now = Instant::now();
        let mut dynamic = DynamicStatus::default();
        dynamic.set_open(now, true, true);
        let delays: Vec<_> = dynamic
            .rise
            .iter()
            .map(|timeline| timeline.as_ref().unwrap().target())
            .collect();
        assert!(delays.iter().all(|target| *target == 1.0));
        assert!(dynamic.full);
        dynamic.set_open(now, false, false);
        assert!(dynamic
            .rise
            .iter()
            .all(|timeline| timeline.as_ref().unwrap().target() == 0.0));
        assert_eq!(dynamic.grow.as_ref().unwrap().target(), 0.0);
        assert!(!dynamic.full);
    }

    fn report(state: crate::agent_status::ProgramReportState, progress: Option<u8>, msg: &str) -> ProgramReport {
        ProgramReport {
            state,
            kind: None,
            progress,
            app: None,
            title: None,
            msg: Some(msg.to_string()),
            children: Vec::new(),
        }
    }

    #[test]
    fn the_status_takes_waits_errors_and_measured_work_only() {
        use crate::agent_status::ProgramReportState as Report;
        let show = crate::native_settings::NativeAgentStatusDisplay::default();
        let blocked = report(Report::Blocked, None, "approve npm test");
        assert!(status_wants(AgentState::Blocked, Some(&blocked), show));
        assert!(status_wants(AgentState::Error, None, show));
        assert!(!status_wants(AgentState::Idle, None, show));
        let unmeasured = report(Report::Working, None, "reading files");
        assert!(!status_wants(AgentState::Working, Some(&unmeasured), show));
        let measured = report(Report::Working, Some(40), "reading files");
        assert!(status_wants(AgentState::Working, Some(&measured), show));
        let hidden = crate::native_settings::NativeAgentStatusDisplay {
            progress: false,
            ..show
        };
        assert!(!status_wants(AgentState::Working, Some(&measured), hidden));
    }

    #[test]
    fn work_that_only_says_something_new_repaints_nothing() {
        use crate::agent_status::ProgramReportState as Report;
        let show = crate::native_settings::NativeAgentStatusDisplay::default();
        let mark = |state, report: &ProgramReport| status_mark(state, Some(report), "fix login", show);
        // At work, the line under the name is not followed; the progress is.
        let reading = mark(AgentState::Working, &report(Report::Working, Some(40), "reading"));
        let writing = mark(AgentState::Working, &report(Report::Working, Some(40), "writing"));
        let further = mark(AgentState::Working, &report(Report::Working, Some(50), "writing"));
        assert!(reading.is_some());
        assert_eq!(reading, writing);
        assert_ne!(writing, further);
        // Waiting, what it waits for is the point.
        let test = mark(AgentState::Blocked, &report(Report::Blocked, None, "approve npm test"));
        let push = mark(AgentState::Blocked, &report(Report::Blocked, None, "approve git push"));
        assert_ne!(test, push);
        // And a wait that turns into an error is a change, saying the same.
        let failed = mark(AgentState::Error, &report(Report::Error, None, "approve npm test"));
        assert_ne!(test, failed);
    }

    fn pane(id: PaneId) -> StatusItem {
        StatusItem::Pane(PaneProgress {
            pane_id: id,
            here: true,
            title: format!("pane {id}"),
            place: String::new(),
            progress: Progress::Percentage(50),
        })
    }

    #[test]
    fn arriving_starts_from_nothing_and_going_ends_there() {
        let close = |a: (f32, f32, f32, f32), b: (f32, f32, f32, f32)| {
            (a.0 - b.0).abs() < 1e-4
                && (a.1 - b.1).abs() < 1e-4
                && (a.2 - b.2).abs() < 1e-4
                && (a.3 - b.3).abs() < 1e-4
        };
        assert!(close(presence_phases(0.0, true), (0.0, 0.0, 0.0, 0.0)));
        assert!(close(presence_phases(1.0, true), (1.0, 1.0, 1.0, 1.0)));
        assert!(close(presence_phases(1.0, false), (1.0, 1.0, 1.0, 1.0)));
        assert!(close(presence_phases(0.0, false), (0.0, 0.0, 0.0, 0.0)));
        // Going, the content is gone before the surface folds away.
        let (_, _, content, surface) = presence_phases(0.6, false);
        assert!(content < 0.01 && surface > 0.99);
    }

    #[test]
    fn a_swap_ends_on_the_new_content_alone() {
        assert_eq!(swap_mix(None), (0.0, 1.0));
        assert_eq!(swap_mix(Some(0.0)), (1.0, 0.0));
        assert_eq!(swap_mix(Some(1.0)), (0.0, 1.0));
    }

    #[test]
    fn a_card_that_goes_from_the_open_stack_shrinks_away_and_the_rest_close_up() {
        let now = Instant::now();
        let mut dynamic = DynamicStatus::default();
        dynamic.full = true;
        dynamic.rise = std::array::from_fn(|_| Some(Timeline::settled(now, 1.0)));
        dynamic.last = vec![pane(1), pane(2), pane(3)];
        dynamic.note_stack(now, &[StatusKey::Pane(2), StatusKey::Pane(3)], false);
        assert!(dynamic.leaving.is_empty());
        // The first card above goes: it is drawn going, where it was, and
        // the one above it moves down into its place.
        dynamic.note_stack(now, &[StatusKey::Pane(3)], false);
        assert_eq!(dynamic.leaving.len(), 1);
        assert_eq!(dynamic.leaving[0].item.key(), StatusKey::Pane(2));
        assert_eq!(dynamic.leaving[0].slot, 0.0);
        let (slot, _) = &dynamic.slots[&StatusKey::Pane(3)];
        assert_eq!(slot.target(), 0.0);
    }

    #[test]
    fn a_card_that_goes_while_folded_just_goes() {
        let now = Instant::now();
        let mut dynamic = DynamicStatus::default();
        dynamic.last = vec![pane(1), pane(2)];
        dynamic.note_stack(now, &[StatusKey::Pane(2)], false);
        dynamic.note_stack(now, &[], false);
        assert!(dynamic.leaving.is_empty());
        assert!(dynamic.slots.is_empty());
    }

    #[test]
    fn a_disc_that_goes_shrinks_away_and_the_row_closes_up() {
        let now = Instant::now();
        let mut dynamic = DynamicStatus::default();
        dynamic.last = vec![pane(1), pane(2), pane(3), pane(4)];
        let keys = [StatusKey::Pane(2), StatusKey::Pane(3), StatusKey::Pane(4)];
        dynamic.note_discs(now, &keys, true);
        assert_eq!(dynamic.discs.len(), 3);
        assert!(dynamic.leaving_discs.is_empty());
        // The nearest goes: drawn going where it was, the others move up.
        dynamic.note_discs(now, &keys[1..], true);
        assert_eq!(dynamic.leaving_discs.len(), 1);
        assert_eq!(dynamic.leaving_discs[0].item.key(), StatusKey::Pane(2));
        assert_eq!(dynamic.leaving_discs[0].slot, 0.0);
        assert_eq!(dynamic.discs[&StatusKey::Pane(3)].0.target(), 0.0);
        assert_eq!(dynamic.discs[&StatusKey::Pane(4)].0.target(), 1.0);
    }

    #[test]
    fn arriving_the_status_brings_its_discs_without_a_show() {
        let now = Instant::now();
        let mut dynamic = DynamicStatus::default();
        dynamic.last = vec![pane(1), pane(2)];
        dynamic.note_discs(now, &[StatusKey::Pane(2)], false);
        dynamic.note_discs(now, &[], false);
        assert!(dynamic.leaving_discs.is_empty());
        assert!(!dynamic.owes_frames());
    }

    #[test]
    fn only_a_transition_keeps_a_window_without_focus_drawing() {
        let now = Instant::now();
        let mut dynamic = DynamicStatus::default();
        assert!(!dynamic.owes_frames());
        dynamic.set_present(now, true);
        assert!(dynamic.owes_frames());
        // Shown and still: nothing owed.
        dynamic.presence = Some(Timeline::settled(now, 1.0));
        assert!(!dynamic.owes_frames());
        dynamic.set_open(now, true, false);
        assert!(dynamic.owes_frames());
    }

    #[test]
    fn only_what_has_just_happened_is_news() {
        use super::super::transfer_card::TransferToast;
        let now = Instant::now();
        let finished = |at: Instant| {
            StatusItem::Transfer(CardView::Toast(TransferToast::download_done(
                7,
                "a.txt".to_string(),
                std::path::PathBuf::from("/tmp/a.txt"),
                false,
                Some(1),
                None,
                at,
            )))
        };
        let news = HashMap::new();
        assert!(finished(now).is_news(now, &news));
        // Finished a while ago and only looked at now: not news.
        assert!(!finished(now).is_news(now + NEWS_FOR + Duration::from_secs(1), &news));
        // A pane's own progress bar never is.
        assert!(!pane(1).is_news(now, &news));
        // Ending is a new state for the same transfer: it arrives again.
        let running = StatusItem::Transfer(CardView::Running {
            kind: crate::termwindow::remote_files::RemoteTransferKind::Download,
            id: 7,
            name: "a.txt".to_string(),
            progress: Default::default(),
            canceling: false,
        });
        assert_eq!(running.key(), finished(now).key());
        assert_ne!(running.phase(), finished(now).phase());
    }

    #[test]
    fn a_reorder_while_folded_moves_nothing_on_screen() {
        let now = Instant::now();
        let mut dynamic = DynamicStatus::default();
        dynamic.last = vec![pane(1), pane(2), pane(3)];
        dynamic.note_stack(now, &[StatusKey::Pane(2), StatusKey::Pane(3)], false);
        dynamic.note_stack(now, &[StatusKey::Pane(3), StatusKey::Pane(2)], false);
        assert!(dynamic.slots.values().all(|(slot, _)| !slot.is_running()));
        assert!(!dynamic.owes_frames());
    }

    #[test]
    fn hidden_halfway_through_a_resize_owes_no_frames() {
        let now = Instant::now();
        let mut dynamic = DynamicStatus::default();
        dynamic.presence = Some(Timeline::settled(now, 1.0));
        dynamic.set_pill_width(now, 100.0, false);
        dynamic.set_pill_width(now, 160.0, true);
        assert!(dynamic.owes_frames());
        dynamic.hide(now);
        assert!(!dynamic.owes_frames());
        assert_eq!(dynamic.pill_width.as_ref().unwrap().target(), 160.0);
    }

    #[test]
    fn a_hit_area_off_the_window_edge_is_cut_not_moved() {
        let item = ui_item(euclid::rect(-30.0, 10.0, 100.0, 20.0), UIItemType::DynamicStatusShowAll);
        assert_eq!((item.x, item.width), (0, 70));
    }

    #[test]
    fn a_banner_grows_the_pill_without_raising_the_stack() {
        let now = Instant::now();
        let mut dynamic = DynamicStatus::default();
        dynamic.set_open(now, true, false);
        assert_eq!(dynamic.grow.as_ref().unwrap().target(), 1.0);
        assert!(dynamic.rise.iter().all(Option::is_none));
    }
}
