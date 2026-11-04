use super::{emit, Intent};
use anyhow::{bail, Context};
use futures_lite::future;
use futures_util::StreamExt;
use nix::unistd::{Uid, User};
use pam_client::{ConversationHandler, ErrorCode, Flag};
use std::collections::HashMap;
use std::ffi::{CStr, CString};
use std::io::{IsTerminal, Read};
use std::os::unix::fs::{FileTypeExt, MetadataExt};
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::Duration;
use zeroize::Zeroizing;
use zvariant::{OwnedObjectPath, OwnedValue};

const SERVICE: &str = "thinkterm-connect-auth";
const ZENITY: &str = "/usr/bin/zenity";

fn local_x_display(display: &str) -> Option<&str> {
    let number = display
        .strip_prefix(':')
        .or_else(|| display.strip_prefix("unix:"))?;
    let mut parts = number.split('.');
    let number = parts.next()?;
    if number.is_empty() || number.len() > 6 || !number.bytes().all(|c| c.is_ascii_digit()) {
        return None;
    }
    if let Some(screen) = parts.next() {
        if screen.is_empty() || screen.len() > 6 || !screen.bytes().all(|c| c.is_ascii_digit()) {
            return None;
        }
    }
    if parts.next().is_some() {
        return None;
    }
    Some(number)
}

fn local_desktop_environment() -> anyhow::Result<()> {
    let uid = Uid::current().as_raw();
    let runtime = std::path::PathBuf::from(format!("/run/user/{uid}"));
    let metadata = std::fs::symlink_metadata(&runtime)?;
    if !metadata.is_dir() || metadata.uid() != uid || metadata.mode() & 0o077 != 0 {
        bail!("desktop runtime directory is unavailable");
    }
    let bus = runtime.join("bus");
    let metadata = std::fs::symlink_metadata(&bus)?;
    if !metadata.file_type().is_socket() || metadata.uid() != uid {
        bail!("desktop bus is unavailable");
    }
    let wayland = std::env::var("WAYLAND_DISPLAY").ok().filter(|name| {
        !name.is_empty()
            && name.len() <= 100
            && name
                .bytes()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'-' | b'_' | b'.'))
            && std::fs::symlink_metadata(runtime.join(name))
                .is_ok_and(|metadata| metadata.file_type().is_socket() && metadata.uid() == uid)
    });
    let display = std::env::var("DISPLAY").ok().filter(|display| {
        local_x_display(display).is_some_and(|number| {
            std::fs::symlink_metadata(format!("/tmp/.X11-unix/X{number}")).is_ok_and(|metadata| {
                metadata.file_type().is_socket() && (metadata.uid() == 0 || metadata.uid() == uid)
            })
        })
    });
    if wayland.is_none() && display.is_none() {
        bail!("local desktop display is unavailable");
    }
    // A forwarded X display or arbitrary D-Bus address must never receive
    // the authentication dialog or its password conversation.
    std::env::set_var("XDG_RUNTIME_DIR", &runtime);
    std::env::set_var(
        "DBUS_SESSION_BUS_ADDRESS",
        format!("unix:path={}", bus.display()),
    );
    std::env::remove_var("DBUS_SYSTEM_BUS_ADDRESS");
    std::env::remove_var("DISPLAY");
    std::env::remove_var("WAYLAND_DISPLAY");
    std::env::set_var(
        "GDK_BACKEND",
        match (&wayland, &display) {
            (Some(_), Some(_)) => "wayland,x11",
            (Some(_), None) => "wayland",
            _ => "x11",
        },
    );
    if let Some(name) = wayland {
        std::env::set_var("WAYLAND_DISPLAY", name);
    }
    if let Some(display) = display {
        std::env::set_var("DISPLAY", display);
    }
    Ok(())
}

