//! Configuration for the gui portion of the terminal

use anyhow::{anyhow, bail, Context, Error};
use lazy_static::lazy_static;
use mlua::Lua;
use ordered_float::NotNan;
use smol::channel::{Receiver, Sender};
use smol::prelude::*;
use std::cell::RefCell;
use std::collections::{BTreeMap, HashMap};
use std::ffi::OsString;
use std::fs::DirBuilder;
#[cfg(unix)]
use std::os::unix::fs::DirBuilderExt;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use wezterm_dynamic::{FromDynamic, FromDynamicOptions, ToDynamic, UnknownFieldAction, Value};
use wezterm_term::UnicodeVersion;

mod background;
mod bell;
mod cell;
mod color;
mod config;
mod daemon;
mod exec_domain;
mod font;
mod frontend;
pub mod keyassignment;
mod keys;
pub mod lua;
pub mod meta;
mod private_data;
pub use private_data::migrate_private_files;
pub mod scheme_data;
mod thinkterm_schemes;
mod serial;
mod ssh;
mod terminal;
mod tls;
mod units;
mod unix;
mod version;
mod web;
pub mod window;
mod wsl;

pub use crate::config::*;
pub use background::*;
pub use bell::*;
pub use cell::*;
pub use color::*;
pub use daemon::*;
pub use exec_domain::*;
pub use font::*;
pub use frontend::*;
pub use keys::*;
pub use serial::*;
pub use ssh::*;
pub use terminal::*;
pub use tls::*;
pub use units::*;
pub use unix::*;
pub use version::*;
pub use web::*;
pub use wsl::*;

type ErrorCallback = fn(&str);

lazy_static! {
    pub static ref HOME_DIR: PathBuf = dirs_next::home_dir().expect("can't find HOME dir");
    pub static ref CONFIG_DIRS: Vec<PathBuf> = config_dirs();
    pub static ref RUNTIME_DIR: PathBuf = compute_runtime_dir().unwrap();
    pub static ref DATA_DIR: PathBuf = compute_data_dir().unwrap();
    pub static ref CACHE_DIR: PathBuf = compute_cache_dir().unwrap();
    static ref CONFIG: Configuration = Configuration::new();
    static ref CONFIG_FILE_OVERRIDE: Mutex<Option<PathBuf>> = Mutex::new(None);
    static ref CONFIG_SKIP: AtomicBool = AtomicBool::new(false);
    static ref CONFIG_OVERRIDES: Mutex<Vec<(String, String)>> = Mutex::new(vec![]);
    static ref SHOW_ERROR: Mutex<Option<ErrorCallback>> =
        Mutex::new(Some(|e| log::error!("{}", e)));
    static ref LUA_PIPE: LuaPipe = LuaPipe::new();
    pub static ref COLOR_SCHEMES: HashMap<String, Palette> = build_default_schemes();
}

type ReloadFilter = Arc<dyn Fn(&[PathBuf]) -> bool + Send + Sync>;

static RELOAD_FILTER: Mutex<Option<ReloadFilter>> = Mutex::new(None);

/// Let the process decide which changes seen by the config watcher reload
/// the configuration. Without a filter every change in a watched directory
/// does. A process that keeps its own files next to the config file uses
/// this to tell its own bookkeeping writes apart; it must answer true for
/// anything it does not recognise.
pub fn set_reload_filter(filter: impl Fn(&[PathBuf]) -> bool + Send + Sync + 'static) {
    *RELOAD_FILTER.lock().unwrap_or_else(|err| err.into_inner()) = Some(Arc::new(filter));
}

fn reload_wanted(paths: &[PathBuf]) -> bool {
    // Run outside the lock: the GUI's filter reads and takes in its settings.
    let filter = RELOAD_FILTER.lock().unwrap_or_else(|err| err.into_inner()).clone();
    let Some(filter) = filter else {
        return true;
    };
    // A panic would end the watcher thread, and with it every reload after.
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| filter(paths))).unwrap_or_else(|_| {
        log::error!("the config reload filter panicked; reloading for {paths:?}");
        true
    })
}

/// Where the GUI keeps the settings its Settings window edits: next to the
/// config file, in the folder the config watcher watches.
pub fn native_settings_path() -> PathBuf {
    HOME_DIR
        .join(".config")
        .join("thinkterm")
        .join("settings.json")
}

