//! The plugins the host knows: the ones built into it, and the ones
//! installed in the plugins directory, each with its switch and, while an
//! installed one runs, its program.
//!
//! The directory is looked at again whenever the list is asked for, and a
//! plugin's manifest and program whenever it is used, so installing,
//! removing, editing and rebuilding a plugin need no restart. Nothing
//! starts a plugin's program but a client using the plugin, and no program
//! starts while one that ran under the same id -- from whichever directory
//! -- is still on its way out: they share a data directory, and calls made
//! meanwhile wait for it to be gone.

use crate::manifest::{self, Manifest};
use crate::process::{self, Exiting, Listener, Process, Refused, Start, Waiter};
use crate::stamp::{stamp, Stamp};
use crate::switches::{self, Switches};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};
use thinkterm_plugin_channel::registry::{Info, State};
use thinkterm_plugin_sdk::protocol::{ToPlugin, API};
use thinkterm_plugin_sdk::Plugin;

/// Crashes within [`CRASH_WINDOW`] that mark a plugin failed.
const CRASH_LIMIT: usize = 3;
const CRASH_WINDOW: Duration = Duration::from_secs(60);
/// Directories looked at in the plugins directory; one with more than this
/// is not a plugins directory someone keeps by hand.
const DIR_LIMIT: usize = 256;
/// How long the programs still running when the host exits have to exit.
const EXIT_GRACE: Duration = Duration::from_secs(1);

/// A plugin built into the host, which runs inside it.
pub struct Builtin {
    pub manifest: Manifest,
    pub plugin: Box<dyn Plugin>,
}

/// A call or a command for a program, made from the id it is sent with.
pub type Message = Box<dyn FnOnce(u64) -> ToPlugin + Send>;

/// A call waiting for the plugin's last program to be gone.
struct Queued {
    waiter: Waiter,
    message: Message,
}

/// What starting an installed plugin's program takes, once it may start.
struct Launch {
    id: String,
    program: Result<(PathBuf, Vec<String>), String>,
    program_stamp: Option<Stamp>,
}

struct Installed {
    dir: PathBuf,
    /// What a plugin whose manifest does not read is known by.
    dir_name: String,
    manifest: Result<Manifest, String>,
    manifest_stamp: Option<Stamp>,
    /// Why it is not used although its manifest reads: another plugin has
    /// its id.
    shadowed: Option<String>,
    run: Run,
    /// When it crashed within the last [`CRASH_WINDOW`], oldest first.
    crashes: Vec<Instant>,
    /// Its programs told to stop and not gone yet. While any that ran under
    /// an id is, no program of that id starts, here or in another
    /// directory.
    exiting: Vec<Exiting>,
    /// The calls waiting for such a program to be gone.
    queued: Vec<Queued>,
}

enum Run {
    Idle,
    Running(Process),
    Crashed(String),
    /// It stays so until it is reloaded, turned off and on, or its manifest
    /// or program changes from how they were when it failed.
    Failed {
        reason: String,
        manifest: Option<Stamp>,
        program: Option<Stamp>,
    },
}

impl Installed {
    fn new(dir_name: String, dir: PathBuf) -> Self {
        let mut installed = Self {
            dir,
            dir_name,
            manifest: Err(String::new()),
            manifest_stamp: None,
            shadowed: None,
            run: Run::Idle,
            crashes: Vec::new(),
            exiting: Vec::new(),
            queued: Vec::new(),
        };
        installed.read_manifest();
        installed
    }

    fn read_manifest(&mut self) {
        self.manifest_stamp = stamp(&self.dir.join(manifest::FILE));
        self.manifest = Manifest::load(&self.dir);
        if let Err(reason) = &self.manifest {
            log::warn!("plugin in {}: {reason}", self.dir.display());
        }
    }

    /// Its id, when its manifest reads.
    fn id(&self) -> Option<&str> {
        self.manifest
            .as_ref()
            .ok()
            .map(|manifest| manifest.id.as_str())
    }

    /// Whether calls addressed to `id` are this plugin's.
    fn answers_to(&self, id: &str) -> bool {
        self.shadowed.is_none() && self.id() == Some(id)
    }

