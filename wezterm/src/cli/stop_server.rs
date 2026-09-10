//! `thinkterm cli stop-server`: stop the background session server at the
//! default socket. Its terminals end with it; the next launch, `connect`
//! or CLI command that needs it starts a fresh one.

use anyhow::Context;
use clap::Parser;
use mux::session_server::StopOutcome;
use std::io::IsTerminal;
use std::time::Duration;

#[derive(Debug, Parser, Clone)]
pub struct StopServerCommand {
    /// Stop without asking, even at a terminal.
    #[arg(long)]
    force: bool,
}

impl StopServerCommand {
    pub fn run(&self, config: &config::ConfigHandle) -> anyhow::Result<()> {
        let pid_file = config.daemon_options.pid_file();
        let socket = config::RUNTIME_DIR.join(config::runtime_file_name("sock"));
        let Some(pid) = mux::session_server::pid_holding(&pid_file) else {
            println!("no session server is running (nothing holds {})", pid_file.display());
            return Ok(());
        };
        if !self.force && std::io::stdin().is_terminal() {
            eprint!(
                "Stop the session server (pid {pid}) at {}? Every terminal in it ends. [y/N] ",
                socket.display()
            );
            let mut answer = String::new();
            std::io::stdin().read_line(&mut answer).context("reading the answer")?;
            if !matches!(answer.trim(), "y" | "Y" | "yes") {
                println!("left running");
                return Ok(());
            }
        }
        match mux::session_server::stop(&pid_file, &socket, Duration::from_secs(10))? {
            StopOutcome::Stopped { pid } => println!("stopped the session server (pid {pid})"),
            StopOutcome::Lingering { pid } => {
                println!("asked the session server (pid {pid}) to stop; it is still shutting down")
            }
            StopOutcome::NotRunning => println!("no session server is running"),
        }
        Ok(())
    }
}