/// Whether `path`, as the config watcher reported it, is the settings file
/// `settings` or the temporary file it is written through. The watcher
/// reports paths with symlinks resolved, so a path that is not literally
/// one of them is compared by its folder.
pub fn is_native_settings_file(path: &Path, settings: &Path) -> bool {
    let tmp = settings.with_extension("json.tmp");
    if path == settings || path == tmp {
        return true;
    }
    let named = path.file_name() == settings.file_name() || path.file_name() == tmp.file_name();
    named
        && match (path.parent(), settings.parent()) {
            (Some(a), Some(b)) => match (a.canonicalize(), b.canonicalize()) {
                (Ok(a), Ok(b)) => a == b,
                _ => false,
            },
            _ => false,
        }
}

/// For a process that never reads the GUI's settings.json. The GUI rewrites
/// it as sidebars and windows move, and a change to it alone does not
/// reload the configuration and the Lua state here.
pub fn ignore_native_settings_changes() {
    set_reload_filter(|paths| {
        let settings = native_settings_path();
        !paths
            .iter()
            .all(|path| is_native_settings_file(path, &settings))
    });
}

thread_local! {
    static LUA_CONFIG: RefCell<Option<LuaConfigState>> = RefCell::new(None);
}

/// Name a file under [`RUNTIME_DIR`], scoped to the build profile.
///
/// A debug build gets its own socket, pid file and log, so that a binary
/// built while developing can never adopt -- or be adopted by -- the
/// release mux server that serves the user's real work. Both ends of the
/// protocol derive their paths from here, so a debug client reaches the
/// debug server and nothing else.
///
/// The protocol's own guard is not enough on its own: `CODEC_VERSION`
/// advances only when the wire format changes, so two builds that differ
/// by a behaviour change alone still handshake happily. A development
/// server left running days ago therefore goes on serving live shells
/// while looking perfectly healthy to a freshly built client.
pub fn runtime_file_name(base: &str) -> String {
    scoped_runtime_file_name(base, cfg!(debug_assertions))
}

fn scoped_runtime_file_name(base: &str, debug_build: bool) -> String {
    if debug_build {
        format!("{base}-debug")
    } else {
        base.to_string()
    }
}

fn toml_table_has_numeric_keys(t: &toml::value::Table) -> bool {
    t.keys().all(|k| k.parse::<isize>().is_ok())
}

fn json_object_has_numeric_keys(t: &serde_json::Map<String, serde_json::Value>) -> bool {
    t.keys().all(|k| k.parse::<isize>().is_ok())
}

fn toml_to_dynamic(value: &toml::Value) -> Value {
    match value {
        toml::Value::String(s) => s.to_dynamic(),
        toml::Value::Integer(n) => n.to_dynamic(),
        toml::Value::Float(n) => n.to_dynamic(),
        toml::Value::Boolean(b) => b.to_dynamic(),
        toml::Value::Datetime(d) => d.to_string().to_dynamic(),
        toml::Value::Array(a) => a
            .iter()
            .map(toml_to_dynamic)
            .collect::<Vec<_>>()
            .to_dynamic(),
        // Allow `colors.indexed` to be passed through with actual integer keys
        toml::Value::Table(t) if toml_table_has_numeric_keys(t) => Value::Object(
            t.iter()
                .map(|(k, v)| (k.parse::<isize>().unwrap().to_dynamic(), toml_to_dynamic(v)))
                .collect::<BTreeMap<_, _>>()
                .into(),
        ),
        toml::Value::Table(t) => Value::Object(
            t.iter()
                .map(|(k, v)| (Value::String(k.to_string()), toml_to_dynamic(v)))
                .collect::<BTreeMap<_, _>>()
                .into(),
        ),
    }
}

pub fn json_to_dynamic(value: &serde_json::Value) -> Value {
    match value {
        serde_json::Value::Null => Value::Null,
        serde_json::Value::Bool(b) => b.to_dynamic(),
        serde_json::Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                i.to_dynamic()
            } else if let Some(i) = n.as_u64() {
                i.to_dynamic()
            } else if let Some(f) = n.as_f64() {
                f.to_dynamic()
            } else {
                Value::Null
            }
        }
        serde_json::Value::String(s) => s.to_dynamic(),
        serde_json::Value::Array(a) => a
            .iter()
            .map(json_to_dynamic)
            .collect::<Vec<_>>()
            .to_dynamic(),
        // Allow `colors.indexed` to be passed through with actual integer keys
        serde_json::Value::Object(t) if json_object_has_numeric_keys(t) => Value::Object(
            t.iter()
                .map(|(k, v)| (k.parse::<isize>().unwrap().to_dynamic(), json_to_dynamic(v)))
                .collect::<BTreeMap<_, _>>()
                .into(),
        ),
        serde_json::Value::Object(t) => Value::Object(
            t.iter()
                .map(|(k, v)| (Value::String(k.to_string()), json_to_dynamic(v)))
                .collect::<BTreeMap<_, _>>()
                .into(),
        ),
    }
}

