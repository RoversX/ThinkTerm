use crate::model::{AppModel, ThreadKey, TreeNodeKey};
use crate::state::{UiState, ViewClass};
use mux::pane::PaneId;
use mux::tab::{PaneStackTab, SplitDirection, Tab, TabId};
use ratatui::layout::Rect;
use std::sync::Arc;
use termwiz::cell::unicode_column_width;

/// A command offered on the tree row it belongs to.
///
/// Every one of these already existed as an `Action` reachable by right click.
/// A touchscreen has no right click, so on a phone none of them could be
/// reached at all; drawing them on the row is what the desktop UI does, and is
/// the only affordance that works for both pointers.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TreeAction {
    /// Everything this row can do, via its existing context menu.
    Menu,
    /// Add the obvious child: a project under a space, a thread under a project.
    Add,
}

impl TreeAction {
    pub fn glyph(self) -> &'static str {
        match self {
            Self::Menu => "⋯",
            Self::Add => "+",
        }
    }
}

/// A command on the tab bar's trailing edge.
///
/// Only what belongs to the window lives here. Zooming and splitting act on a
/// pane, so they are drawn on the pane that owns them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TabBarControl {
    Menu,
    NewTab,
}

impl TabBarControl {
    pub fn glyph(self) -> &'static str {
        match self {
            Self::Menu => "⋮",
            Self::NewTab => "+",
        }
    }
}

/// A command on one pane's own nav bar.
///
/// These act on a pane, not on the window, which is why they are not on the tab
/// bar. Every one already existed as a key chord; none of them had anywhere to
/// be tapped.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PaneTool {
    /// Carries which of the two it currently means, like `TreeAction::Pin`.
    Zoom {
        zoomed: bool,
    },
    SplitRight,
    SplitDown,
    /// Another terminal in *this* pane's stack, drawn as a second level-2 tab
    /// rather than as a new split.
    NewTab,
}

impl PaneTool {
    pub fn glyph(self) -> &'static str {
        match self {
            Self::Zoom { zoomed: false } => "⤢",
            Self::Zoom { zoomed: true } => "⤡",
            Self::SplitRight => "◫",
            Self::SplitDown => "⊟",
            Self::NewTab => "+",
        }
    }
}

/// One pane's own strip: the panes stacked behind it on the left, what can be
/// done to it on the right.
#[derive(Clone, Debug)]
pub struct PaneNav {
    pub rect: Rect,
    /// One entry per pane in this pane's stack, already positioned.
    pub tabs: Vec<PaneNavTab>,
    pub tools: Vec<(Rect, PaneTool)>,
}

#[derive(Clone, Debug)]
pub struct PaneNavTab {
    pub rect: Rect,
    /// Carried for the same reason `TreeLine::key` is: the hit region answers
    /// the tap, this names what was drawn.
    #[allow(dead_code)]
    pub pane_id: PaneId,
    pub label: String,
    pub active: bool,
    /// Every stack tab offers it, whichever one is showing — a tab strip where
    /// only one tab can be closed is a tab strip you have to click twice.
    pub close: Option<Rect>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum HitTarget {
    Tree(TreeNodeKey),
    TreeToggle(TreeNodeKey),
    TreeAction(TreeNodeKey, TreeAction),
    NewThread,
    NewProject,
    OpenSettings,
    Tab(TabId),
    CloseTab(TabId),
    TabControl(TabBarControl),
    Pane(PaneId),
    /// Show this pane, which is stacked behind the one currently drawn there.
    PaneNavTab(PaneId),
    PaneNavClose(PaneId),
    PaneTool(PaneId, PaneTool),
    Scrollbar(PaneId),
    Split(usize),
    SidebarResize,
    Detach,
    Attention,
    SidebarToggle,
    ContextMenu(usize),
    Connection(usize),
    Setting(usize),
    DialogConfirm,
    DialogCancel,
}

#[derive(Clone, Debug)]
pub struct HitRegion {
    pub rect: Rect,
    pub target: HitTarget,
}

#[derive(Clone, Debug)]
pub struct TreeLine {
    pub rect: Rect,
    #[allow(dead_code)]
    pub key: TreeNodeKey,
    pub label: String,
    pub depth: u16,
    pub selected: bool,
    pub expandable: bool,
    pub collapsed: bool,
    pub status: Option<&'static str>,
    /// A pinned thread says so with a ★ before its name, on every row. Nothing
    /// else on the row claims that glyph now, so it can only mean one thing.
    pub pinned: bool,
    /// Buttons drawn at the right end of the row, already positioned. Layout
    /// owns the geometry so the glyph and the region that answers a tap cannot
    /// drift apart.
    pub actions: Vec<(Rect, TreeAction)>,
}

#[derive(Clone, Debug)]
pub struct TabLine {
    pub rect: Rect,
    pub label: String,
    pub selected: bool,
    /// The tab's own close button, when the name still had room beside it.
    pub close: Option<Rect>,
}

#[derive(Clone, Debug)]
pub struct PaneView {
    pub pane_id: PaneId,
    /// The grid itself, already excluding anything drawn beside it.
    pub rect: Rect,
    pub scrollbar: Option<Rect>,
    /// The pane's own strip, when it had a row to spare for one.
    pub nav: Option<PaneNav>,
    /// The frame drawn around the pane, when it has one. Encloses the grid, its
    /// scrollbar and its nav bar.
    pub border: Option<Rect>,
    /// A split branch too short to show normal chrome and a useful grid. It is
    /// painted as an explicit compact pane instead of an orphaned output row.
    pub collapsed: Option<Rect>,
}

#[derive(Clone, Debug)]
pub struct SplitView {
    pub index: usize,
    pub rect: Rect,
    pub direction: SplitDirection,
}

#[derive(Clone, Debug)]
pub struct ViewLayout {
    pub screen: Rect,
    pub class: ViewClass,
    pub sidebar: Option<Rect>,
    pub sidebar_header: Option<Rect>,
    pub sidebar_body: Option<Rect>,
    /// The always-visible primary button and settings row, when the sidebar is
    /// tall enough to spare them a row each.
    pub sidebar_new_thread: Option<Rect>,
    /// The heading over the tree, and the add-project button on its right.
    pub sidebar_heading: Option<Rect>,
    pub sidebar_new_project: Option<Rect>,
    pub sidebar_settings: Option<Rect>,
    pub sidebar_resize: Option<Rect>,
    pub sidebar_toggle: Option<Rect>,
    pub tab_bar: Rect,
    pub content: Rect,
    pub status: Rect,
    pub tree_lines: Vec<TreeLine>,
    pub tabs: Vec<TabLine>,
    /// Trailing tab-bar buttons, already positioned.
    pub tab_controls: Vec<(Rect, TabBarControl)>,
    pub panes: Vec<PaneView>,
    pub splits: Vec<SplitView>,
    pub hits: Vec<HitRegion>,
    pub dialog: Option<Rect>,
    pub context_menu: Option<Rect>,
}

impl Default for ViewLayout {
    fn default() -> Self {
        Self {
            screen: Rect::default(),
            class: ViewClass::Desktop,
            sidebar: None,
            sidebar_header: None,
            sidebar_body: None,
            sidebar_new_thread: None,
            sidebar_heading: None,
            sidebar_new_project: None,
            sidebar_settings: None,
            sidebar_resize: None,
            sidebar_toggle: None,
            tab_bar: Rect::default(),
            content: Rect::default(),
            status: Rect::default(),
            tree_lines: vec![],
            tabs: vec![],
            tab_controls: vec![],
            panes: vec![],
            splits: vec![],
            hits: vec![],
            dialog: None,
            context_menu: None,
        }
    }
}

impl ViewLayout {
    pub fn hit(&self, x: u16, y: u16) -> Option<&HitTarget> {
        let position = (x, y).into();
        // Overlays are appended last and must win over the terminal below.
        self.hits
            .iter()
            .rev()
            .find(|hit| hit.rect.contains(position))
            .map(|hit| &hit.target)
    }

