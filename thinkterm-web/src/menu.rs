//! The context menus as data: the desktop's lists (`mouseevent.rs`
//! `*_context_menu_items`) rebuilt from the model, with their conditions,
//! for a display layer to draw and hand back an action id. Nothing here
//! knows how a menu looks; everything about what it offers is here.

use crate::tree::{SpaceEntry, ThreadRow};
use codec::FrontendAccessMode;
use serde::Serialize;
use thinkterm_i18n::{tr, tr_args, FluentArgs};
use thinkterm_proto::{PaneId, TabId};

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct MenuItem {
    /// The action this row performs (`MenuAction::id`); empty for a
    /// header, a separator or a row that only opens a submenu.
    pub id: String,
    pub label: String,
    /// A lucide icon name, when the row has one.
    pub icon: Option<&'static str>,
    pub kind: Kind,
    pub enabled: bool,
    pub checked: bool,
    pub submenu: Vec<MenuItem>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum Kind {
    Item,
    Header,
    Separator,
}

impl MenuItem {
    fn item(action: MenuAction, label: String, icon: Option<&'static str>) -> Self {
        Self { id: action.id(), label, icon, kind: Kind::Item, enabled: true, checked: false, submenu: Vec::new() }
    }

    fn submenu(label: String, icon: Option<&'static str>, items: Vec<MenuItem>) -> Self {
        Self { id: String::new(), label, icon, kind: Kind::Item, enabled: true, checked: false, submenu: items }
    }

    fn header(label: String) -> Self {
        Self { id: String::new(), label, icon: None, kind: Kind::Header, enabled: false, checked: false, submenu: Vec::new() }
    }

    fn separator() -> Self {
        Self { id: String::new(), label: String::new(), icon: None, kind: Kind::Separator, enabled: false, checked: false, submenu: Vec::new() }
    }

    fn checked(mut self, checked: bool) -> Self {
        self.checked = checked;
        self
    }
}

/// What a menu row does. The id is the wire form (`"split:right:12"`),
/// parsed back when the display layer hands it in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MenuAction {
    Copy,
    Paste,
    Split { pane: PaneId, side: Side },
    FrontendAccess(FrontendAccessMode),
    CloseTabsLeft(TabId),
    CloseTabsRight(TabId),
    CloseOtherTabs(TabId),
    NewTabRight,
    Zoom(PaneId),
    Pin(String, bool),
    RenameThread(String),
    DeleteThread(String),
    MarkUnread(String),
    RenameProject(String),
    NewThread(String),
    ToggleCollapsed(String),
    ArchiveProject(String),
    RemoveProject(String),
    UnarchiveProject(String),
    ShowArchived,
    SwitchSpace(String),
    NewSpace,
    RenameSpace(String),
    DeleteSpace(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Side {
    Right,
    Left,
    Down,
    Up,
}

impl MenuAction {
    pub fn id(&self) -> String {
        match self {
            MenuAction::Copy => "copy".into(),
            MenuAction::Paste => "paste".into(),
            MenuAction::Split { pane, side } => {
                let side = match side {
                    Side::Right => "right",
                    Side::Left => "left",
                    Side::Down => "down",
                    Side::Up => "up",
                };
                format!("split:{side}:{pane}")
            }
            MenuAction::FrontendAccess(FrontendAccessMode::TmuxLatest) => "access:shared".into(),
            MenuAction::FrontendAccess(FrontendAccessMode::Handoff) => "access:handoff".into(),
            MenuAction::CloseTabsLeft(t) => format!("close-tabs-left:{t}"),
            MenuAction::CloseTabsRight(t) => format!("close-tabs-right:{t}"),
            MenuAction::CloseOtherTabs(t) => format!("close-other-tabs:{t}"),
            MenuAction::NewTabRight => "new-tab-right".into(),
            MenuAction::Zoom(p) => format!("zoom:{p}"),
            MenuAction::Pin(id, true) => format!("pin:{id}"),
            MenuAction::Pin(id, false) => format!("unpin:{id}"),
            MenuAction::RenameThread(id) => format!("rename-thread:{id}"),
            MenuAction::DeleteThread(id) => format!("delete-thread:{id}"),
            MenuAction::MarkUnread(id) => format!("unread:{id}"),
            MenuAction::RenameProject(id) => format!("rename-project:{id}"),
            MenuAction::NewThread(id) => format!("new-thread:{id}"),
            MenuAction::ToggleCollapsed(id) => format!("collapse:{id}"),
            MenuAction::ArchiveProject(id) => format!("archive:{id}"),
            MenuAction::RemoveProject(id) => format!("remove-project:{id}"),
            MenuAction::UnarchiveProject(id) => format!("unarchive:{id}"),
            MenuAction::ShowArchived => "show-archived".into(),
            MenuAction::SwitchSpace(id) => format!("space:{id}"),
            MenuAction::NewSpace => "new-space".into(),
            MenuAction::RenameSpace(id) => format!("rename-space:{id}"),
            MenuAction::DeleteSpace(id) => format!("delete-space:{id}"),
        }
    }

    pub fn parse(id: &str) -> Option<Self> {
        let mut parts = id.splitn(3, ':');
        let verb = parts.next()?;
        let arg = parts.next();
        let rest = parts.next();
        let num = |s: Option<&str>| s.and_then(|s| s.parse::<usize>().ok());
        let text = |s: Option<&str>| s.filter(|s| !s.is_empty()).map(str::to_string);
        Some(match verb {
            "copy" => MenuAction::Copy,
            "paste" => MenuAction::Paste,
            "split" => {
                let side = match arg? {
                    "right" => Side::Right,
                    "left" => Side::Left,
                    "down" => Side::Down,
                    "up" => Side::Up,
                    _ => return None,
                };
                MenuAction::Split { pane: num(rest)?, side }
            }
            "access" => MenuAction::FrontendAccess(match arg? {
                "shared" => FrontendAccessMode::TmuxLatest,
                "handoff" => FrontendAccessMode::Handoff,
                _ => return None,
            }),
            "close-tabs-left" => MenuAction::CloseTabsLeft(num(arg)?),
            "close-tabs-right" => MenuAction::CloseTabsRight(num(arg)?),
            "close-other-tabs" => MenuAction::CloseOtherTabs(num(arg)?),
            "new-tab-right" => MenuAction::NewTabRight,
            "zoom" => MenuAction::Zoom(num(arg)?),
            "pin" => MenuAction::Pin(text(arg)?, true),
            "unpin" => MenuAction::Pin(text(arg)?, false),
            "rename-thread" => MenuAction::RenameThread(text(arg)?),
            "delete-thread" => MenuAction::DeleteThread(text(arg)?),
            "unread" => MenuAction::MarkUnread(text(arg)?),
            "rename-project" => MenuAction::RenameProject(text(arg)?),
            "new-thread" => MenuAction::NewThread(text(arg)?),
            "collapse" => MenuAction::ToggleCollapsed(text(arg)?),
            "archive" => MenuAction::ArchiveProject(text(arg)?),
            "remove-project" => MenuAction::RemoveProject(text(arg)?),
            "unarchive" => MenuAction::UnarchiveProject(text(arg)?),
            "show-archived" => MenuAction::ShowArchived,
            "space" => MenuAction::SwitchSpace(text(arg)?),
            "new-space" => MenuAction::NewSpace,
            "rename-space" => MenuAction::RenameSpace(text(arg)?),
            "delete-space" => MenuAction::DeleteSpace(text(arg)?),
            _ => return None,
        })
    }
}

/// The terminal pane's menu (`terminal_context_menu_items`): copy and
/// paste, the four splits, and the frontend access mode with the
/// current one checked. Reset Terminal has no PDU and is left out.
pub fn for_pane(pane: PaneId, access: Option<FrontendAccessMode>) -> Vec<MenuItem> {
    let mut items = vec![
        MenuItem::item(MenuAction::Copy, tr("menu-copy"), Some("copy")),
        MenuItem::item(MenuAction::Paste, tr("menu-paste"), Some("clipboard-paste")),
        MenuItem::separator(),
        MenuItem::item(MenuAction::Split { pane, side: Side::Right }, tr("menu-split-right"), Some("square-split-horizontal")),
        MenuItem::item(MenuAction::Split { pane, side: Side::Left }, tr("menu-split-left"), Some("square-split-horizontal")),
        MenuItem::item(MenuAction::Split { pane, side: Side::Down }, tr("menu-split-down"), Some("square-split-vertical")),
        MenuItem::item(MenuAction::Split { pane, side: Side::Up }, tr("menu-split-up"), Some("square-split-vertical")),
        MenuItem::separator(),
    ];
    let mode = |m: FrontendAccessMode, key: &str| {
        let current = access == Some(m);
        MenuItem::item(MenuAction::FrontendAccess(m), tr(key), Some(if current { "check" } else { "square-terminal" }))
            .checked(current)
    };
    items.push(MenuItem::submenu(
        tr("web-menu-frontend-access"),
        None,
        vec![
            mode(FrontendAccessMode::TmuxLatest, "web-menu-access-shared"),
            mode(FrontendAccessMode::Handoff, "web-menu-access-handoff"),
        ],
    ));
    items
}

/// A tab's menu (`tab_context_menu_items`) for the tab at `index` of
/// `count` in its window, showing `active_pane`. Rename and Move need
/// PDUs the server does not have and are left out.
pub fn for_tab(tab: TabId, index: usize, count: usize, active_pane: PaneId) -> Vec<MenuItem> {
    let mut items = Vec::new();
    let mut close = Vec::new();
    if index > 0 {
        close.push(MenuItem::item(MenuAction::CloseTabsLeft(tab), tr("menu-close-tabs-left"), Some("x")));
    }
    if index + 1 < count {
        close.push(MenuItem::item(MenuAction::CloseTabsRight(tab), tr("menu-close-tabs-right"), Some("x")));
    }
    if count > 1 {
        close.push(MenuItem::item(MenuAction::CloseOtherTabs(tab), tr("menu-close-other-tabs"), Some("x")));
    }
    if !close.is_empty() {
        items.extend(close);
        items.push(MenuItem::separator());
    }
    items.push(MenuItem::item(MenuAction::NewTabRight, tr("menu-new-terminal-tab-right"), Some("plus")));
    items.push(MenuItem::separator());
    items.push(MenuItem::item(MenuAction::Zoom(active_pane), tr("menu-zoom-pane"), Some("maximize-2")));
    items
}

/// A thread row's menu (`workspace_thread_context_menu_items`, the
/// non-ref branch): pin, rename, delete, mark unread.
pub fn for_thread(thread: &ThreadRow) -> Vec<MenuItem> {
    let id = &thread.id;
    vec![
        if thread.pinned {
            MenuItem::item(MenuAction::Pin(id.clone(), false), tr("menu-unpin-thread"), Some("pin-off"))
        } else {
            MenuItem::item(MenuAction::Pin(id.clone(), true), tr("menu-pin-thread"), Some("pin"))
        },
        MenuItem::item(MenuAction::RenameThread(id.clone()), tr("menu-rename-thread"), Some("pencil")),
        MenuItem::item(MenuAction::DeleteThread(id.clone()), tr("menu-delete-thread"), Some("trash-2")),
        MenuItem::item(MenuAction::MarkUnread(id.clone()), tr("menu-mark-unread"), Some("mail")),
    ]
}

/// A project row's menu (`project_context_menu_items`): rename, new
/// thread, collapse, then archive -- flat when nothing runs, a submenu
/// that says what it closes otherwise -- and remove.
pub fn for_project(id: &str, live_panes: usize) -> Vec<MenuItem> {
    let mut items = vec![
        MenuItem::item(MenuAction::RenameProject(id.into()), tr("menu-rename-project"), Some("pencil")),
        MenuItem::item(MenuAction::NewThread(id.into()), tr("menu-new-thread"), Some("plus")),
        MenuItem::item(MenuAction::ToggleCollapsed(id.into()), tr("menu-toggle-threads"), Some("chevron-down")),
        MenuItem::separator(),
    ];
    if live_panes == 0 {
        items.push(MenuItem::item(MenuAction::ArchiveProject(id.into()), tr("menu-archive-project-quiet"), Some("archive")));
    } else {
        let mut args = FluentArgs::new();
        args.set("panes", live_panes as i64);
        items.push(MenuItem::submenu(
            tr("menu-archive-project"),
            Some("archive"),
            vec![
                MenuItem::header(tr_args("menu-archive-project-explain", &args)),
                MenuItem::item(MenuAction::ArchiveProject(id.into()), tr("menu-archive-project-confirm"), Some("archive")),
            ],
        ));
    }
    items.push(MenuItem::item(MenuAction::RemoveProject(id.into()), tr("menu-remove-project"), Some("trash-2")));
    items
}

/// An archived project's menu (`archived_project_context_menu_items`).
pub fn for_archived_project(id: &str, threads: usize) -> Vec<MenuItem> {
    let mut args = FluentArgs::new();
    args.set("threads", threads as i64);
    vec![
        MenuItem::item(MenuAction::UnarchiveProject(id.into()), tr("menu-unarchive-project"), Some("archive-restore")),
        MenuItem::separator(),
        MenuItem::submenu(
            tr("menu-delete-project-permanently"),
            Some("trash-2"),
            vec![
                MenuItem::header(tr_args("menu-delete-project-permanently-explain", &args)),
                MenuItem::item(MenuAction::RemoveProject(id.into()), tr("menu-delete-project-permanently-confirm"), Some("trash-2")),
            ],
        ),
    ]
}

/// The Space menu (`space_menu_items`, the local part): every Space with
/// the current one checked, New Space, Rename, and Delete for a Space
/// that is not the last one.
pub fn for_space(spaces: &[SpaceEntry]) -> Vec<MenuItem> {
    let mut items: Vec<MenuItem> = spaces
        .iter()
        .map(|s| {
            MenuItem::item(
                MenuAction::SwitchSpace(s.id.clone()),
                s.name.clone(),
                Some(if s.default { "house" } else { "layers" }),
            )
            .checked(s.current)
        })
        .collect();
    items.push(MenuItem::separator());
    items.push(MenuItem::item(MenuAction::NewSpace, tr("menu-new-space"), Some("plus")));
    if let Some(current) = spaces.iter().find(|s| s.current) {
        let mut args = FluentArgs::new();
        args.set("name", current.name.as_str());
        items.push(MenuItem::item(
            MenuAction::RenameSpace(current.id.clone()),
            tr_args("menu-rename-named-space", &args),
            Some("pencil"),
        ));
        if spaces.len() > 1 {
            items.push(MenuItem::item(
                MenuAction::DeleteSpace(current.id.clone()),
                tr_args("menu-delete-space", &args),
                Some("trash-2"),
            ));
        }
    }
    items
}

/// The bell's menu: the desktop lists pending work notifications, one row
/// per thread; the page has none yet, so it is the desktop's empty state.
pub fn for_notifications() -> Vec<MenuItem> {
    let mut item = MenuItem::header(tr("menu-no-notifications"));
    item.kind = Kind::Item;
    vec![item]
}

/// The sidebar's view options: what the page has of the desktop's
/// (`workspace_sidebar_view_options_menu_items`) -- the archived group.
pub fn for_sidebar_options(show_archived: bool, archived: usize) -> Vec<MenuItem> {
    let label = if archived > 0 {
        let mut args = FluentArgs::new();
        args.set("count", archived as i64);
        tr_args("menu-show-archived-count", &args)
    } else {
        tr("menu-show-archived")
    };
    let mut item = MenuItem::item(MenuAction::ShowArchived, label, Some("archive")).checked(show_archived);
    item.enabled = archived > 0 || show_archived;
    vec![item]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tree::{Dot, Status};

    fn labels(items: &[MenuItem]) -> Vec<String> {
        items
            .iter()
            .map(|i| match i.kind {
                Kind::Separator => "—".into(),
                Kind::Header => format!("# {}", i.label),
                Kind::Item if !i.submenu.is_empty() => format!("{} ▸", i.label),
                Kind::Item => i.label.clone(),
            })
            .collect()
    }

    #[test]
    fn every_action_survives_its_id() {
        let all = vec![
            MenuAction::Copy,
            MenuAction::Paste,
            MenuAction::Split { pane: 12, side: Side::Up },
            MenuAction::FrontendAccess(FrontendAccessMode::TmuxLatest),
            MenuAction::FrontendAccess(FrontendAccessMode::Handoff),
            MenuAction::CloseTabsLeft(3),
            MenuAction::CloseTabsRight(3),
            MenuAction::CloseOtherTabs(3),
            MenuAction::NewTabRight,
            MenuAction::Zoom(7),
            MenuAction::Pin("thread-a".into(), true),
            MenuAction::Pin("thread-a".into(), false),
            MenuAction::RenameThread("thread-a".into()),
            MenuAction::DeleteThread("thread-a".into()),
            MenuAction::MarkUnread("thread-a".into()),
            MenuAction::RenameProject("project-b".into()),
            MenuAction::NewThread("project-b".into()),
            MenuAction::ToggleCollapsed("project-b".into()),
            MenuAction::ArchiveProject("project-b".into()),
            MenuAction::RemoveProject("project-b".into()),
            MenuAction::UnarchiveProject("project-b".into()),
            MenuAction::ShowArchived,
            MenuAction::SwitchSpace("space-c".into()),
            MenuAction::NewSpace,
            MenuAction::RenameSpace("space-c".into()),
            MenuAction::DeleteSpace("space-c".into()),
        ];
        for action in all {
            assert_eq!(MenuAction::parse(&action.id()), Some(action.clone()), "{}", action.id());
        }
        assert_eq!(MenuAction::parse("split:sideways:1"), None);
        assert_eq!(MenuAction::parse("pin:"), None);
        assert_eq!(MenuAction::parse("zoom:x"), None);
    }

    #[test]
    fn the_pane_menu_checks_the_current_access_mode() {
        let items = for_pane(4, Some(FrontendAccessMode::Handoff));
        assert_eq!(
            labels(&items),
            ["Copy", "Paste", "—", "Split Right", "Split Left", "Split Down", "Split Up", "—", "Frontend access ▸"]
        );
        let modes = &items.last().unwrap().submenu;
        assert_eq!((modes[0].checked, modes[1].checked), (false, true));
        assert_eq!(modes[1].icon, Some("check"));
        assert_eq!(items[3].id, "split:right:4");
    }

    #[test]
    fn the_tab_menu_offers_only_the_closes_that_exist() {
        assert_eq!(labels(&for_tab(1, 0, 1, 9)), ["New Terminal Tab to Right", "—", "Zoom Pane"]);
        assert_eq!(
            labels(&for_tab(1, 0, 3, 9)),
            ["Close Tabs to Right", "Close Other Tabs", "—", "New Terminal Tab to Right", "—", "Zoom Pane"]
        );
        assert_eq!(labels(&for_tab(1, 2, 3, 9))[0], "Close Tabs to Left");
        assert_eq!(labels(&for_tab(1, 1, 3, 9))[..3], ["Close Tabs to Left", "Close Tabs to Right", "Close Other Tabs"]);
    }

    #[test]
    fn the_space_menu_keeps_the_last_space() {
        let one = vec![SpaceEntry { id: "s1".into(), name: "Default".into(), current: true, default: true }];
        assert_eq!(labels(&for_space(&one)), ["Default", "—", "New Space", "Rename “Default”…"]);
        assert!(for_space(&one)[0].checked);
        let two = vec![
            SpaceEntry { id: "s1".into(), name: "Default".into(), current: false, default: true },
            SpaceEntry { id: "s2".into(), name: "Work".into(), current: true, default: false },
        ];
        let items = for_space(&two);
        assert_eq!(labels(&items), ["Default", "Work", "—", "New Space", "Rename “Work”…", "Delete “Work”"]);
        assert_eq!((items[0].checked, items[1].checked), (false, true));
        assert_eq!(items[5].id, "delete-space:s2");
    }

    #[test]
    fn the_thread_menu_flips_pin_and_the_project_menu_explains_an_archive() {
        let row = ThreadRow {
            id: "t1".into(),
            project_id: "p1".into(),
            name: "main".into(),
            status: Status::Idle,
            dot: Dot::Quiet,
            pinned: true,
            unread: false,
            live: true,
            selected: false,
            deleting: false,
        };
        assert_eq!(labels(&for_thread(&row)), ["Unpin Thread", "Rename Thread…", "Delete Thread", "Mark as Unread"]);
        assert_eq!(for_thread(&row)[0].id, "unpin:t1");
        assert_eq!(
            labels(&for_project("p1", 0)),
            ["Rename Project…", "New Thread", "Collapse / Expand Threads", "—", "Archive Project", "Remove Project"]
        );
        let busy = for_project("p1", 2);
        assert_eq!(labels(&busy)[4], "Archive Project… ▸");
        assert_eq!(labels(&busy[4].submenu), ["# Closes 2 running panes; layouts are kept", "Archive and Close Panes"]);
        let archived = for_archived_project("p1", 1);
        assert_eq!(labels(&archived), ["Unarchive Project", "—", "Delete Permanently… ▸"]);
        assert_eq!(labels(&archived[2].submenu)[0], "# Deletes 1 thread for good; this cannot be undone");
        let options = for_sidebar_options(false, 0);
        assert!(!options[0].enabled && !options[0].checked);
        assert_eq!(for_sidebar_options(true, 3)[0].label, "Archived (3)");
    }
}
