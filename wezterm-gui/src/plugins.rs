//! ThinkTerm's plugins, as the desktop sees them.
//!
//! Every plugin lives in the plugin host, the thinkterm-plugin-server
//! process (see thinkterm-plugin-channel): the desktop shows what the host
//! answers and forwards what the user does. This holds what the windows
//! share -- the one session with the host, the plugin list it last sent,
//! and the windows to tell when something changes. Snippets is built on it
//! (`crate::snippets`), and so is the plugin list in Settings › Sidebar &
//! Plugins. So are the panels plugins draw in the right sidebar: each one
//! on show is a view, numbered here, whose frames are read on the session's
//! thread and handed to the window showing it
//! (`termwindow::ui::plugin_panel`). Nothing of a plugin shows outside the
//! sidebar.
//!
//! The session is let go once nothing has used it for a while. The list
//! stays as what was last heard, and is asked for afresh on the next use:
//! it is never taken for current once the session that sent it is gone.
//! While a plugin runs always, the host itself is kept up for as long as
//! the desktop runs, on a connection of its own (`keep_host_up`).
//!
//! What a panel's plugin asks of the other machine its terminal runs on
//! comes over the session too, and goes to the window showing the panel,
//! which answers it (`plugin_panel`).

use crate::termwindow::{TermWindow, TermWindowNotif};
use parking_lot::{Mutex, MutexGuard};
use serde_json::Value;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};
use thinkterm_plugin_channel::client::{Answer, Host, Notice, Session};
use thinkterm_plugin_channel::registry::{self as api, Background, Info};
use thinkterm_plugin_channel::wire::{PanelEvent, PanelRequest, Raw};
use thinkterm_plugin_panel::{Ask, Env, Frame, Rows};
use window::{Window, WindowOps};

/// How long the plugins may go unused before the session is let go.
const RELEASE_AFTER: Duration = Duration::from_secs(120);
/// How often that is looked at.
const RELEASE_CHECK: Duration = Duration::from_secs(15);

/// The plugin list as last heard, and asking for it: one request at a time,
/// with changes that arrive meanwhile folded into one more.
struct Listing {
    plugins: Option<Vec<Info>>,
    /// The language the list was last asked for in, answered or not; one in
    /// another is asked for again.
    locale: &'static str,
    asking: bool,
    /// What was last heard is out of date: something changed since, or the
    /// session that sent it is gone.
    stale: bool,
}

impl Listing {
    /// Whether to ask for the list now, in `locale`, marking it asked: when
    /// what was heard is out of date or in another language, and nothing is
    /// asking. A list that cannot be had is not asked for again until
    /// something changes.
    fn begin(&mut self, locale: &'static str) -> bool {
        if self.locale != locale {
            self.stale = true;
        }
        if self.asking || !self.stale {
            return false;
        }
        self.asking = true;
        self.stale = false;
        self.locale = locale;
        true
    }

    /// The answer came: the plugins, or `None` when there was an error and
    /// what was shown stays.
    fn heard(&mut self, plugins: Option<Vec<Info>>) {
        self.asking = false;
        if let Some(plugins) = plugins {
            self.plugins = Some(plugins);
        }
    }
}

struct Shared {
    session: Option<Arc<Session>>,
    /// Counts sessions, so a release check stops with the one it watched.
    generation: u64,
    /// The windows showing a panel a plugin sends events about -- the
    /// Snippets panel -- since the session started. A handful at most; one
    /// that has closed ignores what it is told.
    windows: Vec<Window>,
    /// Why the host cannot be reached, while it cannot.
    trouble: Option<String>,
    last_used: Option<Instant>,
    listing: Listing,
    /// Switches flipped here whose change is on its way, and to what.
    switching: Vec<(String, bool)>,
    /// How long plugins run unused, chosen here, on its way.
    choosing: Vec<(String, Background)>,
    /// Why the host refused the last change asked of it here.
    refused: Option<String>,
    /// The plugin panels on show, by number, and the window showing each.
    /// The session is kept while there are any.
    views: Option<HashMap<u64, Window>>,
    next_view: u64,
    /// A panel was opened with no session to send it on: the panels are
    /// opened again once one connects, the first time included.
    unsent: bool,
}