/// The reverse of `json_to_dynamic`. JSON object keys are strings, so a
/// number used as a key is written as its digits.
pub fn dynamic_to_json(value: &Value) -> serde_json::Value {
    match value {
        Value::Null => serde_json::Value::Null,
        Value::Bool(b) => serde_json::Value::Bool(*b),
        Value::String(s) => serde_json::Value::String(s.clone()),
        Value::U64(n) => (*n).into(),
        Value::I64(n) => (*n).into(),
        Value::F64(n) => serde_json::Number::from_f64(n.into_inner())
            .map(serde_json::Value::Number)
            .unwrap_or(serde_json::Value::Null),
        Value::Array(a) => serde_json::Value::Array(a.iter().map(dynamic_to_json).collect()),
        Value::Object(o) => serde_json::Value::Object(
            o.iter()
                .map(|(k, v)| {
                    let key = match k {
                        Value::String(s) => s.clone(),
                        Value::U64(n) => n.to_string(),
                        Value::I64(n) => n.to_string(),
                        other => dynamic_to_json(other).to_string(),
                    };
                    (key, dynamic_to_json(v))
                })
                .collect(),
        ),
    }
}

pub fn build_default_schemes() -> HashMap<String, Palette> {
    let mut color_schemes = HashMap::new();
    for (scheme_name, data) in scheme_data::SCHEMES
        .iter()
        .chain(thinkterm_schemes::SCHEMES.iter())
    {
        let scheme_name = scheme_name.to_string();
        let scheme = ColorSchemeFile::from_toml_str(data).unwrap();
        color_schemes.insert(scheme_name, scheme.colors.clone());
        for alias in scheme.metadata.aliases {
            color_schemes.insert(alias, scheme.colors.clone());
        }
    }
    color_schemes
}

struct LuaPipe {
    sender: Sender<mlua::Lua>,
    receiver: Receiver<mlua::Lua>,
}
impl LuaPipe {
    pub fn new() -> Self {
        let (sender, receiver) = smol::channel::unbounded();
        Self { sender, receiver }
    }
}

/// The implementation is only slightly crazy...
/// `Lua` is Send but !Sync.
/// We take care to reference this only from the main thread of
/// the application.
/// We also need to take care to keep this `lua` alive if a long running
/// future is outstanding while a config reload happens.
/// We have to use `Rc` to manage its lifetime, but due to some issues
/// with rust's async lifetime tracking we need to indirectly schedule
/// some of the futures to avoid it thinking that the generated future
/// in the async block needs to be Send.
///
/// A further complication is that config reloading tends to happen in
/// a background filesystem watching thread.
///
/// The result of all these constraints is that the LuaPipe struct above
/// is used as a channel to transport newly loaded lua configs to the
/// main thread.
///
/// The main thread pops the loaded configs to obtain the latest one
/// and updates LuaConfigState
struct LuaConfigState {
    lua: Option<Rc<mlua::Lua>>,
}

impl LuaConfigState {
    /// Consume any lua contexts sent to us via the
    /// config loader until we end up with the most
    /// recent one being referenced by LUA_CONFIG.
    fn update_to_latest(&mut self) {
        while let Ok(lua) = LUA_PIPE.receiver.try_recv() {
            self.lua.replace(Rc::new(lua));
        }
    }

    /// Take a reference on the latest generation of the lua context
    fn get_lua(&self) -> Option<Rc<mlua::Lua>> {
        self.lua.as_ref().map(Rc::clone)
    }
}

pub fn designate_this_as_the_main_thread() {
    LUA_CONFIG.with(|lc| {
        let mut lc = lc.borrow_mut();
        if lc.is_none() {
            lc.replace(LuaConfigState { lua: None });
        }
    });
}

#[must_use = "Cancels the subscription when dropped"]
pub struct ConfigSubscription(usize);

impl Drop for ConfigSubscription {
    fn drop(&mut self) {
        CONFIG.unsub(self.0);
    }
}

pub fn subscribe_to_config_reload<F>(subscriber: F) -> ConfigSubscription
where
    F: Fn() -> bool + 'static + Send,
{
    ConfigSubscription(CONFIG.subscribe(subscriber))
}

