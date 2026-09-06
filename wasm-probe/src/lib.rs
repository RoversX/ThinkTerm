//! Staged probes for the wasm build. Each entry point does strictly more than
//! the last, so a failure identifies which layer is not portable rather than
//! just reporting that something panicked.

use std::sync::Arc;
use std::sync::Mutex;

// wasm32-unknown-unknown has nowhere to print a panic, and the trap that
// follows tells you only "unreachable". Stash the message where JS can read it
// out of linear memory after catching the trap.
static PANIC: Mutex<String> = Mutex::new(String::new());
static mut PANIC_BUF: [u8; 1024] = [0; 1024];

#[no_mangle]
pub extern "C" fn install_panic_hook() {
    std::panic::set_hook(Box::new(|info| {
        if let Ok(mut slot) = PANIC.lock() {
            *slot = info.to_string();
        }
    }));
}

#[no_mangle]
pub extern "C" fn panic_msg_ptr() -> *const u8 {
    let msg = PANIC.lock().map(|m| m.clone()).unwrap_or_default();
    let bytes = msg.as_bytes();
    let n = bytes.len().min(1024);
    unsafe {
        let buf = (&raw mut PANIC_BUF) as *mut u8;
        std::ptr::copy_nonoverlapping(bytes.as_ptr(), buf, n);
        buf as *const u8
    }
}

#[no_mangle]
pub extern "C" fn panic_msg_len() -> u32 {
    PANIC.lock().map(|m| m.len().min(1024) as u32).unwrap_or(0)
}
use termwiz::escape::parser::Parser;
use wezterm_term::{Terminal, TerminalConfiguration, TerminalSize};

#[derive(Debug)]
struct Config;

impl TerminalConfiguration for Config {
    fn color_palette(&self) -> wezterm_term::color::ColorPalette {
        Default::default()
    }
}

fn size() -> TerminalSize {
    TerminalSize {
        rows: 24,
        cols: 80,
        pixel_width: 800,
        pixel_height: 480,
        dpi: 96,
    }
}

#[no_mangle]
pub extern "C" fn stage1_alloc() -> u32 {
    let v: Vec<u8> = (0..64).collect();
    v.len() as u32
}

#[no_mangle]
pub extern "C" fn stage2_parser() -> u32 {
    let mut p = Parser::new();
    let mut n = 0u32;
    p.parse(b"hello\x1b[1;31m wasm\x1b[0m", |_| n += 1);
    n
}

#[no_mangle]
pub extern "C" fn stage3_palette() -> u32 {
    let pal = Config.color_palette();
    pal.colors.0.len() as u32
}

#[no_mangle]
pub extern "C" fn stage4_terminal() -> u32 {
    let term = Terminal::new(
        size(),
        Arc::new(Config),
        "probe",
        "0.1",
        Box::new(Vec::new()),
    );
    term.screen().physical_rows as u32
}

#[no_mangle]
pub extern "C" fn stage5_advance() -> u32 {
    let mut term = Terminal::new(
        size(),
        Arc::new(Config),
        "probe",
        "0.1",
        Box::new(Vec::new()),
    );
    term.advance_bytes(b"hello\x1b[1;31m wasm\x1b[0m");
    let mut out = String::new();
    term.screen().for_each_phys_line(|idx, line| {
        if idx == 0 {
            out = line.as_str().trim_end().to_string();
        }
    });
    if out == "hello wasm" {
        0
    } else {
        1
    }
}

/// A CSI parameter is a u32; on a 32-bit usize `cursor.x + n as usize` in the
/// ECH handler overflows before .min() can clamp it. Any program writing to the
/// pty can send this.
#[no_mangle]
pub extern "C" fn stage6_ech_overflow() -> u32 {
    let mut term = Terminal::new(
        size(),
        Arc::new(Config),
        "probe",
        "0.1",
        Box::new(Vec::new()),
    );
    term.advance_bytes(b"abcdef\x1b[3G\x1b[4294967295X");
    let mut out = String::new();
    term.screen().for_each_phys_line(|idx, line| {
        if idx == 0 {
            out = line.as_str().trim_end().to_string();
        }
    });
    out.len() as u32
}

