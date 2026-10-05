//! The plugins as the page shows them: the list in Settings › Sidebar &
//! Plugins, and whether the right panel offers Snippets. Nothing of a
//! plugin shows outside the sidebar. Like the snippets, the list is the
//! plugin host's, on
//! the server's machine, reached through the server (`PluginFrame`): it is
//! asked for while something on show needs it, and its changes are
//! followed while they are. What was last heard stays on show, but is never
//! taken for current once the way to the host went: it is asked for afresh.

use crate::plugins::Answer;
use serde::Serialize;
use thinkterm_i18n::{tr, tr_args};
use thinkterm_plugin_channel::registry::{Background, Info, State};

/// What wants the list: Settings › Sidebar & Plugins, the right panel.
pub type Want = &'static str;

#[derive(Debug, Default)]
pub struct PluginsModel {
    wanted: Vec<Want>,
    plugins: Option<Vec<Info>>,
    /// The list was asked for over the way to the host that is open now,
    /// so its changes come here.
    followed: bool,
    asking: bool,
    /// What was last heard is out of date.
    stale: bool,
    /// The way to the host went while the list was on its way: it is asked
    /// for again after the pause that follows, or when something new wants
    /// it -- not at once, over and over.
    resting: bool,
    /// The language the list was last asked for in.
    locale: String,
    /// Why the host cannot be reached through the server, while it cannot.
    trouble: Option<String>,
    /// Switches flipped here whose change is on its way, and to what.
    switching: Vec<(String, bool)>,
    /// How long plugins run unused, chosen here, on its way.
    choosing: Vec<(String, Background)>,
    /// Why the host refused the last change asked of it here.
    refused: Option<String>,
    /// Times in a row the list could not be had, for the pause before the
    /// next try.
    failures: u32,
    /// Counts changes to what the views show, so the page re-reads them
    /// only when they moved.
    revision: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct PluginsView {
    /// `loading`, `ready` or `unavailable`.
    pub state: &'static str,
    /// What the section says besides its list: loading, before there is
    /// one, or why the host cannot be reached -- which makes a list on show
    /// only what was last heard.
    pub status: String,
    pub rows: Vec<PluginRow>,
    /// Why the host refused the last change asked of it here, as a sentence.
    pub refused: Option<String>,
    pub revision: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct PluginRow {
    /// Tells rows apart: two installed plugins can claim one id, and the
    /// one not used is listed too, with why.
    pub key: String,
    pub id: String,
    pub name: String,
    /// Where it came from, its state when that is worth saying, and what it
    /// does, in one line.
    pub detail: String,
    /// The switch as it is to be shown: the change on its way, if one is.
    pub enabled: bool,
    /// Whether a switch could do anything for it.
    pub switchable: bool,
    /// Never let run: offered a button that lets it, which takes a press
    /// made for it, rather than a switch.
    pub new: bool,
    /// Built into ThinkTerm: its switch is its panel's, among the panels,
    /// not in the list.
    pub builtin: bool,
    /// How long it runs unused -- `always`, `briefly` or `never`, a choice on
    /// its way included -- for an installed plugin that is on; none for any
    /// other, which offers no choice.
    pub background: Option<&'static str>,
    /// What that choice means, and whether it is the plugin's own.
    pub background_detail: String,
}

fn background_name(background: Background) -> &'static str {
    match background {
        Background::Always => "always",
        Background::Briefly => "briefly",
        Background::Never => "never",
    }
}

/// What running `background` means, as the desktop's settings say it, and
/// that it is the plugin's own when it is `default`.
fn background_detail(background: Background, default: Background) -> String {
    let meaning = tr(match background {
        Background::Always => "settings-plugins-background-always-description",
        Background::Briefly => "settings-plugins-background-briefly-description",
        Background::Never => "settings-plugins-background-never-description",
    });
    if background == default {
        format!("{meaning} · {}", tr("settings-plugins-background-default"))
    } else {
        meaning
    }
}

/// The choice a page names, `always`, `briefly` or `never`.
pub fn background_named(name: &str) -> Option<Background> {
    match name {
        "always" => Some(Background::Always),
        "briefly" => Some(Background::Briefly),
        "never" => Some(Background::Never),
        _ => None,
    }
}

fn with_reason(key: &str, reason: &str) -> String {
    let mut args = thinkterm_i18n::FluentArgs::new();
    args.set("reason", reason.to_string());
    tr_args(key, &args)
}

/// The line under a plugin's name, as the desktop's settings write it: a
/// built-in plugin says which panel it provides.
fn detail(plugin: &Info) -> String {
    if plugin.builtin && plugin.id == thinkterm_snippets::wire::PLUGIN {
        let mut args = thinkterm_i18n::FluentArgs::new();
        args.set("panel", tr("right-mode-snippets"));
        return tr_args("settings-plugins-builtin-panel", &args);
    }
    let origin = if plugin.builtin {
        tr("settings-plugins-builtin")
    } else {
        plugin.version.clone()
    };
    let state = match &plugin.state {
        State::Off | State::Idle => None,
        State::New => Some(tr("settings-plugins-new")),
        State::Starting => Some(tr("settings-plugins-starting")),
        State::Running => Some(tr("settings-plugins-running")),
        State::Crashed { reason } => Some(with_reason("settings-plugins-crashed", reason)),
        State::Failed { reason } => Some(with_reason("settings-plugins-failed", reason)),
        State::Invalid { reason } => Some(with_reason("settings-plugins-invalid", reason)),
        State::Unsupported { .. } => Some(tr("settings-plugins-unsupported")),
    };
    // A new one says where it is, on the server's machine: that is what
    // allowing it lets run.
    let place = plugin
        .target
        .clone()
        .or_else(|| plugin.dir.clone())
        .filter(|_| plugin.state == State::New);
    [Some(origin), state, place, Some(plugin.description.clone())]
        .into_iter()
        .flatten()
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join(" · ")
}

impl PluginsModel {
    pub fn revision(&self) -> u64 {
        self.revision
    }