/// Spawn a future that will run with an optional Lua state from the most
/// recently loaded lua configuration.
/// The `func` argument is passed the lua state and must return a Future.
///
/// This function MUST only be called from the main thread.
/// In exchange for the caller checking for this, the parameters to
/// this method are not required to be Send.
///
/// Calling this function from a secondary thread will panic.
/// You should use `with_lua_config` if you are triggering a
/// call from a secondary thread.
pub async fn with_lua_config_on_main_thread<F, RETF, RET>(func: F) -> anyhow::Result<RET>
where
    F: FnOnce(Option<Rc<mlua::Lua>>) -> RETF,
    RETF: Future<Output = anyhow::Result<RET>>,
{
    let lua = LUA_CONFIG.with(|lc| {
        let mut lc = lc.borrow_mut();
        let lc = lc.as_mut().expect(
            "with_lua_config_on_main_thread not called
             from main thread, use with_lua_config instead!",
        );
        lc.update_to_latest();
        lc.get_lua()
    });

    func(lua).await
}

pub fn run_immediate_with_lua_config<F, RET>(func: F) -> anyhow::Result<RET>
where
    F: FnOnce(Option<Rc<mlua::Lua>>) -> anyhow::Result<RET>,
{
    let lua = LUA_CONFIG.with(|lc| {
        let mut lc = lc.borrow_mut();
        let lc = lc.as_mut().expect(
            "with_lua_config_on_main_thread not called
             from main thread, use with_lua_config instead!",
        );
        lc.update_to_latest();
        lc.get_lua()
    });

    func(lua)
}

fn schedule_with_lua<F, RETF, RET>(func: F) -> promise::spawn::Task<anyhow::Result<RET>>
where
    F: 'static,
    RET: 'static,
    F: Fn(Option<Rc<mlua::Lua>>) -> RETF,
    RETF: Future<Output = anyhow::Result<RET>>,
{
    promise::spawn::spawn(async move { with_lua_config_on_main_thread(func).await })
}

/// Spawn a future that will run with an optional Lua state from the most
/// recently loaded lua configuration.
/// The `func` argument is passed the lua state and must return a Future.
pub async fn with_lua_config<F, RETF, RET>(func: F) -> anyhow::Result<RET>
where
    F: Fn(Option<Rc<mlua::Lua>>) -> RETF,
    RETF: Future<Output = anyhow::Result<RET>> + Send + 'static,
    F: Send + 'static,
    RET: Send + 'static,
{
    promise::spawn::spawn_into_main_thread(async move { schedule_with_lua(func).await }).await
}

fn default_config_with_overrides_applied(
    overrides: &wezterm_dynamic::Value,
) -> anyhow::Result<Config> {
    // Cause the default config to be re-evaluated with the overrides applied
    let lua = lua::make_lua_context(Path::new("override")).context("make_lua_context")?;
    let table = mlua::Value::Table(lua.create_table()?);
    let config = Config::apply_overrides_to(&lua, table).context("apply_overrides_to")?;
    // Then a window's own, as `try_load` applies them over a file. Settings'
    // colour scheme and opacity are window overrides, and must apply with
    // no config file too.
    let config = Config::apply_overrides_obj_to(&lua, config, overrides)
        .context("apply_overrides_obj_to")?;

    let dyn_config = luahelper::lua_value_to_dynamic(config)?;

    let cfg: Config = Config::from_dynamic(
        &dyn_config,
        FromDynamicOptions {
            unknown_fields: UnknownFieldAction::Deny,
            deprecated_fields: UnknownFieldAction::Warn,
        },
    )
    .context("Error converting lua value from overrides to Config struct")?;
    // Compute but discard the key bindings here so that we raise any
    // problems earlier than we use them.
    let _ = cfg.key_bindings();

    cfg.check_consistency().context("check_consistency")?;

    Ok(cfg)
}

pub fn common_init(
    config_file: Option<&OsString>,
    overrides: &[(String, String)],
    skip_config: bool,
) -> anyhow::Result<()> {
    if let Some(config_file) = config_file {
        set_config_file_override(Path::new(config_file));
    } else if skip_config {
        CONFIG_SKIP.store(true, Ordering::Relaxed);
    }

    set_config_overrides(overrides).context("common_init: set_config_overrides")?;
    reload();
    Ok(())
}

pub fn assign_error_callback(cb: ErrorCallback) {
    let mut factory = SHOW_ERROR.lock().unwrap();
    factory.replace(cb);
}

pub fn show_error(err: &str) {
    let factory = SHOW_ERROR.lock().unwrap();
    if let Some(cb) = factory.as_ref() {
        cb(err)
    }
}

