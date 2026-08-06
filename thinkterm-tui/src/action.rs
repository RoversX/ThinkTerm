use crate::model::{ThreadKey, TreeNodeKey};
use config::keyassignment::PaneDirection;
use mux::pane::PaneId;
use mux::tab::TabId;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SplitAxis {
    Right,
    Down,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DestructiveAction {
    ClosePane {
        pane_id: PaneId,
    },
    CloseTab {
        tab_id: TabId,
    },
    DeleteThread {
        key: ThreadKey,
        end_sessions: bool,
    },
    RemoveProject {
        domain_name: String,
        project_id: String,
        end_sessions: bool,
    },
    DeleteSpace {
        domain_name: String,
        space_id: String,
        end_sessions: bool,
    },
}

/// Input handlers are deliberately side-effect free: keyboard and mouse both
/// produce an Action, and only the runtime dispatcher may touch mux/server
/// state.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Action {
    None,
    Detach,
    ToggleSidebar,
    ToggleHelp,
    OpenNavigator,
    OpenConnections,
    OpenSettings,
    AdjustSetting {
        index: usize,
        delta: isize,
    },
    CloseOverlay,
    SelectThread(ThreadKey),
    SelectRelativeThread(isize),
    SelectTab(TabId),
    SelectRelativeTab(isize),
    SelectNextAttention,
    FocusPane(PaneDirection),
    FocusPaneId(PaneId),
    CyclePane(isize),
    NewTab,
    RenameTab,
    /// Another terminal behind this pane, sharing its rectangle, reached by the
    /// pane's own level-2 tabs. Not a split: the geometry does not change.
    NewPaneInStack(PaneId),
    SplitPane(SplitAxis),
    ToggleZoom,
    EnterResize,
    ResizePane(PaneDirection, isize),
    ResizeSplit {
        split_index: usize,
        delta: isize,
    },
    EnterCopyMode,
    LeaveCopyMode,
    ScrollPane {
        pane_id: PaneId,
        lines: isize,
    },
    ScrollToBottom {
        pane_id: PaneId,
    },
    CopySelection,
    PasteClipboard,
    BeginSearch {
        backwards: bool,
    },
    SearchNext {
        backwards: bool,
    },
    SubmitPrompt,
    CancelPrompt,
    NewSpace {
        domain_name: String,
    },
    NewProject {
        domain_name: String,
        space_id: String,
    },
    NewThread {
        domain_name: String,
        project_id: String,
    },
    RenameNode(TreeNodeKey),
    ToggleThreadPinned(ThreadKey),
    MoveThread {
        key: ThreadKey,
        before: Option<String>,
    },
    MoveProject {
        domain_name: String,
        space_id: String,
        project_id: String,
        before: Option<String>,
    },
    Confirm(DestructiveAction),
    AcceptConfirmation,
    ConnectDomain(String),
    DisconnectDomain(String),
    RetryDomain(String),
}