fn trusted_system_file(path: &Path) -> anyhow::Result<()> {
    let path = path.canonicalize()?;
    for component in path.ancestors() {
        let metadata = std::fs::metadata(component)?;
        if metadata.uid() != 0 || metadata.mode() & 0o022 != 0 {
            bail!("system authentication component is not protected");
        }
    }
    Ok(())
}

fn dialog(args: &[&str]) -> anyhow::Result<Zeroizing<Vec<u8>>> {
    let mut child = Command::new(ZENITY)
        .args(args)
        .arg("--timeout=110")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()?;
    let mut bytes = Zeroizing::new(Vec::new());
    child
        .stdout
        .take()
        .context("dialog pipe")?
        .take(4097)
        .read_to_end(&mut bytes)?;
    if bytes.len() > 4096 {
        let _ = child.kill();
        let _ = child.wait();
        bail!("authentication response is too large");
    }
    if !child.wait()?.success() {
        bail!("authentication cancelled");
    }
    if bytes.last() == Some(&b'\n') {
        bytes.pop();
    }
    Ok(bytes)
}

struct Conversation {
    username: CString,
    title: String,
    prompted: bool,
    prompts: usize,
}

impl ConversationHandler for Conversation {
    fn prompt_echo_on(&mut self, _: &CStr) -> std::result::Result<CString, ErrorCode> {
        Ok(self.username.clone())
    }

    fn prompt_echo_off(&mut self, prompt: &CStr) -> std::result::Result<CString, ErrorCode> {
        self.prompts += 1;
        if self.prompts > 3 {
            return Err(ErrorCode::CONV_ERR);
        }
        let prompt = prompt.to_str().map_err(|_| ErrorCode::CONV_ERR)?;
        if prompt.len() > 512 || prompt.chars().any(char::is_control) {
            return Err(ErrorCode::CONV_ERR);
        }
        let password = dialog(&[
            "--password",
            &format!("--title={}", self.title),
            "--no-markup",
            "--text",
            prompt,
        ])
        .map_err(|_| ErrorCode::CONV_ERR)?;
        if password.is_empty() {
            return Err(ErrorCode::CONV_ERR);
        }
        self.prompted = true;
        // PAM owns its response allocation. The collector's temporary bytes
        // are erased, and this helper exits at the end of the transaction.
        CString::new(password.as_slice()).map_err(|_| ErrorCode::CONV_ERR)
    }

    fn text_info(&mut self, _: &CStr) {}
    fn error_msg(&mut self, _: &CStr) {}
}

fn authenticate(intent: Intent) -> anyhow::Result<()> {
    trusted_system_file(Path::new(ZENITY))?;
    trusted_system_file(Path::new("/etc/pam.d/thinkterm-connect-auth"))?;
    let user = User::from_uid(Uid::current())?.context("account is unavailable")?;
    if user.uid.is_root() {
        bail!("root authentication is unavailable");
    }
    let text = &intent.text;
    let summary = format!(
        "{}\n\n{}\n\n{}\n{}\n\n{}",
        text.heading, text.connection, text.terminals, intent.targets, text.detail,
    );
    // Declining is an answer, as a wrong password is below.
    if dialog(&[
        "--question",
        &format!("--title={}", text.title),
        "--no-markup",
        "--width=620",
        &format!("--ok-label={}", text.approve),
        &format!("--cancel-label={}", text.cancel),
        "--text",
        &summary,
    ])
    .is_err()
    {
        return Ok(emit("rejected", Some(&intent.id))?);
    }
    let conversation = Conversation {
        username: CString::new(user.name.clone())?,
        title: text.password.clone(),
        prompted: false,
        prompts: 0,
    };
    let mut pam = pam_client::Context::new(SERVICE, Some(&user.name), conversation)
        .map_err(|_| anyhow::anyhow!("system authentication is unavailable"))?;
    let approved = pam.authenticate(Flag::DISALLOW_NULL_AUTHTOK).is_ok()
        && pam.conversation().prompted
        && pam.acct_mgmt(Flag::DISALLOW_NULL_AUTHTOK).is_ok()
        && pam.user().ok().as_deref() == Some(user.name.as_str());
    drop(pam);
    emit(
        if approved { "approved" } else { "rejected" },
        Some(&intent.id),
    )?;
    Ok(())
}

