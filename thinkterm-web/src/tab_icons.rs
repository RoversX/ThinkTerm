//! The icons heading the tabs, as the desktop draws them: a coloured
//! circle carrying the mark of what a pane runs. The cards are the
//! desktop's, as its settings make them on the machine this page reached
//! (`thinkterm_tab_icons::WireCatalog`, sent over the relay's control
//! socket); which card a pane gets is the desktop's own rule (`Matcher`):
//! its agent's, else its program's, else the terminal's. A window tab
//! always gets the terminal's, as it holds panes that may each run
//! something else. The page draws a card from its id.

use thinkterm_proto::ForegroundProgram;
use thinkterm_tab_icons::{Matcher, WireCatalog};

pub struct TabIcons {
    /// The cards' ids, in the catalog's order: what `Matcher` indexes.
    ids: Vec<String>,
    /// The machine's own switch: off, no tab gets an icon.
    enabled: bool,
    matcher: Matcher,
}

impl TabIcons {
    pub fn parse(json: &str) -> Result<Self, String> {
        let catalog: WireCatalog = serde_json::from_str(json).map_err(|e| e.to_string())?;
        Ok(Self::new(&catalog))
    }

    pub fn new(catalog: &WireCatalog) -> Self {
        Self {
            ids: catalog.cards.iter().map(|card| card.id.clone()).collect(),
            enabled: catalog.enabled,
            matcher: catalog.matcher(),
        }
    }

    /// The card for a pane running `agent` and led by `program`.
    pub fn card(&self, agent: Option<&str>, program: Option<&ForegroundProgram>) -> Option<&str> {
        self.id(self.matcher.find(agent, program))
    }

    /// The terminal's card, for a window tab.
    pub fn terminal(&self) -> Option<&str> {
        self.id(self.matcher.terminal())
    }

    fn id(&self, index: usize) -> Option<&str> {
        if !self.enabled {
            return None;
        }
        self.ids.get(index).map(String::as_str)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn catalog(enabled: bool) -> String {
        serde_json::json!({
            "enabled": enabled,
            "terminal": "terminal",
            "cards": [
                { "id": "terminal", "circle": "#333333", "glyph": "#ffffff", "svg": "<svg/>", "programs": [], "agents": [] },
                { "id": "vim", "circle": "#22aa44", "glyph": "#ffffff", "svg": "<svg/>", "programs": ["vim", "nvim"], "agents": [] },
                { "id": "claude", "circle": "#cc7744", "glyph": "#ffffff", "svg": "<svg/>", "programs": [], "agents": ["claude"] },
            ],
        })
        .to_string()
    }

    fn program(executable: &str) -> ForegroundProgram {
        ForegroundProgram {
            executable: executable.to_string(),
            runs: None,
        }
    }

    #[test]
    fn a_pane_gets_its_agents_card_then_its_programs_then_the_terminals() {
        let icons = TabIcons::parse(&catalog(true)).unwrap();
        assert_eq!(icons.card(Some("claude"), Some(&program("node"))), Some("claude"));
        assert_eq!(icons.card(None, Some(&program("nvim"))), Some("vim"));
        assert_eq!(icons.card(None, Some(&program("zsh"))), Some("terminal"));
        assert_eq!(icons.card(None, None), Some("terminal"));
        assert_eq!(icons.terminal(), Some("terminal"));
    }

    #[test]
    fn the_machines_switch_turns_every_icon_off() {
        let icons = TabIcons::parse(&catalog(false)).unwrap();
        assert_eq!(icons.card(None, Some(&program("vim"))), None);
        assert_eq!(icons.terminal(), None);
    }

    #[test]
    fn a_catalog_that_does_not_parse_is_refused() {
        assert!(TabIcons::parse("{}").is_err());
    }
}