// ---- stages 7 and 8: the session layer ----------------------------------
//
// A host made of cells and vectors: time the stage advances by hand, tasks
// the stage runs by hand, a link answering from a table. None of it is
// `Send`, which is the point: the session crate must not ask for it.

mod session_probe {
    use codec::{GetLinesResponse, GetPaneRenderChangesResponse, InputSerial, Pdu, UnitResponse};
    use std::cell::{Cell, RefCell};
    use std::future::Future;
    use std::pin::Pin;
    use std::sync::atomic::AtomicUsize;
    use std::sync::Arc;
    use std::task::{Context, Poll, Waker};
    use termwiz::cell::CellAttributes;
    use termwiz::surface::{Line, SEQ_ZERO};
    use thinkterm_proto::{RenderableDimensions, StableCursorPosition};
    use thinkterm_session::clock::{Clock, Timestamp};
    use thinkterm_session::host::{
        DetachedFuture, HostConfig, HostPaneId, LinkError, PduLink, SessionEvents, SessionHost,
        Spawner,
    };
    use thinkterm_session::images::ImageStore;
    use thinkterm_session::input::{
        drain_pane_inputs, InputQueue, LocalFuture, PaneInput, PaneLink,
    };
    use thinkterm_session::pane::PaneSession;
    use thinkterm_session::{Lock, SessionConfig};
    use wezterm_term::{KeyCode, KeyModifiers, StableRowIndex};

    /// Poll `fut` to completion with a no-op waker. Every future here is
    /// ready on its first or second poll: nothing waits on the outside.
    pub fn spin_on<F: Future>(mut fut: F) -> F::Output {
        let mut fut = unsafe { Pin::new_unchecked(&mut fut) };
        let mut cx = Context::from_waker(Waker::noop());
        for _ in 0..10_000 {
            if let Poll::Ready(out) = fut.as_mut().poll(&mut cx) {
                return out;
            }
        }
        panic!("a probe future never became ready");
    }

    pub struct ProbeClock(pub Cell<u64>);
    impl Clock for ProbeClock {
        fn now(&self) -> Timestamp {
            Timestamp::from_micros(self.0.get())
        }
        fn wall_millis(&self) -> u64 {
            5_000
        }
    }

    #[derive(Default)]
    pub struct ProbeSpawner(RefCell<Vec<DetachedFuture>>);
    impl Spawner for ProbeSpawner {
        fn spawn_detached(&self, fut: DetachedFuture) {
            self.0.borrow_mut().push(fut);
        }
    }
    impl ProbeSpawner {
        /// Run every spawned task to completion, including the ones they
        /// spawn while running.
        pub fn drain(&self) {
            for _ in 0..100 {
                let tasks = std::mem::take(&mut *self.0.borrow_mut());
                if tasks.is_empty() {
                    return;
                }
                for task in tasks {
                    spin_on(task);
                }
            }
            panic!("probe tasks kept spawning tasks");
        }
    }

    #[derive(Default)]
    pub struct ProbeEvents {
        pub outputs: Cell<u32>,
    }
    impl SessionEvents for ProbeEvents {
        fn pane_output(&self, _pane: HostPaneId) {
            self.outputs.set(self.outputs.get() + 1);
        }
        fn alert(&self, _pane: HostPaneId, _alert: wezterm_term::Alert) {}
        fn agent_status_changed(&self, _pane: HostPaneId) {}
        fn pane_removed(&self, _pane: HostPaneId) {}
        fn pane_focused(&self, _pane: HostPaneId) {}
        fn input_recorded(&self) {}
    }

    pub struct ProbeConfig;
    impl HostConfig for ProbeConfig {
        fn hyperlink_rules(&self) -> Arc<Vec<termwiz::hyperlink::Rule>> {
            Arc::new(Vec::new())
        }
        fn fetch_rate_per_second(&self) -> u32 {
            100
        }
    }