    fn changed(&mut self) {
        self.revision += 1;
    }

    /// `who` needs the list, or no longer does. Something coming on show
    /// asks for it afresh, and with it the host looks at its plugins
    /// directory again. True when that makes it wanted where it was not.
    pub fn want(&mut self, who: Want, on: bool) -> bool {
        let before = self.wanted();
        let had = self.wanted.contains(&who);
        self.wanted.retain(|wanting| *wanting != who);
        if on {
            self.wanted.push(who);
            if !had {
                self.stale = true;
                self.resting = false;
            }
        }
        !before && self.wanted()
    }

    pub fn wanted(&self) -> bool {
        !self.wanted.is_empty()
    }

    /// The language to ask for the list in now, if it is to be asked for:
    /// one request at a time, while it is wanted, when what was heard is
    /// out of date, or was heard over a way to the host that is gone, or in
    /// another language.
    pub fn next_list(&mut self, locale: &str) -> Option<String> {
        if !self.wanted() || self.asking || self.resting {
            return None;
        }
        if !self.stale && self.followed && self.locale == locale {
            return None;
        }
        self.asking = true;
        self.stale = false;
        self.followed = true;
        self.locale = locale.to_string();
        Some(self.locale.clone())
    }

    /// The host's answer to a list asked for in `locale`. An error is not
    /// asked about again until something changes -- the way to the host
    /// going and coming back, say: an unreachable host is not asked in a
    /// loop.
    pub fn listed(&mut self, answer: Answer) {
        self.asking = false;
        let plugins = answer.and_then(|body| {
            serde_json::from_value::<Vec<Info>>(body).map_err(|err| err.to_string())
        });
        match plugins {
            Ok(plugins) => {
                self.plugins = Some(plugins);
                self.trouble = None;
                self.failures = 0;
            }
            Err(why) => {
                log::warn!("plugins: {why}");
                self.trouble = Some(why);
                self.failures = self.failures.saturating_add(1);
                // Failed because the way went meanwhile: the pause decides
                // when to try again.
                if !self.followed {
                    self.resting = true;
                }
            }
        }
        self.changed();
    }

    /// How long to wait before asking again after the way to the host
    /// went: a second, doubling while the list cannot be had, to half a
    /// minute.
    pub fn retry_delay_ms(&self) -> f64 {
        1000.0 * f64::from(1u32 << self.failures.min(5)).min(30.0)
    }

    /// The host said the list changed.
    pub fn list_changed(&mut self) {
        self.stale = true;
    }

    /// The pause after the way to the host went is over.
    pub fn retry(&mut self) {
        self.resting = false;
    }