    /// Stops its program, if it runs, handing its unanswered calls to
    /// `fallout` with `why`. The program is exiting until it is gone.
    fn stop(&mut self, why: &str, fallout: &mut Fallout) {
        if let Run::Running(process) = std::mem::replace(&mut self.run, Run::Idle) {
            log::info!("stopping plugin {}: {why}", self.dir_name);
            let (waiters, exiting) = process.stop();
            fallout.lost(waiters, why);
            self.exiting.push(exiting);
        }
    }

    /// Stops it, answers the calls waiting to reach it, and forgets that it
    /// crashed or failed.
    fn reset(&mut self, why: &str, fallout: &mut Fallout) {
        self.stop(why, fallout);
        let queued = std::mem::take(&mut self.queued);
        fallout.lost(queued.into_iter().map(|call| call.waiter).collect(), why);
        self.run = Run::Idle;
        self.crashes.clear();
    }
}

/// What an operation leaves for the host to tell the clients.
#[derive(Default)]
pub struct Fallout {
    /// Calls that will not be answered, and why.
    pub unanswered: Vec<(Waiter, String)>,
    /// The list changed: whoever shows it is to be told.
    pub changed: bool,
}

impl Fallout {
    fn lost(&mut self, waiters: Vec<Waiter>, why: &str) {
        self.unanswered
            .extend(waiters.into_iter().map(|waiter| (waiter, why.to_string())));
        self.changed = true;
    }
}

/// Which plugin a call is for.
pub enum Lookup {
    Builtin(usize),
    Installed(usize),
    Missing,
}

pub struct Registry {
    plugins_dir: PathBuf,
    /// Where each installed plugin gets a directory of its own.
    plugin_data: PathBuf,
    switches: Switches,
    builtins: Vec<Builtin>,
    installed: Vec<Installed>,
    /// Counts program starts, so what a program's threads report is never
    /// taken for another run's.
    generation: u64,
    ready_within: Duration,
}

impl Registry {
    pub fn new(data_dir: &Path, builtins: Vec<Builtin>, ready_within: Duration) -> Self {
        let mut registry = Self {
            plugins_dir: thinkterm_plugin_channel::paths::plugins_dir_in(data_dir),
            plugin_data: data_dir.join("plugin-data"),
            switches: Switches::load(data_dir.join(switches::FILE)),
            builtins,
            installed: Vec::new(),
            generation: 0,
            ready_within,
        };
        // Nobody is connected yet to be told what it found.
        registry.scan(&mut Fallout::default());
        registry
    }

    /// Looks at the plugins directory and the switches again: plugins added
    /// or removed, manifests changed, switches moved by another host.
    pub fn scan(&mut self, fallout: &mut Fallout) {
        self.follow_switches(fallout);
        let found = self.read_dir();
        let before = self.installed.len();
        let mut removed = false;
        self.installed.retain_mut(|installed| {
            let kept = found.iter().any(|(name, _)| *name == installed.dir_name);
            if !kept {
                installed.reset("it was removed", fallout);
                log::info!("plugin {} was removed", installed.dir_name);
                removed = true;
            }
            kept
        });
        for (name, dir) in found {
            match self
                .installed
                .iter()
                .position(|installed| installed.dir_name == name)
            {
                Some(index) => self.refresh_manifest(index, fallout),
                None => {
                    log::info!("found plugin {}", dir.display());
                    self.installed.push(Installed::new(name, dir));
                }
            }
        }
        if removed || self.installed.len() != before {
            fallout.changed = true;
        }
        self.installed.sort_by(|a, b| a.dir_name.cmp(&b.dir_name));
        self.resolve_ids(fallout);
    }

    /// The plugin directories, by name: not hidden, at most [`DIR_LIMIT`].
    /// Takes in the switches another host moved -- a debug build's, say:
    /// a plugin turned off there stops here too.
    fn follow_switches(&mut self, fallout: &mut Fallout) {
        if !self.switches.refresh() {
            return;
        }
        fallout.changed = true;
        for installed in &mut self.installed {
            let off = installed.id().is_some_and(|id| !self.switches.enabled(id));
            if off {
                installed.reset("it was turned off", fallout);
            }
        }
    }

