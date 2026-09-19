//! The handle the page holds: what the chrome shows, as JSON (`views`),
//! and the few things it can ask for. The display layer is not told what
//! changed, only that something did; it reads the views it draws.

use crate::page::WebApp;
use std::rc::Rc;
use wasm_bindgen::prelude::*;

#[wasm_bindgen]
pub struct Client {
    app: Rc<WebApp>,
}

impl Client {
    pub(crate) fn new(app: Rc<WebApp>) -> Self {
        Self { app }
    }
}

fn json<T: serde::Serialize>(value: &T) -> String {
    serde_json::to_string(value).unwrap_or_else(|err| format!("{{\"error\":{:?}}}", err.to_string()))
}

#[wasm_bindgen]
impl Client {
    /// Called once right away, then once per task in which any view
    /// changed.
    pub fn on_change(&self, callback: js_sys::Function) {
        self.app.set_on_change(Rc::new(move || {
            if let Err(err) = callback.call0(&JsValue::NULL) {
                log::warn!("the page's change handler failed: {err:?}");
            }
        }));
    }

    pub fn sidebar(&self) -> String {
        json(&self.app.sidebar_view())
    }

    pub fn tabs(&self) -> String {
        json(&self.app.tabs_view())
    }

    pub fn navs(&self) -> String {
        json(&self.app.navs_view())
    }

    pub fn status(&self) -> String {
        json(&self.app.status_view())
    }

    pub fn layout(&self) -> String {
        self.app.layout_view()
    }

    /// The page's own labels in the active locale, keyed by catalogue id.
    pub fn strings(&self) -> String {
        json(&crate::views::strings())
    }

    /// Switch the language: a preference (`"system"` or a tag) and the
    /// browser's own list. Returns the locale it resolved to.
    pub fn set_locale(&self, preference: String, languages: Vec<String>) -> String {
        let code = thinkterm_i18n::activate_preference(&preference, &languages);
        self.app.locale_changed();
        code.to_string()
    }

    /// A panel's edge is being dragged (`on`) or was let go: while it is,
    /// the canvas follows but the tab is not reshaped on the server; the
    /// one reshape comes at the end.
    pub fn panel_drag(&self, on: bool) {
        self.app.set_panel_drag(on);
    }

    /// Refit the canvas to its box now, in the same task as the layout
    /// change that moved it, so no frame shows the old bitmap stretched.
    pub fn resize(&self) {
        self.app.resize();
    }

    /// The card's button, or a press on the card: ask for the terminal
    /// another device holds. What comes of it is the card's next state.
    pub fn take_over(&self) {
        self.app.take_over();
    }

    /// A click in the sidebar: `kind` names the row or button, `id` the
    /// thread, project or window it is about, `flag` the pin state.
    /// Returns false for a kind the model does not know.
    pub fn side_click(&self, kind: String, id: Option<String>, flag: Option<bool>) -> bool {
        let Some(click) = crate::commands::side_click(&kind, id, flag) else {
            return false;
        };
        self.app.on_side_click(click);
        true
    }

    /// The context menu for `kind` ("pane", "tab", "thread", "project",
    /// "archived-project", "sidebar-options") and `id`: JSON `MenuItem[]`.
    pub fn context_menu(&self, kind: String, id: String) -> String {
        json(&self.app.context_menu(&kind, &id))
    }

    /// Perform a menu row's action by its id: JSON `MenuOutcome`.
    pub fn menu_action(&self, id: String) -> String {
        json(&self.app.menu_action(&id))
    }

    /// The Agents panel: JSON `AgentsView`.
    pub fn agents(&self) -> String {
        json(&self.app.agents_view())
    }

    /// Bring an agent's pane on show.
    pub fn agent_reveal(&self, pane: u32) -> bool {
        self.app.agent_reveal(pane as usize)
    }

    /// The languages the page can be set to: JSON `[{preference, label}]`,
    /// "system" first, labelled in the active locale.
    pub fn languages(&self) -> String {
        let list: Vec<serde_json::Value> = thinkterm_i18n::LANGUAGE_OPTIONS
            .iter()
            .map(|o| serde_json::json!({ "preference": o.preference, "label": thinkterm_i18n::language_option_label(*o) }))
            .collect();
        json(&list)
    }