async fn session_path(connection: &zbus::Connection) -> anyhow::Result<OwnedObjectPath> {
    let manager = zbus::Proxy::new(
        connection,
        "org.freedesktop.login1",
        "/org/freedesktop/login1",
        "org.freedesktop.login1.Manager",
    )
    .await?;
    if let Ok(path) = manager
        .call::<_, _, OwnedObjectPath>("GetSessionByPID", &(std::process::id(),))
        .await
    {
        return Ok(path);
    }
    // A persistent session host can be outside a logind session's cgroup.
    // Only an unambiguous active, local graphical session of this UID qualifies.
    let sessions: Vec<(String, u32, String, String, OwnedObjectPath)> =
        manager.call("ListSessions", &()).await?;
    let mut selected = None;
    for (_, uid, _, _, path) in sessions {
        if uid != Uid::current().as_raw() {
            continue;
        }
        let session = zbus::Proxy::new(
            connection,
            "org.freedesktop.login1",
            path.clone(),
            "org.freedesktop.login1.Session",
        )
        .await?;
        if available(&session).await.is_ok() {
            if selected.is_some() {
                bail!("desktop session is ambiguous");
            }
            selected = Some(path);
        }
    }
    selected.context("no active desktop session")
}

async fn available(session: &zbus::Proxy<'_>) -> anyhow::Result<()> {
    let (uid, _): (u32, OwnedObjectPath) = session.get_property("User").await?;
    let active: bool = session.get_property("Active").await?;
    let locked: bool = session.get_property("LockedHint").await?;
    let remote: bool = session.get_property("Remote").await?;
    let class: String = session.get_property("Class").await?;
    let kind: String = session.get_property("Type").await?;
    let seat: (String, OwnedObjectPath) = session.get_property("Seat").await?;
    if uid != Uid::current().as_raw()
        || !active
        || locked
        || remote
        || class != "user"
        || !matches!(kind.as_str(), "x11" | "wayland")
        || seat.0.is_empty()
    {
        bail!("desktop session is unavailable");
    }
    Ok(())
}

async fn screensaver(connection: &zbus::Connection) -> anyhow::Result<zbus::Proxy<'_>> {
    for (service, path, interface) in [
        (
            "org.gnome.ScreenSaver",
            "/org/gnome/ScreenSaver",
            "org.gnome.ScreenSaver",
        ),
        (
            "org.freedesktop.ScreenSaver",
            "/ScreenSaver",
            "org.freedesktop.ScreenSaver",
        ),
    ] {
        let proxy = zbus::Proxy::new(connection, service, path, interface).await?;
        if let Ok(active) = proxy.call::<_, _, bool>("GetActive", &()).await {
            if active {
                bail!("desktop is locked");
            }
            return Ok(proxy);
        }
    }
    bail!("desktop lock notifications are unavailable")
}

