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
    /// Where each stands in a thread's row (`look::row_rank`).
    ranks: Vec<u8>,
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
            ranks: catalog
                .cards
                .iter()
                .map(|card| thinkterm_tab_icons::look::row_rank(!card.agents.is_empty(), card.id == catalog.terminal))
                .collect(),
            matcher: catalog.matcher(),
        }
    }

    /// Where the card `id` stands among the cards of a thread's row, as
    /// the desktop orders them: agents first, the terminal last.
    pub fn row_rank(&self, id: &str) -> u8 {
        self.ids
            .iter()
            .position(|known| known == id)
            .and_then(|index| self.ranks.get(index).copied())
            .unwrap_or(1)
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
        self.ids.get(index).map(String::as_str)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn catalog() -> String {
        serde_json::json!({
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
            task: None,
        }
    }

    #[test]
    fn a_pane_gets_its_agents_card_then_its_programs_then_the_terminals() {
        let icons = TabIcons::parse(&catalog()).unwrap();
        assert_eq!(icons.card(Some("claude"), Some(&program("node"))), Some("claude"));
        assert_eq!(icons.card(None, Some(&program("nvim"))), Some("vim"));
        assert_eq!(icons.card(None, Some(&program("zsh"))), Some("terminal"));
        assert_eq!(icons.card(None, None), Some("terminal"));
        assert_eq!(icons.terminal(), Some("terminal"));
    }

    #[test]
    fn a_row_s_cards_stand_as_the_desktop_s_do() {
        let icons = TabIcons::parse(&catalog()).unwrap();
        let mut row = vec!["terminal", "vim", "claude"];
        row.sort_by_key(|id| icons.row_rank(id));
        assert_eq!(row, ["claude", "vim", "terminal"]);
        // A card from before the host's came is another program's.
        assert_eq!(icons.row_rank("gone"), icons.row_rank("vim"));
    }

    #[test]
    fn a_catalog_that_does_not_parse_is_refused() {
        assert!(TabIcons::parse("{}").is_err());
    }
}
