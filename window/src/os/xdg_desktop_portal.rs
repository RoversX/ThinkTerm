#![cfg(all(unix, not(target_os = "macos")))]

//! <https://github.com/flatpak/xdg-desktop-portal/blob/main/data/org.freedesktop.portal.Settings.xml>

use crate::{Appearance, Connection, ConnectionOps};
use anyhow::Context;
use futures_lite::future::FutureExt;
use futures_util::stream::StreamExt;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::Instant;
use url::Url;
use zbus::proxy;
use zvariant::{OwnedObjectPath, OwnedValue, Value};

#[proxy(
    interface = "org.freedesktop.portal.Settings",
    default_service = "org.freedesktop.portal.Desktop",
    default_path = "/org/freedesktop/portal/desktop"
)]
trait PortalSettings {
    fn ReadAll(
        &self,
        namespaces: &[&str],
    ) -> zbus::Result<HashMap<String, HashMap<String, OwnedValue>>>;

    fn Read(&self, namespace: &str, key: &str) -> zbus::Result<OwnedValue>;

    #[zbus(signal)]
    fn SettingChanged(&self, namespace: &str, key: &str, value: OwnedValue) -> zbus::Result<()>;
}

#[proxy(
    interface = "org.freedesktop.portal.FileChooser",
    default_service = "org.freedesktop.portal.Desktop",
    default_path = "/org/freedesktop/portal/desktop"
)]
trait PortalFileChooser {
    fn OpenFile(
        &self,
        parent_window: &str,
        title: &str,
        options: HashMap<&str, Value<'_>>,
    ) -> zbus::Result<OwnedObjectPath>;
}

#[proxy(
    interface = "org.freedesktop.portal.Request",
    default_service = "org.freedesktop.portal.Desktop"
)]
trait PortalRequest {
    #[zbus(signal)]
    fn Response(&self, response: u32, results: HashMap<String, OwnedValue>) -> zbus::Result<()>;
}

#[derive(PartialEq)]
enum CachedAppearance {
    /// Never tried to determine appearance
    Unknown,
    /// Tried and failed
    None,
    /// We got it
    Some(Appearance),
}

impl CachedAppearance {
    fn to_result(&self) -> anyhow::Result<Option<Appearance>> {
        match self {
            Self::Unknown => anyhow::bail!("Appearance is Unknown"),
            Self::None => Ok(None),
            Self::Some(a) => Ok(Some(*a)),
        }
    }
}

struct State {
    appearance: CachedAppearance,
    subscribe_running: bool,
    last_update: Instant,
}

lazy_static::lazy_static! {
  static ref STATE: Mutex<State> = Mutex::new(
          State {
              appearance: CachedAppearance::Unknown,
              subscribe_running: false,
              last_update: Instant::now(),
          }
   );
}

pub async fn read_setting(namespace: &str, key: &str) -> anyhow::Result<OwnedValue> {
    let connection = zbus::ConnectionBuilder::session()?.build().await?;
    let proxy = PortalSettingsProxy::new(&connection)
        .await
        .context("make proxy")?;

    proxy
        .Read(namespace, key)
        .or(async {
            async_io::Timer::after(std::time::Duration::from_secs(1)).await;
            Err(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                "Timed out reading from xdg-portal; this indicates a problem \
                 with your graphical environment. Consider running \
                 'systemctl restart --user xdg-desktop-portal.service'",
            )
            .into())
        })
        .await
        .with_context(|| format!("Reading xdg-portal {namespace} {key}"))
}

fn value_to_appearance(value: OwnedValue) -> anyhow::Result<Appearance> {
    Ok(match value.downcast_ref::<u32>() {
        Ok(1) => Appearance::Dark,
        Ok(_) => Appearance::Light,
        Err(err) => {
            anyhow::bail!(
                "Unable to resolve appearance \
                 using xdg-desktop-portal: {err:#?}"
            );
        }
    })
}

pub async fn get_appearance() -> anyhow::Result<Option<Appearance>> {
    let mut state = STATE.lock().unwrap();

    match &state.appearance {
        CachedAppearance::Some(_)
            if (state.subscribe_running || state.last_update.elapsed().as_secs() < 1) =>
        {
            // Known values are considered good while our subscription is running,
            // or for 1 second since we last queried
            return state.appearance.to_result();
        }
        CachedAppearance::None => {
            // Permanently cache the error state
            return Ok(None);
        }
        CachedAppearance::Some(_) | CachedAppearance::Unknown => {
            // We'll need to query for these
        }
    }

    match read_setting("org.freedesktop.appearance", "color-scheme").await {
        Ok(value) => {
            let appearance = value_to_appearance(value).context("value_to_appearance")?;
            state.appearance = CachedAppearance::Some(appearance);
            state.last_update = Instant::now();
            Ok(Some(appearance))
        }
        Err(err) => {
            // Cache that we didn't get any value, so we can avoid
            // repeating this query again later
            state.appearance = CachedAppearance::None;
            state.last_update = Instant::now();
            // but bubble up the underlying message so that we can
            // log a warning elsewhere
            Err(err).context("get_appearance.read_setting")
        }
    }
}

fn first_file_uri_to_path(
    results: &HashMap<String, OwnedValue>,
) -> anyhow::Result<Option<PathBuf>> {
    let Some(uris) = results.get("uris") else {
        return Ok(None);
    };
    let uris = Vec::<String>::try_from(
        uris.try_clone()
            .context("clone portal returned uris value")?,
    )
    .map_err(|err| anyhow::anyhow!("portal returned invalid uris: {err:#?}"))?;
    let Some(uri) = uris.first() else {
        return Ok(None);
    };
    let url = Url::parse(uri).with_context(|| format!("parsing portal uri {uri:?}"))?;
    url.to_file_path()
        .map(Some)
        .map_err(|()| anyhow::anyhow!("portal uri is not a file path: {uri:?}"))
}

