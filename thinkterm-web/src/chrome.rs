//! The chrome around the canvas: a strip of the server's tabs, the panes of
//! the tab on show, and a switch between following the desktop's focus and
//! staying on one pane.
//!
//! It is HTML. The canvas draws the terminal and nothing else; buttons, text
//! and layout are the browser's job, which it does with accessibility,
//! wrapping and zoom for free. The model below is pure so it can be tested
//! natively; only `dom` touches the page.

use codec::ListPanesResponse;
use thinkterm_proto::layout::{PaneEntry, PaneNode};
use thinkterm_proto::{PaneId, TabId, WindowId};

/// One tab in the strip.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TabView {
    pub tab_id: TabId,
    pub window_id: WindowId,
    pub title: String,
    /// The pane a click on the tab shows: the one on show if it is in
    /// this tab, else the tab's active pane.
    pub target: PaneId,
    pub current: bool,
    /// The tab's panes, listed only for the tab on show and only when
    /// there is more than one.
    pub panes: Vec<PaneView>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PaneView {
    pub pane_id: PaneId,
    pub title: String,
    pub current: bool,
}

pub use crate::layout::leaves;

/// The pane a tab is showing: its active one, or the first.
pub fn active_pane(node: &PaneNode) -> Option<PaneEntry> {
    match node {
        PaneNode::Empty => None,
        PaneNode::Leaf(e) => Some(e.clone()),
        PaneNode::Stack(s) => s.panes.get(s.active).or(s.panes.first()).cloned(),
        PaneNode::Split { left, right, .. } => {
            let (l, r) = (active_pane(left), active_pane(right));
            match (&l, &r) {
                (Some(a), _) if a.is_active_pane => l,
                (_, Some(b)) if b.is_active_pane => r,
                _ => l.or(r),
            }
        }
    }
}

/// The entry for a pane, wherever it is.
pub fn entry(layout: &ListPanesResponse, pane_id: PaneId) -> Option<PaneEntry> {
    layout
        .tabs
        .iter()
        .flat_map(leaves)
        .find(|e| e.pane_id == pane_id)
        .cloned()
}

/// What a page shows when it has nothing better: the first tab's pane.
pub fn first_choice(layout: &ListPanesResponse) -> Option<PaneEntry> {
    layout.tabs.iter().find_map(active_pane)
}

/// The strip for a layout, given the pane on show. `current_title` is
/// that pane's title as the page knows it, which is fresher than the
/// listing's copy.
/// The tabs of one window, like a desktop window's row; every window's
/// when `window` is `None`.
pub fn model(
    layout: &ListPanesResponse,
    current: PaneId,
    current_title: &str,
    window: Option<WindowId>,
) -> Vec<TabView> {
    let mut tabs = Vec::new();
    for (i, node) in layout.tabs.iter().enumerate() {
        let panes = leaves(node);
        let Some(first) = panes.first() else {
            continue;
        };
        if window.is_some_and(|w| w != first.window_id) {
            continue;
        }
        let is_current = panes.iter().any(|e| e.pane_id == current);
        let target = if is_current {
            current
        } else {
            active_pane(node).map(|e| e.pane_id).unwrap_or(first.pane_id)
        };
        let title_of = |e: &PaneEntry| {
            if e.pane_id == current {
                current_title.to_string()
            } else {
                e.title.clone()
            }
        };
        let title = layout
            .tab_titles
            .get(i)
            .filter(|t| !t.is_empty())
            .cloned()
            .or_else(|| panes.iter().find(|e| e.pane_id == target).map(|e| title_of(e)))
            .unwrap_or_default();
        let pane_views = if is_current && panes.len() > 1 {
            panes
                .iter()
                .map(|e| PaneView {
                    pane_id: e.pane_id,
                    title: title_of(e),
                    current: e.pane_id == current,
                })
                .collect()
        } else {
            vec![]
        };
        tabs.push(TabView {
            tab_id: first.tab_id,
            window_id: first.window_id,
            title,
            target,
            current: is_current,
            panes: pane_views,
        });
    }
    tabs
}

#[cfg(target_arch = "wasm32")]
mod dom {
    use super::TabView;
    use thinkterm_proto::{PaneId, TabId};
    use wasm_bindgen::JsCast;

    /// The strip's element and what it draws into it.
    pub struct TabStrip {
        root: web_sys::Element,
    }