    fn read_dir(&self) -> Vec<(String, PathBuf)> {
        let Ok(entries) = std::fs::read_dir(&self.plugins_dir) else {
            return Vec::new();
        };
        let mut found: Vec<(String, PathBuf)> = entries
            .filter_map(Result::ok)
            .filter_map(|entry| {
                let name = entry.file_name().into_string().ok()?;
                let dir = entry.path();
                // A link to a directory counts: that is how one works on a
                // plugin in place.
                (!name.starts_with('.') && dir.is_dir()).then_some((name, dir))
            })
            .take(DIR_LIMIT)
            .collect();
        found.sort();
        found
    }

    /// Reads a plugin's manifest again if it changed on disk; a plugin
    /// whose manifest changed is stopped, and forgets that it failed.
    fn refresh_manifest(&mut self, index: usize, fallout: &mut Fallout) {
        let installed = &mut self.installed[index];
        if stamp(&installed.dir.join(manifest::FILE)) == installed.manifest_stamp {
            return;
        }
        installed.reset("its manifest changed", fallout);
        installed.read_manifest();
        fallout.changed = true;
        self.resolve_ids(fallout);
    }

    /// Settles which plugin answers to an id two of them claim: a built-in
    /// plugin, else the one whose directory sorts first. The others are
    /// stopped and shown as invalid.
    fn resolve_ids(&mut self, fallout: &mut Fallout) {
        let builtin_ids: Vec<String> = self
            .builtins
            .iter()
            .map(|builtin| builtin.manifest.id.clone())
            .collect();
        let mut claimed: Vec<(String, String)> = Vec::new();
        for installed in &mut self.installed {
            let Some(id) = installed.id().map(str::to_string) else {
                installed.shadowed = None;
                continue;
            };
            let shadowed = if builtin_ids.contains(&id) {
                Some(format!("the id {id:?} belongs to a built-in plugin"))
            } else if let Some((_, first)) = claimed.iter().find(|(known, _)| *known == id) {
                Some(format!("{first} has the same id, {id:?}"))
            } else {
                claimed.push((id, installed.dir_name.clone()));
                None
            };
            if shadowed.is_some() && installed.shadowed.is_none() {
                installed.reset("another plugin has its id", fallout);
                fallout.changed = true;
            }
            installed.shadowed = shadowed;
        }
    }

    /// Every plugin, as a list shows them: the built-in ones first, then
    /// the installed ones by name.
    pub fn list(&self, locale: &str) -> Vec<Info> {
        let mut list: Vec<Info> = self
            .builtins
            .iter()
            .map(|builtin| self.builtin_info(builtin, locale))
            .collect();
        let mut installed: Vec<Info> = self
            .installed
            .iter()
            .map(|installed| self.installed_info(installed, locale))
            .collect();
        installed.sort_by(|a, b| {
            a.name
                .to_lowercase()
                .cmp(&b.name.to_lowercase())
                .then_with(|| a.id.cmp(&b.id))
        });
        list.extend(installed);
        list
    }

    fn builtin_info(&self, builtin: &Builtin, locale: &str) -> Info {
        let manifest = &builtin.manifest;
        let enabled = self.switches.enabled(&manifest.id);
        let localized = manifest.localized(locale);
        Info {
            id: manifest.id.clone(),
            name: localized.name.to_string(),
            description: localized.description.to_string(),
            version: String::new(),
            builtin: true,
            dir: None,
            enabled,
            state: if enabled { State::Idle } else { State::Off },
        }
    }