/// What a window hears about the plugin panel it shows, read on the
/// session's thread: frames and rows parsed there, not on the thread that
/// paints.
pub enum PanelNews {
    Frame(Frame),
    Rows(Rows),
    /// The host no longer serves it; `again` when opening it anew may work.
    Closed {
        reason: String,
        again: bool,
    },
    /// The host cannot be reached, and why: the panel is opened again once
    /// it can.
    Unreachable(String),
    /// The session connected anew, without the panel: it is to be opened
    /// again.
    Reconnected,
    /// Its plugin asks something of `machine`, the other machine it was
    /// told the panel's terminal runs on, to be answered with [`answer`]
    /// and `id`.
    Remote {
        id: u64,
        machine: String,
        ask: Ask,
    },
}

static SHARED: Mutex<Shared> = parking_lot::const_mutex(Shared {
    session: None,
    generation: 0,
    windows: Vec::new(),
    trouble: None,
    last_used: None,
    listing: Listing {
        plugins: None,
        locale: "",
        asking: false,
        stale: true,
    },
    switching: Vec::new(),
    choosing: Vec::new(),
    refused: None,
    views: None,
    next_view: 0,
    unsent: false,
});

/// The shared state, marked as in use, with a session to the host. On the
/// GUI thread: it may start the release check.
fn in_use() -> MutexGuard<'static, Shared> {
    let mut shared = SHARED.lock();
    shared.last_used = Some(Instant::now());
    if shared.session.is_none() {
        let started = Host::for_this_build().and_then(|host| Session::start(host, on_notice));
        match started {
            Ok(session) => {
                shared.session = Some(Arc::new(session));
                shared.generation += 1;
                release_when_idle(shared.generation);
            }
            Err(err) => shared.trouble = Some(format!("{err:#}")),
        }
    }
    shared
}

/// `window` shows a panel a plugin sends events about: it is told of them,
/// and the session is kept.
pub fn window_in_use(window: &Window) {
    let mut shared = in_use();
    if !shared.windows.contains(window) {
        shared.windows.push(window.clone());
    }
}

/// Why the host cannot be reached, while it cannot.
pub fn trouble() -> Option<String> {
    SHARED.lock().trouble.clone()
}

/// Asks `plugin` something, waiting `wait` for the answer, which `reply` is
/// given on the session's thread -- or why there is none.
pub fn call_within(
    plugin: &str,
    body: Value,
    wait: Duration,
    reply: impl FnOnce(Answer) + Send + 'static,
) {
    let (session, trouble) = {
        let shared = in_use();
        (shared.session.clone(), shared.trouble.clone())
    };
    // Called with the lock let go: a session that has ended answers at once.
    match session {
        Some(session) => session.call_within(plugin, body, wait, reply),
        None => {
            reply(Err(trouble.unwrap_or_else(|| {
                "the plugin host cannot be reached".into()
            })))
        }
    }
}

/// [`call_within`], with the wait a call is given.
pub fn call(plugin: &str, body: Value, reply: impl FnOnce(Answer) + Send + 'static) {
    call_within(
        plugin,
        body,
        thinkterm_plugin_channel::client::CALL_TIMEOUT,
        reply,
    )
}

/// Has every window showing a plugin's panel apply `change` to itself.
pub fn tell_windows(change: fn(&mut TermWindow)) {
    let windows: Vec<Window> = SHARED.lock().windows.clone();
    promise::spawn::spawn_into_main_thread(async move {
        for window in windows {
            let repaint = window.clone();
            window.notify(TermWindowNotif::Apply(Box::new(move |term_window| {
                change(term_window);
                repaint.invalidate();
            })));
        }
    })
    .detach();
}

/// What shows the list brought up to date: the settings window.
fn list_changed() {
    promise::spawn::spawn_into_main_thread(async move {
        crate::settings_window::invalidate_open_settings_window();
    })
    .detach();
}

