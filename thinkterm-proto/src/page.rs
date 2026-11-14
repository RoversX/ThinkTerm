//! A plugin's page: what a pane says it shows when no terminal is in it.

use serde::{Deserialize, Serialize};

/// The most a page is opened with, in bytes of JSON: it rides in every
/// listing of the panes, and is kept with the layout.
pub const PAGE_ARGS_LIMIT: usize = 4096;
/// The longest a plugin's id is, as its manifest holds it to.
const PLUGIN_ID_LIMIT: usize = 64;
/// The longest title a page's tab is given.
const TITLE_LIMIT: usize = 256;
/// The longest id a page is given: a mux gives sixteen characters.
const ID_LIMIT: usize = 64;
/// The longest directory a page is opened from, in bytes.
const CWD_LIMIT: usize = 4096;

/// A page of a plugin's in a pane's place. The mux holds no more of a page
/// than this: which plugin draws it, on the machine the pane is on, what it
/// was opened with, and where the user was when it was. What it shows is
/// between the plugin and each client that has the pane on show.
///
/// This type is used directly by the codec, take care to bump the codec
/// version if you change this. It is also kept with a saved layout.
#[derive(Deserialize, Serialize, PartialEq, Eq, Debug, Clone, Default)]
pub struct PageSpec {
    /// The plugin, by id.
    pub plugin: String,
    /// What the page was opened with, as JSON text: the plugin's to make
    /// sense of. Empty for nothing.
    #[serde(default)]
    pub args: String,
    /// What its tab is called: the plugin's name, as whoever opened the
    /// page knew it.
    #[serde(default)]
    pub title: String,
    /// The icon its tab shows, by the name a plugin's manifest gives one:
    /// a client that has none of that name shows a puzzle piece.
    #[serde(default)]
    pub icon: String,
    /// The page's own name, given it by the mux that keeps it when it is
    /// first opened and kept with it after -- moved, kept with the layout,
    /// handed to a new server, shown on another device: its plugin tells
    /// its pages apart by it, and knows one it saw before. Empty only for
    /// a page on its way to being opened.
    #[serde(default)]
    pub id: String,
    /// The directory of the terminal the page was opened from, then: a
    /// path on the machine the page is on, which its plugin runs on.
    /// Empty for none known.
    #[serde(default)]
    pub cwd: String,
    /// That terminal, by its pane's id in the mux that keeps the page.
    /// None for a page opened from no terminal, and for one restored: a
    /// pane's id in a server before is another pane's in the next.
    #[serde(default)]
    pub terminal: Option<u64>,
}

/// What a page's plugin changes of it: each that is some, in place of what
/// the page had. The rest of a page -- whose it is, its id, where it was
/// opened from -- is the mux's, and stays as it is.
///
/// This type is used directly by the codec, take care to bump the codec
/// version if you change this.
#[derive(Deserialize, Serialize, PartialEq, Eq, Debug, Clone, Default)]
pub struct PageEdit {
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub icon: Option<String>,
    /// What the page is opened with from now on -- when it is next opened,
    /// here or elsewhere, and when it is restored -- as JSON text.
    #[serde(default)]
    pub args: Option<String>,
}

impl PageSpec {
    /// What the page goes by where nothing better is known: the title it
    /// was opened with, or its plugin's id.
    pub fn name(&self) -> &str {
        match self.title.is_empty() {
            true => &self.plugin,
            false => &self.title,
        }
    }

    /// The page with `edit` made to it, or why it cannot be.
    pub fn edited(&self, edit: &PageEdit) -> Result<PageSpec, String> {
        let mut page = self.clone();
        if let Some(title) = &edit.title {
            page.title = title.clone();
        }
        if let Some(icon) = &edit.icon {
            page.icon = icon.clone();
        }
        if let Some(args) = &edit.args {
            page.args = args.clone();
        }
        match page.refusal() {
            Some(why) => Err(why),
            None => Ok(page),
        }
    }

    /// The page as a saved layout restores it: the panes it is restored
    /// among are given ids anew, so the one it names as its terminal would
    /// be another's, or nobody's.
    pub fn restored(self) -> PageSpec {
        PageSpec {
            terminal: None,
            ..self
        }
    }

    /// What whoever sends pane `pane_id` something meant for a terminal is
    /// told, when the pane holds this page.
    pub fn no_terminal(&self, pane_id: usize) -> String {
        format!("pane {pane_id} is a page of {}, not a terminal", self.plugin)
    }

