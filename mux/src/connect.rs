//! Host-side operations for a Connect export. Ordinary pane input APIs do
//! not enter this module, and none of these handles are protocol values.

use crate::client::ClientId;
use crate::localpane::LocalPane;
use crate::pane::{Pane, PaneId};
use crate::tab::{PaneMembership, Tab, TabId};
use crate::{ClientRegistrationId, FrontendAccessMode, FrontendLeaseState, Mux};
use parking_lot::Mutex;
use std::io;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc, Weak};
use std::task::Poll;
use std::time::{Duration, Instant};
use thinkterm_reverse_access::budget::Reservation;
use thinkterm_reverse_access::channel::Deadline;
use thinkterm_reverse_access::protocol::{InputResult, InputStatus, ReadOptions};
use thinkterm_reverse_access::{Error, Id, RequestPermit, Result, Target};

mod host;
pub mod service;
pub use host::{ActiveGrant, Host, HostRoute, HostStatus, PaneChoice, PreparedReply};

#[derive(Clone, PartialEq, Eq)]
struct Ownership {
    mode: FrontendAccessMode,
    global_generation: u64,
    tab_generation: u64,
    owner: Option<ClientId>,
}

impl Ownership {
    fn capture(lease: &FrontendLeaseState, tab: TabId, client: &ClientId) -> Result<Self> {
        let state = lease.tabs.get(&tab);
        let owner = match lease.access_mode {
            FrontendAccessMode::Handoff => lease.handoff_owner.as_ref(),
            FrontendAccessMode::TmuxLatest => state.and_then(|tab| tab.owner.as_ref()),
        };
        if owner.is_some_and(|owner| owner != client)
            || (owner.is_none()
                && lease.access_mode == FrontendAccessMode::Handoff
                && lease.handoff_ever_owned)
            || lease.ownership_generation == u64::MAX
            || state.is_some_and(|state| state.ownership_generation == u64::MAX)
        {
            return Err(Error::Unavailable);
        }
        Ok(Self {
            mode: lease.access_mode,
            global_generation: lease.ownership_generation,
            tab_generation: state.map_or(0, |state| state.ownership_generation),
            owner: owner.cloned(),
        })
    }
}

pub struct ExportedPane {
    target: Target,
    mux: Weak<Mux>,
    pane: Weak<dyn Pane>,
    tab: Weak<Tab>,
    tab_id: TabId,
    client: ClientId,
    registration: ClientRegistrationId,
    ownership: Ownership,
    membership: PaneMembership,
}

impl ExportedPane {
    /// The caller is the trusted local management path. Identity, terminal
    /// instance, and ownership are captured here by the actual terminal host.
    pub fn capture(mux: &Arc<Mux>, pane_id: PaneId, client: &ClientId) -> Result<Arc<Self>> {
        let pane = mux.get_pane(pane_id).ok_or(Error::Unavailable)?;
        let local = pane.downcast_ref::<LocalPane>().ok_or(Error::Unavailable)?;
        local.connect_eligible()?;
        let (_, _, tab_id) = mux.resolve_pane_id(pane_id).ok_or(Error::Unavailable)?;
        let tab = mux.get_tab(tab_id).ok_or(Error::Unavailable)?;
        let registration = mux
            .client_registrations
            .try_read()
            .ok_or(Error::Busy)?
            .get(client)
            .copied()
            .ok_or(Error::Unavailable)?;
        let ownership = Ownership::capture(&mux.frontend_lease.lock(), tab_id, client)?;
        let membership = tab.capture_delegated_pane(pane_id)?;
        let mut owner = [0; 16];
        getrandom::fill(&mut owner).map_err(|_| Error::Random)?;
        let exported = Arc::new(Self {
            target: Target::new(pane_id as u64, Id::from_bytes(owner))?,
            mux: Arc::downgrade(mux),
            pane: Arc::downgrade(&pane),
            tab: Arc::downgrade(&tab),
            tab_id,
            client: client.clone(),
            registration,
            ownership,
            membership,
        });
        exported.with_current(|_| Ok(()))?;
        Ok(exported)
    }

    pub fn target(&self) -> Target {
        self.target
    }

