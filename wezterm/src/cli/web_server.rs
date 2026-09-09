//! `thinkterm cli web-server …`: turn the browser listener on and off, and
//! see where it is accepting.
//!
//! The port is bound by the mux server, and `web_servers` in the
//! configuration is read only when one starts; this and the Web section of
//! the desktop's Settings are the two ways to change it while it runs.
//! Both talk to the mux server, never to a GUI's in-process mux.

use anyhow::bail;
use clap::Parser;
use wezterm_client::client::Client;

#[derive(Debug, Parser, Clone)]
pub struct WebServerCommand {
    #[command(subcommand)]
    sub: WebServerSubCommand,
}

#[derive(Debug, Parser, Clone)]
enum WebServerSubCommand {
    /// Show whether the server is accepting browser clients, and where.
    #[command(name = "status")]
    Status,

    /// Start accepting browser clients.
    ///
    /// Nobody gets in without a token: mint one with
    /// `thinkterm cli web-token mint`.
    #[command(name = "on")]
    On(OnCommand),

    /// Stop accepting browser clients and close the port.
    ///
    /// Live browsers lose their connection. Tokens are left alone; revoke
    /// them separately if that is what you meant.
    #[command(name = "off")]
    Off,
}

#[derive(Debug, Parser, Clone)]
struct OnCommand {
    /// The address to listen on, such as `127.0.0.1:8088`. Defaults to the
    /// first `web_servers` entry in the server's configuration.
    ///
    /// An address that is in the configuration brings its TLS, allowed
    /// origins and bundle directory with it; one that is not gets the
    /// defaults, which is loopback-only and plain HTTP.
    #[arg(long)]
    bind_address: Option<String>,
}

fn report(status: &codec::WebServerStatus) {
    if status.listening.is_empty() {
        println!("not accepting browser clients");
    } else {
        for address in &status.listening {
            println!("accepting browser clients on {address}");
        }
        for url in &status.urls {
            println!("  {url}");
        }
        if status.urls.is_empty() {
            // Listening on an address the configuration does not name, so
            // there is no `urls()` to build one from.
            println!("  (no page URL: this address is not in web_servers)");
        }
    }
    let extra: Vec<&String> = status
        .configured
        .iter()
        .filter(|address| !status.listening.contains(address))
        .collect();
    for address in extra {
        println!("configured but not accepting: {address}");
    }
}

impl WebServerCommand {
    pub async fn run(&self, client: Client) -> anyhow::Result<()> {
        match &self.sub {
            WebServerSubCommand::Status => {
                report(&client.get_web_server_status().await?);
            }
            WebServerSubCommand::On(cmd) => {
                let status = client
                    .set_web_server(codec::SetWebServer {
                        enabled: true,
                        bind_address: cmd.bind_address.clone(),
                    })
                    .await?;
                if status.listening.is_empty() {
                    bail!("the server did not start a web listener");
                }
                report(&status);
            }
            WebServerSubCommand::Off => {
                let status = client
                    .set_web_server(codec::SetWebServer {
                        enabled: false,
                        bind_address: None,
                    })
                    .await?;
                report(&status);
            }
        }
        Ok(())
    }
}