fn on_notice(notice: Notice) {
    match notice {
        // Whatever the host said before, it is asked again.
        Notice::Connected { again } => {
            let views = {
                let mut shared = SHARED.lock();
                shared.trouble = None;
                shared.listing.stale = true;
                let again = again || std::mem::take(&mut shared.unsent);
                match (again, &shared.views) {
                    (true, Some(views)) => views.clone(),
                    _ => HashMap::new(),
                }
            };
            ask_for_list();
            tell_windows(TermWindow::snippets_changed);
            for (view, window) in views {
                tell_panel_window(&window, view, PanelNews::Reconnected);
            }
        }
        Notice::Panel { view, event } => heard_for_panel(view, event),
        Notice::Event { plugin, body } if plugin == api::PLUGIN => {
            match serde_json::from_value(body) {
                Ok(api::Event::Changed) => {
                    SHARED.lock().listing.stale = true;
                    ask_for_list();
                }
                Err(err) => log::warn!("a plugin list event that does not read: {err}"),
            }
        }
        Notice::Event { plugin, body } => crate::snippets::on_event(&plugin, body),
        Notice::Trouble(why) => {
            let views = {
                let mut shared = SHARED.lock();
                shared.trouble = Some(why.clone());
                shared.views.clone().unwrap_or_default()
            };
            for (view, window) in views {
                tell_panel_window(&window, view, PanelNews::Unreachable(why.clone()));
            }
            // Rows already on show stay; an empty panel says why.
            tell_windows(|_| {});
            list_changed();
        }
    }
}

/// Lets the session go once nothing has used it for a while, and the
/// windows' rows with it -- without asking them to paint, which would
/// only ask for them again.
fn release_when_idle(generation: u64) {
    promise::spawn::spawn(async move {
        loop {
            smol::Timer::after(RELEASE_CHECK).await;
            let mut shared = SHARED.lock();
            if shared.generation != generation || shared.session.is_none() {
                return;
            }
            let idle = shared
                .last_used
                .map_or(true, |used| used.elapsed() >= RELEASE_AFTER);
            let panels = shared.views.as_ref().is_some_and(|views| !views.is_empty());
            if !idle || panels {
                continue;
            }
            shared.session = None;
            shared.trouble = None;
            shared.listing.stale = true;
            shared.listing.asking = false;
            for window in shared.windows.drain(..) {
                window.notify(TermWindowNotif::Apply(Box::new(|term_window| {
                    term_window.snippets_released();
                })));
            }
            return;
        }
    })
    .detach();
}

/// Shows plugin `plugin`'s panel in `window` for `env` -- the extended view
/// of panel `extends`, with one: its number, which what the window hears
/// about it and what it sends carry.
pub fn open_panel(plugin: &str, env: &Env, extends: Option<u64>, window: &Window) -> u64 {
    let view = {
        let mut shared = SHARED.lock();
        shared.next_view += 1;
        let view = shared.next_view;
        shared
            .views
            .get_or_insert_with(HashMap::new)
            .insert(view, window.clone());
        view
    };
    send_open(view, open_request(plugin, env, extends));
    view
}

/// Opens panel `view` again: the host closed it, or the session that had it
/// is gone.
pub fn reopen_panel(view: u64, plugin: &str, env: &Env) {
    send_open(view, open_request(plugin, env, None));
}

/// Sends the open of panel `view`; with no session to send it on, the
/// window says why, and it is opened once a session connects.
fn send_open(view: u64, request: PanelRequest) {
    let (session, lost) = {
        let mut shared = in_use();
        match shared.session.clone() {
            Some(session) => (Some(session), None),
            None => {
                shared.unsent = true;
                let why = shared
                    .trouble
                    .clone()
                    .unwrap_or_else(|| "the plugin host cannot be reached".into());
                let window = shared
                    .views
                    .as_ref()
                    .and_then(|views| views.get(&view).cloned());
                (None, window.map(|window| (window, why)))
            }
        }
    };
    match (session, lost) {
        (Some(session), _) => session.panel(view, request),
        (None, Some((window, why))) => {
            tell_panel_window(&window, view, PanelNews::Unreachable(why))
        }
        (None, None) => {}
    }
}

fn open_request(plugin: &str, env: &Env, extends: Option<u64>) -> PanelRequest {
    PanelRequest::Open {
        plugin: plugin.to_string(),
        env: Raw::new(env),
        extends,
    }
}

/// Tells the host about panel `view`.
pub fn tell_panel(view: u64, request: PanelRequest) {
    let session = in_use().session.clone();
    if let Some(session) = session {
        session.panel(view, request);
    }
}