pub async fn pick_folder() -> anyhow::Result<Option<PathBuf>> {
    let connection = zbus::ConnectionBuilder::session()?.build().await?;
    let proxy = PortalFileChooserProxy::new(&connection)
        .await
        .context("make file chooser proxy")?;

    let mut options = HashMap::new();
    options.insert("directory", Value::from(true));
    options.insert("modal", Value::from(true));

    let handle = proxy
        .OpenFile("", "Open Project", options)
        .or(async {
            async_io::Timer::after(std::time::Duration::from_secs(1)).await;
            Err(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                "Timed out opening xdg-desktop-portal folder picker",
            )
            .into())
        })
        .await
        .context("Opening xdg-desktop-portal folder picker")?;

    let request = PortalRequestProxy::builder(&connection)
        .path(handle)?
        .build()
        .await
        .context("make portal request proxy")?;
    let mut stream = request
        .receive_Response()
        .await
        .context("subscribe to portal request response")?;
    let signal = stream
        .next()
        .or(async {
            async_io::Timer::after(std::time::Duration::from_secs(60)).await;
            None
        })
        .await
        .ok_or_else(|| anyhow::anyhow!("Timed out waiting for folder picker response"))?;
    let args = signal.args().context("decode folder picker response")?;
    if args.response != 0 {
        return Ok(None);
    }

    first_file_uri_to_path(&args.results)
}

pub fn pick_folder_async(callback: Box<dyn FnOnce(Option<PathBuf>) + 'static>) {
    promise::spawn::spawn(async move {
        let path = match pick_folder().await {
            Ok(path) => path,
            Err(err) => {
                log::warn!("failed to show xdg-desktop-portal folder picker: {err:#}");
                None
            }
        };
        callback(path);
    })
    .detach();
}

/// File picker used to choose an application (a .desktop entry or an
/// executable); the caller normalizes the selection.
pub async fn pick_app() -> anyhow::Result<Option<PathBuf>> {
    let connection = zbus::ConnectionBuilder::session()?.build().await?;
    let proxy = PortalFileChooserProxy::new(&connection)
        .await
        .context("make file chooser proxy")?;

    let mut options = HashMap::new();
    options.insert("modal", Value::from(true));

    let handle = proxy
        .OpenFile("", "Choose Application", options)
        .or(async {
            async_io::Timer::after(std::time::Duration::from_secs(1)).await;
            Err(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                "Timed out opening xdg-desktop-portal app picker",
            )
            .into())
        })
        .await
        .context("Opening xdg-desktop-portal app picker")?;

    let request = PortalRequestProxy::builder(&connection)
        .path(handle)?
        .build()
        .await
        .context("make portal request proxy")?;
    let mut stream = request
        .receive_Response()
        .await
        .context("subscribe to portal request response")?;
    let signal = stream
        .next()
        .or(async {
            async_io::Timer::after(std::time::Duration::from_secs(60)).await;
            None
        })
        .await
        .ok_or_else(|| anyhow::anyhow!("Timed out waiting for app picker response"))?;
    let args = signal.args().context("decode app picker response")?;
    if args.response != 0 {
        return Ok(None);
    }

    first_file_uri_to_path(&args.results)
}

pub fn pick_app_async(callback: Box<dyn FnOnce(Option<PathBuf>) + 'static>) {
    promise::spawn::spawn(async move {
        let path = match pick_app().await {
            Ok(path) => path,
            Err(err) => {
                log::warn!("failed to show xdg-desktop-portal app picker: {err:#}");
                None
            }
        };
        callback(path);
    })
    .detach();
}

pub async fn run_signal_loop(stream: &mut SettingChangedStream<'_>) -> Result<(), anyhow::Error> {
    // query appearance again as it might have changed without us knowing
    if let Ok(value) =
        value_to_appearance(read_setting("org.freedesktop.appearance", "color-scheme").await?)
    {
        let mut state = STATE.lock().unwrap();
        if state.appearance != CachedAppearance::Some(value) {
            state.appearance = CachedAppearance::Some(value);
            state.last_update = Instant::now();
            drop(state);
            let conn = Connection::get().ok_or_else(|| anyhow::anyhow!("connection is dead"))?;
            conn.advise_of_appearance_change(value);
        }
    }

    while let Some(signal) = stream.next().await {
        let args = signal.args()?;
        if args.namespace == "org.freedesktop.appearance" && args.key == "color-scheme" {
            if let Ok(appearance) = value_to_appearance(args.value) {
                let mut state = STATE.lock().unwrap();
                state.appearance = CachedAppearance::Some(appearance);
                state.last_update = Instant::now();
                drop(state);
                let conn =
                    Connection::get().ok_or_else(|| anyhow::anyhow!("connection is dead"))?;
                conn.advise_of_appearance_change(appearance);
            }
        }
    }
    Result::<(), anyhow::Error>::Ok(())
}

pub fn subscribe() {
    promise::spawn::spawn(async move {
        let connection = zbus::ConnectionBuilder::session()?.build().await?;
        let proxy = PortalSettingsProxy::new(&connection)
            .await
            .context("make proxy")?;
        let mut stream = proxy.receive_SettingChanged().await?;

        STATE.lock().unwrap().subscribe_running = true;
        let res = run_signal_loop(&mut stream).await;
        STATE.lock().unwrap().subscribe_running = false;

        res
    })
    .detach();
}