    /// Answers GetLines from a table of rows and everything else with
    /// UnitResponse, recording what was asked.
    #[derive(Default)]
    pub struct ProbeLink {
        pub rows: RefCell<Vec<(StableRowIndex, String)>>,
        pub asked: RefCell<Vec<String>>,
    }
    impl PduLink for ProbeLink {
        type Request = LocalFuture<Result<Pdu, LinkError>>;
        fn request(&self, pdu: Pdu) -> Self::Request {
            self.asked.borrow_mut().push(pdu.pdu_name().to_string());
            let answer = match pdu {
                Pdu::GetLines(req) => {
                    let table = self.rows.borrow();
                    let lines: Vec<(StableRowIndex, Line)> = req
                        .lines
                        .iter()
                        .flat_map(|range| range.clone())
                        .filter_map(|row| {
                            table.iter().find(|(r, _)| *r == row).map(|(r, text)| {
                                (
                                    *r,
                                    Line::from_text(
                                        text,
                                        &CellAttributes::default(),
                                        SEQ_ZERO,
                                        None,
                                    ),
                                )
                            })
                        })
                        .collect();
                    Pdu::GetLinesResponse(GetLinesResponse {
                        pane_id: req.pane_id,
                        lines: lines.into(),
                    })
                }
                Pdu::GetPaneRenderChanges(req) => Pdu::LivenessResponse(codec::LivenessResponse {
                    pane_id: req.pane_id,
                    is_alive: true,
                }),
                Pdu::WriteToPane(w) => {
                    // recorded by content, as the input tests read it
                    self.asked.borrow_mut().pop();
                    self.asked
                        .borrow_mut()
                        .push(format!("bytes:{}", String::from_utf8_lossy(&w.data)));
                    Pdu::UnitResponse(UnitResponse {})
                }
                _ => Pdu::UnitResponse(UnitResponse {}),
            };
            Box::pin(async move { Ok(answer) })
        }
        fn is_reconnectable(&self) -> bool {
            true
        }
        fn connection_generation(&self) -> u64 {
            1
        }
    }
    impl PaneLink for ProbeLink {
        type Prepare = LocalFuture<anyhow::Result<bool>>;
        fn prepare(&self, _remote_tab_id: thinkterm_proto::TabId) -> Self::Prepare {
            Box::pin(async { Ok(true) })
        }
    }

    pub struct ProbeHost {
        pub clock: ProbeClock,
        pub spawner: ProbeSpawner,
        pub events: ProbeEvents,
        pub link: ProbeLink,
        pub config: ProbeConfig,
    }
    impl SessionHost for ProbeHost {
        type Clock = ProbeClock;
        type Spawner = ProbeSpawner;
        type Events = ProbeEvents;
        type Link = ProbeLink;
        type Config = ProbeConfig;
        fn clock(&self) -> &ProbeClock {
            &self.clock
        }
        fn spawner(&self) -> &ProbeSpawner {
            &self.spawner
        }
        fn events(&self) -> &ProbeEvents {
            &self.events
        }
        fn link(&self) -> &ProbeLink {
            &self.link
        }
        fn config(&self) -> &ProbeConfig {
            &self.config
        }
        fn image_domain(&self) -> usize {
            1
        }
    }

    fn dims() -> RenderableDimensions {
        RenderableDimensions {
            cols: 80,
            viewport_rows: 24,
            scrollback_rows: 24,
            physical_top: 0,
            scrollback_top: 0,
            dpi: 96,
            pixel_width: 800,
            pixel_height: 480,
            reverse_video: false,
        }
    }

    fn delta(
        seqno: usize,
        cursor_x: usize,
        serial: Option<u64>,
        bonus: Vec<(StableRowIndex, &str)>,
        dirty: Vec<std::ops::Range<StableRowIndex>>,
    ) -> GetPaneRenderChangesResponse {
        GetPaneRenderChangesResponse {
            pane_id: 9,
            mouse_grabbed: false,
            alt_screen: false,
            keyboard_encoding: Default::default(),
            cursor_position: StableCursorPosition {
                x: cursor_x,
                y: 2,
                ..Default::default()
            },
            dimensions: dims(),
            dirty_lines: dirty,
            title: "probe".into(),
            working_dir: None,
            bonus_lines: bonus
                .into_iter()
                .map(|(row, text)| {
                    (
                        row,
                        Line::from_text(text, &CellAttributes::default(), SEQ_ZERO, None),
                    )
                })
                .collect::<Vec<_>>()
                .into(),
            input_serial: serial.map(InputSerial::from_millis),
            seqno,
        }
    }