    fn installed_info(&self, installed: &Installed, locale: &str) -> Info {
        let dir = Some(installed.dir.display().to_string());
        let manifest = match &installed.manifest {
            Ok(manifest) => manifest,
            Err(reason) => {
                return Info {
                    id: installed.dir_name.clone(),
                    name: installed.dir_name.clone(),
                    description: String::new(),
                    version: String::new(),
                    builtin: false,
                    dir,
                    enabled: self.switches.enabled(&installed.dir_name),
                    state: State::Invalid {
                        reason: reason.clone(),
                    },
                }
            }
        };
        let enabled = self.switches.enabled(&manifest.id);
        let state = if let Some(reason) = &installed.shadowed {
            State::Invalid {
                reason: reason.clone(),
            }
        } else if !manifest.supported() {
            State::Unsupported {
                reason: format!(
                    "it runs on {}, not {}",
                    manifest.platforms.join(", "),
                    std::env::consts::OS
                ),
            }
        } else if !enabled {
            State::Off
        } else {
            match &installed.run {
                Run::Idle if !installed.queued.is_empty() => State::Starting,
                Run::Idle => State::Idle,
                Run::Running(process) if process.ready => State::Running,
                Run::Running(_) => State::Starting,
                Run::Crashed(reason) => State::Crashed {
                    reason: reason.clone(),
                },
                Run::Failed { reason, .. } => State::Failed {
                    reason: reason.clone(),
                },
            }
        };
        let localized = manifest.localized(locale);
        Info {
            id: manifest.id.clone(),
            name: localized.name.to_string(),
            description: localized.description.to_string(),
            version: manifest.version.clone(),
            builtin: false,
            dir,
            enabled,
            state,
        }
    }

    /// Which plugin answers to `id`, looking at the plugins directory
    /// again when none does: it may have been installed since.
    pub fn find(&mut self, id: &str, fallout: &mut Fallout) -> Lookup {
        match self.lookup(id) {
            Lookup::Missing => {
                self.scan(fallout);
                self.lookup(id)
            }
            found => found,
        }
    }

    /// Which plugin answers to `id`.
    pub fn lookup(&self, id: &str) -> Lookup {
        if let Some(index) = self
            .builtins
            .iter()
            .position(|builtin| builtin.manifest.id == id)
        {
            return Lookup::Builtin(index);
        }
        match self
            .installed
            .iter()
            .position(|installed| installed.answers_to(id))
        {
            Some(index) => Lookup::Installed(index),
            None => Lookup::Missing,
        }
    }

    pub fn builtin(&mut self, index: usize) -> &mut Builtin {
        &mut self.builtins[index]
    }

    pub fn enabled(&self, id: &str) -> bool {
        self.switches.enabled(id)
    }

    pub fn set_enabled(
        &mut self,
        id: &str,
        enabled: bool,
        fallout: &mut Fallout,
    ) -> Result<(), String> {
        let known = self
            .builtins
            .iter()
            .any(|builtin| builtin.manifest.id == id)
            || self
                .installed
                .iter()
                .any(|installed| installed.id() == Some(id));
        if !known {
            return Err(format!("there is no plugin {id:?}"));
        }
        // The file is written over what it says now, so what another host
        // changed in it is acted on first, not just carried along.
        self.follow_switches(fallout);
        let moved = self
            .switches
            .set(id, enabled)
            .map_err(|err| format!("cannot save the switch: {err:#}"))?;
        if !moved {
            return Ok(());
        }
        log::info!("plugin {id} turned {}", if enabled { "on" } else { "off" });
        fallout.changed = true;
        for installed in &mut self.installed {
            if installed.id() == Some(id) {
                installed.reset(
                    if enabled {
                        "it was turned on"
                    } else {
                        "it was turned off"
                    },
                    fallout,
                );
            }
        }
        Ok(())
    }

    /// Stops a plugin, reads its manifest again and forgets that it failed:
    /// every installed plugin when `id` is `None`, after looking for new and
    /// removed ones.
    pub fn reload(&mut self, id: Option<&str>, fallout: &mut Fallout) -> Result<(), String> {
        fallout.changed = true;
        let Some(id) = id else {
            self.scan(fallout);
            for installed in &mut self.installed {
                installed.reset("it was reloaded", fallout);
            }
            return Ok(());
        };
        if self
            .builtins
            .iter()
            .any(|builtin| builtin.manifest.id == id)
        {
            // Nothing to reload: it is as new as the host.
            return Ok(());
        }
        let index = self
            .installed
            .iter()
            .position(|installed| installed.id() == Some(id) || installed.dir_name == id)
            .ok_or_else(|| format!("there is no plugin {id:?}"))?;
        let installed = &mut self.installed[index];
        installed.reset("it was reloaded", fallout);
        installed.read_manifest();
        self.resolve_ids(fallout);
        Ok(())
    }