async fn monitor() -> anyhow::Result<()> {
    let system = zbus::Connection::system().await?;
    let desktop = zbus::Connection::session().await?;
    let path = session_path(&system).await?;
    let session = zbus::Proxy::new(
        &system,
        "org.freedesktop.login1",
        path.clone(),
        "org.freedesktop.login1.Session",
    )
    .await?;
    let properties = zbus::Proxy::new(
        &system,
        "org.freedesktop.login1",
        path,
        "org.freedesktop.DBus.Properties",
    )
    .await?;
    let manager = zbus::Proxy::new(
        &system,
        "org.freedesktop.login1",
        "/org/freedesktop/login1",
        "org.freedesktop.login1.Manager",
    )
    .await?;
    let saver = screensaver(&desktop).await?;
    let desktop_bus = zbus::fdo::DBusProxy::new(&desktop).await?;
    let saver_name: zbus::names::BusName<'_> = saver.destination().clone();
    let saver_owner = desktop_bus.get_name_owner(saver_name.clone()).await?;
    let mut changes = futures_util::stream::select_all(vec![
        session.receive_signal("Lock").await?.map(|_| 0).boxed(),
        session.receive_signal("Unlock").await?.map(|_| 0).boxed(),
        manager
            .receive_signal("PrepareForSleep")
            .await?
            .map(|_| 0)
            .boxed(),
        saver
            .receive_signal("ActiveChanged")
            .await?
            .map(|_| 0)
            .boxed(),
        properties
            .receive_signal("PropertiesChanged")
            .await?
            .map(|message| {
                match message
                    .body()
                    .deserialize::<(String, HashMap<String, OwnedValue>, Vec<String>)>()
                {
                    Ok((_, changed, invalidated))
                        if !["LockedHint", "Active", "State"].iter().any(|key| {
                            changed.contains_key(*key)
                                || invalidated.iter().any(|entry| entry == key)
                        }) =>
                    {
                        2
                    }
                    _ => 1,
                }
            })
            .boxed(),
    ]);
    available(&session).await?;
    emit("ready", None)?;
    loop {
        let event = future::or(async { changes.next().await.unwrap_or(0) }, async {
            smol::Timer::after(Duration::from_secs(1)).await;
            1
        })
        .await;
        if event == 0 {
            bail!("desktop session changed");
        }
        if event == 2 {
            continue;
        }
        available(&session).await?;
        if saver.call::<_, _, bool>("GetActive", &()).await?
            || desktop_bus.get_name_owner(saver_name.clone()).await? != saver_owner
        {
            bail!("desktop lock monitor changed");
        }
        emit("alive", None)?;
    }
}

pub(super) fn run() -> anyhow::Result<()> {
    if std::io::stdin().is_terminal() || std::io::stdout().is_terminal() || Uid::current().is_root()
    {
        bail!("a private control pipe and desktop account are required");
    }
    if nix::unistd::getpgrp() != nix::unistd::getpid() {
        bail!("an isolated helper process group is required");
    }
    local_desktop_environment()?;
    let mut args = std::env::args().skip(1);
    let mode = args.next().context("mode is required")?;
    if args.next().is_some() {
        bail!("unexpected argument");
    }
    let intent = if mode == "--authenticate" {
        Some(Intent::read(&mut std::io::stdin().lock()).context("invalid intent")?)
    } else if mode == "--monitor" {
        None
    } else {
        bail!("unknown mode");
    };
    std::thread::spawn(|| {
        let mut byte = [0];
        let _ = std::io::stdin().read(&mut byte);
        // Parent death/cancellation must also close any native child dialog.
        let _ = nix::sys::signal::killpg(nix::unistd::getpgrp(), nix::sys::signal::Signal::SIGKILL);
    });
    if let Some(intent) = intent {
        std::thread::spawn(|| {
            std::thread::sleep(Duration::from_secs(120));
            let _ =
                nix::sys::signal::killpg(nix::unistd::getpgrp(), nix::sys::signal::Signal::SIGKILL);
        });
        authenticate(intent)
    } else {
        let result = smol::block_on(monitor());
        let _ = emit("unavailable", None);
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn authentication_display_never_uses_an_ssh_or_tcp_forward() {
        assert_eq!(local_x_display(":0"), Some("0"));
        assert_eq!(local_x_display(":1.0"), Some("1"));
        assert_eq!(local_x_display("unix:2.1"), Some("2"));
        for display in [
            "localhost:10.0",
            "server-a:0",
            "192.0.2.1:0",
            ":",
            ":1.",
            ":1.0.0",
        ] {
            assert_eq!(local_x_display(display), None);
        }
    }
}
