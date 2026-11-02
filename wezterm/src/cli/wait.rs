use anyhow::{anyhow, bail};
use clap::Parser;
use codec::GetPaneRunState;
use mux::pane::PaneId;
use std::time::{Duration, Instant};
use thinkterm_proto::{PaneRunState, ProgramState, ShellCommands};
use wezterm_client::client::Client;

/// What `wait` exits with when it gives up.
pub const TIMED_OUT: i32 = 124;
/// What it exits with when a program or command ended without a known status.
pub const NO_STATUS: i32 = 125;

/// How long `wait --command` gives a command just typed to start.
const START_GRACE: Duration = Duration::from_secs(1);

#[derive(Debug, Parser, Clone)]
pub struct WaitCommand {
    /// The pane to wait for.
    /// The default is to use the current pane based on the
    /// environment variable WEZTERM_PANE.
    #[arg(long)]
    pane_id: Option<PaneId>,

    /// The pane by name rather than id: its label, its thread's name (a
    /// thread with one pane), or `thread/label`.
    #[arg(long, conflicts_with = "pane_id")]
    pane: Option<String>,

    /// Wait for the command running in the pane's shell to finish rather
    /// than for the pane's program to end, and exit with that command's
    /// status (125 when the shell gave none). When no command has started
    /// within a second, exits with the last command's status. To run a
    /// command and wait for it, `send-key --wait enter` is surer. Needs a
    /// shell that reports its commands (the shell integration's OSC 133
    /// marks).
    #[arg(long)]
    command: bool,

    /// Give up after this long, exiting with status 124: a number of
    /// seconds, or a number with s, m or h after it ("90", "10m"). `--for`
    /// is the same.
    #[arg(long, visible_alias = "for", value_parser = parse_timeout)]
    timeout: Option<Duration>,
}

impl WaitCommand {
    pub async fn run(self, client: Client) -> anyhow::Result<()> {
        let deadline = self.timeout.map(|t| Instant::now() + t);
        let code = with_deadline(deadline, async {
            let (client, pane_id) = crate::cli::target::resolve_reaching_servers(
                client.clone(),
                self.pane_id,
                self.pane.as_deref(),
            )
            .await?;
            Ok(if self.command {
                let before = shell_commands(&client, pane_id).await?;
                // A command typed a moment ago may not have reached the shell.
                let (watch_until, cut_short) = start_watch(Instant::now(), deadline);
                let mut shell = before;
                while !shell.running
                    && shell.finished == before.finished
                    && Instant::now() < watch_until
                {
                    let left = watch_until.saturating_duration_since(Instant::now());
                    smol::Timer::after(left.min(Duration::from_millis(50))).await;
                    shell = run_state(&client, pane_id).await?.shell;
                }
                if shell.running || shell.finished > before.finished {
                    wait_for_command(&client, pane_id, before.finished, deadline).await?
                } else if cut_short {
                    eprintln!("timed out");
                    TIMED_OUT
                } else if shell.finished == 0 {
                    0
                } else {
                    command_status(pane_id, shell.last_exit)
                }
            } else {
                wait_for_program(&client, pane_id, deadline).await?
            })
        })
        .await?;
        exit_with(code)
    }
}

/// Until when `wait --command` watches for a command to start, and whether
/// the timeout is what ends the watch: one shorter than the grace gives up
/// before a command could be told from none.
fn start_watch(now: Instant, deadline: Option<Instant>) -> (Instant, bool) {
    let grace_end = now + START_GRACE;
    match deadline {
        Some(deadline) if deadline < grace_end => (deadline, true),
        _ => (grace_end, false),
    }
}

/// Seconds, or a number followed by s, m or h.
pub fn parse_timeout(text: &str) -> anyhow::Result<Duration> {
    let text = text.trim();
    let (number, unit) = match text.find(|c: char| !c.is_ascii_digit() && c != '.') {
        Some(at) => text.split_at(at),
        None => (text, "s"),
    };
    let number: f64 = number
        .parse()
        .map_err(|_| anyhow!("{text:?} is not a duration such as 90, 30s or 10m"))?;
    let seconds = match unit {
        "s" => number,
        "m" => number * 60.0,
        "h" => number * 3600.0,
        _ => bail!("{text:?} is not a duration such as 90, 30s or 10m"),
    };
    Duration::try_from_secs_f64(seconds).map_err(|err| anyhow!("{text:?}: {err}"))
}