    /// Hands a call or a command made for `id` to the installed plugin at
    /// `index`, found answering to it: to its program, started if it is not
    /// running, and started again if it was rebuilt since. While a program
    /// that ran under the id is still exiting, the call waits for it to be
    /// gone. An error is why it cannot reach the plugin at all; one met on
    /// the way is the call's answer, in `fallout`.
    pub fn ask(
        &mut self,
        id: &str,
        index: usize,
        waiter: Waiter,
        message: Message,
        listener: &Arc<dyn Listener>,
        fallout: &mut Fallout,
    ) -> Result<(), String> {
        let launch = self.launchable(index, fallout)?;
        // Its manifest, read again, may name another id now: the call is
        // not for that plugin.
        if launch.id != id {
            return Err(format!("there is no plugin named {id:?}"));
        }
        let installed = &mut self.installed[index];
        if let Run::Running(process) = &installed.run {
            if stamp(&process.program) != process.program_stamp {
                installed.stop("its program changed, and it is started again", fallout);
            }
        }
        let call = Queued { waiter, message };
        if self.still_exiting(id) {
            let installed = &mut self.installed[index];
            if installed.queued.len() >= process::PENDING_LIMIT {
                return Err(format!(
                    "it has {} calls waiting already",
                    process::PENDING_LIMIT
                ));
            }
            installed.queued.push(call);
            fallout.changed = true;
            return Ok(());
        }
        let mut calls = std::mem::take(&mut self.installed[index].queued);
        calls.push(call);
        self.deliver(index, launch, calls, listener, fallout);
        Ok(())
    }

    /// Whether a program that ran under `id` is still on its way out, from
    /// whichever directory: a new one would share its data directory.
    fn still_exiting(&mut self, id: &str) -> bool {
        let mut exiting = false;
        for installed in &mut self.installed {
            // Gone without word reaching here counts as gone.
            installed.exiting.retain(|exiting| !exiting.gone());
            exiting |= installed.exiting.iter().any(|exiting| exiting.id == id);
        }
        exiting
    }

    /// Sends the calls that waited for programs on their way out, for each
    /// plugin whose id has none left.
    fn send_queued(&mut self, listener: &Arc<dyn Listener>, fallout: &mut Fallout) {
        for index in 0..self.installed.len() {
            if self.installed[index].queued.is_empty() {
                continue;
            }
            // A manifest read again here answers the calls it no longer
            // takes: the plugin is reset.
            let launch = match self.launchable(index, fallout) {
                Ok(launch) => launch,
                Err(why) => {
                    let calls = std::mem::take(&mut self.installed[index].queued);
                    fallout.lost(calls.into_iter().map(|call| call.waiter).collect(), &why);
                    continue;
                }
            };
            if self.still_exiting(&launch.id) {
                continue;
            }
            let calls = std::mem::take(&mut self.installed[index].queued);
            if !calls.is_empty() {
                self.deliver(index, launch, calls, listener, fallout);
            }
        }
    }

    /// Whether the installed plugin at `index` may run now, reading its
    /// manifest again if it changed, and what starting it takes.
    fn launchable(&mut self, index: usize, fallout: &mut Fallout) -> Result<Launch, String> {
        self.refresh_manifest(index, fallout);
        let installed = &self.installed[index];
        let manifest = installed.manifest.as_ref().map_err(Clone::clone)?;
        if let Some(reason) = &installed.shadowed {
            return Err(reason.clone());
        }
        if !manifest.supported() {
            return Err(format!("{} does not run on this system", manifest.name));
        }
        if !self.switches.enabled(&manifest.id) {
            return Err(format!("{} is turned off", manifest.name));
        }
        let id = manifest.id.clone();
        let program = manifest.program(&installed.dir);
        let program_stamp = program.as_ref().ok().and_then(|(path, _)| stamp(path));

        let installed = &mut self.installed[index];
        if let Run::Failed {
            reason,
            manifest: manifest_then,
            program: program_then,
        } = &installed.run
        {
            if *manifest_then == installed.manifest_stamp && *program_then == program_stamp {
                return Err(reason.clone());
            }
            // Something changed since: worth another try.
            installed.run = Run::Idle;
            installed.crashes.clear();
            fallout.changed = true;
        }
        Ok(Launch {
            id,
            program,
            program_stamp,
        })
    }

