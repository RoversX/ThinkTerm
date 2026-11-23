//! The bridge without a mux, for trying it out against an agent:
//! `bridge <agent> <pane> [cwd]`, ops on stdin, events on stdout.

fn main() -> anyhow::Result<()> {
    #[cfg(unix)]
    {
        let mut args = std::env::args().skip(1);
        let agent = args.next().unwrap_or_else(|| "codex".into());
        let pane = args.next().and_then(|p| p.parse().ok()).unwrap_or(0);
        let cwd = args.next().map(std::path::PathBuf::from);
        let target = thinkterm_agent_chat::host::Target {
            pane,
            agent,
            cwd,
            mux_socket: None,
        };
        thinkterm_agent_chat::host::run(
            target,
            std::io::BufReader::new(std::io::stdin()),
            thinkterm_agent_chat::host::Out::stdout(),
            // No mux here: nothing is typed into the pane.
            None,
        )?;
    }
    Ok(())
}
