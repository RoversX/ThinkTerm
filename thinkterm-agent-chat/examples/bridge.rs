//! The bridge without a mux, for trying it out against an agent:
//! `bridge <agent> <pane> [cwd] [mux socket]`, ops on stdin, events on
//! stdout. The mux socket the pane's programs name keeps another mux's
//! pane of the same number out.

fn main() -> anyhow::Result<()> {
    #[cfg(unix)]
    {
        let mut args = std::env::args().skip(1);
        let agent = args.next().unwrap_or_else(|| "codex".into());
        let pane = args.next().and_then(|p| p.parse().ok()).unwrap_or(0);
        let cwd = args.next().filter(|cwd| !cwd.is_empty()).map(std::path::PathBuf::from);
        let mux_socket = args.next();
        let target = thinkterm_agent_chat::host::Target {
            pane,
            agent,
            cwd,
            mux_socket,
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