pub fn create_user_owned_dirs(p: &Path) -> anyhow::Result<()> {
    let mut builder = DirBuilder::new();
    builder.recursive(true);

    #[cfg(unix)]
    {
        builder.mode(0o700);
    }

    builder.create(p)?;
    Ok(())
}

fn xdg_config_home(product_dir: &str) -> PathBuf {
    match std::env::var_os("XDG_CONFIG_HOME").map(|s| PathBuf::from(s).join(product_dir)) {
        Some(p) => p,
        None => HOME_DIR.join(".config").join(product_dir),
    }
}

fn config_dirs() -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    dirs.push(xdg_config_home("thinkterm"));
    dirs.push(xdg_config_home("wezterm"));

    #[cfg(unix)]
    if let Some(d) = std::env::var_os("XDG_CONFIG_DIRS") {
        for base in std::env::split_paths(&d) {
            dirs.push(base.join("thinkterm"));
            dirs.push(base.join("wezterm"));
        }
    }

    dirs
}

pub fn set_config_file_override(path: &Path) {
    CONFIG_FILE_OVERRIDE
        .lock()
        .unwrap()
        .replace(path.to_path_buf());
}

pub fn set_config_overrides(items: &[(String, String)]) -> anyhow::Result<()> {
    *CONFIG_OVERRIDES.lock().unwrap() = items.to_vec();

    let _ = default_config_with_overrides_applied(&wezterm_dynamic::Value::default())?;
    Ok(())
}

pub fn is_config_overridden() -> bool {
    CONFIG_SKIP.load(Ordering::Relaxed)
        || !CONFIG_OVERRIDES.lock().unwrap().is_empty()
        || CONFIG_FILE_OVERRIDE.lock().unwrap().is_some()
}

/// Discard the current configuration and replace it with
/// the default configuration
pub fn use_default_configuration() {
    CONFIG.use_defaults();
}

/// Use a config that doesn't depend on the user's
/// environment and is suitable for unit testing
pub fn use_test_configuration() {
    CONFIG.use_test();
}

pub fn use_this_configuration(config: Config) {
    CONFIG.use_this_config(config);
}

/// Returns a handle to the current configuration
pub fn configuration() -> ConfigHandle {
    CONFIG.get()
}

/// Returns a version of the config (loaded from the config file)
/// with some field overridden based on the supplied overrides object.
pub fn overridden_config(overrides: &wezterm_dynamic::Value) -> Result<ConfigHandle, Error> {
    CONFIG.overridden(overrides)
}

pub fn reload() {
    CONFIG.reload();
}

/// If there was an error loading the preferred configuration,
/// return it, otherwise return the current configuration
pub fn configuration_result() -> Result<ConfigHandle, Error> {
    if let Some(error) = CONFIG.get_error() {
        bail!("{}", error);
    }
    Ok(CONFIG.get())
}

/// Returns the combined set of errors + warnings encountered
/// while loading the preferred configuration
pub fn configuration_warnings_and_errors() -> Vec<String> {
    CONFIG.get_warnings_and_errors()
}

pub fn configuration_file() -> Option<PathBuf> {
    CONFIG.config_file()
}

pub fn load_config_file_for_import(path: &Path) -> anyhow::Result<ConfigImportResult> {
    Config::load_file_for_import(path)
}

struct ConfigInner {
    config: Arc<Config>,
    config_file: Option<PathBuf>,
    error: Option<String>,
    warnings: Vec<String>,
    generation: usize,
    watcher: Option<notify::RecommendedWatcher>,
    subscribers: HashMap<usize, Box<dyn Fn() -> bool + Send>>,
}

impl ConfigInner {
    fn new() -> Self {
        Self {
            config: Arc::new(Config::default_config()),
            config_file: None,
            error: None,
            warnings: vec![],
            generation: 0,
            watcher: None,
            subscribers: HashMap::new(),
        }
    }

    fn subscribe<F>(&mut self, subscriber: F) -> usize
    where
        F: Fn() -> bool + 'static + Send,
    {
        static SUB_ID: AtomicUsize = AtomicUsize::new(0);
        let sub_id = SUB_ID.fetch_add(1, Ordering::Relaxed);
        self.subscribers.insert(sub_id, Box::new(subscriber));
        sub_id
    }

    fn unsub(&mut self, sub_id: usize) {
        self.subscribers.remove(&sub_id);
    }

    fn notify(&mut self) {
        self.subscribers.retain(|_, notify| notify());
    }