/// Panel `view` went off show.
pub fn close_panel(view: u64) {
    let session = {
        let mut shared = SHARED.lock();
        if let Some(views) = shared.views.as_mut() {
            views.remove(&view);
        }
        shared.session.clone()
    };
    if let Some(session) = session {
        session.panel(view, PanelRequest::Close);
    }
}

/// Reads what the host said about panel `view`, and hands it to the window
/// showing it. A frame that does not read is still taken in, so the next
/// one can come: said over the session it came on, from its thread, where
/// nothing is to be started.
fn heard_for_panel(view: u64, event: PanelEvent) {
    let news = match event {
        PanelEvent::Frame { frame } => match frame.read::<Frame>() {
            Ok(frame) => PanelNews::Frame(frame),
            Err(err) => {
                log::warn!("plugin panel {view} sent a frame that does not read: {err}");
                let session = SHARED.lock().session.clone();
                if let Some(session) = session {
                    session.panel(view, PanelRequest::Shown);
                }
                return;
            }
        },
        PanelEvent::Rows { rows } => match rows.read::<Rows>() {
            Ok(rows) => PanelNews::Rows(rows),
            Err(err) => {
                log::warn!("plugin panel {view} sent rows that do not read: {err}");
                return;
            }
        },
        PanelEvent::Closed { reason, again } => PanelNews::Closed { reason, again },
        PanelEvent::Remote { id, machine, ask } => match ask.read::<Ask>() {
            Ok(ask) => PanelNews::Remote { id, machine, ask },
            Err(err) => {
                let why = format!("not something this ThinkTerm can do: {err}");
                answer(view, id, &thinkterm_plugin_panel::Answer::failed(why));
                return;
            }
        },
    };
    let window = SHARED
        .lock()
        .views
        .as_ref()
        .and_then(|views| views.get(&view).cloned());
    if let Some(window) = window {
        tell_panel_window(&window, view, news);
    }
}

/// Tells the plugin of panel `view` what came of what it asked, `id`.
pub fn answer(view: u64, id: u64, answer: &thinkterm_plugin_panel::Answer) {
    let session = SHARED.lock().session.clone();
    if let Some(session) = session {
        let answer = Raw::new(answer);
        session.panel(view, PanelRequest::Answer { id, answer });
    }
}

fn tell_panel_window(window: &Window, view: u64, news: PanelNews) {
    window.notify(TermWindowNotif::Apply(Box::new(move |term_window| {
        term_window.plugin_panel_heard(view, news);
    })));
}

/// Asks for the list if what was last heard is out of date and nothing is
/// asking already.
fn ask_for_list() {
    let (session, locale) = {
        let mut shared = SHARED.lock();
        let Some(session) = shared.session.clone() else {
            return;
        };
        let locale = crate::i18n::current_locale();
        if !shared.listing.begin(locale) {
            return;
        }
        (session, locale)
    };
    let request = api::Request::List {
        locale: locale.to_string(),
    };
    session.call(api::PLUGIN, to_body(&request), listed);
}

fn listed(answer: Answer) {
    let plugins = answer.and_then(|body| from_body::<Vec<Info>>(body));
    let heard = match plugins {
        Ok(plugins) => Some(plugins),
        // What was shown stays; the next change asks again.
        Err(why) => {
            log::warn!("plugins: {why}");
            None
        }
    };
    SHARED.lock().listing.heard(heard.clone());
    if let Some(plugins) = heard {
        promise::spawn::spawn_into_main_thread(async move {
            follow_snippets_switch(&plugins);
            follow_panels(&plugins);
        })
        .detach();
    }
    ask_for_list();
    list_changed();
}

/// At start, whether any plugin is installed, looked at off the GUI thread:
/// if one is, the list is asked for, which brings the panels plugins add to
/// the sidebar up to date; if none is, the panels kept from an earlier run
/// are let go. A machine with no plugin installed starts no plugin host.
/// The host is kept up from then on while a plugin runs always.
pub fn look_for_panels() {
    thinkterm_plugin_channel::client::keep_host_up();
    let dir = thinkterm_plugin_channel::paths::plugins_dir();
    let looking = std::thread::Builder::new()
        .name("plugin-look".into())
        .spawn(move || {
            let installed = thinkterm_plugin_channel::paths::any_installed(&dir);
            promise::spawn::spawn_into_main_thread(async move {
                if installed {
                    refresh();
                } else {
                    follow_panels(&[]);
                }
            })
            .detach();
        });
    if let Err(err) = looking {
        log::warn!("cannot look for installed plugins: {err}");
    }
}