    pub fn pane_at(&self, x: u16, y: u16) -> Option<&PaneView> {
        let position = (x, y).into();
        self.panes.iter().find(|pane| pane.rect.contains(position))
    }
}

pub fn classify(width: u16, narrow_width: u16) -> ViewClass {
    if width <= narrow_width {
        ViewClass::Narrow
    } else if width < narrow_width.saturating_add(24) {
        ViewClass::Compact
    } else {
        ViewClass::Desktop
    }
}

pub fn compute_view(
    area: Rect,
    model: &AppModel,
    ui: &UiState,
    tab: Option<&Arc<Tab>>,
) -> ViewLayout {
    let mut view = ViewLayout {
        screen: area,
        class: classify(area.width, ui.narrow_width),
        ..Default::default()
    };
    if area.width == 0 || area.height == 0 {
        return view;
    }

    let narrow = view.class == ViewClass::Narrow;
    // A finger needs more than one row to land on reliably, so a screen being
    // pointed at with one spends a second row on the bar that carries every
    // button — and buys that row back from the footer, which without a keyboard
    // was spending it to name key chords. Messages still get a row, but only
    // while there is a message to put in it.
    //
    // Tying this to the layout being narrow is only ever a guess: the same
    // phone turned sideways reports enough columns to look like a desktop and
    // still has no mouse. The guess is the default; the setting is the answer.
    let touch = ui.touch_targets.unwrap_or(narrow);
    let header_height = if touch && area.height >= 6 {
        2
    } else {
        u16::from(area.height >= 3)
    };
    let status_height = if touch {
        u16::from(area.height >= 2 && (!ui.status.is_empty() || ui.toast.is_some()))
    } else {
        u16::from(area.height >= 2)
    };
    view.status = Rect::new(
        area.x,
        area.bottom().saturating_sub(status_height),
        area.width,
        status_height,
    );
    let middle_y = area.y.saturating_add(header_height);
    let middle_height = area.height.saturating_sub(header_height + status_height);

    // Too narrow to seat the tree beside a usable terminal, the tree takes the
    // whole screen for as long as it is asked for. Refusing to draw it at all
    // is what used to happen, and it left the toggle button on screen with
    // nothing to toggle — no way to reach another thread from a phone.
    // Narrow enough that a tree beside a terminal leaves neither of them
    // usable. Whether the tree is *actually* covering the screen also depends
    // on it being asked for — see `tree_covers_screen` below.
    let overlay = view.class == ViewClass::Narrow;
    let sidebar_allowed = ui.sidebar_visible;
    let sidebar_width = if sidebar_allowed {
        match view.class {
            ViewClass::Desktop => ui
                .sidebar_width
                .clamp(18, 36)
                .min(area.width.saturating_sub(24))
                .max(1),
            ViewClass::Compact => 18u16.min(area.width.saturating_sub(24)).max(1),
            ViewClass::Narrow => area.width,
        }
    } else {
        0
    };

    if sidebar_width > 0 {
        let sidebar = Rect::new(area.x, area.y, sidebar_width, area.height - status_height);
        let resize = Rect::new(
            area.x + sidebar_width,
            area.y,
            1,
            area.height - status_height,
        );
        view.sidebar = Some(sidebar);
        view.sidebar_header = Some(Rect::new(
            sidebar.x,
            sidebar.y,
            sidebar.width,
            header_height,
        ));
        if let Some(header) = view.sidebar_header {
            view.sidebar_toggle = Some(header);
            view.hits.push(HitRegion {
                rect: header,
                target: HitTarget::SidebarToggle,
            });
        }
        // The desktop UI keeps its two most-used commands permanently in view,
        // one at each end of the tree: a primary button to start work, and
        // settings where a settings button belongs. Both existed only inside
        // the ⋮ menu here, which on a phone is an extra tap for the two things
        // reached most often. They are only worth their rows once the sidebar
        // is tall enough that the tree still has somewhere to go.
        let mut body = Rect::new(sidebar.x, middle_y, sidebar.width, middle_height);
        if middle_height >= 5 && sidebar.width >= MIN_LABEL_COLUMNS {
            let new_thread = Rect::new(body.x, body.y, body.width, 1);
            view.sidebar_new_thread = Some(new_thread);
            view.hits.push(HitRegion {
                rect: new_thread,
                target: HitTarget::NewThread,
            });
            let settings = Rect::new(body.x, body.bottom() - 1, body.width, 1);
            view.sidebar_settings = Some(settings);
            view.hits.push(HitRegion {
                rect: settings,
                target: HitTarget::OpenSettings,
            });
            body = Rect::new(body.x, body.y + 1, body.width, body.height - 2);
            // A heading over the tree, with the one command that belongs to the
            // tree as a whole beside it. Adding a project is otherwise reachable
            // only by first selecting the space it goes in, which is a tap spent
            // on saying where before you can say what.
            if body.height >= 4 {
                let heading = Rect::new(body.x, body.y, body.width, 1);
                let button = if touch { 3u16 } else { 2u16 };
                view.sidebar_new_project =
                    Some(Rect::new(heading.right() - button, heading.y, button, 1));
                view.sidebar_heading = Some(heading);
                if let Some(rect) = view.sidebar_new_project {
                    view.hits.push(HitRegion {
                        rect,
                        target: HitTarget::NewProject,
                    });
                }
                body = Rect::new(body.x, body.y + 1, body.width, body.height - 1);
            }
        }
        view.sidebar_body = Some(body);
        if overlay {
            // The tree covers the screen, so there is no divider to drag and
            // nothing of the terminal to see. `content` still describes the
            // grid the terminal *has* — the whole width, since there is no
            // sidebar beside it once the tree is dismissed.
            //
            // It used to be zeroed here, which read as "this renderer has no
            // viewport": a phone held in portrait is exactly the case where the
            // tree covers the screen, so a phone could never take the size from
            // whatever else was attached, and the terminal it opened was still
            // the other device's shape. Panes are skipped below rather than
            // sized to nothing.
            view.tab_bar = Rect::new(area.x, area.y, 0, header_height);
            view.content = Rect::new(area.x, middle_y, area.width, middle_height);
        } else {
            view.sidebar_resize = Some(resize);
            view.hits.push(HitRegion {
                rect: resize,
                target: HitTarget::SidebarResize,
            });
            view.tab_bar = Rect::new(
                resize.right(),
                area.y,
                area.right().saturating_sub(resize.right()),
                header_height,
            );
            view.content = Rect::new(
                resize.right(),
                middle_y,
                area.right().saturating_sub(resize.right()),
                middle_height,
            );
        }
    } else {
        view.tab_bar = Rect::new(area.x, area.y, area.width, header_height);
        view.content = Rect::new(area.x, middle_y, area.width, middle_height);
        let toggle_width = if touch { 6 } else { 4 };
        if header_height > 0 && area.width >= toggle_width {
            let toggle = Rect::new(area.x, area.y, toggle_width, header_height);
            view.sidebar_toggle = Some(toggle);
            view.hits.push(HitRegion {
                rect: toggle,
                target: HitTarget::SidebarToggle,
            });
        }
    }

    if let Some(body) = view.sidebar_body {
        compute_tree(model, ui, body, &mut view);
    }
    let tabs_area = if view.sidebar.is_none() {
        view.sidebar_toggle.map_or(view.tab_bar, |toggle| {
            Rect::new(
                toggle.right(),
                view.tab_bar.y,
                view.tab_bar.right().saturating_sub(toggle.right()),
                view.tab_bar.height,
            )
        })
    } else {
        view.tab_bar
    };
    compute_tabs(model, tabs_area, &mut view);
    // Skipped only when the tree is genuinely on top of them. Testing `overlay`
    // alone was wrong: a narrow screen with the tree *dismissed* is the normal
    // way to use a phone zoomed in, and it left the terminal undrawn — a tab
    // bar over an empty screen. `content` still says how big the panes are
    // either way, which is what gets advertised to the server.
    if let (Some(tab), false) = (tab, tree_covers_terminal(&view)) {
        compute_panes(tab, ui, view.content, &mut view);
    }

    if view.status.width >= 8 {
        let detach = Rect::new(
            view.status.right() - 7,
            view.status.y,
            7,
            view.status.height,
        );
        view.hits.push(HitRegion {
            rect: detach,
            target: HitTarget::Detach,
        });
    }
    if model.attention_count() > 0 && view.status.width >= 18 {
        let attention = Rect::new(
            view.status.right() - 17,
            view.status.y,
            9,
            view.status.height,
        );
        view.hits.push(HitRegion {
            rect: attention,
            target: HitTarget::Attention,
        });
    }

    compute_overlays(ui, &mut view);
    view
}

/// Whether the tree is drawn *on top of* the terminal rather than beside it.
///
/// Both halves matter. Narrow alone is not enough — a narrow screen with the
/// tree dismissed is the ordinary way a phone is used once it is zoomed in, and
/// treating that as "covered" leaves a tab bar over an empty screen.
fn tree_covers_terminal(view: &ViewLayout) -> bool {
    view.class == ViewClass::Narrow && view.sidebar.is_some()
}

fn compute_tree(model: &AppModel, ui: &UiState, body: Rect, view: &mut ViewLayout) {
    #[derive(Clone)]
    struct Candidate {
        key: TreeNodeKey,
        label: String,
        depth: u16,
        selected: bool,
        expandable: bool,
        collapsed: bool,
        status: Option<&'static str>,
        pinned: bool,
    }

    let touch = ui.touch_targets.unwrap_or(view.class == ViewClass::Narrow);
    let selected = model.selected_key();
    let mut all = Vec::new();
    for (domain_name, snapshot) in model.domains() {
        let domain_key = TreeNodeKey::Domain(domain_name.to_string());
        let domain_collapsed = ui.tree_collapsed.contains(&domain_key);
        all.push(Candidate {
            key: domain_key.clone(),
            label: domain_name.to_string(),
            depth: 0,
            selected: false,
            expandable: true,
            collapsed: domain_collapsed,
            status: None,
            pinned: false,
        });
        if domain_collapsed {
            continue;
        }

        for space in &snapshot.spaces {
            let space_key = TreeNodeKey::Space {
                domain_name: domain_name.to_string(),
                space_id: space.id.clone(),
            };
            let space_collapsed = ui.tree_collapsed.contains(&space_key);
            let projects = snapshot
                .projects
                .iter()
                .filter(|project| project.space_id == space.id)
                .collect::<Vec<_>>();
            all.push(Candidate {
                key: space_key.clone(),
                label: space.name.clone(),
                depth: 1,
                selected: false,
                expandable: !projects.is_empty(),
                collapsed: space_collapsed,
                status: None,
                pinned: false,
            });
            if space_collapsed {
                continue;
            }

            for project in projects {
                let project_key = TreeNodeKey::Project {
                    domain_name: domain_name.to_string(),
                    project_id: project.id.clone(),
                };
                let project_collapsed = ui.tree_collapsed.contains(&project_key);
                all.push(Candidate {
                    key: project_key.clone(),
                    label: project.name.clone(),
                    depth: 2,
                    selected: false,
                    expandable: !project.threads.is_empty(),
                    collapsed: project_collapsed,
                    status: None,
                    pinned: false,
                });
                if project_collapsed {
                    continue;
                }
                for thread in &project.threads {
                    let key = ThreadKey {
                        domain_name: domain_name.to_string(),
                        server_id: snapshot.server_id.clone(),
                        thread_id: thread.id.clone(),
                    };
                    let status = model.row(&key).map(|row| row.status_marker());
                    all.push(Candidate {
                        key: TreeNodeKey::Thread(key.clone()),
                        label: thread.name.clone(),
                        depth: 3,
                        selected: selected == Some(&key),
                        expandable: false,
                        collapsed: false,
                        status,
                        pinned: thread.is_pinned,
                    });
                }
            }
        }
    }

    let available = body.height as usize;
    if available == 0 || all.is_empty() {
        return;
    }
    let selected_index = all.iter().position(|line| line.selected).unwrap_or(0);
    let requested = ui.sidebar_scroll.min(all.len().saturating_sub(1));
    let start = if selected_index < requested || selected_index >= requested + available {
        selected_index
            .saturating_sub(available / 2)
            .min(all.len().saturating_sub(available))
    } else {
        requested.min(all.len().saturating_sub(available))
    };

    // Every row's buttons are the same width so a column of them lines up, and
    // a finger gets a wider one than a mouse needs.
    let button = if touch { 3u16 } else { 2u16 };
    for (offset, line) in all.into_iter().skip(start).take(available).enumerate() {
        let rect = Rect::new(body.x, body.y + offset as u16, body.width, 1);
        view.hits.push(HitRegion {
            rect,
            target: HitTarget::Tree(line.key.clone()),
        });

        // Every row carries the same buttons whether or not it is selected.
        // Showing them only on the selected row meant the sidebar's controls
        // appeared and vanished as the selection moved, and no row could be
        // read as "this is what a row can do".
        //
        // Which buttons: one that opens everything else, plus — where a row has
        // an obvious child — one that adds it. A per-row pin and a per-row
        // delete used to be here and are gone: two glyphs whose meaning had to
        // be guessed, one of them a red ✕ repeated down a list of names, which
        // reads as a warning rather than as a control. Both still live in the ⋯
        // menu, where they are spelled out in words.
        let wanted: &[TreeAction] = match &line.key {
            TreeNodeKey::Domain(_) => &[TreeAction::Menu],
            TreeNodeKey::Space { .. } => &[TreeAction::Add, TreeAction::Menu],
            TreeNodeKey::Project { .. } => &[TreeAction::Add, TreeAction::Menu],
            TreeNodeKey::Thread(_) => &[TreeAction::Menu],
        };
        let mut actions = Vec::new();
        if rect.width >= MIN_LABEL_COLUMNS + button * wanted.len() as u16 {
            let mut edge = rect.right();
            for kind in wanted.iter().rev() {
                edge -= button;
                let hit = Rect::new(edge, rect.y, button, rect.height);
                view.hits.push(HitRegion {
                    rect: hit,
                    target: HitTarget::TreeAction(line.key.clone(), *kind),
                });
                actions.push((hit, *kind));
            }
            // Laid out right to left; handed on left to right, so the order a
            // caller sees is the order the eye sees.
            actions.reverse();
        }
        if line.expandable {
            let toggle_x = rect.x.saturating_add(line.depth.saturating_mul(2));
            if toggle_x < rect.right() {
                view.hits.push(HitRegion {
                    rect: Rect::new(toggle_x, rect.y, 2.min(rect.right() - toggle_x), 1),
                    target: HitTarget::TreeToggle(line.key.clone()),
                });
            }
        }
        view.tree_lines.push(TreeLine {
            rect,
            key: line.key,
            label: line.label,
            depth: line.depth,
            selected: line.selected,
            expandable: line.expandable,
            collapsed: line.collapsed,
            status: line.status,
            pinned: line.pinned,
            actions,
        });
    }
}

/// Columns a tree row keeps for its own name before it will host any buttons.
const MIN_LABEL_COLUMNS: u16 = 12;

fn compute_tabs(model: &AppModel, area: Rect, view: &mut ViewLayout) {
    if area.height == 0 || area.width == 0 {
        return;
    }
    // Laid out right to left in the order they are worth keeping, so a bar too
    // narrow for all of them loses the splits before it loses the menu that can
    // still reach everything.
    let width = tab_bar_control_width(area.height);
    let mut edge = area.right();
    for control in TAB_BAR_CONTROLS {
        if edge.saturating_sub(area.x) < MIN_TAB_COLUMNS + width {
            break;
        }
        edge -= width;
        let rect = Rect::new(edge, area.y, width, area.height);
        view.hits.push(HitRegion {
            rect,
            target: HitTarget::TabControl(control),
        });
        view.tab_controls.push((rect, control));
    }
    let tabs_right = edge;
    let selected = model.selected_tab().map(|tab| tab.tab_id);
    let mut x = area.x;
    for (index, tab) in model.tabs_for_selected_thread().iter().enumerate() {
        if x >= tabs_right {
            break;
        }
        let title = if tab.title.trim().is_empty() {
            "shell"
        } else {
            tab.title.trim()
        };
        let label = format!(" {}:{} ", index + 1, title);
        let desired = unicode_column_width(&label, None).min(u16::MAX as usize) as u16;
        // Room for the tab's own close button, but only where the name still
        // has somewhere to go afterwards.
        let close_width = tab_bar_control_width(area.height);
        let want_close = tabs_right.saturating_sub(x) >= desired + close_width;
        let width = (desired + if want_close { close_width } else { 0 }).min(tabs_right - x);
        if width == 0 {
            break;
        }
        let rect = Rect::new(x, area.y, width, area.height);
        let close = want_close.then(|| {
            let rect = Rect::new(rect.right() - close_width, rect.y, close_width, rect.height);
            view.hits.push(HitRegion {
                rect,
                target: HitTarget::CloseTab(tab.tab_id),
            });
            rect
        });
        view.tabs.push(TabLine {
            rect,
            label,
            selected: selected == Some(tab.tab_id),
            close,
        });
        // Pushed before the close button above so the narrower target wins the
        // hit test — `hit` searches from the back.
        view.hits.insert(
            view.hits.len().saturating_sub(usize::from(close.is_some())),
            HitRegion {
                rect,
                target: HitTarget::Tab(tab.tab_id),
            },
        );
        x = x.saturating_add(width);
    }
}

/// Trailing tab-bar controls, most worth keeping first. A bar too narrow for
/// all of them drops from the end, so the menu — which can still reach
/// everything below it — is the last to go.
pub const TAB_BAR_CONTROLS: [TabBarControl; 2] = [TabBarControl::Menu, TabBarControl::NewTab];

/// Columns the tab bar keeps for tabs before it will host any control.
const MIN_TAB_COLUMNS: u16 = 10;

/// Width of one tab-bar control. A bar with room for two rows is a bar meant to
/// be touched, so its buttons widen to match their new height.
pub fn tab_bar_control_width(height: u16) -> u16 {
    if height > 1 {
        4
    } else {
        3
    }
}

/// Sizes at which a pane can afford to give space to chrome.
///
/// Both of these are paid for out of the grid, and on a small pane that is the
/// difference between a shell prompt fitting on one line and wrapping into
/// nonsense. A frame costs two columns and two rows, a scrollbar one column —
/// worth it on a desktop split, ruinous on a phone that has just been split in
/// half. The thresholds are what "there is room to spare" means here.
const SCROLLBAR_MIN_COLUMNS: u16 = 34;
/// A frame is cheaper than it looks, because the pane's nav bar is drawn *on*
/// its top edge rather than under it — the way a titled box works. So the frame
/// costs one row (the bottom edge) and two columns beyond what the bar already
/// spends, not two rows and two columns. That is affordable on a phone, which
/// is why these are low.
/// Tied to what the nav bar already needs, plus the one row and two columns the
/// frame adds. A pane that can afford a bar can afford a frame, so the two
/// never disagree — a split where one half is framed and the other is not reads
/// as broken rather than as economical.
const BORDER_MIN_COLUMNS: u16 = MIN_LABEL_COLUMNS + 2;
const BORDER_MIN_ROWS: u16 = PANE_NAV_MIN_ROWS + 1;

/// Rows a pane keeps for its grid before it will spend one on its own nav bar.
///
/// Deliberately almost nothing. This started at eight, which on a phone meant
/// the shorter half of a split silently lost its bar — and with it every
/// control that pane had, since the bar is the only place a finger can reach
/// zoom, split, close and new-terminal. A pane you cannot act on is worse than
/// a pane one row shorter, and a split where only one half has a bar reads as
/// broken. Two rows of grid is the floor; below that the pane is unusable for
/// reasons the bar has nothing to do with.
const PANE_NAV_MIN_ROWS: u16 = 3;

/// Widest a level-2 tab may grow, so one long title cannot take the row from
/// the rest of the stack.
const PANE_NAV_TAB_MAX_COLUMNS: u16 = 18;

fn compute_panes(tab: &Arc<Tab>, ui: &UiState, area: Rect, view: &mut ViewLayout) {
    if area.width == 0 || area.height == 0 {
        return;
    }
    let touch = ui.touch_targets.unwrap_or(view.class == ViewClass::Narrow);
    let panes = tab.iter_panes();
    let pane_count = panes.len();
    for positioned in panes {
        let left = positioned.left.min(area.width as usize);
        let top = positioned.top.min(area.height as usize);
        let width = positioned
            .width
            .min(area.width.saturating_sub(left as u16) as usize) as u16;
        let height = positioned
            .height
            .min(area.height.saturating_sub(top as u16) as usize) as u16;
        let rect = Rect::new(
            area.x.saturating_add(left as u16),
            area.y.saturating_add(top as u16),
            width,
            height,
        );
        if rect.is_empty() {
            continue;
        }
        let collapsed = (pane_count > 1 && rect.height < PANE_NAV_MIN_ROWS).then_some(rect);
        // Only where there is another pane to be told apart from. A lone pane
        // already has the window's own edges; framing it spends a row and two
        // columns to draw a line around the only thing on screen.
        let framed = ui.pane_borders
            && pane_count > 1
            && rect.width >= BORDER_MIN_COLUMNS
            && rect.height >= BORDER_MIN_ROWS;
        let border = framed.then_some(rect);
        let pane_id = positioned.pane.pane_id();
        // The bar sits *on* the frame's top edge, between its corners, so a
        // framed pane pays one row for the bar and one for the bottom edge
        // rather than one for each of three lines.
        let bar_area = if framed {
            Rect::new(rect.x + 1, rect.y, rect.width - 2, rect.height)
        } else {
            rect
        };
        // Above the grid, never over it, for the same reason as the scrollbar:
        // the row it takes is a row the terminal is then told it does not have.
        let stack = tab.pane_stack_tabs(pane_id);
        let nav = if collapsed.is_some() && ui.pane_nav_bar {
            compute_collapsed_pane_nav(pane_id, &stack, positioned.is_zoomed, touch, bar_area, view)
        } else {
            compute_pane_nav(
                pane_id,
                &stack,
                positioned.is_zoomed,
                ui.pane_nav_bar,
                touch,
                bar_area,
                view,
            )
        };
        let bar_rows = nav.as_ref().map_or(0, |nav| nav.rect.height);
        let rect = if framed {
            // The top edge is already spent on the bar (or is a plain edge when
            // there is no bar), so the grid starts one row down either way.
            Rect::new(
                rect.x + 1,
                rect.y + 1,
                rect.width - 2,
                rect.height.saturating_sub(2),
            )
        } else {
            Rect::new(
                rect.x,
                rect.y + bar_rows,
                rect.width,
                rect.height - bar_rows,
            )
        };
        // The scrollbar lives beside the grid, never over it: the column it
        // takes is a column the terminal is then told it does not have, so a
        // program's own right margin still lands where it drew it.
        let scrollbar = (ui.pane_scrollbars && rect.width >= SCROLLBAR_MIN_COLUMNS).then(|| {
            let rect = Rect::new(rect.right() - 1, rect.y, 1, rect.height);
            view.hits.push(HitRegion {
                rect,
                target: HitTarget::Scrollbar(pane_id),
            });
            rect
        });
        let rect = if scrollbar.is_some() {
            Rect::new(rect.x, rect.y, rect.width - 1, rect.height)
        } else {
            rect
        };
        view.panes.push(PaneView {
            pane_id,
            rect,
            scrollbar,
            nav,
            border,
            collapsed,
        });
        view.hits.push(HitRegion {
            rect,
            target: HitTarget::Pane(pane_id),
        });
    }

    for split in tab.iter_splits() {
        let Some(rect) =
            clipped_split_rect(area, split.left, split.top, split.size, split.direction)
        else {
            continue;
        };
        view.splits.push(SplitView {
            index: split.index,
            rect,
            direction: split.direction,
        });
        view.hits.push(HitRegion {
            rect,
            target: HitTarget::Split(split.index),
        });
    }
}

/// Lay out one pane's own strip, and say how much of its rectangle is left.
///
/// Tools are placed right to left in the order they are worth keeping, so a
/// pane too narrow for all four drops the splits first. That is the opposite of
/// the desktop's order and deliberately so: on a phone, a pane that has just
/// been split needs to be able to un-split itself by zooming far more than it
/// needs to split again.
/// A two-row split cannot afford the normal pane bar and its promised two rows
/// of grid. Spend one row on a compact, zoomable identity and leave the other
/// as the PTY grid. A one-row split keeps its only row as the grid, but the
/// renderer still replaces its output with an explicit collapsed marker.
fn compute_collapsed_pane_nav(
    pane_id: PaneId,
    stack: &[PaneStackTab],
    zoomed: bool,
    touch: bool,
    rect: Rect,
    view: &mut ViewLayout,
) -> Option<PaneNav> {
    let button = if touch { 3u16 } else { 2u16 };
    if rect.height < 2 || rect.width < MIN_LABEL_COLUMNS.saturating_add(button) {
        return None;
    }
    let bar = Rect::new(rect.x, rect.y, rect.width, 1);
    let zoom = PaneTool::Zoom { zoomed };
    let tool = Rect::new(bar.right() - button, bar.y, button, 1);
    view.hits.push(HitRegion {
        rect: tool,
        target: HitTarget::PaneTool(pane_id, zoom),
    });

    let tabs = stack
        .iter()
        .find(|entry| entry.is_active)
        .or_else(|| stack.first())
        .map(|entry| {
            let title = if entry.title.trim().is_empty() {
                "shell"
            } else {
                entry.title.trim()
            };
            let tab = Rect::new(bar.x, bar.y, bar.width - button, 1);
            view.hits.push(HitRegion {
                rect: tab,
                target: HitTarget::PaneNavTab(entry.pane_id),
            });
            PaneNavTab {
                rect: tab,
                pane_id: entry.pane_id,
                label: format!(" … {title} "),
                active: true,
                close: None,
            }
        })
        .into_iter()
        .collect();

    Some(PaneNav {
        rect: bar,
        tabs,
        tools: vec![(tool, zoom)],
    })
}

fn compute_pane_nav(
    pane_id: PaneId,
    stack: &[PaneStackTab],
    zoomed: bool,
    enabled: bool,
    touch: bool,
    rect: Rect,
    view: &mut ViewLayout,
) -> Option<PaneNav> {
    if !enabled || rect.height < PANE_NAV_MIN_ROWS || rect.width < MIN_LABEL_COLUMNS {
        return None;
    }
    let bar = Rect::new(rect.x, rect.y, rect.width, 1);
    let button = if touch { 3u16 } else { 2u16 };

    let zoom = PaneTool::Zoom { zoomed };
    let priority = [
        zoom,
        PaneTool::NewTab,
        PaneTool::SplitRight,
        PaneTool::SplitDown,
    ];
    let order = [
        zoom,
        PaneTool::SplitRight,
        PaneTool::SplitDown,
        PaneTool::NewTab,
    ];
    let room = (bar.width.saturating_sub(MIN_LABEL_COLUMNS) / button) as usize;
    let kept = &priority[..room.min(priority.len())];

    let mut tools = Vec::new();
    let mut edge = bar.right();
    for kind in order.iter().rev() {
        if !kept.contains(kind) {
            continue;
        }
        edge -= button;
        let hit = Rect::new(edge, bar.y, button, bar.height);
        view.hits.push(HitRegion {
            rect: hit,
            target: HitTarget::PaneTool(pane_id, *kind),
        });
        tools.push((hit, *kind));
    }
    tools.reverse();

    let mut tabs = Vec::new();
    let mut x = bar.x;
    for entry in stack {
        if x >= edge {
            break;
        }
        let title = if entry.title.trim().is_empty() {
            "shell"
        } else {
            entry.title.trim()
        };
        // No icon before the name. `▯` was there to mark a terminal, and on a
        // phone whose font does not carry U+25AF it drew a tofu box that read
        // as a bug. The name is doing the naming already.
        let label = format!(" {title} ");
        let desired = unicode_column_width(&label, None)
            .min(u16::MAX as usize)
            .min(PANE_NAV_TAB_MAX_COLUMNS as usize) as u16;
        let want_close = edge.saturating_sub(x) >= desired + button;
        let width = (desired + if want_close { button } else { 0 }).min(edge - x);
        if width == 0 {
            break;
        }
        let rect = Rect::new(x, bar.y, width, bar.height);
        view.hits.push(HitRegion {
            rect,
            target: HitTarget::PaneNavTab(entry.pane_id),
        });
        // Pushed after the tab so the narrower target wins the hit test —
        // `hit` searches from the back.
        let close = want_close.then(|| {
            let rect = Rect::new(rect.right() - button, rect.y, button, rect.height);
            view.hits.push(HitRegion {
                rect,
                target: HitTarget::PaneNavClose(entry.pane_id),
            });
            rect
        });
        tabs.push(PaneNavTab {
            rect,
            pane_id: entry.pane_id,
            label,
            active: entry.is_active,
            close,
        });
        x = x.saturating_add(width);
    }

    Some(PaneNav {
        rect: bar,
        tabs,
        tools,
    })
}

fn clipped_split_rect(
    area: Rect,
    left: usize,
    top: usize,
    size: usize,
    direction: SplitDirection,
) -> Option<Rect> {
    let left = left.min(area.width as usize);
    let top = top.min(area.height as usize);
    let remaining_width = area.width as usize - left;
    let remaining_height = area.height as usize - top;
    if remaining_width == 0 || remaining_height == 0 || size == 0 {
        return None;
    }

    let (width, height) = match direction {
        SplitDirection::Horizontal => (1, size.min(remaining_height)),
        SplitDirection::Vertical => (size.min(remaining_width), 1),
    };
    Some(Rect::new(
        area.x.saturating_add(left as u16),
        area.y.saturating_add(top as u16),
        width as u16,
        height as u16,
    ))
}

fn compute_overlays(ui: &UiState, view: &mut ViewLayout) {
    if let Some(menu) = &ui.context_menu {
        let width = menu
            .entries
            .iter()
            .map(|entry| unicode_column_width(&entry.label, None) as u16 + 4)
            .max()
            .unwrap_or(12)
            .clamp(12, view.screen.width.max(12));
        let height = (menu.entries.len() as u16 + 2).min(view.screen.height);
        let x = menu.anchor.0.min(view.screen.right().saturating_sub(width));
        let y = menu
            .anchor
            .1
            .min(view.screen.bottom().saturating_sub(height));
        let rect = Rect::new(x, y, width, height);
        view.context_menu = Some(rect);
        for (index, _) in menu.entries.iter().enumerate() {
            let row = rect.y.saturating_add(1 + index as u16);
            if row >= rect.bottom().saturating_sub(1) {
                break;
            }
            view.hits.push(HitRegion {
                rect: Rect::new(rect.x + 1, row, rect.width.saturating_sub(2), 1),
                target: HitTarget::ContextMenu(index),
            });
        }
    }

    if ui.mode == crate::state::AppMode::Connections {
        let width = view.screen.width.saturating_sub(4).clamp(1, 72);
        let height = (ui.connections.len() as u16 + 4)
            .min(view.screen.height.saturating_sub(2).max(1))
            .max(5.min(view.screen.height));
        let rect = Rect::new(
            view.screen.x + view.screen.width.saturating_sub(width) / 2,
            view.screen.y + view.screen.height.saturating_sub(height) / 2,
            width,
            height,
        );
        view.dialog = Some(rect);
        for (index, _) in ui.connections.iter().enumerate() {
            let row = rect.y.saturating_add(2 + index as u16);
            if row >= rect.bottom().saturating_sub(1) {
                break;
            }
            view.hits.push(HitRegion {
                rect: Rect::new(rect.x + 1, row, rect.width.saturating_sub(2), 1),
                target: HitTarget::Connection(index),
            });
        }
    } else if ui.mode == crate::state::AppMode::Settings {
        let width = view.screen.width.saturating_sub(4).clamp(1, 64);
        let height = 10.min(view.screen.height.saturating_sub(2).max(1));
        let rect = Rect::new(
            view.screen.x + view.screen.width.saturating_sub(width) / 2,
            view.screen.y + view.screen.height.saturating_sub(height) / 2,
            width,
            height,
        );
        view.dialog = Some(rect);
        for index in 0..5 {
            let row = rect.y.saturating_add(2 + index as u16);
            if row < rect.bottom().saturating_sub(1) {
                view.hits.push(HitRegion {
                    rect: Rect::new(rect.x + 1, row, rect.width.saturating_sub(2), 1),
                    target: HitTarget::Setting(index),
                });
            }
        }
    } else if ui.prompt.is_some() || ui.confirmation.is_some() {
        let width = view.screen.width.saturating_sub(4).clamp(1, 64);
        let height = if ui.confirmation.is_some() { 8 } else { 6 }
            .min(view.screen.height.saturating_sub(2).max(1));
        let rect = Rect::new(
            view.screen.x + view.screen.width.saturating_sub(width) / 2,
            view.screen.y + view.screen.height.saturating_sub(height) / 2,
            width,
            height,
        );
        view.dialog = Some(rect);
        if ui.confirmation.is_some() && rect.height >= 3 {
            let button_y = rect.bottom().saturating_sub(2);
            let half = rect.width.saturating_sub(2) / 2;
            view.hits.push(HitRegion {
                rect: Rect::new(rect.x + 1, button_y, half, 1),
                target: HitTarget::DialogConfirm,
            });
            view.hits.push(HitRegion {
                rect: Rect::new(rect.x + 1 + half, button_y, rect.width - 2 - half, 1),
                target: HitTarget::DialogCancel,
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::{ConnectionItem, ConnectionStatus};
    use codec::{ThinkTermSessionProject, ThinkTermSessionSpace, ThinkTermSessionState};

    fn stack(titles: &[&str], active: usize) -> Vec<PaneStackTab> {
        titles
            .iter()
            .enumerate()
            .map(|(index, title)| PaneStackTab {
                pane_id: index + 1,
                title: (*title).to_string(),
                is_active: index == active,
            })
            .collect()
    }

    fn nav(rect: Rect, stack: &[PaneStackTab], zoomed: bool, touch: bool) -> (PaneNav, ViewLayout) {
        let mut view = ViewLayout::default();
        let nav = compute_pane_nav(1, stack, zoomed, true, touch, rect, &mut view)
            .expect("a pane this size has room for its own bar");
        (nav, view)
    }

    #[test]
    fn a_pane_bar_carries_its_stack_on_the_left_and_its_tools_on_the_right() {
        let (bar, view) = nav(
            Rect::new(0, 0, 60, 20),
            &stack(&["shell", "vim"], 0),
            false,
            false,
        );
        assert_eq!(bar.rect, Rect::new(0, 0, 60, 1), "one row, full width");

        let tools = bar.tools.iter().map(|(_, t)| *t).collect::<Vec<_>>();
        assert_eq!(
            tools,
            vec![
                PaneTool::Zoom { zoomed: false },
                PaneTool::SplitRight,
                PaneTool::SplitDown,
                PaneTool::NewTab,
            ],
            "drawn left to right in a fixed order"
        );
        assert!(
            bar.tools.windows(2).all(|w| w[0].0.x < w[1].0.x),
            "and positioned in that order"
        );

        assert_eq!(bar.tabs.len(), 2);
        assert!(bar.tabs[0].rect.right() <= bar.tabs[1].rect.x);
        assert!(
            bar.tabs[1].rect.right() <= bar.tools[0].0.x,
            "stack tabs stop where the tools begin"
        );

        // Every terminal in the stack offers to close, not only the one showing.
        assert!(bar.tabs[0].active && bar.tabs[0].close.is_some());
        assert!(!bar.tabs[1].active && bar.tabs[1].close.is_some());

        let close = bar.tabs[0].close.unwrap();
        assert_eq!(
            view.hit(close.x, close.y),
            Some(&HitTarget::PaneNavClose(1)),
            "the close button wins over the tab it sits in"
        );
        assert_eq!(
            view.hit(bar.tabs[1].rect.x, 0),
            Some(&HitTarget::PaneNavTab(2)),
            "and the pane behind is one tap away"
        );
        for (rect, tool) in &bar.tools {
            assert_eq!(
                view.hit(rect.x, rect.y),
                Some(&HitTarget::PaneTool(1, *tool))
            );
        }
    }

    #[test]
    fn a_narrowing_pane_keeps_zoom_longest_and_drops_the_splits_first() {
        let entries = stack(&["shell"], 0);
        let kept = |width: u16| {
            nav(Rect::new(0, 0, width, 20), &entries, false, false)
                .0
                .tools
                .iter()
                .map(|(_, tool)| *tool)
                .collect::<Vec<_>>()
        };
        let zoom = PaneTool::Zoom { zoomed: false };
        assert_eq!(
            kept(60),
            vec![
                zoom,
                PaneTool::SplitRight,
                PaneTool::SplitDown,
                PaneTool::NewTab
            ]
        );
        assert_eq!(kept(19), vec![zoom, PaneTool::SplitRight, PaneTool::NewTab]);
        assert_eq!(kept(17), vec![zoom, PaneTool::NewTab]);
        assert_eq!(kept(15), vec![zoom]);
        assert_eq!(kept(13), Vec::new());
    }

    #[test]
    fn the_zoom_button_says_which_way_it_goes() {
        let entries = stack(&["shell"], 0);
        let glyph = |zoomed| {
            nav(Rect::new(0, 0, 60, 20), &entries, zoomed, false)
                .0
                .tools[0]
                .1
                .glyph()
        };
        assert_eq!(glyph(false), "⤢");
        assert_eq!(glyph(true), "⤡");
    }

    #[test]
    fn a_pane_with_no_row_to_spare_keeps_all_of_them_for_its_grid() {
        let entries = stack(&["shell"], 0);
        let mut view = ViewLayout::default();
        assert!(
            compute_pane_nav(
                1,
                &entries,
                false,
                true,
                false,
                Rect::new(0, 0, 60, 2),
                &mut view
            )
            .is_none(),
            "a pane with no grid left to speak of goes without"
        );

        assert!(
            compute_pane_nav(
                1,
                &entries,
                false,
                true,
                false,
                Rect::new(0, 0, 11, 20),
                &mut view
            )
            .is_none(),
            "and so does one with no room for a name"
        );
        assert!(
            compute_pane_nav(
                1,
                &entries,
                false,
                false,
                false,
                Rect::new(0, 0, 60, 20),
                &mut view
            )
            .is_none(),
            "turning it off leaves nothing behind"
        );
        assert!(view.hits.is_empty(), "and no targets either");

        // The short half of a split is exactly the pane that used to lose its
        // bar, and with it every control it had.
        let mut short = ViewLayout::default();
        assert!(
            compute_pane_nav(
                1,
                &entries,
                false,
                true,
                false,
                Rect::new(0, 0, 60, 7),
                &mut short
            )
            .is_some(),
            "a seven-row pane keeps its controls"
        );
    }

    #[test]
    fn a_two_row_collapsed_pane_keeps_a_zoomable_identity() {
        let entries = stack(&["shell"], 0);
        let mut view = ViewLayout::default();
        let nav = compute_collapsed_pane_nav(
            1,
            &entries,
            false,
            false,
            Rect::new(4, 7, 30, 2),
            &mut view,
        )
        .expect("two rows can show a compact bar and one grid row");
        assert_eq!(nav.rect, Rect::new(4, 7, 30, 1));
        assert_eq!(nav.tabs[0].label, " … shell ");
        assert_eq!(nav.tools[0].1, PaneTool::Zoom { zoomed: false });
        assert_eq!(
            view.hit(nav.tools[0].0.x, nav.tools[0].0.y),
            Some(&HitTarget::PaneTool(1, PaneTool::Zoom { zoomed: false }))
        );
    }

    #[test]
    fn touch_targets_widen_the_pane_buttons_without_costing_a_second_row() {
        let entries = stack(&["shell"], 0);
        let (mouse, _) = nav(Rect::new(0, 0, 60, 20), &entries, false, false);
        let (touch, _) = nav(Rect::new(0, 0, 60, 20), &entries, false, true);
        assert_eq!(mouse.rect.height, 1);
        assert_eq!(touch.rect.height, 1);
        assert_eq!(mouse.tools[0].0.width, 2);
        assert_eq!(touch.tools[0].0.width, 3);
    }

    fn model_with_threads() -> AppModel {
        let mut model = AppModel::default();
        model.apply_snapshot(
            "server",
            ThinkTermSessionState {
                server_id: "runtime".into(),
                generation: 1,
                spaces: vec![ThinkTermSessionSpace {
                    id: "space".into(),
                    name: "Space".into(),
                    ..Default::default()
                }],
                projects: vec![ThinkTermSessionProject {
                    id: "project".into(),
                    space_id: "space".into(),
                    name: "Project".into(),
                    threads: ["Thread 1", "Thread 2", "Thread 3"]
                        .into_iter()
                        .enumerate()
                        .map(|(index, name)| codec::ThinkTermSessionThread {
                            id: format!("t{index}"),
                            project_id: "project".into(),
                            name: name.into(),
                            ..Default::default()
                        })
                        .collect(),
                    ..Default::default()
                }],
                ..Default::default()
            },
        );
        model
    }

    #[test]
    fn view_classes_are_deterministic() {
        assert_eq!(classify(64, 64), ViewClass::Narrow);
        assert_eq!(classify(65, 64), ViewClass::Compact);
        assert_eq!(classify(87, 64), ViewClass::Compact);
        assert_eq!(classify(88, 64), ViewClass::Desktop);
    }

    /// A phone reporting 88 columns is not a desktop, and the only way to say
    /// so is to raise the threshold; the classes have to move with it.
    #[test]
    fn raising_the_narrow_threshold_moves_every_class_with_it() {
        assert_eq!(classify(88, 64), ViewClass::Desktop);
        assert_eq!(classify(88, 90), ViewClass::Narrow);
        assert_eq!(classify(100, 90), ViewClass::Compact);
        assert_eq!(classify(114, 90), ViewClass::Desktop);
    }

    #[test]
    fn top_bar_exposes_new_tab_and_main_menu_mouse_targets() {
        let model = model_with_threads();
        let ui = UiState::new(String::new());
        let view = compute_view(Rect::new(0, 0, 100, 30), &model, &ui, None);
        assert_eq!(
            view.hit(98, 0),
            Some(&HitTarget::TabControl(TabBarControl::Menu))
        );
        assert_eq!(
            view.hit(95, 0),
            Some(&HitTarget::TabControl(TabBarControl::NewTab))
        );
        assert_eq!(view.hit(1, 0), Some(&HitTarget::SidebarToggle));
    }

    #[test]
    fn every_settings_row_has_an_exact_modal_hit() {
        let model = model_with_threads();
        let mut ui = UiState::new(String::new());
        ui.mode = crate::state::AppMode::Settings;
        let view = compute_view(Rect::new(0, 0, 100, 30), &model, &ui, None);
        let dialog = view.dialog.unwrap();
        for index in 0..5 {
            assert_eq!(
                view.hit(dialog.x + 2, dialog.y + 2 + index as u16),
                Some(&HitTarget::Setting(index))
            );
        }
    }

    #[test]
    fn each_thread_row_hits_only_its_own_stable_id() {
        let model = model_with_threads();
        let ui = UiState::new(String::new());
        let view = compute_view(Rect::new(0, 0, 100, 30), &model, &ui, None);
        let threads = view
            .tree_lines
            .iter()
            .filter(|line| matches!(line.key, TreeNodeKey::Thread(_)))
            .collect::<Vec<_>>();
        assert_eq!(threads.len(), 3);
        for (index, line) in threads.iter().enumerate() {
            // Probe the name, not the far edge: the row's right-hand columns
            // now belong to its own buttons.
            let hit = view.hit(line.rect.x, line.rect.y);
            let HitTarget::Tree(TreeNodeKey::Thread(key)) = hit.unwrap() else {
                panic!("thread row did not resolve to a thread target")
            };
            assert_eq!(key.thread_id, format!("t{index}"));
        }
    }

    /// The commands that used to need a right click — which a touchscreen does
    /// not have — are now on the row itself. Only the row being worked on
    /// carries them, so the list stays a list.
    #[test]
    fn every_row_offers_the_same_commands_as_buttons() {
        let model = model_with_threads();
        let ui = UiState::new(String::new());
        let view = compute_view(Rect::new(0, 0, 100, 30), &model, &ui, None);

        for line in &view.tree_lines {
            let expected = match &line.key {
                TreeNodeKey::Domain(_) | TreeNodeKey::Thread(_) => vec![TreeAction::Menu],
                TreeNodeKey::Space { .. } | TreeNodeKey::Project { .. } => {
                    vec![TreeAction::Add, TreeAction::Menu]
                }
            };
            assert_eq!(
                line.actions.iter().map(|(_, a)| *a).collect::<Vec<_>>(),
                expected,
                "{}: the same buttons whether or not it is selected",
                line.label
            );
            for (rect, action) in &line.actions {
                assert_eq!(
                    view.hit(rect.x, rect.y),
                    Some(&HitTarget::TreeAction(line.key.clone(), *action))
                );
            }
        }
        assert_eq!(
            view.tree_lines.iter().filter(|line| line.selected).count(),
            1,
            "exactly one row is being worked on"
        );
    }

    #[test]
    fn the_tree_is_headed_by_the_one_command_that_belongs_to_all_of_it() {
        let model = model_with_threads();
        let ui = UiState::new(String::new());
        let view = compute_view(Rect::new(0, 0, 100, 30), &model, &ui, None);

        let heading = view.sidebar_heading.expect("a tall sidebar gets a heading");
        let add = view.sidebar_new_project.expect("and an add-project button");
        assert_eq!(heading.height, 1);
        assert_eq!(add.right(), heading.right(), "the button sits on its right");
        assert_eq!(view.hit(add.x, add.y), Some(&HitTarget::NewProject));

        let body = view.sidebar_body.expect("the tree still has a body");
        assert_eq!(
            body.y,
            heading.bottom(),
            "the tree starts below the heading"
        );
        assert!(
            view.tree_lines.iter().all(|line| line.rect.y >= body.y),
            "and no row is drawn over it"
        );
        assert_eq!(
            view.hit(heading.x, heading.y),
            None,
            "the heading is a label"
        );
    }

    /// A pinned thread has to look pinned from across the list, selected or
    /// not. Nothing else on the row claims ★ now, so it can only mean one thing.
    #[test]
    fn pinning_is_marked_on_every_row_that_is_pinned() {
        let mut model = AppModel::default();
        model.apply_snapshot(
            "server",
            ThinkTermSessionState {
                server_id: "runtime".into(),
                generation: 1,
                spaces: vec![ThinkTermSessionSpace {
                    id: "space".into(),
                    name: "Space".into(),
                    ..Default::default()
                }],
                projects: vec![ThinkTermSessionProject {
                    id: "project".into(),
                    space_id: "space".into(),
                    name: "Project".into(),
                    threads: ["one", "two"]
                        .into_iter()
                        .map(|id| codec::ThinkTermSessionThread {
                            id: id.into(),
                            project_id: "project".into(),
                            name: id.into(),
                            is_pinned: true,
                            ..Default::default()
                        })
                        .collect(),
                    ..Default::default()
                }],
                ..Default::default()
            },
        );
        let ui = UiState::new(String::new());
        let view = compute_view(Rect::new(0, 0, 100, 30), &model, &ui, None);

        let threads = view
            .tree_lines
            .iter()
            .filter(|line| matches!(line.key, TreeNodeKey::Thread(_)))
            .collect::<Vec<_>>();
        assert_eq!(threads.len(), 2);
        assert!(
            threads.iter().all(|line| line.pinned),
            "both threads are pinned, so both say so"
        );
    }

    /// A button that covers the name it belongs to is worse than no button, so
    /// even at the narrowest sidebar the row keeps its reading space.
    #[test]
    fn buttons_never_eat_into_the_space_a_name_needs() {
        let model = model_with_threads();
        let mut ui = UiState::new(String::new());
        ui.sidebar_width = 18;
        let view = compute_view(Rect::new(0, 0, 46, 30), &model, &ui, None);
        let body = view.sidebar_body.expect("a sidebar");
        for line in &view.tree_lines {
            let leftmost = line
                .actions
                .iter()
                .map(|(rect, _)| rect.x)
                .min()
                .unwrap_or(line.rect.right());
            assert!(
                leftmost.saturating_sub(body.x) >= MIN_LABEL_COLUMNS,
                "{:?} left only {} columns for its name",
                line.key,
                leftmost - body.x
            );
        }
    }

    #[test]
    fn only_the_active_thread_is_painted_as_selected() {
        let model = model_with_threads();
        let ui = UiState::new(String::new());
        let view = compute_view(Rect::new(0, 0, 100, 30), &model, &ui, None);
        let selected = view
            .tree_lines
            .iter()
            .filter(|line| line.selected)
            .collect::<Vec<_>>();
        assert_eq!(selected.len(), 1);
        assert!(matches!(selected[0].key, TreeNodeKey::Thread(_)));
    }

    #[test]
    fn narrow_view_gives_the_terminal_every_column_once_the_tree_is_dismissed() {
        let model = model_with_threads();
        let mut ui = UiState::new(String::new());
        ui.sidebar_visible = false;
        let view = compute_view(Rect::new(0, 0, 40, 20), &model, &ui, None);
        assert_eq!(view.class, ViewClass::Narrow);
        assert!(view.sidebar.is_none());
        assert!(view.tree_lines.is_empty());
        assert_eq!(view.hit(1, 0), Some(&HitTarget::SidebarToggle));
        // Two rows of header, and no footer at all while there is nothing to
        // say: the terminal keeps the row the key-chord hint used to hold.
        assert_eq!(view.content, Rect::new(0, 2, 40, 18));
    }

    /// Every button on a touch screen is worth two rows, and a finger that
    /// misses by one row must still land on the thing it aimed at.
    #[test]
    fn narrow_view_gives_every_button_the_full_height_of_a_two_row_bar() {
        let model = model_with_threads();
        let mut ui = UiState::new(String::new());
        ui.sidebar_visible = false;
        let view = compute_view(Rect::new(0, 0, 40, 20), &model, &ui, None);
        assert_eq!(view.tab_bar.height, 2);
        for row in 0..2 {
            assert_eq!(
                view.hit(1, row),
                Some(&HitTarget::SidebarToggle),
                "row {row}"
            );
            assert_eq!(
                view.hit(37, row),
                Some(&HitTarget::TabControl(TabBarControl::Menu)),
                "row {row}"
            );
            assert_eq!(
                view.hit(33, row),
                Some(&HitTarget::TabControl(TabBarControl::NewTab)),
                "row {row}"
            );
        }
        // The toggle is wider here than the four columns a mouse gets.
        assert_eq!(view.hit(5, 1), Some(&HitTarget::SidebarToggle));
    }

    /// The same phone turned sideways reports enough columns to look like a
    /// desktop and still has no mouse, so the setting has to beat the guess in
    /// both directions.
    #[test]
    fn touch_targets_follow_the_setting_rather_than_the_width() {
        let model = model_with_threads();
        let mut ui = UiState::new(String::new());
        ui.sidebar_visible = false;

        // Wide enough to be a desktop by every other measure.
        let wide = Rect::new(0, 0, 154, 32);
        assert_eq!(
            compute_view(wide, &model, &ui, None).class,
            ViewClass::Desktop
        );
        assert_eq!(compute_view(wide, &model, &ui, None).tab_bar.height, 1);
        ui.touch_targets = Some(true);
        let touched = compute_view(wide, &model, &ui, None);
        assert_eq!(touched.tab_bar.height, 2);
        assert_eq!(touched.hit(1, 1), Some(&HitTarget::SidebarToggle));

        // ...and a narrow screen driven by a mouse gets the compact bar back.
        ui.touch_targets = Some(false);
        let narrow = compute_view(Rect::new(0, 0, 40, 20), &model, &ui, None);
        assert_eq!(narrow.class, ViewClass::Narrow);
        assert_eq!(narrow.tab_bar.height, 1);
    }

    /// A message still needs somewhere to go; it just does not get to keep a
    /// row of the terminal when there is nothing to report.
    #[test]
    fn narrow_view_reclaims_the_footer_row_only_while_it_is_empty() {
        let model = model_with_threads();
        let mut ui = UiState::new(String::new());
        ui.sidebar_visible = false;
        let quiet = compute_view(Rect::new(0, 0, 40, 20), &model, &ui, None);
        assert_eq!(quiet.status.height, 0);

        ui.status = "Viewport: something went wrong".to_string();
        let noisy = compute_view(Rect::new(0, 0, 40, 20), &model, &ui, None);
        assert_eq!(noisy.status.height, 1);
        assert_eq!(noisy.content.height, quiet.content.height - 1);
    }

    /// The toggle used to sit on a narrow screen with nothing to toggle: the
    /// tree was refused at that width, so there was no way to reach another
    /// thread from a phone. Asking for it now covers the screen with it.
    /// Zooming a phone in until the screen is narrow, with the tree dismissed,
    /// is the ordinary way it is used — and it left the terminal undrawn: a tab
    /// bar over an empty screen.
    #[test]
    fn a_narrow_screen_with_the_tree_dismissed_still_draws_its_terminal() {
        let model = model_with_threads();
        let mut ui = UiState::new(String::new());
        ui.sidebar_visible = false;
        let view = compute_view(Rect::new(0, 0, 40, 20), &model, &ui, None);
        assert_eq!(view.class, ViewClass::Narrow);
        assert!(view.sidebar.is_none(), "the tree was dismissed");
        assert!(
            !tree_covers_terminal(&view),
            "so it is not covering anything"
        );
        assert_eq!(view.content.width, 40, "and the terminal has the screen");

        let mut covered = view.clone();
        covered.sidebar = Some(Rect::new(0, 0, 40, 20));
        assert!(tree_covers_terminal(&covered));
        covered.class = ViewClass::Desktop;
        assert!(!tree_covers_terminal(&covered), "beside, not on top");
    }

    #[test]
    fn narrow_view_shows_the_tree_as_a_full_width_overlay_when_asked() {
        let model = model_with_threads();
        let ui = UiState::new(String::new());
        let view = compute_view(Rect::new(0, 0, 40, 20), &model, &ui, None);
        assert_eq!(view.class, ViewClass::Narrow);
        assert_eq!(view.sidebar, Some(Rect::new(0, 0, 40, 20)));
        assert!(!view.tree_lines.is_empty());
        assert!(view.panes.is_empty(), "the tree is over the top of them");
        // But the grid the terminal has is still described, so a phone held in
        // portrait can take the size from whatever else is attached rather than
        // reading as a renderer with no viewport at all.
        assert_eq!(view.content.width, 40);
        assert!(view.sidebar_resize.is_none());
        assert_eq!(view.hit(1, 0), Some(&HitTarget::SidebarToggle));
    }

    #[test]
    fn hidden_sidebar_returns_all_content_columns_and_keeps_a_toggle() {
        let model = model_with_threads();
        let mut ui = UiState::new(String::new());
        ui.sidebar_visible = false;
        let view = compute_view(Rect::new(0, 0, 100, 30), &model, &ui, None);
        assert!(view.sidebar.is_none());
        assert_eq!(view.content, Rect::new(0, 1, 100, 28));
        assert_eq!(view.hit(1, 0), Some(&HitTarget::SidebarToggle));
        assert!(view.tabs.iter().all(|tab| tab.rect.x >= 4));
    }

    #[test]
    fn connection_rows_have_exact_overlay_hits() {
        let model = model_with_threads();
        let mut ui = UiState::new(String::new());
        ui.mode = crate::state::AppMode::Connections;
        ui.connections = (0..3)
            .map(|index| ConnectionItem {
                name: format!("server-{index}"),
                label: format!("Server {index}"),
                detail: String::new(),
                status: ConnectionStatus::Disconnected,
                connectable: true,
            })
            .collect();
        let view = compute_view(Rect::new(0, 0, 100, 30), &model, &ui, None);
        let dialog = view.dialog.unwrap();
        for index in 0..3 {
            assert_eq!(
                view.hit(dialog.x + 2, dialog.y + 2 + index as u16),
                Some(&HitTarget::Connection(index))
            );
        }
    }

    #[test]
    fn split_rectangles_are_clipped_to_the_remaining_viewport() {
        let area = Rect::new(10, 5, 20, 8);
        let horizontal = clipped_split_rect(area, 19, 6, 8, SplitDirection::Horizontal).unwrap();
        assert!(horizontal.right() <= area.right());
        assert!(horizontal.bottom() <= area.bottom());

        let vertical = clipped_split_rect(area, 16, 7, 20, SplitDirection::Vertical).unwrap();
        assert!(vertical.right() <= area.right());
        assert!(vertical.bottom() <= area.bottom());

        assert!(clipped_split_rect(area, 20, 0, 8, SplitDirection::Horizontal).is_none());
        assert!(clipped_split_rect(area, 0, 8, 8, SplitDirection::Vertical).is_none());
    }
}