    fn with_current<T>(&self, action: impl FnOnce(&LocalPane) -> Result<T>) -> Result<T> {
        let mux = self.mux.upgrade().ok_or(Error::Unavailable)?;
        let pane = self.pane.upgrade().ok_or(Error::Unavailable)?;
        let tab = self.tab.upgrade().ok_or(Error::Unavailable)?;
        let panes = mux.panes.try_read().ok_or(Error::Busy)?;
        if !panes
            .get(&pane.pane_id())
            .is_some_and(|current| Arc::ptr_eq(current, &pane))
        {
            return Err(Error::Unavailable);
        }
        let tabs = mux.tabs.try_read().ok_or(Error::Busy)?;
        if !tabs
            .get(&self.tab_id)
            .is_some_and(|current| Arc::ptr_eq(current, &tab))
        {
            return Err(Error::Unavailable);
        }
        tab.with_delegated_panes(&[(pane.pane_id(), &self.membership)], || {
            let registrations = mux.client_registrations.try_read().ok_or(Error::Busy)?;
            if registrations.get(&self.client) != Some(&self.registration) {
                return Err(Error::Unavailable);
            }
            let lease = mux.frontend_lease.try_lock().ok_or(Error::Busy)?;
            if Ownership::capture(&lease, self.tab_id, &self.client)? != self.ownership {
                return Err(Error::Unavailable);
            }
            let local = pane.downcast_ref::<LocalPane>().ok_or(Error::Unavailable)?;
            local.connect_eligible()?;
            action(local)
        })?
    }

    pub fn is_current(&self) -> bool {
        matches!(self.with_current(|_| Ok(())), Ok(()) | Err(Error::Busy))
    }

    pub fn execute<T>(
        &self,
        permit: &RequestPermit,
        action: impl FnOnce(&LocalPane) -> T,
    ) -> Result<T> {
        self.with_current(|local| permit.execute(self.target, || action(local)))
    }

    /// `execute` retried for as long as a lock another thread holds is what
    /// stands in the way. Busy means that attempt did nothing, and a terminal
    /// with heavy output is held often. The locks are only ever tried, and
    /// this waits on the executor rather than on any of them.
    async fn execute_when_free<T>(
        &self,
        permit: &RequestPermit,
        deadline: &Deadline,
        mut action: impl FnMut(&LocalPane) -> Result<T>,
    ) -> Result<T> {
        deadline
            .retry_busy(|_| {
                Poll::Ready(
                    self.execute(permit, |local| action(local))
                        .and_then(|result| result),
                )
            })
            .await
    }

    pub async fn read(
        self: &Arc<Self>,
        permit: Arc<RequestPermit>,
        options: ReadOptions,
        mut reservation: Reservation,
        deadline: &Deadline,
    ) -> Result<TextSnapshot> {
        // Capacity is charged before allocation, including the temporary
        // reference array in Screen::with_phys_lines.
        let capacity = thinkterm_reverse_access::MAX_READ_BYTES;
        reservation.grow(
            capacity + thinkterm_reverse_access::MAX_READ_LINES * std::mem::size_of::<usize>(),
        )?;
        let text = self
            .execute_when_free(&permit, deadline, |local| local.connect_read(options))
            .await?;
        Ok(TextSnapshot {
            text,
            target: Arc::clone(self),
            permit,
            _reservation: reservation,
        })
    }

    pub async fn input(
        self: &Arc<Self>,
        permit: Arc<RequestPermit>,
        input: Input,
        mut reservation: Reservation,
        deadline: &Deadline,
    ) -> Result<PendingInput> {
        reservation.grow(6 * thinkterm_reverse_access::MAX_INPUT_BYTES + 1024)?;
        let (writer, encoded) = self
            .execute_when_free(&permit, deadline, |local| local.connect_input(&input))
            .await?;
        let (sender, receiver) = mpsc::sync_channel(1);
        let task = Arc::new(InputTask {
            cancelled: AtomicBool::new(false),
            state: Mutex::new(InputState {
                bytes: Some(encoded),
                reservation,
                progress: InputResult {
                    status: InputStatus::RejectedNotSubmitted,
                    bytes: 0,
                    enter_submitted: false,
                    paste_may_be_open: false,
                },
            }),
        });
        let queued = Arc::clone(&task);
        let target = Arc::clone(self);
        let queued_permit = Arc::clone(&permit);
        let mut submit: Option<Box<dyn FnOnce() + Send>> = Some(Box::new(move || {
            let result = queued.submit(target, queued_permit, writer);
            let _ = sender.send(result);
        }));
        self.execute_when_free(&permit, deadline, |local| local.connect_queue(&mut submit))
            .await?;
        Ok(PendingInput {
            task,
            receiver: Mutex::new(receiver),
        })
    }
}