/// Someone is looking again -- Settings › Sidebar & Plugins came on show:
/// the list is asked for afresh, and with it the host looks at the plugins
/// directory again, finding what was installed or removed since.
pub fn refresh() {
    in_use().listing.stale = true;
    ask_for_list();
}

/// The plugins as last heard, asking for them when that is out of date.
/// `None` until the host first answers. Cheap: for painting.
pub fn plugins() -> Option<Vec<Info>> {
    let plugins = in_use().listing.plugins.clone();
    ask_for_list();
    plugins
}

/// The switch of plugin `id` as it is to be shown: the change on its way,
/// if one is.
pub fn switching(id: &str) -> Option<bool> {
    SHARED
        .lock()
        .switching
        .iter()
        .find(|(switching, _)| switching == id)
        .map(|(_, enabled)| *enabled)
}

/// Why the host refused the last change asked of it here.
pub fn refused() -> Option<String> {
    SHARED.lock().refused.clone()
}

/// Turns plugin `id` on or off. The switch shows the change until the host
/// has answered, and goes back if it refuses.
pub fn set_enabled(id: &str, enabled: bool) {
    {
        let mut shared = in_use();
        shared.refused = None;
        shared.switching.retain(|(switching, _)| switching != id);
        shared.switching.push((id.to_string(), enabled));
    }
    let request = api::Request::SetEnabled {
        id: id.to_string(),
        enabled,
    };
    let id = id.to_string();
    call(api::PLUGIN, to_body(&request), move |answer| {
        {
            let mut shared = SHARED.lock();
            shared.switching.retain(|(switching, _)| *switching != id);
            if let Err(why) = answer {
                shared.refused = Some(why);
            }
            // The answer comes before the change is announced: the list
            // on show is out of date whichever way it went.
            shared.listing.stale = true;
        }
        ask_for_list();
        list_changed();
    });
    list_changed();
}

/// How long plugin `id` runs unused as it is to be shown: the choice on its
/// way, if one is.
pub fn choosing(id: &str) -> Option<Background> {
    SHARED
        .lock()
        .choosing
        .iter()
        .find(|(choosing, _)| choosing == id)
        .map(|(_, background)| *background)
}

/// Lets plugin `id` run unused as `background` says; its manifest's
/// `default` is kept as no choice at all. Shown at once, and as it was if
/// the host refuses.
pub fn set_background(id: &str, background: Background, default: Background) {
    {
        let mut shared = in_use();
        shared.refused = None;
        shared.choosing.retain(|(choosing, _)| choosing != id);
        shared.choosing.push((id.to_string(), background));
    }
    let request = api::Request::SetBackground {
        id: id.to_string(),
        background: (background != default).then_some(background),
    };
    let id = id.to_string();
    call(api::PLUGIN, to_body(&request), move |answer| {
        {
            let mut shared = SHARED.lock();
            shared.choosing.retain(|(choosing, _)| *choosing != id);
            if let Err(why) = answer {
                shared.refused = Some(why);
            }
            shared.listing.stale = true;
        }
        ask_for_list();
        list_changed();
    });
    list_changed();
}

/// Looks for new and removed plugins, and restarts the running ones.
pub fn reload() {
    SHARED.lock().refused = None;
    let request = api::Request::Reload { id: None };
    call(api::PLUGIN, to_body(&request), |answer| {
        if let Err(why) = answer {
            SHARED.lock().refused = Some(why);
        }
        list_changed();
    });
}