    /// Sends `calls` to the program of the installed plugin at `index`,
    /// starting it first if it is not running.
    fn deliver(
        &mut self,
        index: usize,
        launch: Launch,
        calls: Vec<Queued>,
        listener: &Arc<dyn Listener>,
        fallout: &mut Fallout,
    ) {
        if !matches!(self.installed[index].run, Run::Running(_)) {
            if let Err(why) = self.start(index, launch, listener, fallout) {
                fallout.lost(calls.into_iter().map(|call| call.waiter).collect(), &why);
                return;
            }
        }
        let Run::Running(process) = &mut self.installed[index].run else {
            unreachable!("just started");
        };
        let expired = process.expire();
        fallout.lost(expired, "the plugin did not answer in time");
        let mut gone = None;
        let mut calls = calls.into_iter();
        for call in calls.by_ref() {
            match process.ask(call.waiter, call.message) {
                Ok(()) => {}
                Err(Refused::Busy(why)) => fallout.unanswered.push((call.waiter, why)),
                Err(Refused::Gone(why)) => {
                    fallout.unanswered.push((call.waiter, why.clone()));
                    gone = Some((process.generation, why));
                    break;
                }
            }
        }
        if let Some((generation, why)) = gone {
            let rest: Vec<Waiter> = calls.map(|call| call.waiter).collect();
            fallout.lost(rest, &why);
            // It is not coming back to answer anything: a crash.
            if let Ok(id) = self.installed[index]
                .manifest
                .as_ref()
                .map(|m| m.id.clone())
            {
                self.gave_up(&id, generation, why, false, fallout);
            }
        }
    }

    /// Starts the program of the installed plugin at `index`.
    fn start(
        &mut self,
        index: usize,
        launch: Launch,
        listener: &Arc<dyn Listener>,
        fallout: &mut Fallout,
    ) -> Result<(), String> {
        let Launch {
            id,
            program,
            program_stamp,
        } = launch;
        let installed = &mut self.installed[index];
        fallout.changed = true;
        let (path, args) = match program {
            Ok(found) => found,
            Err(reason) => {
                installed.run = Run::Failed {
                    reason: reason.clone(),
                    manifest: installed.manifest_stamp,
                    program: None,
                };
                return Err(reason);
            }
        };
        self.generation += 1;
        let data_dir = self.plugin_data.join(&id);
        let start = Start {
            id: &id,
            generation: self.generation,
            program: &path,
            args: &args,
            dir: &installed.dir,
            data_dir: &data_dir,
            ready_within: self.ready_within,
        };
        match Process::start(start, program_stamp, Arc::clone(listener)) {
            Ok(process) => {
                installed.run = Run::Running(process);
                Ok(())
            }
            Err(reason) => {
                log::warn!("plugin {id}: {reason}");
                installed.run = Run::Failed {
                    reason: reason.clone(),
                    manifest: installed.manifest_stamp,
                    program: program_stamp,
                };
                Err(reason)
            }
        }
    }

    /// Lets go of what `client` asked that is still waiting: it has gone.
    pub fn forget_client(&mut self, client: u64) {
        for installed in &mut self.installed {
            installed.queued.retain(|call| call.waiter.client != client);
            if let Run::Running(process) = &mut installed.run {
                process.forget_client(client);
            }
        }
    }

