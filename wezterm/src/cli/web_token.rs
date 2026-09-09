//! `thinkterm cli web-token …`: mint, list and revoke the tokens a browser
//! presents at the server's web port.

use crate::cli::CliOutputFormatKind;
use anyhow::{anyhow, bail};
use clap::Parser;
use tabout::{tabulate_output, Alignment, Column};
use wezterm_client::client::Client;

#[derive(Debug, Parser, Clone)]
pub struct WebTokenCommand {
    #[command(subcommand)]
    sub: WebTokenSubCommand,
}

#[derive(Debug, Parser, Clone)]
enum WebTokenSubCommand {
    /// Mint a token and print the URL to open.
    ///
    /// Whoever holds the token has the same access as you do on this
    /// machine: a shell as your user, every pane's scrollback. Treat it
    /// like an ssh key. `revoke` cuts it off at once.
    #[command(name = "mint")]
    Mint(MintCommand),

    /// List the tokens the server knows, with their live connections.
    #[command(name = "list")]
    List(ListCommand),

    /// Revoke a token (or every token) and drop its connections.
    #[command(name = "revoke")]
    Revoke(RevokeCommand),
}

#[derive(Debug, Parser, Clone)]
struct MintCommand {
    /// A name to recognise the token by; the browser shows up under it in
    /// `list-clients`.
    #[arg(long)]
    label: Option<String>,

    /// How long the token lives: a number of seconds, or with a suffix
    /// such as 30m, 12h, 7d. Without it the token lasts until revoked or
    /// until the server forgets it.
    #[arg(long)]
    ttl: Option<String>,

    /// Print only the URL, for scripts.
    #[arg(long)]
    url_only: bool,
}

#[derive(Debug, Parser, Clone)]
struct ListCommand {
    /// Controls the output format.
    /// "table" and "json" are possible formats.
    #[arg(long = "format", default_value = "table")]
    format: CliOutputFormatKind,
}

#[derive(Debug, Parser, Clone)]
struct RevokeCommand {
    /// The token id, as shown by `list`.
    id: Option<String>,

    /// Revoke every token.
    #[arg(long, conflicts_with = "id")]
    all: bool,
}

impl WebTokenCommand {
    pub async fn run(&self, client: Client) -> anyhow::Result<()> {
        match &self.sub {
            WebTokenSubCommand::Mint(cmd) => cmd.run(client).await,
            WebTokenSubCommand::List(cmd) => cmd.run(client).await,
            WebTokenSubCommand::Revoke(cmd) => cmd.run(client).await,
        }
    }
}

/// `90`, `90s`, `30m`, `12h`, `7d` → seconds.
fn parse_ttl(spec: &str) -> anyhow::Result<u64> {
    let spec = spec.trim();
    let (number, unit) = match spec.char_indices().rfind(|(_, c)| c.is_ascii_digit()) {
        Some((idx, _)) => spec.split_at(idx + 1),
        None => bail!("ttl `{spec}` has no number"),
    };
    let n: u64 = number
        .parse()
        .map_err(|_| anyhow!("ttl `{spec}` is not a number"))?;
    let scale = match unit.trim() {
        "" | "s" => 1,
        "m" => 60,
        "h" => 60 * 60,
        "d" => 24 * 60 * 60,
        other => bail!("ttl unit `{other}` is not one of s, m, h, d"),
    };
    n.checked_mul(scale)
        .ok_or_else(|| anyhow!("ttl `{spec}` is too large"))
}

fn local_time(secs: u64) -> String {
    chrono::DateTime::<chrono::Utc>::from_timestamp(secs as i64, 0)
        .map(|t| {
            t.with_timezone(&chrono::Local)
                .format("%Y-%m-%d %H:%M")
                .to_string()
        })
        .unwrap_or_else(|| secs.to_string())
}