/// End the process with `code`, once what was printed has gone out.
pub fn exit_with(code: i32) -> ! {
    use std::io::Write;
    let _ = std::io::stdout().flush();
    std::process::exit(code)
}

async fn run_state(client: &Client, pane_id: PaneId) -> anyhow::Result<PaneRunState> {
    Ok(client
        .get_pane_run_state(GetPaneRunState { pane_id })
        .await?
        .state)
}

/// What the pane's shell has reported about its commands, or an error
/// saying it reports nothing.
pub async fn shell_commands(client: &Client, pane_id: PaneId) -> anyhow::Result<ShellCommands> {
    let shell = run_state(client, pane_id).await?.shell;
    if !shell.reported {
        bail!(
            "pane {pane_id}'s shell does not report when its commands finish; \
             load ThinkTerm's shell integration in it, or wait for a program \
             started with `spawn --wait` instead"
        );
    }
    Ok(shell)
}

/// Enforce the deadline across the whole wait, including in-flight RPCs.
/// Check again after polling: even an immediately ready result can be late.
/// The wait goes on the heap before the timer is wrapped around it: waits
/// nest, and each kept its own copy of the one inside, which outgrew the
/// main thread's stack in a debug build.
pub fn with_deadline<'a>(
    deadline: Option<Instant>,
    wait: impl std::future::Future<Output = anyhow::Result<i32>> + 'a,
) -> impl std::future::Future<Output = anyhow::Result<i32>> + 'a {
    let wait = Box::pin(wait);
    async move {
        let deadline = match deadline {
            Some(deadline) => deadline,
            None => return wait.await,
        };
        let result = smol::future::or(
            async {
                smol::Timer::at(deadline).await;
                None
            },
            async { Some(wait.await) },
        )
        .await;
        if Instant::now() < deadline {
            if let Some(result) = result {
                return result;
            }
        }
        eprintln!("timed out");
        Ok(TIMED_OUT)
    }
}

fn program_status(pane_id: PaneId, exit: &thinkterm_proto::PaneExit) -> i32 {
    exit.code.map(|code| code as i32).unwrap_or_else(|| {
        eprintln!("pane {pane_id}'s program ended without a known exit status");
        NO_STATUS
    })
}

/// Waits between asks: quick at first, for the short commands, and no
/// slower than a few times a second after that.
struct Pacing(Duration);

impl Pacing {
    fn new() -> Self {
        Self(Duration::from_millis(25))
    }

    async fn pause(&mut self) {
        smol::Timer::after(self.0).await;
        self.0 = (self.0 * 2).min(Duration::from_millis(250));
    }
}

/// Wait for the program `pane_id` started to end, and give back the status
/// to exit with.
pub async fn wait_for_program(
    client: &Client,
    pane_id: PaneId,
    deadline: Option<Instant>,
) -> anyhow::Result<i32> {
    with_deadline(deadline, async {
        let mut pacing = Pacing::new();
        let mut seen = false;
        let mut gone_since = None;
        loop {
            match run_state(client, pane_id).await {
                Ok(state) => match state.program {
                    ProgramState::Exited(exit) => {
                        if let Some(signal) = &exit.signal {
                            eprintln!("pane {pane_id}'s program was ended by {signal}");
                        }
                        return Ok(program_status(pane_id, &exit));
                    }
                    // A pane that cannot say still closes in the end.
                    ProgramState::Running | ProgramState::Unknown => seen = true,
                },
                // Gone. Its status can still be on its way: a pane is also
                // closed by its pty ending, before its program is reaped.
                Err(err) if seen && crate::cli::is_no_such_pane(&err) => {
                    let since = *gone_since.get_or_insert_with(Instant::now);
                    if since.elapsed() >= LATE_STATUS {
                        eprintln!("pane {pane_id} closed without saying how its program ended");
                        return Ok(1);
                    }
                }
                Err(err) => return Err(err),
            }
            pacing.pause().await;
        }
    })
    .await
}

/// How long a closed pane's status may still take to arrive.
const LATE_STATUS: Duration = Duration::from_secs(1);

/// What to exit with for a finished command's reported status.
fn command_status(pane_id: PaneId, status: Option<i32>) -> i32 {
    status.unwrap_or_else(|| {
        eprintln!("pane {pane_id}'s shell did not say how the command ended");
        NO_STATUS
    })
}