    fn text_of(lines: &[Line]) -> Vec<String> {
        lines
            .iter()
            .map(|l| l.as_str().trim_end().to_string())
            .collect()
    }

    /// Returns 0 when every check passes, else the number of the first
    /// failed check.
    pub fn stage7() -> u32 {
        let host = Arc::new(ProbeHost {
            clock: ProbeClock(Cell::new(0)),
            spawner: ProbeSpawner::default(),
            events: ProbeEvents::default(),
            link: ProbeLink::default(),
            config: ProbeConfig,
        });
        *host.link.rows.borrow_mut() = vec![(2, "row 2".into()), (3, "row 3".into())];
        let session = PaneSession::new(
            Arc::clone(&host),
            Arc::new(Lock::new(ImageStore::default())),
            SessionConfig {
                scrollback_lines: 256,
                local_echo_threshold_ms: Some(0),
                overlay_lag_indicator: false,
            },
            9,
            Arc::new(AtomicUsize::new(0)),
            1,
            dims(),
            "probe",
            false,
        );

        // A push with two bonus rows and two dirty rows; the dirty ones are
        // fetched through the link.
        session.queue_render_delta(delta(
            5,
            0,
            Some(1_000),
            vec![(0, "row 0"), (1, "row 1")],
            vec![2..4],
        ));
        host.spawner.drain();
        if session.current_seqno() != 5 {
            return 1;
        }
        if session.dimensions().cols != 80 {
            return 2;
        }
        if host.events.outputs.get() == 0 {
            return 3;
        }
        if !host
            .link
            .asked
            .borrow()
            .iter()
            .any(|name| name == "GetLines")
        {
            return 4;
        }
        let (start, lines) = session.get_lines(0..4);
        host.spawner.drain();
        if start != 0 || text_of(&lines) != ["row 0", "row 1", "row 2", "row 3"] {
            return 5;
        }

        // Predictive echo: a key typed at the cursor shows up before the
        // server answers, and the cursor moves with it.
        session.predict_from_key_event(
            InputSerial::from_millis(2_000),
            KeyCode::Char('x'),
            KeyModifiers::NONE,
        );
        let (_, lines) = session.get_lines(2..3);
        if text_of(&lines) != ["xow 2"] {
            return 6;
        }
        if session.cursor_position().x != 1 {
            return 7;
        }

        // A push answering an OLDER key must not move the cursor back.
        session.queue_render_delta(delta(6, 0, Some(1_500), vec![], vec![]));
        host.spawner.drain();
        if session.cursor_position().x != 1 {
            return 8;
        }
        // One answering the latest key does.
        session.queue_render_delta(delta(7, 0, Some(2_000), vec![], vec![]));
        host.spawner.drain();
        if session.cursor_position().x != 0 {
            return 9;
        }
        0
    }

    pub fn stage8() -> u32 {
        let link = Arc::new(ProbeLink::default());
        let queue: Arc<Lock<InputQueue>> = Default::default();
        {
            let mut q = queue.lock();
            q.push(PaneInput::Bytes(b"ni".to_vec())).unwrap();
            q.push(PaneInput::Bytes(b"hao".to_vec())).unwrap();
            q.push(PaneInput::Key {
                event: termwiz::input::KeyEvent {
                    key: KeyCode::Enter,
                    modifiers: KeyModifiers::NONE,
                },
                input_serial: InputSerial::from_millis(1),
            })
            .unwrap();
            q.push(PaneInput::Paste("p".into())).unwrap();
        }
        spin_on(drain_pane_inputs(
            &*link,
            7,
            Arc::new(AtomicUsize::new(0)),
            Arc::clone(&queue),
        ));
        let asked = link.asked.borrow().clone();
        if asked != ["bytes:nihao", "SendKeyDown", "SendPaste"] {
            return 1;
        }
        // Everything was answered and settled: the queue takes the limit again.
        if queue
            .lock()
            .push(PaneInput::Bytes(vec![b'x'; 4 * 1024 * 1024]))
            .is_err()
        {
            return 2;
        }
        0
    }
}

#[no_mangle]
pub extern "C" fn stage7_pane_session() -> u32 {
    session_probe::stage7()
}

#[no_mangle]
pub extern "C" fn stage8_input_queue() -> u32 {
    session_probe::stage8()
}