    /// The way to the host went: whatever was heard is kept on show, and
    /// asked for afresh on the next need.
    pub fn connection_lost(&mut self) {
        self.followed = false;
        self.asking = false;
        self.switching.clear();
        self.choosing.clear();
        self.changed();
    }

    /// A switch flipped here, on its way to the host.
    pub fn switching(&mut self, id: &str, enabled: bool) {
        self.refused = None;
        self.switching.retain(|(switching, _)| switching != id);
        self.switching.push((id.to_string(), enabled));
        self.changed();
    }

    /// The host's answer to a switch flipped here: the list is out of date
    /// whichever way it went.
    pub fn switched(&mut self, id: &str, answer: Result<(), String>) {
        self.switching.retain(|(switching, _)| switching != id);
        if let Err(why) = answer {
            self.refused = Some(why);
        }
        self.stale = true;
        self.changed();
    }

    /// How long plugin `id` runs unused, chosen here, on its way to the
    /// host.
    pub fn choosing(&mut self, id: &str, background: Background) {
        self.refused = None;
        self.choosing.retain(|(choosing, _)| choosing != id);
        self.choosing.push((id.to_string(), background));
        self.changed();
    }

    /// The host's answer to a choice made here, as to a switch.
    pub fn chose(&mut self, id: &str, answer: Result<(), String>) {
        self.choosing.retain(|(choosing, _)| choosing != id);
        if let Err(why) = answer {
            self.refused = Some(why);
        }
        self.stale = true;
        self.changed();
    }

    /// The manifest's choice for plugin `id`, as last heard.
    pub fn background_default(&self, id: &str) -> Option<Background> {
        self.plugins
            .iter()
            .flatten()
            .find(|plugin| plugin.id == id && !plugin.builtin)
            .map(|plugin| plugin.background_default)
    }

    /// The host refused something asked of it here.
    pub fn refused(&mut self, why: String) {
        self.refused = Some(why);
        self.changed();
    }

    /// The panels plugins add to the right panel, as last heard: those of
    /// the plugins that are on and not broken, in the list's order.
    pub fn panels(&self) -> Vec<crate::agents::PluginPanel> {
        let Some(plugins) = &self.plugins else {
            return Vec::new();
        };
        plugins
            .iter()
            .filter(|plugin| plugin.usable())
            .filter_map(|plugin| {
                Some(crate::agents::PluginPanel {
                    id: plugin.id.clone(),
                    name: plugin.name.clone(),
                    icon: plugin.panel.as_ref()?.icon.clone(),
                })
            })
            .collect()
    }

    /// Whether the list has been heard at all.
    pub fn known(&self) -> bool {
        self.plugins.is_some()
    }

    /// Whether plugin `id` is on and working, as last heard; `None` before
    /// anything was.
    pub fn usable(&self, id: &str) -> Option<bool> {
        let plugins = self.plugins.as_ref()?;
        Some(
            plugins
                .iter()
                .any(|plugin| plugin.id == id && plugin.usable()),
        )
    }