    /// What a click on the strip meant.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub enum Click {
        Pane(PaneId),
        Follow,
        NewTab,
        /// The bar's pane, or the focused one from a chord.
        SplitRight(Option<PaneId>),
        SplitBelow(Option<PaneId>),
        Zoom(Option<PaneId>),
        /// The focused pane's close button (a chord, or a bar).
        Close,
        /// A capsule's own close button.
        ClosePane(PaneId),
        /// A tab's close button: every pane of it.
        CloseTab(TabId),
        /// The sidebar toggle at the row's start.
        Sidebar,
    }

    /// The state the strip shows.
    #[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
    pub struct Controls {
        pub following: bool,
        pub fit: bool,
        /// The tab whose close button was pressed once and waits for the
        /// press that means it.
        pub closing_tab: Option<TabId>,
        /// The page shows less than the tab: the grid it fits.
        pub clipped: Option<(usize, usize)>,
    }

    fn escape(text: &str) -> String {
        let mut out = String::with_capacity(text.len());
        for c in text.chars() {
            match c {
                '&' => out.push_str("&amp;"),
                '<' => out.push_str("&lt;"),
                '>' => out.push_str("&gt;"),
                '"' => out.push_str("&quot;"),
                c => out.push(c),
            }
        }
        out
    }

    /// Titles are shown short; the full one is the tooltip.
    fn short(title: &str) -> String {
        const MAX: usize = 28;
        if title.chars().count() <= MAX {
            title.to_string()
        } else {
            let head: String = title.chars().take(MAX - 1).collect();
            format!("{head}…")
        }
    }

    impl TabStrip {
        pub fn mount(id: &str) -> Option<Self> {
            let root = web_sys::window()?.document()?.get_element_by_id(id)?;
            Some(Self { root })
        }

        pub fn element(&self) -> &web_sys::Element {
            &self.root
        }

        /// Redraw the whole strip. It is a few dozen elements; rebuilding
        /// them is cheaper to get right than diffing them.
        pub fn render(&self, tabs: &[TabView], controls: Controls) {
            let following = controls.following;
            let icon = crate::icons::svg;
            let mut html = format!(
                "<span class=\"act\" data-action=\"sidebar\" title=\"Show or hide the sidebar\">{}</span>",
                icon("panel-left")
            );
            for tab in tabs {
                let class = if tab.current { "tab current" } else { "tab" };
                let (title, _) = crate::navbar::display_title(&tab.title);
                let (close_class, close_body) = if controls.closing_tab == Some(tab.tab_id) {
                    ("x danger", "close?")
                } else {
                    ("x", icon("x"))
                };
                html.push_str(&format!(
                    "<span class=\"{class}\" data-pane=\"{}\" title=\"{}\">{}<span class=\"t\">{}</span><span class=\"{close_class}\" data-action=\"close-tab\" data-tab=\"{}\" title=\"Close this tab and end its programs. Asks twice.\">{close_body}</span></span>",
                    tab.target,
                    escape(&tab.title),
                    icon("square-terminal"),
                    escape(&short(title)),
                    tab.tab_id
                ));
            }
            let (class, text, hint) = if following {
                (
                    "follow on",
                    "following the desktop",
                    "This page shows whichever pane the desktop focuses. Click to stay on this one.",
                )
            } else {
                (
                    "follow off",
                    "staying here",
                    "This page stays on this pane. Click to follow the desktop's focus again.",
                )
            };
            html.push_str(&format!(
                "<span class=\"act\" data-action=\"new-tab\" title=\"New tab\">{}</span>",
                icon("plus")
            ));
            if controls.fit {
                html.push_str(
                    "<span class=\"trail\"><span class=\"hint\" title=\"The tab is fitted to this window; Ctrl+Shift+F gives the size back\">fitted</span></span>",
                );
            }
            let _ = (following, class, text, hint);
            self.root.set_inner_html(&html);
        }

        /// What a click landed on, if anything the strip owns.
        pub fn click_target(ev: &web_sys::MouseEvent) -> Option<Click> {
            let target: web_sys::Element = ev.target()?.dyn_into().ok()?;
            let hit = target.closest("[data-pane],[data-follow],[data-action]").ok()??;
            if hit.has_attribute("data-follow") {
                return Some(Click::Follow);
            }
            if let Some(action) = hit.get_attribute("data-action") {
                // A bar's buttons act on the bar's pane, whichever is focused.
                let bar = hit
                    .closest("[data-nav]")
                    .ok()
                    .flatten()
                    .and_then(|nav| nav.get_attribute("data-nav")?.parse::<PaneId>().ok());
                return match action.as_str() {
                    "new-tab" => Some(Click::NewTab),
                    "split-right" => Some(Click::SplitRight(bar)),
                    "split-below" => Some(Click::SplitBelow(bar)),
                    "zoom" => Some(Click::Zoom(bar)),
                    "close" => Some(Click::Close),
                    "close-pane" => hit.get_attribute("data-pane")?.parse().ok().map(Click::ClosePane),
                    "close-tab" => hit.get_attribute("data-tab")?.parse().ok().map(Click::CloseTab),
                    "sidebar" => Some(Click::Sidebar),
                    _ => None,
                };
            }
            hit.get_attribute("data-pane")?.parse().ok().map(Click::Pane)
        }
    }
}