    /// The page's preferences as the model holds them: JSON `WebSettings`.
    pub fn settings(&self) -> String {
        json(&self.app.settings_view())
    }

    /// The stored preferences, whole, at boot. Returns an error text or "".
    pub fn apply_settings(&self, json: String) -> String {
        self.app.apply_settings(&json).err().unwrap_or_default()
    }

    /// One preference (`key` as in the JSON, `value` as JSON). Returns an
    /// error text or "".
    pub fn set_setting(&self, key: String, value: String) -> String {
        self.app.set_setting(&key, &value).err().unwrap_or_default()
    }

    /// The colours of the scheme named by `terminal-scheme`, as the page
    /// read them out of `schemes.json`; `None` follows the server's
    /// configuration. Returns an error text or "".
    pub fn set_terminal_palette(&self, json: Option<String>) -> String {
        let palette = match json.as_deref() {
            None => None,
            Some(text) => match crate::settings::SchemeColors::parse(text)
                .and_then(|colors| colors.to_palette())
            {
                Ok(palette) => Some(palette),
                Err(err) => return err,
            },
        };
        self.app.set_terminal_palette(palette);
        String::new()
    }

    /// The search palette's entries for `query`, ranked: JSON `Results`.
    pub fn palette(&self, query: String) -> String {
        json(&self.app.palette(&query))
    }

    /// Perform a palette pick by its entry id: JSON `PaletteOutcome`.
    pub fn palette_run(&self, id: String) -> String {
        json(&self.app.palette_run(&id))
    }

    /// The page's remembered picks (most recent first), at boot.
    pub fn set_recent(&self, ids: Vec<String>) {
        self.app.set_recent(ids);
    }

    /// Show a Space in the sidebar (the page remembers the last one).
    pub fn set_space(&self, id: String) -> bool {
        self.app.set_space(&id)
    }

    /// A key the page pressed for the user (its key bar on a phone), by
    /// its DOM name ("Escape", "ArrowUp", "c"), with modifiers. Returns
    /// whether the terminal took it.
    pub fn key(&self, name: String, ctrl: bool, alt: bool, shift: bool) -> bool {
        let dom = crate::keymap::DomKey { key: &name, code: "", ctrl, alt, shift, meta: false, composing: false };
        match crate::keymap::map_key(&dom) {
            Some((key, mods)) => self.app.key_down(key, mods, shift),
            None => false,
        }
    }

    /// Text from the page's clipboard, typed into the focused pane.
    pub fn paste(&self, text: String) {
        self.app.paste(&text);
    }

    /// Enter or Escape in the sidebar's inline input, with its value.
    pub fn side_key(&self, key: String, value: String) {
        self.app.on_side_key(&key, value);
    }

    /// A click on the tab row or a pane's bar. Returns false for an
    /// action the model does not know.
    /// A drop, decided here: `kind` is "tab" (id = tab id, `at` = the
    /// index it lands on), "pane" (id = pane id, `target` = the pane it
    /// was dropped on, `edge` = "left"|"right"|"top"|"bottom"), "thread"
    /// or "project" (`target` = the row it lands before, None for last).
    /// False when the model refuses; the listing or tree that follows
    /// redraws the page.
    pub fn drop(&self, kind: String, id: String, target: Option<String>, edge: Option<String>, at: Option<u32>) -> bool {
        match kind.as_str() {
            "tab" => match (id.parse(), at) {
                (Ok(tab), Some(at)) => self.app.move_tab(tab, at as usize),
                _ => false,
            },
            "pane" => {
                let (Ok(pane), Some(Ok(target))) = (id.parse(), target.map(|t| t.parse())) else {
                    return false;
                };
                let Some((direction, second)) = edge.as_deref().and_then(crate::commands::drop_edge) else {
                    return false;
                };
                self.app.move_pane(pane, target, direction, second)
            }
            "thread" => self.app.move_thread(&id, target.as_deref()),
            "project" => self.app.move_project(&id, target.as_deref()),
            _ => false,
        }
    }

    pub fn chrome_click(&self, action: String, pane: Option<u32>, tab: Option<u32>) -> bool {
        let pane = pane.map(|p| p as usize);
        let Some(click) = crate::commands::chrome_click(&action, pane, tab.map(|t| t as usize)) else {
            return false;
        };
        self.app.on_chrome_click(click);
        true
    }
}