    pub fn view(&self) -> PluginsView {
        let (state, status) = match (&self.plugins, &self.trouble) {
            (Some(_), None) => ("ready", String::new()),
            (Some(_), Some(why)) => ("ready", with_reason("settings-plugins-unavailable", why)),
            (None, Some(why)) => (
                "unavailable",
                with_reason("settings-plugins-unavailable", why),
            ),
            (None, None) => ("loading", tr("settings-plugins-loading")),
        };
        let rows = self
            .plugins
            .iter()
            .flatten()
            .map(|plugin| {
                let enabled = self
                    .switching
                    .iter()
                    .find(|(id, _)| *id == plugin.id)
                    .map_or(plugin.enabled, |(_, enabled)| *enabled);
                let switchable = !matches!(
                    plugin.state,
                    State::Invalid { .. } | State::Unsupported { .. }
                );
                let background = self
                    .choosing
                    .iter()
                    .find(|(id, _)| *id == plugin.id)
                    .map_or(plugin.background, |(_, background)| *background);
                let offered = !plugin.builtin && switchable && enabled;
                PluginRow {
                    key: plugin.dir.clone().unwrap_or_else(|| plugin.id.clone()),
                    id: plugin.id.clone(),
                    name: plugin.name.clone(),
                    detail: detail(plugin),
                    enabled,
                    switchable,
                    new: plugin.state == State::New
                        && !self.switching.iter().any(|(id, _)| *id == plugin.id),
                    builtin: plugin.builtin,
                    background: offered.then(|| background_name(background)),
                    background_detail: if offered {
                        background_detail(background, plugin.background_default)
                    } else {
                        String::new()
                    },
                }
            })
            .collect();
        PluginsView {
            state,
            status,
            rows,
            refused: self
                .refused
                .as_deref()
                .map(|why| with_reason("settings-plugins-refused", why)),
            revision: self.revision,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn listed(model: &mut PluginsModel, plugins: serde_json::Value) {
        model.next_list("en-US").expect("asked");
        model.listed(Ok(plugins));
    }

    fn plugin(id: &str, enabled: bool, state: serde_json::Value) -> serde_json::Value {
        json!({
            "id": id, "name": id.to_uppercase(), "version": "1.0", "enabled": enabled,
            "state": state, "description": "Does things."
        })
    }

    #[test]
    fn the_list_is_asked_for_while_wanted_and_again_when_it_changed() {
        let mut model = PluginsModel::default();
        assert_eq!(model.next_list("en-US"), None, "nothing wants it");
        assert!(model.want("settings", true));
        assert!(!model.want("panel", true), "wanted already");
        assert_eq!(model.next_list("en-US").as_deref(), Some("en-US"));
        assert_eq!(model.next_list("en-US"), None, "one at a time");
        model.listed(Ok(json!([plugin("a", true, json!({"kind": "idle"}))])));
        assert_eq!(model.next_list("en-US"), None, "heard, and nothing changed");
        model.list_changed();
        assert!(model.next_list("en-US").is_some());
        model.listed(Ok(json!([])));
        assert!(model.next_list("zh-CN").is_some(), "another language");
        model.listed(Ok(json!([])));
        model.connection_lost();
        assert!(
            model.next_list("zh-CN").is_some(),
            "the way to the host went"
        );
    }

    #[test]
    fn a_list_that_cannot_be_had_is_not_asked_for_in_a_loop() {
        let mut model = PluginsModel::default();
        model.want("settings", true);
        model.next_list("en-US").unwrap();
        model.listed(Err("no plugin host".into()));
        assert_eq!(model.next_list("en-US"), None, "not again at once");
        assert_eq!(model.view().state, "unavailable");
        assert!(model.view().status.contains("no plugin host"));
        let first = model.retry_delay_ms();
        model.connection_lost();
        model
            .next_list("en-US")
            .expect("asked again once the way went");
        model.listed(Err("no plugin host".into()));
        assert!(model.retry_delay_ms() > first, "and less and less often");
    }

    #[test]
    fn a_list_heard_before_the_host_went_says_it_is_only_that() {
        let mut model = PluginsModel::default();
        model.want("settings", true);
        listed(
            &mut model,
            json!([plugin("a", true, json!({"kind": "idle"}))]),
        );
        assert_eq!(model.view().status, "");
        model.connection_lost();
        model.next_list("en-US").expect("asked again");
        model.listed(Err("no plugin host".into()));
        let view = model.view();
        assert_eq!(view.state, "ready");
        assert_eq!(view.rows.len(), 1, "what was heard stays on show");
        assert!(view.status.contains("no plugin host"), "{}", view.status);
    }

    #[test]
    fn a_list_lost_with_the_way_to_the_host_waits_for_the_pause() {
        let mut model = PluginsModel::default();
        model.want("settings", true);
        model.next_list("en-US").unwrap();
        // The way goes with the list on its way, which then fails.
        model.connection_lost();
        model.listed(Err("the plugin host went away".into()));
        assert_eq!(model.next_list("en-US"), None, "asked again at once");
        model.retry();
        assert!(model.next_list("en-US").is_some(), "asked after the pause");
        model.listed(Err("the plugin host went away".into()));

        model.connection_lost();
        model.listed(Err("the plugin host went away".into()));
        model.want("panel", true);
        assert!(
            model.next_list("en-US").is_some(),
            "the panel coming on show asks"
        );
    }

    #[test]
    fn coming_on_show_again_asks_for_the_list_afresh() {
        let mut model = PluginsModel::default();
        model.want("settings", true);
        listed(&mut model, json!([]));
        model.want("panel", true);
        assert!(model.next_list("en-US").is_some(), "the panel came on show");
        model.listed(Ok(json!([])));
        model.want("panel", true);
        assert_eq!(model.next_list("en-US"), None, "still open: nothing new");
        model.want("panel", false);
        model.want("panel", true);
        assert!(model.next_list("en-US").is_some(), "opened again");
    }

    #[test]
    fn two_plugins_claiming_one_id_are_two_rows() {
        let mut model = PluginsModel::default();
        model.want("settings", true);
        let mut used = plugin("a", true, json!({"kind": "idle"}));
        used["dir"] = json!("/plugins/a");
        let mut unused = plugin(
            "a",
            true,
            json!({"kind": "invalid", "reason": "a has the same id"}),
        );
        unused["dir"] = json!("/plugins/b");
        listed(
            &mut model,
            json!([
                plugin("snippets", true, json!({"kind": "idle"})),
                used,
                unused
            ]),
        );
        let keys: Vec<String> = model.view().rows.into_iter().map(|row| row.key).collect();
        assert_eq!(keys, ["snippets", "/plugins/a", "/plugins/b"]);
    }

    #[test]
    fn a_switch_shows_its_change_until_the_host_answers() {
        let mut model = PluginsModel::default();
        model.want("settings", true);
        listed(
            &mut model,
            json!([plugin("a", true, json!({"kind": "running"}))]),
        );
        assert!(model.view().rows[0].enabled);
        model.switching("a", false);
        assert!(!model.view().rows[0].enabled);
        model.switched("a", Err("cannot save".into()));
        let view = model.view();
        assert!(view.rows[0].enabled, "went back");
        assert!(view.refused.unwrap().contains("cannot save"));
        assert!(model.next_list("en-US").is_some(), "asked again either way");
    }

    #[test]
    fn a_plugin_that_is_on_offers_how_long_it_runs_unused() {
        let mut model = PluginsModel::default();
        model.want("settings", true);
        let mut kept = plugin("a", true, json!({"kind": "running"}));
        kept["background"] = json!("always");
        kept["background_default"] = json!("always");
        listed(
            &mut model,
            json!([
                plugin("snippets", true, json!({"kind": "idle"})),
                kept,
                plugin("b", false, json!({"kind": "off"})),
            ]),
        );
        let view = model.view();
        assert_eq!(view.rows[1].background, Some("always"));
        assert!(view.rows[1].background_detail.contains("The plugin's default"));
        assert_eq!(view.rows[2].background, None, "off: nothing to choose");
        model.choosing("a", Background::Never);
        let row = &model.view().rows[1];
        assert_eq!(row.background, Some("never"), "the choice on its way");
        assert!(!row.background_detail.contains("default"), "{}", row.background_detail);
        model.chose("a", Err("cannot save".into()));
        assert_eq!(model.view().rows[1].background, Some("always"), "went back");
        assert_eq!(model.background_default("a"), Some(Background::Always));
        assert_eq!(background_named("briefly"), Some(Background::Briefly));
        assert_eq!(background_named("sometimes"), None);
    }

    #[test]
    fn a_built_in_plugin_says_which_panel_it_provides() {
        let mut model = PluginsModel::default();
        model.want("settings", true);
        listed(
            &mut model,
            json!([{
                "id": "snippets", "name": "Snippets", "version": "", "builtin": true,
                "enabled": true, "state": {"kind": "idle"}, "description": "Saved commands."
            }]),
        );
        let row = &model.view().rows[0];
        assert!(row.builtin);
        assert!(row.detail.contains("Snippets"), "{}", row.detail);
        assert!(!row.detail.contains("Saved commands."), "{}", row.detail);
    }

    #[test]
    fn rows_say_what_state_a_plugin_is_in_and_what_it_offers() {
        let mut model = PluginsModel::default();
        model.want("panel", true);
        listed(
            &mut model,
            json!([
                plugin("a", true, json!({"kind": "running"})),
                plugin("b", false, json!({"kind": "off"})),
                plugin(
                    "c",
                    true,
                    json!({"kind": "invalid", "reason": "no plugin.toml"})
                ),
            ]),
        );
        let view = model.view();
        assert_eq!(view.state, "ready");
        assert_eq!(view.rows[0].detail, "1.0 · Running · Does things.");
        assert!(!view.rows[2].switchable);
        assert!(view.rows[2].detail.contains("no plugin.toml"));
        assert_eq!(model.usable("a"), Some(true));
        assert_eq!(model.usable("b"), Some(false));
        assert_eq!(PluginsModel::default().usable("b"), None);
    }
}