    /// The program of plugin `id` in its run number `generation`, if that
    /// run is the one going on.
    pub fn process(&mut self, id: &str, generation: u64) -> Option<&mut Process> {
        self.installed
            .iter_mut()
            .filter(|installed| installed.answers_to(id))
            .find_map(|installed| match &mut installed.run {
                Run::Running(process) if process.generation == generation => Some(process),
                _ => None,
            })
    }

    /// The run's program said it is ready, speaking plugin API `api`.
    pub fn ready(&mut self, id: &str, generation: u64, api: u32, fallout: &mut Fallout) {
        let Some(process) = self.process(id, generation) else {
            return;
        };
        process.ready = true;
        fallout.changed = true;
        if api == API {
            return;
        }
        self.gave_up(
            id,
            generation,
            format!("it speaks plugin API {api}; this ThinkTerm speaks {API}"),
            true,
            fallout,
        );
    }

    /// The run's program has exited. One told to stop is gone at last: the
    /// calls that waited for it go to a new program. Any other ended
    /// without being asked to.
    pub fn ended(
        &mut self,
        id: &str,
        generation: u64,
        why: String,
        listener: &Arc<dyn Listener>,
        fallout: &mut Fallout,
    ) {
        // Runs are numbered across every plugin, so an exiting one is found
        // by its number: its plugin may answer to another id by now.
        let exited = self.installed.iter().position(|installed| {
            installed
                .exiting
                .iter()
                .any(|exiting| exiting.generation == generation)
        });
        if let Some(index) = exited {
            self.installed[index]
                .exiting
                .retain(|exiting| exiting.generation != generation);
        } else {
            let ready = self
                .process(id, generation)
                .is_some_and(|process| process.ready);
            let why = if ready {
                why
            } else {
                format!("before it was ready, {why}")
            };
            self.gave_up(id, generation, why, false, fallout);
        }
        // Calls may wait for this run, or for one already found gone before
        // word of it came.
        self.send_queued(listener, fallout);
    }

    /// The run's program has not said it is ready in time.
    pub fn late(&mut self, id: &str, generation: u64, fallout: &mut Fallout) {
        let ready = self
            .process(id, generation)
            .is_some_and(|process| process.ready);
        if !ready {
            let why = format!("it did not say it was ready within {:?}", self.ready_within);
            self.gave_up(id, generation, why, false, fallout);
        }
    }

    /// Ends the run for `why`: its calls go unanswered, and it counts as a
    /// crash -- or, with `for_good`, as a failure at once.
    fn gave_up(
        &mut self,
        id: &str,
        generation: u64,
        why: String,
        for_good: bool,
        fallout: &mut Fallout,
    ) {
        let Some(installed) = self.installed.iter_mut().find(|installed| {
            installed.answers_to(id)
                && matches!(&installed.run, Run::Running(process) if process.generation == generation)
        }) else {
            return;
        };
        let Run::Running(process) = std::mem::replace(&mut installed.run, Run::Idle) else {
            return;
        };
        let program = stamp(&process.program);
        log::warn!("plugin {id} stopped: {why}");
        fallout.lost(process.kill(), &format!("the plugin stopped: {why}"));

        let now = Instant::now();
        installed
            .crashes
            .retain(|crashed| now.duration_since(*crashed) < CRASH_WINDOW);
        installed.crashes.push(now);
        let too_often = installed.crashes.len() >= CRASH_LIMIT;
        installed.run = if for_good || too_often {
            let reason = if too_often {
                format!("it stopped {CRASH_LIMIT} times within a minute; last, {why}")
            } else {
                why
            };
            Run::Failed {
                reason,
                manifest: installed.manifest_stamp,
                program,
            }
        } else {
            Run::Crashed(why)
        };
    }

    /// Stops every program, for the host to exit.
    pub fn shut_down(&mut self) {
        let mut running = Vec::new();
        let mut exiting = Vec::new();
        for installed in &mut self.installed {
            if let Run::Running(process) = std::mem::replace(&mut installed.run, Run::Idle) {
                running.push(process);
            }
            exiting.append(&mut installed.exiting);
        }
        if !running.is_empty() || !exiting.is_empty() {
            process::stop_all(running, exiting, EXIT_GRACE);
        }
    }
}