    fn watch_path(&mut self, path: PathBuf) {
        if self.watcher.is_none() {
            let (tx, rx) = std::sync::mpsc::channel();
            const DELAY: Duration = Duration::from_millis(200);
            let watcher = notify::recommended_watcher(tx).unwrap();

            std::thread::spawn(move || {
                // block until we get an event
                use notify::EventKind;

                fn extract_path(event: notify::Event) -> Vec<PathBuf> {
                    match event.kind {
                        EventKind::Modify(_) | EventKind::Create(_) | EventKind::Remove(_) => {
                            event.paths
                        }
                        _ => vec![],
                    }
                }

                while let Ok(event) = rx.recv() {
                    log::debug!("event:{:?}", event);
                    match event {
                        Ok(event) => {
                            let mut paths = extract_path(event);
                            if !paths.is_empty() {
                                // Grace period to allow events to settle
                                std::thread::sleep(DELAY);
                                // Drain any other immediately ready events
                                while let Ok(Ok(event)) = rx.try_recv() {
                                    paths.append(&mut extract_path(event));
                                }
                                paths.sort();
                                paths.dedup();
                                if reload_wanted(&paths) {
                                    log::debug!("paths {:?} changed, reload config", paths);
                                    reload();
                                } else {
                                    log::debug!("paths {:?} changed, no reload wanted", paths);
                                }
                            }
                        }
                        Err(_) => {
                            reload();
                        }
                    }
                }
            });
            self.watcher.replace(watcher);
        }
        if let Some(watcher) = self.watcher.as_mut() {
            use notify::Watcher;
            watcher
                .watch(&path, notify::RecursiveMode::NonRecursive)
                .ok();
        }
    }

    fn accumulate_watch_paths(lua: &Lua, watch_paths: &mut Vec<PathBuf>) {
        if let Ok(mlua::Value::Table(tbl)) = lua.named_registry_value("wezterm-watch-paths") {
            for path in tbl.sequence_values::<String>() {
                if let Ok(path) = path {
                    watch_paths.push(PathBuf::from(path));
                }
            }
        }
    }

    /// Attempt to load the user's configuration.
    /// On success, clear any error and replace the current
    /// configuration.
    /// On failure, retain the existing configuration but
    /// replace any captured error message.
    fn reload(&mut self) {
        let LoadedConfig {
            config,
            file_name,
            lua,
            warnings,
        } = Config::load();

        self.warnings = warnings;

        // Before we process the success/failure, extract and update
        // any paths that we should be watching
        let mut watch_paths = vec![];
        let loaded_file_name = file_name.clone();
        if let Some(path) = file_name {
            // Let's also watch the parent directory for folks that do
            // things with symlinks:
            if let Some(parent) = path.parent() {
                // But avoid watching the home dir itself, so that we
                // don't keep reloading every time something in the
                // home dir changes!
                // <https://github.com/wezterm/wezterm/issues/1895>
                if parent != &*HOME_DIR {
                    watch_paths.push(parent.to_path_buf());
                }
            }
            watch_paths.push(path);
        }
        if let Some(lua) = &lua {
            ConfigInner::accumulate_watch_paths(lua, &mut watch_paths);
        }

        match config {
            Ok(config) => {
                self.config = Arc::new(config);
                self.config_file = loaded_file_name;
                self.error.take();
                self.bump_generation();

                // If we loaded a user config, publish this latest version of
                // the lua state to the LUA_PIPE.  This allows a subsequent
                // call to `with_lua_config` to reference this lua context
                // even though we are (probably) resolving this from a background
                // reloading thread.
                if let Some(lua) = lua {
                    LUA_PIPE.sender.try_send(lua).ok();
                }
                log::debug!("Reloaded configuration! generation={}", self.generation);
            }
            Err(err) => {
                let err = format!("{:#}", err);
                if self.generation > 0 {
                    // Only generate the message for an actual reload
                    show_error(&err);
                }
                self.error.replace(err);
            }
        }

        self.notify();
        if self.config.automatically_reload_config {
            for path in watch_paths {
                self.watch_path(path);
            }
        }
    }

    /// Count a change of the effective configuration and publish the new
    /// generation for [`configuration_generation`] before the lock that
    /// installed it is released.
    fn bump_generation(&mut self) {
        self.generation += 1;
        CONFIG_GENERATION.store(self.generation, Ordering::Release);
    }

    /// Discard the current configuration and any recorded
    /// error message; replace them with the default
    /// configuration
    fn use_defaults(&mut self) {
        self.config = Arc::new(Config::default_config());
        self.config_file = None;
        self.error.take();
        self.bump_generation();
    }