/// Snippets appears as a panel of the right sidebar only while its plugin
/// is on. The desktop keeps the switch as last heard, so the panel is
/// offered at once when it starts; the host's word wins when it comes. On
/// the GUI thread, with the settings.
fn follow_snippets_switch(plugins: &[Info]) {
    let Some(snippets) = plugins
        .iter()
        .find(|plugin| plugin.id == thinkterm_snippets::wire::PLUGIN)
    else {
        return;
    };
    let enabled = snippets.enabled;
    let mut settings = crate::native_settings::load();
    let chrome = &mut settings.chrome;
    // The panel was turned off in Settings › Sidebar before it was a
    // plugin: that choice becomes the plugin's switch, once.
    if chrome.right_sidebar_snippets_enabled == Some(false) {
        chrome.right_sidebar_snippets_enabled = None;
        chrome.snippets_plugin_enabled = Some(false);
        if let Err(err) = crate::native_settings::save(&settings) {
            log::warn!("saving the Snippets switch: {err:#}");
        }
        crate::settings_window::refresh_open_settings_window_chrome();
        if enabled {
            let request = api::Request::SetEnabled {
                id: thinkterm_snippets::wire::PLUGIN.into(),
                enabled: false,
            };
            call(api::PLUGIN, to_body(&request), |_| {});
        }
        return;
    }
    if chrome.snippets_plugin_enabled == Some(enabled) {
        return;
    }
    chrome.snippets_plugin_enabled = Some(enabled);
    if let Err(err) = crate::native_settings::save(&settings) {
        log::warn!("saving the Snippets switch: {err:#}");
    }
    // An open settings window holds the settings as they were, and would
    // write them back so.
    crate::settings_window::refresh_open_settings_window_chrome();
    if let Some(front_end) = crate::frontend::try_front_end() {
        for gui_window in front_end.gui_windows() {
            gui_window
                .window
                .notify(TermWindowNotif::Apply(Box::new(|term_window| {
                    term_window.right_sidebar_panels_changed();
                })));
        }
    }
}

/// The plugins' panels the right sidebar offers: those of the plugins that
/// are on and not broken. The desktop keeps them as last heard, so they are
/// offered from the start, without asking the host; its word wins when it
/// comes. On the GUI thread, with the settings.
fn follow_panels(plugins: &[Info]) {
    let panels: Vec<crate::native_settings::NativePluginPanel> = plugins
        .iter()
        .filter(|plugin| plugin.usable())
        .filter_map(|plugin| {
            Some(crate::native_settings::NativePluginPanel {
                id: plugin.id.clone(),
                name: plugin.name.clone(),
                icon: plugin.panel.as_ref()?.icon.clone(),
            })
        })
        .collect();
    let mut settings = crate::native_settings::load();
    // The width kept for a plugin's extended view goes with the plugin.
    let widths = settings.chrome.plugin_extended_widths.len();
    settings
        .chrome
        .plugin_extended_widths
        .retain(|id, _| plugins.iter().any(|plugin| plugin.id == *id));
    let forgotten = settings.chrome.plugin_extended_widths.len() != widths;
    if settings.chrome.plugin_panels == panels && !forgotten {
        return;
    }
    settings.chrome.plugin_panels = panels;
    if let Err(err) = crate::native_settings::save(&settings) {
        log::warn!("saving the plugins' panels: {err:#}");
    }
    crate::settings_window::refresh_open_settings_window_chrome();
    if let Some(front_end) = crate::frontend::try_front_end() {
        for gui_window in front_end.gui_windows() {
            gui_window
                .window
                .notify(TermWindowNotif::Apply(Box::new(|term_window| {
                    term_window.right_sidebar_panels_changed();
                })));
        }
    }
}

fn to_body(request: &api::Request) -> Value {
    serde_json::to_value(request).expect("a plugin request always serialises")
}

pub fn from_body<T: serde::de::DeserializeOwned>(body: Value) -> Result<T, String> {
    serde_json::from_value(body).map_err(|err| format!("an answer that does not read: {err}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_list_that_cannot_be_had_is_not_asked_for_in_a_loop() {
        let mut listing = Listing {
            plugins: None,
            locale: "",
            asking: false,
            stale: true,
        };
        assert!(listing.begin("en-US"));
        assert!(!listing.begin("en-US"), "one request at a time");
        listing.heard(None);
        assert!(
            !listing.begin("en-US"),
            "an error is asked about again at once"
        );
        listing.stale = true;
        assert!(listing.begin("en-US"), "a change asks again");
        listing.heard(Some(Vec::new()));
        assert!(listing.begin("zh-CN"), "another language asks again");
    }
}