impl MintCommand {
    async fn run(&self, client: Client) -> anyhow::Result<()> {
        let ttl_secs = self.ttl.as_deref().map(parse_ttl).transpose()?;
        let minted = client
            .web_token_mint(codec::WebTokenMint {
                label: self.label.clone(),
                ttl_secs,
            })
            .await?;
        if self.url_only {
            // The one address another device can use, when there is one;
            // a loopback URL is only good on this machine.
            let reachable = minted.urls.iter().find(|url| {
                url.split("://")
                    .nth(1)
                    .and_then(|rest| rest.split('/').next())
                    .and_then(config::split_authority)
                    .is_some_and(|(host, _)| !config::is_loopback_host(&host))
            });
            match reachable.or(minted.urls.first()) {
                Some(url) => println!("{url}"),
                None => {
                    // A script asked for a URL and there is none: do not
                    // leave a live token behind for it to try again with.
                    let _ = client
                        .web_token_revoke(codec::WebTokenRevoke {
                            id: Some(minted.id.clone()),
                        })
                        .await;
                    bail!("the server has no web_servers configured; nothing minted");
                }
            }
            return Ok(());
        }
        match &minted.label {
            Some(label) => println!("Token {} ({label}) minted.", minted.id),
            None => println!("Token {} minted.", minted.id),
        }
        match minted.expires_at {
            Some(at) => println!("Expires {}.", local_time(at)),
            None => println!("Does not expire; revoke it with `thinkterm cli web-token revoke {}`.", minted.id),
        }
        println!();
        println!("This token is a login as your user on this machine. Anyone holding it can");
        println!("open a shell and read every pane. Treat it like an ssh key.");
        println!();
        if minted.urls.is_empty() {
            println!("The server has no web_servers configured. The token is:");
            println!("  {}", minted.token);
        } else {
            println!("Open one of:");
            for url in &minted.urls {
                println!("  {url}");
            }
        }
        Ok(())
    }
}

#[derive(serde::Serialize)]
struct CliWebTokenItem {
    id: String,
    label: Option<String>,
    created_at: u64,
    expires_at: Option<u64>,
    last_used_at: Option<u64>,
    /// What the browser said it was, last time this link was used.
    last_device: Option<String>,
    live_connections: u32,
}

impl ListCommand {
    async fn run(&self, client: Client) -> anyhow::Result<()> {
        let response = client.web_token_list().await?;
        let out = std::io::stdout();
        match self.format {
            CliOutputFormatKind::Json => {
                let items: Vec<CliWebTokenItem> = response
                    .tokens
                    .into_iter()
                    .map(|t| CliWebTokenItem {
                        id: t.id,
                        label: t.label,
                        created_at: t.created_at,
                        expires_at: t.expires_at,
                        last_used_at: t.last_used_at,
                        last_device: t.last_device,
                        live_connections: t.live_connections,
                    })
                    .collect();
                serde_json::to_writer_pretty(out.lock(), &items)?;
                println!();
            }
            CliOutputFormatKind::Table => {
                let columns = [
                    Column { name: "ID".into(), alignment: Alignment::Left },
                    Column { name: "LABEL".into(), alignment: Alignment::Left },
                    Column { name: "LAST DEVICE".into(), alignment: Alignment::Left },
                    Column { name: "CREATED".into(), alignment: Alignment::Left },
                    Column { name: "EXPIRES".into(), alignment: Alignment::Left },
                    Column { name: "LAST USED".into(), alignment: Alignment::Left },
                    Column { name: "LIVE".into(), alignment: Alignment::Right },
                ];
                let rows: Vec<Vec<String>> = response
                    .tokens
                    .iter()
                    .map(|t| {
                        vec![
                            t.id.clone(),
                            t.label.clone().unwrap_or_else(|| "-".into()),
                            t.last_device.clone().unwrap_or_else(|| "-".into()),
                            local_time(t.created_at),
                            t.expires_at.map(local_time).unwrap_or_else(|| "never".into()),
                            t.last_used_at.map(local_time).unwrap_or_else(|| "-".into()),
                            t.live_connections.to_string(),
                        ]
                    })
                    .collect();
                tabulate_output(&columns, &rows, &mut out.lock())?;
            }
        }
        Ok(())
    }
}

impl RevokeCommand {
    async fn run(&self, client: Client) -> anyhow::Result<()> {
        let id = match (&self.id, self.all) {
            (Some(id), false) => Some(id.clone()),
            (None, true) => None,
            _ => bail!("give a token id, or --all"),
        };
        let response = client.web_token_revoke(codec::WebTokenRevoke { id }).await?;
        println!("Revoked {} token(s).", response.revoked);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::parse_ttl;

    #[test]
    fn ttl_specs() {
        assert_eq!(parse_ttl("90").unwrap(), 90);
        assert_eq!(parse_ttl("90s").unwrap(), 90);
        assert_eq!(parse_ttl("30m").unwrap(), 1800);
        assert_eq!(parse_ttl("12h").unwrap(), 43_200);
        assert_eq!(parse_ttl("7d").unwrap(), 604_800);
        assert!(parse_ttl("soon").is_err());
        assert!(parse_ttl("3w").is_err());
    }
}