    fn use_this_config(&mut self, cfg: Config) {
        self.config = Arc::new(cfg);
        self.config_file = None;
        self.error.take();
        self.bump_generation();
    }

    fn overridden(&mut self, overrides: &wezterm_dynamic::Value) -> Result<ConfigHandle, Error> {
        let config = Config::load_with_overrides(overrides);
        Ok(ConfigHandle {
            config: Arc::new(config.config?),
            generation: self.generation,
        })
    }

    fn use_test(&mut self) {
        let mut config = Config::default_config();
        config.font_locator = FontLocatorSelection::ConfigDirsOnly;
        let exe_name = std::env::current_exe().unwrap();
        let exe_dir = exe_name.parent().unwrap();
        config.font_dirs.push(exe_dir.join("../../../assets/fonts"));
        // If we're building for a specific target, the dir
        // level is one deeper.
        #[cfg(target_os = "macos")]
        config
            .font_dirs
            .push(exe_dir.join("../../../../assets/fonts"));
        // Specify the same DPI used on non-mac systems so
        // that we have consistent values regardless of the
        // operating system that we're running tests on
        config.dpi.replace(96.0);
        self.config = Arc::new(config);
        self.error.take();
        self.bump_generation();
    }
}

/// The global configuration's generation, readable without its lock.
static CONFIG_GENERATION: AtomicUsize = AtomicUsize::new(0);

/// The generation of the global configuration, read without taking its
/// lock: every change to what [`configuration`] returns publishes a new
/// value here before that change's lock is released.
pub fn configuration_generation() -> usize {
    CONFIG_GENERATION.load(Ordering::Acquire)
}

pub struct Configuration {
    inner: Mutex<ConfigInner>,
}

impl Configuration {
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(ConfigInner::new()),
        }
    }

    /// Returns the effective configuration.
    pub fn get(&self) -> ConfigHandle {
        let inner = self.inner.lock().unwrap();
        ConfigHandle {
            config: Arc::clone(&inner.config),
            generation: inner.generation,
        }
    }

    /// Subscribe to config reload events
    fn subscribe<F>(&self, subscriber: F) -> usize
    where
        F: Fn() -> bool + 'static + Send,
    {
        let mut inner = self.inner.lock().unwrap();
        inner.subscribe(subscriber)
    }

    fn unsub(&self, sub_id: usize) {
        let mut inner = self.inner.lock().unwrap();
        inner.unsub(sub_id);
    }

    /// Reset the configuration to defaults
    pub fn use_defaults(&self) {
        let mut inner = self.inner.lock().unwrap();
        inner.use_defaults();
    }

    fn use_this_config(&self, cfg: Config) {
        let mut inner = self.inner.lock().unwrap();
        inner.use_this_config(cfg);
    }

    fn overridden(&self, overrides: &wezterm_dynamic::Value) -> Result<ConfigHandle, Error> {
        let mut inner = self.inner.lock().unwrap();
        inner.overridden(overrides)
    }

    /// Use a config that doesn't depend on the user's
    /// environment and is suitable for unit testing
    pub fn use_test(&self) {
        let mut inner = self.inner.lock().unwrap();
        inner.use_test();
    }

    /// Reload the configuration
    pub fn reload(&self) {
        let mut inner = self.inner.lock().unwrap();
        inner.reload();
    }

    /// Returns a copy of any captured error message.
    /// The error message is not cleared.
    pub fn get_error(&self) -> Option<String> {
        let inner = self.inner.lock().unwrap();
        inner.error.as_ref().cloned()
    }

    pub fn config_file(&self) -> Option<PathBuf> {
        let inner = self.inner.lock().unwrap();
        inner.config_file.clone()
    }

    pub fn get_warnings_and_errors(&self) -> Vec<String> {
        let mut result = vec![];
        let inner = self.inner.lock().unwrap();
        if let Some(error) = &inner.error {
            result.push(error.clone());
        }
        for warning in &inner.warnings {
            result.push(warning.clone());
        }
        result
    }

    /// Returns any captured error message, and clears
    /// it from the config state.
    #[allow(dead_code)]
    pub fn clear_error(&self) -> Option<String> {
        let mut inner = self.inner.lock().unwrap();
        inner.error.take()
    }
}

#[derive(Clone, Debug)]
pub struct ConfigHandle {
    config: Arc<Config>,
    generation: usize,
}

impl ConfigHandle {
    /// Returns the generation number for the configuration,
    /// allowing consuming code to know whether the config
    /// has been reloading since they last derived some
    /// information from the configuration
    pub fn generation(&self) -> usize {
        self.generation
    }

