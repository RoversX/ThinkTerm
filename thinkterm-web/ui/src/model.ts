// The shapes the wasm hands over as JSON (thinkterm-web/src/views.rs and the
// models it serialises: chrome.rs, navbar.rs). Field names are the serde
// ones, so `pane`/`tab`/`window` rather than the Rust struct's own.

export type PaneView = { pane: number; title: string; current: boolean };

export type TabView = {
  tab: number;
  window: number;
  /** The tab's title as the server has it: the tooltip. */
  title: string;
  /** What the capsule says: the desktop's title rule, applied by the wasm. */
  label: string;
  /** The pane a click on the tab shows. */
  target: number;
  current: boolean;
  panes: PaneView[];
};

export type Controls = {
  following: boolean;
  fit: boolean;
  /** The tab whose close button was pressed once. */
  closing: number | null;
  clipped: [number, number] | null;
};

export type TabsView = { tabs: TabView[]; controls: Controls };

export type NavRect = { pane: number; left: number; top: number; width: number; height: number };

export type CapsuleView = { pane: number; title: string; busy: boolean; current: boolean };

export type NavView = {
  rect: NavRect;
  members: CapsuleView[];
  focused: boolean;
  zoomed: boolean;
  closing: boolean;
};

export type NavsView = NavView[];

/** The page's own labels, keyed by catalogue id (thinkterm-web/src/views.rs STRING_KEYS). */
export type Strings = Record<string, string>;

export type Toast = { text: string; sticky: boolean; at: number };
export type Card = { title: string; hint: string };
export type StatusView = { toast: Toast | null; card: Card | null; summary: string };

// The sidebar (thinkterm-web/src/tree.rs `Row`, `ThreadRow`; sidebar.rs
// `Editing`), tagged by `kind` the way serde writes it.

export type ThreadStatus = 'Idle' | 'Running' | 'NeedsAttention' | 'Done';
export type ThreadDot = 'Active' | 'Unread' | 'Pinned' | 'Open' | 'Quiet';

export type ThreadRow = {
  kind: 'thread';
  id: string;
  /** The project the thread belongs to. */
  project: string;
  name: string;
  status: ThreadStatus;
  dot: ThreadDot;
  pinned: boolean;
  unread: boolean;
  /** The thread has terminals on the server. */
  live: boolean;
  selected: boolean;
  /** Its delete button was pressed once and waits for the press that means it. */
  deleting: boolean;
};

export type SideRow =
  | { kind: 'space'; id: string; name: string }
  | { kind: 'new-thread' }
  | { kind: 'pinned' }
  | { kind: 'workspaces' }
  | { kind: 'project'; id: string; name: string; path: string; collapsed: boolean; archived: boolean }
  | ThreadRow
  | { kind: 'archived'; count: number; open: boolean; label: string }
  | { kind: 'others' }
  | { kind: 'window'; id: number; title: string; selected: boolean };

/** A name or path being typed into the panel. */
export type Editing =
  | { kind: 'none' }
  | { kind: 'new-project' }
  | { kind: 'thread'; id: string }
  | { kind: 'project'; id: string }
  | { kind: 'space'; id: string };

/** `space` is the Space on show, for the page to remember. */
export type SidebarView = { rows: SideRow[]; editing: Editing; space: string | null };

// The context menus (thinkterm-web/src/menu.rs `MenuItem`, `Kind`; views.rs
// `MenuOutcome`): what a menu offers, and what running a row came to.

export type MenuKind = 'item' | 'header' | 'separator';

export type MenuItem = {
  /** The action id (`"split:right:12"`); empty for a header, a separator
      or a row that only opens a submenu. */
  id: string;
  label: string;
  /** A lucide icon name, when the row has one. */
  icon: string | null;
  kind: MenuKind;
  enabled: boolean;
  checked: boolean;
  submenu: MenuItem[];
};

/** `copy` is text for the page to put on the clipboard; `paste` asks it to
    read the clipboard and type what is there. */
export type MenuOutcome = { handled: boolean; copy: string | null; paste: boolean };

// The search palette (thinkterm-web/src/palette.rs `Results`, `Section`,
// `Entry`; views.rs `PaletteOutcome`): what a query found, and what
// running a pick came to.

export type PaletteGroup = 'threads' | 'tabs' | 'panes' | 'spaces' | 'commands';

export type PaletteEntry = {
  /** `thread:<id>`, `tab:<id>`, `pane:<id>`, `space:<id>`, `cmd:<name>`, `lang:<tag>`. */
  id: string;
  title: string;
  subtitle: string;
  /** A lucide icon name. */
  icon: string;
  /** Right-hand text: a shortcut, a Space, a state. */
  accessory: string;
  group: PaletteGroup;
};

export type PaletteSection = { group: PaletteGroup; label: string; entries: PaletteEntry[] };

export type PaletteResults = { sections: PaletteSection[]; placeholder: string; empty: string };

/** `page` names what only the page does; `recent` is the updated list of
    picks for the page to keep. */
export type PaletteOutcome = { handled: boolean; page: string | null; recent: string[] };

// The right-hand Agents panel (thinkterm-web/src/agents.rs `AgentRow`,
// `AgentsView`): one row per pane an agent runs in, and the line above them.

export type AgentState = 'working' | 'blocked' | 'idle' | 'unknown';

export type AgentRow = {
  pane: number;
  /** The window the pane is in; nothing while the server has not said. */
  window: number | null;
  /** The pane is in the window this page shows. */
  here: boolean;
  agent_id: string;
  name: string;
  title: string;
  state: AgentState;
  state_label: string;
  /** "Project \u00b7 Thread", or the workspace when no thread claims it. */
  place: string;
  /** `brand-claude`, `brand-copilot` or `bot`. */
  icon: string;
};

/** `summary` is the line above the rows; "No agents detected" when empty. */
export type AgentsView = { rows: AgentRow[]; summary: string };

// The page's own preferences (thinkterm-web/src/settings.rs `WebSettings`),
// whose JSON names are kebab-case.

export type Theme = 'dark' | 'light' | 'system';
export type FontMode = { mode: 'follow' } | { mode: 'pinned'; pt: number };
export type Hotkey = 'cmd-k' | 'cmd-shift-p' | 'ctrl-shift-p';

export type WebSettings = {
  /** `"system"` or a locale tag. */
  language: string;
  theme: Theme;
  font: FontMode;
  'hover-reveal': boolean;
  'agents-panel': boolean;
  'palette-hotkey': Hotkey;
  'sidebar-width': number;
};

/** One row of `client.languages()`. */
export type LanguageOption = { preference: string; label: string };