pub struct TextSnapshot {
    text: String,
    target: Arc<ExportedPane>,
    permit: Arc<RequestPermit>,
    _reservation: Reservation,
}

impl TextSnapshot {
    pub fn len(&self) -> usize {
        self.text.len()
    }

    /// `send` must be a nonblocking write. The body is not released to an
    /// ordinary unbounded output queue after this final permission check.
    pub fn send<T>(&self, offset: usize, limit: usize, send: impl FnOnce(&[u8]) -> T) -> Result<T> {
        if offset > self.text.len() {
            return Err(Error::Protocol);
        }
        let end = offset.saturating_add(limit).min(self.text.len());
        self.target
            .execute(&self.permit, |_| send(&self.text.as_bytes()[offset..end]))
    }
}

pub enum Input {
    Text {
        text: String,
        no_paste: bool,
        enter: bool,
    },
    Keys(Vec<(wezterm_term::KeyCode, wezterm_term::KeyModifiers)>),
}

pub(crate) struct EncodedInput {
    pub bytes: Vec<u8>,
    pub text_end: usize,
    pub enter: bool,
    /// Between the text and its Enter, inside this one queue operation.
    pub enter_delay: Duration,
    pub paste: bool,
}

struct InputState {
    bytes: Option<EncodedInput>,
    reservation: Reservation,
    progress: InputResult,
}

struct InputTask {
    cancelled: AtomicBool,
    state: Mutex<InputState>,
}

impl InputTask {
    fn cancel(&self) {
        self.cancelled.store(true, Ordering::Release);
        let mut state = self.state.lock();
        state.bytes = None;
        // Keep the concurrency slot until this callback leaves the existing
        // writer queue. Cancellation cannot accumulate unbounded empty jobs.
        let bytes = state.reservation.bytes();
        let _ = state.reservation.release_bytes(bytes);
    }

    fn submit(
        &self,
        target: Arc<ExportedPane>,
        permit: Arc<RequestPermit>,
        mut writer: Box<dyn portable_pty::TryInputWriter>,
    ) -> InputResult {
        self.submit_checked(
            || permit.execute(target.target, || ()),
            |bytes| target.execute(&permit, |_| writer.try_write(bytes)),
        )
    }

    fn submit_checked(
        &self,
        mut check: impl FnMut() -> Result<()>,
        mut write: impl FnMut(&[u8]) -> Result<io::Result<usize>>,
    ) -> InputResult {
        let mut text_submitted = None;
        loop {
            let mut state = self.state.lock();
            if self.cancelled.load(Ordering::Acquire) {
                return state.progress;
            }
            if check().is_err() {
                return state.progress;
            }
            let Some(encoded) = state.bytes.as_ref() else {
                return state.progress;
            };
            let offset = state.progress.bytes;
            let text_end = encoded.text_end;
            let enter = encoded.enter;
            let paste = encoded.paste;
            let length = encoded.bytes.len();
            if offset == length {
                state.progress.status = InputStatus::Submitted;
                state.progress.enter_submitted = enter;
                state.progress.paste_may_be_open = false;
                state.bytes = None;
                return state.progress;
            }
            if enter && offset == text_end && text_end > 0 {
                // Every turn of this wait repeats the checks above, so
                // revocation still stops the Enter.
                let submitted = *text_submitted.get_or_insert_with(Instant::now);
                if submitted.elapsed() < encoded.enter_delay {
                    drop(state);
                    std::thread::sleep(Duration::from_millis(10));
                    continue;
                }
            }
            let block_end = if offset < text_end { text_end } else { length };
            let end = offset.saturating_add(4096).min(block_end);
            match write(&encoded.bytes[offset..end]) {
                Ok(Ok(count)) if count > 0 && count <= end - offset => {
                    state.progress.bytes += count;
                    state.progress.status = if state.progress.bytes == length {
                        InputStatus::Submitted
                    } else {
                        InputStatus::PartiallySubmitted
                    };
                    state.progress.enter_submitted = enter && state.progress.bytes == length;
                    state.progress.paste_may_be_open = paste && state.progress.bytes < text_end;
                }
                Ok(Err(error))
                    if matches!(
                        error.kind(),
                        io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted
                    ) =>
                {
                    drop(state);
                    std::thread::sleep(Duration::from_millis(1));
                }
                Err(Error::Busy) => {
                    drop(state);
                    std::thread::sleep(Duration::from_millis(1));
                }
                Err(_) => return state.progress,
                _ => {
                    state.progress.status = InputStatus::OutcomeUnknown;
                    return state.progress;
                }
            }
        }
    }
}

