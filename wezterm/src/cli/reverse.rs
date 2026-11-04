use super::{NoSuchTarget, UsageError};
use anyhow::{anyhow, bail};
use clap::Parser;
use smol::future::FutureExt;
use smol::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use thinkterm_reverse_access::protocol::{Command, ExportRef, InputResult, InputStatus, Reply};
use thinkterm_reverse_access::{Id, Permission};
use wezterm_client::client::Client;
use wezterm_client::connect::{Endpoint, RouterClient};

#[derive(Debug, Clone, Parser)]
pub struct Proxy {}

impl Proxy {
    pub async fn run(&self, client: Client) -> anyhow::Result<()> {
        let stream = Endpoint::for_client(&client).await?.stream().await?;
        drop(client);
        let (mut reader, mut writer) = futures::io::AsyncReadExt::split(stream);
        let mut input = smol::Unblock::with_capacity(8192, std::io::stdin());
        let mut output = smol::Unblock::with_capacity(8192, std::io::stdout());
        pump(&mut input, &mut writer)
            .or(pump(&mut reader, &mut output))
            .await?;
        Ok(())
    }
}

async fn pump<R: AsyncRead + Unpin, W: AsyncWrite + Unpin>(
    reader: &mut R,
    writer: &mut W,
) -> std::io::Result<()> {
    let mut bytes = [0; 8192];
    loop {
        let count = reader.read(&mut bytes).await?;
        if count == 0 {
            return Ok(());
        }
        writer.write_all(&bytes[..count]).await?;
        writer.flush().await?;
    }
}

#[derive(serde::Serialize)]
pub struct Listed {
    pub pane_id: String,
    pub machine: String,
    pub label: String,
    pub title: String,
    pub reverse_access: bool,
    pub permission: Permission,
    pub remaining_ms: u64,
}

pub async fn list(client: &Client) -> anyhow::Result<Vec<Listed>> {
    let router = RouterClient::new(client).await?;
    let mut listed = Vec::new();
    for route in router.routes().await? {
        // One expired or disconnected desktop must not hide other routes.
        let Ok(exports) = router.exports(route.id).await else {
            continue;
        };
        for item in exports {
            listed.push(Listed {
                pane_id: format!("{}/{}", route.name, item.export.target.pane),
                machine: route.name.clone(),
                label: item.name,
                title: item.title,
                reverse_access: true,
                permission: item.permission,
                remaining_ms: item.remaining_ms,
            });
        }
    }
    Ok(listed)
}

pub struct Target {
    pub router: RouterClient,
    pub route: Id,
    pub export: ExportRef,
    permission: Permission,
}

pub fn looks_like_target(name: &str) -> bool {
    let Some((machine, _)) = name.trim().split_once('/') else {
        return false;
    };
    let Some(suffix) = machine.strip_prefix("desktop-") else {
        return false;
    };
    suffix.len() == 8 && suffix.bytes().all(|b| b.is_ascii_hexdigit())
}

pub async fn resolve(client: &Client, name: Option<&str>) -> anyhow::Result<Option<Target>> {
    let Some(name) = name.filter(|name| looks_like_target(name)) else {
        return Ok(None);
    };
    let (machine, pane) = name
        .trim()
        .split_once('/')
        .ok_or_else(|| UsageError("invalid reverse target".into()))?;
    let router = RouterClient::new(client).await?;
    let routes = router.routes().await?;
    let matching: Vec<_> = routes
        .into_iter()
        .filter(|route| route.name == machine)
        .collect();
    let [route] = matching.as_slice() else {
        return Err(NoSuchTarget("reverse terminal is unavailable or ambiguous".into()).into());
    };
    let exports = router.exports(route.id).await?;
    let matching: Vec<_> = exports
        .into_iter()
        .filter(|item| {
            pane.parse::<u64>().ok() == Some(item.export.target.pane) || item.name == pane
        })
        .collect();
    let [export] = matching.as_slice() else {
        return Err(NoSuchTarget("reverse terminal is unavailable or ambiguous".into()).into());
    };
    if super::target::local_match(client, name).await? {
        return Err(NoSuchTarget(
            "the reverse terminal name also identifies a local terminal".into(),
        )
        .into());
    }
    Ok(Some(Target {
        route: route.id,
        export: export.export,
        permission: export.permission,
        router,
    }))
}

pub fn input_result(reply: Reply) -> anyhow::Result<()> {
    match reply {
        Reply::Input(InputResult { status: InputStatus::Submitted, .. }) => Ok(()),
        Reply::Input(result) => bail!("input {:?}: {} bytes submitted; Enter submitted: {}; paste may be open: {}. Do not retry automatically",
            result.status, result.bytes, result.enter_submitted, result.paste_may_be_open),
        Reply::Failed(failure) => bail!("reverse input was rejected: {:?}", failure),
        _ => Err(anyhow!("invalid reverse input response; input outcome is unknown")),
    }
}

/// The desktop refuses this too, and without a reason, as it does anything it
/// refuses. The listing has already told this caller the permission, so saying
/// what is wrong gives nothing away. The desktop's check is still the one that
/// counts: this copy can be a moment old.
fn check_input_allowed(permission: Permission) -> anyhow::Result<()> {
    if permission != Permission::ReadAndInput {
        bail!("this reverse terminal is read-only; nothing was sent");
    }
    Ok(())
}

pub async fn send(target: Target, command: Command, input: Option<&str>) -> anyhow::Result<()> {
    check_input_allowed(target.permission)?;
    let (reply, _) = target
        .router
        .request(target.route, command, input)
        .await
        .map_err(|error| {
            anyhow!("reverse input outcome is unknown ({error}); do not retry automatically")
        })?;
    input_result(reply)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn input_on_read_only_access_says_so() {
        assert!(check_input_allowed(Permission::ReadAndInput).is_ok());
        let refused = check_input_allowed(Permission::Read).unwrap_err();
        assert!(refused.to_string().contains("read-only"));
    }
}