#[cfg(target_arch = "wasm32")]
pub use dom::{Click, Controls, TabStrip};

#[cfg(test)]
mod tests {
    use super::*;
    use thinkterm_proto::layout::PaneStackEntry;
    use thinkterm_proto::split::{SplitDirection, SplitDirectionAndSize};
    use wezterm_term::TerminalSize;

    fn pane(tab_id: TabId, pane_id: PaneId, title: &str, active: bool) -> PaneEntry {
        PaneEntry {
            window_id: 0,
            tab_id,
            pane_id,
            title: title.to_string(),
            size: TerminalSize::default(),
            working_dir: None,
            is_active_pane: active,
            is_zoomed_pane: false,
            alt_screen: false,
            workspace: "default".to_string(),
            cursor_pos: Default::default(),
            physical_top: 0,
            top_row: 0,
            left_col: 0,
            tty_name: None,
        }
    }

    fn layout() -> ListPanesResponse {
        ListPanesResponse {
            tabs: vec![
                PaneNode::Leaf(pane(1, 10, "zsh", true)),
                PaneNode::Split {
                    left: Box::new(PaneNode::Leaf(pane(2, 20, "vim", false))),
                    right: Box::new(PaneNode::Leaf(pane(2, 21, "htop", true))),
                    node: SplitDirectionAndSize {
                        direction: SplitDirection::Horizontal,
                        first: TerminalSize::default(),
                        second: TerminalSize::default(),
                    },
                },
                PaneNode::Empty,
            ],
            tab_titles: vec!["".to_string(), "work".to_string(), "".to_string()],
            window_titles: Default::default(),
        }
    }

    #[test]
    fn a_tab_is_named_by_its_title_or_its_target_pane() {
        let tabs = model(&layout(), 10, "zsh · ~", None);
        assert_eq!(tabs.len(), 2, "the empty tab is not listed");
        assert_eq!(model(&layout(), 10, "zsh · ~", Some(0)).len(), 2, "both tabs are in window 0");
        assert!(model(&layout(), 10, "zsh · ~", Some(9)).is_empty(), "no tab is in window 9");
        assert_eq!(tabs[0].title, "zsh · ~", "the page's own title is fresher");
        assert_eq!(tabs[1].title, "work");
        assert!(tabs[0].current && !tabs[1].current);
        assert_eq!(tabs[1].target, 21, "a click on the other tab goes to its active pane");
    }

    #[test]
    fn panes_are_listed_only_for_the_tab_on_show_and_only_when_there_are_several() {
        let tabs = model(&layout(), 20, "vim", None);
        assert!(tabs[0].panes.is_empty());
        assert_eq!(
            tabs[1].panes,
            vec![
                PaneView { pane_id: 20, title: "vim".into(), current: true },
                PaneView { pane_id: 21, title: "htop".into(), current: false },
            ]
        );
        assert_eq!(tabs[1].target, 20, "the tab on show points at the pane on show");
        let tabs = model(&layout(), 10, "zsh", None);
        assert!(tabs[0].panes.is_empty(), "one pane is not a choice");
    }

    #[test]
    fn a_pane_is_found_wherever_it_is_and_the_first_tab_is_the_default() {
        assert_eq!(entry(&layout(), 21).map(|e| e.tab_id), Some(2));
        assert!(entry(&layout(), 99).is_none());
        assert_eq!(first_choice(&layout()).map(|e| e.pane_id), Some(10));
        let stack = ListPanesResponse {
            tabs: vec![PaneNode::Stack(PaneStackEntry {
                panes: vec![pane(3, 30, "a", false), pane(3, 31, "b", false)],
                active: 1,
                pane_stack_id: None,
            })],
            tab_titles: vec![],
            window_titles: Default::default(),
        };
        assert_eq!(first_choice(&stack).map(|e| e.pane_id), Some(31));
    }
}