pub struct PendingInput {
    task: Arc<InputTask>,
    receiver: Mutex<mpsc::Receiver<InputResult>>,
}

impl PendingInput {
    pub fn try_result(&self) -> Option<InputResult> {
        match self.receiver.lock().try_recv() {
            Ok(result) => Some(result),
            Err(mpsc::TryRecvError::Empty) => None,
            Err(mpsc::TryRecvError::Disconnected) => Some(self.task.state.lock().progress),
        }
    }

    pub fn cancel(&self) -> InputResult {
        self.task.cancel();
        self.task.state.lock().progress
    }
}

impl Drop for PendingInput {
    fn drop(&mut self) {
        self.task.cancel();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::TabFrontendLease;

    fn client(id: usize) -> ClientId {
        ClientId {
            hostname: "devbox".into(),
            username: "example".into(),
            pid: 42,
            ssh_auth_sock: None,
            epoch: 1,
            id,
        }
    }

    #[test]
    fn ownership_cannot_return_to_an_old_delegation_after_a_round_trip() {
        let first = client(1);
        let second = client(2);
        let mut lease = FrontendLeaseState::default();
        lease.set_handoff_owner(Some(first.clone()));
        let captured = Ownership::capture(&lease, 1, &first).unwrap();
        lease.set_handoff_owner(Some(second));
        lease.set_handoff_owner(Some(first.clone()));
        assert!(Ownership::capture(&lease, 1, &first).unwrap() != captured);
        let mut tab = TabFrontendLease::default();
        tab.set_owner(Some(first.clone()));
        let generation = tab.ownership_generation;
        tab.set_owner(Some(first));
        assert_eq!(tab.ownership_generation, generation);
        tab.set_owner(None);
        assert_ne!(tab.ownership_generation, generation);
    }

    fn input_task(
        bytes: &[u8],
        enter: bool,
        paste: bool,
    ) -> (InputTask, thinkterm_reverse_access::budget::Budgets) {
        let budgets = thinkterm_reverse_access::budget::Budgets::new().unwrap();
        let route = budgets.open_route(Id::from_bytes([1; 16])).unwrap();
        let reservation = route.acquire(bytes.len()).unwrap();
        (
            InputTask {
                cancelled: AtomicBool::new(false),
                state: Mutex::new(InputState {
                    bytes: Some(EncodedInput {
                        bytes: bytes.to_vec(),
                        text_end: bytes.len() - usize::from(enter),
                        enter,
                        enter_delay: Duration::ZERO,
                        paste,
                    }),
                    reservation,
                    progress: InputResult {
                        status: InputStatus::RejectedNotSubmitted,
                        bytes: 0,
                        enter_submitted: false,
                        paste_may_be_open: false,
                    },
                }),
            },
            budgets,
        )
    }

    #[test]
    fn revoked_input_cannot_submit_enter_or_another_paste_chunk() {
        let (task, _) = input_task(b"text\r", true, true);
        let revoked = AtomicBool::new(false);
        let mut written = Vec::new();
        let result = task.submit_checked(
            || {
                if revoked.load(Ordering::Acquire) {
                    Err(Error::Revoked)
                } else {
                    Ok(())
                }
            },
            |bytes| {
                written.extend_from_slice(&bytes[..2]);
                revoked.store(true, Ordering::Release);
                Ok(Ok(2))
            },
        );
        assert_eq!(written, b"te");
        assert_eq!(result.status, InputStatus::PartiallySubmitted);
        assert_eq!(result.bytes, 2);
        assert!(!result.enter_submitted);
        assert!(result.paste_may_be_open);
    }

    #[test]
    fn completed_input_remains_submitted_when_permission_is_then_revoked() {
        let (task, _) = input_task(b"text\r", true, false);
        let revoked = AtomicBool::new(false);
        let result = task.submit_checked(
            || {
                if revoked.load(Ordering::Acquire) {
                    Err(Error::Revoked)
                } else {
                    Ok(())
                }
            },
            |bytes| {
                revoked.store(bytes == b"\r", Ordering::Release);
                Ok(Ok(bytes.len()))
            },
        );
        assert_eq!(result.status, InputStatus::Submitted);
        assert_eq!(result.bytes, 5);
        assert!(result.enter_submitted);
    }

    #[test]
    fn enter_follows_the_text_after_its_delay_as_a_separate_write() {
        let (task, _) = input_task(b"text\r", true, true);
        task.state.lock().bytes.as_mut().unwrap().enter_delay = Duration::from_millis(40);
        let started = Instant::now();
        let mut writes: Vec<(Vec<u8>, Duration)> = Vec::new();
        let result = task.submit_checked(
            || Ok(()),
            |bytes| {
                writes.push((bytes.to_vec(), started.elapsed()));
                Ok(Ok(bytes.len()))
            },
        );
        assert_eq!(result.status, InputStatus::Submitted);
        assert!(result.enter_submitted);
        assert!(!result.paste_may_be_open);
        assert_eq!(writes[0].0, b"text");
        assert_eq!(writes[1].0, b"\r");
        assert!(writes[1].1 - writes[0].1 >= Duration::from_millis(40));
    }

    #[test]
    fn revocation_during_the_enter_delay_keeps_the_enter_back() {
        let (task, _) = input_task(b"text\r", true, false);
        task.state.lock().bytes.as_mut().unwrap().enter_delay = Duration::from_secs(30);
        let revoked = AtomicBool::new(false);
        let mut written = Vec::new();
        let result = task.submit_checked(
            || {
                if revoked.load(Ordering::Acquire) {
                    Err(Error::Revoked)
                } else {
                    Ok(())
                }
            },
            |bytes| {
                written.extend_from_slice(bytes);
                revoked.store(true, Ordering::Release);
                Ok(Ok(bytes.len()))
            },
        );
        assert_eq!(written, b"text");
        assert_eq!(result.status, InputStatus::PartiallySubmitted);
        assert!(!result.enter_submitted);
        assert!(!result.paste_may_be_open);
    }

    #[test]
    fn empty_text_presses_enter_without_waiting() {
        let (task, _) = input_task(b"\r", true, false);
        task.state.lock().bytes.as_mut().unwrap().enter_delay = Duration::from_secs(30);
        let result = task.submit_checked(|| Ok(()), |bytes| Ok(Ok(bytes.len())));
        assert_eq!(result.status, InputStatus::Submitted);
        assert!(result.enter_submitted);
    }

    #[test]
    fn cancellation_releases_queued_body_but_retains_its_queue_slot() {
        let (task, budgets) = input_task(b"example", false, false);
        assert_eq!(budgets.retained().unwrap(), (1, 7));
        task.cancel();
        assert!(task.state.lock().bytes.is_none());
        assert_eq!(budgets.retained().unwrap(), (1, 0));
        let result = task.submit_checked(
            || panic!("cancelled input was checked"),
            |_| panic!("cancelled input was submitted"),
        );
        assert_eq!(result.status, InputStatus::RejectedNotSubmitted);
        drop(task);
        assert_eq!(budgets.retained().unwrap(), (0, 0));
    }

    #[test]
    fn backpressure_rechecks_permission_and_write_errors_do_not_imply_non_execution() {
        let (task, _) = input_task(b"example", false, false);
        let revoked = AtomicBool::new(false);
        let mut attempts = 0;
        let result = task.submit_checked(
            || {
                if revoked.load(Ordering::Acquire) {
                    Err(Error::Revoked)
                } else {
                    Ok(())
                }
            },
            |_| {
                attempts += 1;
                revoked.store(true, Ordering::Release);
                Ok(Err(io::ErrorKind::WouldBlock.into()))
            },
        );
        assert_eq!(attempts, 1);
        assert_eq!(result.status, InputStatus::RejectedNotSubmitted);
        let result = task.submit_checked(|| Ok(()), |_| Ok(Err(io::ErrorKind::BrokenPipe.into())));
        assert_eq!(result.status, InputStatus::OutcomeUnknown);
    }
}