/// Wait until the pane's shell has finished more than `finished` commands,
/// and give back the status the last one reported.
pub async fn wait_for_command(
    client: &Client,
    pane_id: PaneId,
    finished: u64,
    deadline: Option<Instant>,
) -> anyhow::Result<i32> {
    with_deadline(deadline, async {
        let mut pacing = Pacing::new();
        loop {
            let state = run_state(client, pane_id).await?;
            if state.shell.finished > finished {
                return Ok(command_status(pane_id, state.shell.last_exit));
            }
            if let ProgramState::Exited(exit) = state.program {
                eprintln!("pane {pane_id}'s shell ended before the command finished");
                return Ok(program_status(pane_id, &exit));
            }
            pacing.pause().await;
        }
    })
    .await
}

#[cfg(test)]
mod tests {
    use super::{parse_timeout, start_watch, START_GRACE};
    use std::time::{Duration, Instant};

    #[test]
    fn a_stalled_rpc_times_out() {
        smol::block_on(async {
            let deadline = Instant::now() + Duration::from_millis(10);
            let code = super::with_deadline(Some(deadline), std::future::pending())
                .await
                .unwrap();
            assert_eq!(code, super::TIMED_OUT);
        });
    }

    #[test]
    fn an_expired_deadline_does_not_poll_the_rpc() {
        smol::block_on(async {
            let code = super::with_deadline(Some(Instant::now()), async {
                panic!("expired wait must not poll the RPC");
            })
            .await
            .unwrap();
            assert_eq!(code, super::TIMED_OUT);
        });
    }

    #[test]
    fn a_result_that_becomes_ready_after_the_deadline_is_not_success() {
        smol::block_on(async {
            let deadline = Instant::now() + Duration::from_millis(10);
            let code = super::with_deadline(Some(deadline), async {
                // Simulate a poll that runs across the deadline before returning.
                std::thread::sleep(Duration::from_millis(20));
                Ok(0)
            })
            .await
            .unwrap();
            assert_eq!(code, super::TIMED_OUT);
        });
    }

    #[test]
    fn a_deadline_keeps_its_wait_on_the_heap() {
        // However large the wait, the future around it is the same size.
        let small = super::with_deadline(None, async { Ok(0) });
        let large = super::with_deadline(None, async {
            let buffer = [0u8; 1 << 16];
            std::future::ready(()).await;
            Ok(i32::from(buffer[0]))
        });
        assert_eq!(std::mem::size_of_val(&small), std::mem::size_of_val(&large));
    }

    #[test]
    fn a_wait_without_a_deadline_preserves_errors_and_statuses() {
        smol::block_on(async {
            assert_eq!(
                super::with_deadline(None, async { Ok(7) }).await.unwrap(),
                7
            );
            let error = super::with_deadline(None, async { anyhow::bail!("RPC failed") })
                .await
                .unwrap_err();
            assert_eq!(error.to_string(), "RPC failed");
            let deadline = Instant::now() + Duration::from_secs(60);
            assert_eq!(
                super::with_deadline(Some(deadline), async { Ok(0) })
                    .await
                    .unwrap(),
                0
            );
        });
    }

    #[test]
    fn an_unknown_program_status_is_not_a_success() {
        let mut exit = thinkterm_proto::PaneExit {
            code: None,
            signal: None,
        };
        assert_eq!(super::program_status(1, &exit), super::NO_STATUS);
        exit.code = Some(0);
        assert_eq!(super::program_status(1, &exit), 0);
        exit.code = Some(7);
        assert_eq!(super::program_status(1, &exit), 7);
    }

    #[test]
    fn a_timeout_inside_the_start_grace_ends_the_watch() {
        let now = Instant::now();
        let short = now + Duration::from_millis(100);
        assert_eq!(start_watch(now, Some(short)), (short, true));
        assert_eq!(start_watch(now, None), (now + START_GRACE, false));
        let long = now + Duration::from_secs(5);
        assert_eq!(start_watch(now, Some(long)), (now + START_GRACE, false));
    }

    #[test]
    fn timeouts_read_seconds_minutes_and_hours() {
        assert_eq!(parse_timeout("90").unwrap(), Duration::from_secs(90));
        assert_eq!(parse_timeout("30s").unwrap(), Duration::from_secs(30));
        assert_eq!(parse_timeout("10m").unwrap(), Duration::from_secs(600));
        assert_eq!(parse_timeout("1.5h").unwrap(), Duration::from_secs(5400));
        assert_eq!(parse_timeout("0.5").unwrap(), Duration::from_millis(500));
        assert!(parse_timeout("soon").is_err());
        assert!(parse_timeout("10d").is_err());
        assert!(parse_timeout("").is_err());
    }
}