    pub fn default_config() -> Self {
        Self {
            config: Arc::new(Config::default_config()),
            generation: 0,
        }
    }

    /// This configuration with `change` made to a copy of it, under the
    /// same generation: for what a window derives from its configuration
    /// without reloading it.
    pub fn adjusted(&self, change: impl FnOnce(&mut Config)) -> Self {
        let mut config = (*self.config).clone();
        change(&mut config);
        Self {
            config: Arc::new(config),
            generation: self.generation,
        }
    }

    pub fn unicode_version(&self) -> UnicodeVersion {
        UnicodeVersion {
            version: self.config.unicode_version,
            ambiguous_are_wide: self.config.treat_east_asian_ambiguous_width_as_wide,
            cell_widths: CellWidth::compile_to_map(self.config.cell_widths.clone()),
        }
    }
}

impl std::ops::Deref for ConfigHandle {
    type Target = Config;
    fn deref(&self) -> &Config {
        &*self.config
    }
}

pub struct LoadedConfig {
    pub config: anyhow::Result<Config>,
    pub file_name: Option<PathBuf>,
    pub lua: Option<mlua::Lua>,
    pub warnings: Vec<String>,
}

fn default_one_point_oh_f64() -> f64 {
    1.0
}

fn default_one_point_oh() -> f32 {
    1.0
}

fn default_true() -> bool {
    true
}

#[cfg(test)]
mod config_handle_tests {
    use super::*;

    #[test]
    fn an_adjusted_handle_changes_only_its_own_copy() {
        let handle = ConfigHandle::default_config();
        let adjusted = handle.adjusted(|config| config.window_background_opacity = 0.5);
        assert_eq!(adjusted.window_background_opacity, 0.5);
        assert_eq!(handle.window_background_opacity, 1.0);
        // Nothing keyed on the generation is told to rebuild.
        assert_eq!(adjusted.generation(), handle.generation());
    }
}

#[cfg(test)]
mod runtime_file_name_tests {
    use super::scoped_runtime_file_name;

    #[test]
    fn a_release_build_keeps_the_documented_runtime_file_names() {
        assert_eq!(scoped_runtime_file_name("sock", false), "sock");
        assert_eq!(scoped_runtime_file_name("pid", false), "pid");
        assert_eq!(scoped_runtime_file_name("log", false), "log");
    }

    #[test]
    fn a_debug_build_never_shares_a_runtime_file_with_the_release_one() {
        for base in ["sock", "pid", "log"] {
            let debug = scoped_runtime_file_name(base, true);
            assert_ne!(debug, scoped_runtime_file_name(base, false));
            assert_eq!(debug, format!("{base}-debug"));
        }
    }
}

#[cfg(test)]
mod reload_filter_tests {
    use super::{is_native_settings_file, reload_wanted, set_reload_filter, RELOAD_FILTER};
    use std::path::PathBuf;

    #[test]
    fn a_reload_filter_decides_which_watched_changes_reload() {
        let lua = PathBuf::from("/devbox/.config/thinkterm/thinkterm.lua");
        let settings = PathBuf::from("/devbox/.config/thinkterm/settings.json");
        assert!(reload_wanted(&[settings.clone()]), "without a filter everything reloads");

        set_reload_filter(|paths| !paths.iter().all(|path| path.ends_with("settings.json")));
        assert!(!reload_wanted(&[settings.clone()]));
        assert!(reload_wanted(&[settings.clone(), lua]));

        set_reload_filter(|_| panic!("a broken filter"));
        assert!(reload_wanted(&[settings]), "a filter that panics reloads");

        *RELOAD_FILTER.lock().unwrap() = None;
    }

    #[cfg(unix)]
    #[test]
    fn the_settings_file_is_recognised_through_a_symlinked_folder() {
        let dir = tempfile::tempdir().unwrap();
        let real = dir.path().join("real");
        std::fs::create_dir(&real).unwrap();
        let link = dir.path().join("link");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        let settings = link.join("settings.json");

        assert!(is_native_settings_file(&settings, &settings));
        assert!(is_native_settings_file(&real.join("settings.json"), &settings));
        assert!(is_native_settings_file(&real.join("settings.json.tmp"), &settings));
        assert!(!is_native_settings_file(&real.join("thinkterm.lua"), &settings));
        assert!(
            !is_native_settings_file(&dir.path().join("settings.json"), &settings),
            "a settings.json in another folder"
        );
    }
}