    /// Why this cannot be a page's, if it cannot: kept by every mux that
    /// is asked to open one, so that none holds or hands on more than a
    /// page may be.
    pub fn refusal(&self) -> Option<String> {
        let id = &self.plugin;
        let named = !id.is_empty()
            && id.len() <= PLUGIN_ID_LIMIT
            && !id.starts_with('-')
            && id
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-');
        if !named {
            return Some(format!("{id:?} is no plugin's id"));
        }
        if self.args.len() > PAGE_ARGS_LIMIT {
            return Some(format!(
                "a page is opened with at most {PAGE_ARGS_LIMIT} bytes, not {}",
                self.args.len()
            ));
        }
        if self.title.chars().count() > TITLE_LIMIT {
            return Some(format!("a page's title is at most {TITLE_LIMIT} characters"));
        }
        if self.icon.len() > PLUGIN_ID_LIMIT {
            return Some(format!("{:?} is no icon's name", self.icon));
        }
        let id = &self.id;
        let named = id.len() <= ID_LIMIT
            && id
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-');
        if !named {
            return Some(format!("{id:?} is no page's id"));
        }
        if self.cwd.len() > CWD_LIMIT {
            return Some(format!("a page is opened from a directory of at most {CWD_LIMIT} bytes"));
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_page_goes_by_its_title_or_its_plugins_id_and_says_it_is_no_terminal() {
        let mut page = PageSpec {
            plugin: "board".into(),
            ..Default::default()
        };
        assert_eq!(page.name(), "board");
        page.title = "Release board".into();
        assert_eq!(page.name(), "Release board");
        assert_eq!(page.no_terminal(7), "pane 7 is a page of board, not a terminal");
    }

    #[test]
    fn a_page_is_a_plugins_and_no_larger_than_it_may_be() {
        let page = |plugin: &str, args: &str| PageSpec {
            plugin: plugin.into(),
            args: args.into(),
            ..Default::default()
        };
        assert_eq!(page("board", "").refusal(), None);
        assert_eq!(page("board", "{\"board\":\"release\"}").refusal(), None);
        assert!(page("", "").refusal().is_some());
        assert!(page("Board", "").refusal().is_some());
        assert!(page("../board", "").refusal().is_some());
        let most = "x".repeat(PAGE_ARGS_LIMIT);
        assert_eq!(page("board", &most).refusal(), None);
        assert!(page("board", &format!("{most}x")).refusal().is_some());
        let long = PageSpec {
            title: "t".repeat(TITLE_LIMIT + 1),
            ..page("board", "")
        };
        assert!(long.refusal().is_some());
        let named = |id: &str| PageSpec {
            id: id.into(),
            ..page("board", "")
        };
        assert_eq!(named("0f3a9c2e5b7d1846").refusal(), None);
        assert!(named("../board").refusal().is_some());
        assert!(named(&"a".repeat(ID_LIMIT + 1)).refusal().is_some());
        let far = PageSpec {
            cwd: "/".repeat(CWD_LIMIT + 1),
            ..page("board", "")
        };
        assert!(far.refusal().is_some());
    }

    #[test]
    fn its_plugin_changes_what_a_page_says_of_itself_and_nothing_else() {
        let page = PageSpec {
            plugin: "board".into(),
            args: "{\"board\":\"release\"}".into(),
            title: "Board".into(),
            icon: "list-todo".into(),
            id: "0f3a9c2e5b7d1846".into(),
            cwd: "/home/user/project".into(),
            terminal: Some(4),
        };
        let edit = PageEdit {
            title: Some("Release board".into()),
            args: Some("{\"board\":\"release\",\"card\":12}".into()),
            ..Default::default()
        };
        let edited = page.edited(&edit).unwrap();
        assert_eq!(edited.title, "Release board");
        assert_eq!(edited.args, "{\"board\":\"release\",\"card\":12}");
        assert_eq!(edited.icon, "list-todo", "left as it was");
        assert_eq!((edited.id.as_str(), edited.terminal), (page.id.as_str(), Some(4)));
        assert_eq!(page.edited(&PageEdit::default()).unwrap(), page);
        let too_much = PageEdit {
            args: Some("x".repeat(PAGE_ARGS_LIMIT + 1)),
            ..Default::default()
        };
        assert!(page.edited(&too_much).is_err());
        let restored = page.clone().restored();
        assert_eq!(restored.terminal, None, "its terminal's id was another server's");
        assert_eq!(restored.cwd, page.cwd);
        assert_eq!(restored.id, page.id);
    }
}
