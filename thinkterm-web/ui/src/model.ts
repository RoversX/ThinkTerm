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
export type Card = { title: string; hint: string; state: 'busy' | 'free' | 'taking' | 'refused'; action: string };
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

/** `space` is the Space on show, for the page to remember;
    `new_project_error` is why the last path typed into Add workspace was
    refused, shown beside the field. */
/** What a hover over the window's left edge does with the panel put away
    (thinkterm-web/src/sidebar.rs `REVEAL`). Lengths are CSS pixels. */
export type Reveal = { edge: number; dwell_ms: number; retreat_ms: number };

/** One action in the panel's footer (`sidebar.rs` `FooterAction`); `label`
    is drawn beside the icon while the panel is wide enough for it. */
export type FooterAction = {
  id: string;
  icon: string;
  label: string | null;
  tip: string;
  enabled: boolean;
  /** Right-aligned, as the desktop keeps everything but the gear. */
  trailing: boolean;
};

export type SidebarView = {
  rows: SideRow[];
  editing: Editing;
  space: string | null;
  new_project_error: string | null;
  reveal: Reveal;
  footer: FooterAction[];
  /** Below this the footer shows the gear without its label. */
  footer_label_min_width: number;
};

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
/** One tab of the right panel's selector (`agents.rs` `PanelTab`): which
    exist, and which this browser can open. */
export type PanelTab = {
  id: string;
  icon: string;
  label: string;
  available: boolean;
  tip: string;
};

/** `labeled`: the active tab carries its label; past five tabs, none
    does (`agents.rs` `LABELED_TABS`). */
export type AgentsView = { rows: AgentRow[]; summary: string; tabs: PanelTab[]; active: string; labeled: boolean };

// The right panel's Snippets tab (thinkterm-web/src/snippets.rs
// `SnippetsView`): the snippets the plugin host keeps, filtered by the
// search, as the desktop's panel lists them.

export type SnippetRow = { id: string; title: string; preview: string };

export type SnippetsView = {
  /** `loading` until the host has sent them; `unavailable` when this
      server cannot reach a plugin host, with `reason`. */
  state: 'loading' | 'ready' | 'unavailable';
  reason: string | null;
  rows: SnippetRow[];
  query: string;
  /** What an empty list says. */
  empty: string;
  /** The search the rows answer: `query`, once they have caught up. */
  answers: string | null;
  /** How many times the rows were asked for, for probes. */
  asked: number;
  revision: number;
};

// The plugin list in Settings › Sidebar & Plugins
// (thinkterm-web/src/plugin_list.rs `PluginsView`): the plugins the plugin
// host on the server's machine runs, as the desktop's lists them.

export type PluginRow = {
  /** Tells rows apart: two installed plugins can claim one id. */
  key: string;
  id: string;
  name: string;
  /** Where it came from, its state, what it does: one line. */
  detail: string;
  /** The switch as it is to be shown, a change on its way included. */
  enabled: boolean;
  /** Whether a switch could do anything for it. */
  switchable: boolean;
  /** Built into ThinkTerm: its switch is its panel's, not in the list. */
  builtin: boolean;
  /** How long an installed plugin that is on runs unused; none for others. */
  background: 'always' | 'briefly' | 'never' | null;
  /** What that choice means, and whether it is the plugin's own. */
  background_detail: string;
};

export type PluginsView = {
  state: 'loading' | 'ready' | 'unavailable';
  /** What the section says besides its list: loading, or why the host
      cannot be reached (then the list is only what was last heard). */
  status: string;
  rows: PluginRow[];
  /** Why the last change asked here was refused. */
  refused: string | null;
  revision: number;
};

// A plugin's panel in the right panel (thinkterm-web/src/plugin_panel.rs
// `PanelView`; its painting is thinkterm-plugin-panel's `Painting`): what to
// paint, in CSS pixels from the panel's corner, each op cut to a region.
// Colours are ThinkTerm's by name (`text`, `bg-hover`, `positive-bg`...),
// or `#rrggbb[aa]`.

export type PanelOp =
  | { op: 'rect'; clip: number; x: number; y: number; w: number; h: number; fill?: string; border?: string; radius: number }
  | {
      op: 'text';
      clip: number;
      x: number;
      y: number;
      w: number;
      h: number;
      text: string;
      color: string;
      font: 'ui' | 'mono';
      size: 'small' | 'body' | 'title';
      bold: boolean;
      align: 'left' | 'center' | 'right';
    }
  | { op: 'line'; clip: number; points: number[]; width: number; color: string }
  | { op: 'area'; clip: number; points: number[]; base: number; color: string; fade: boolean }
  | { op: 'thumb'; clip: number; x: number; y: number; w: number; h: number };

export type Painting = {
  /** Left, top, right, bottom. */
  clips: [number, number, number, number][];
  ops: PanelOp[];
  cursor: 'pointer' | 'arrow' | null;
};

export type PanelView = {
  state: 'starting' | 'open' | 'stopped';
  /** Said instead of a painting: starting, or why it stopped. */
  message: string | null;
  painting: Painting | null;
};

// The page's own preferences (thinkterm-web/src/settings.rs `WebSettings`),
// whose JSON names are kebab-case.

export type Theme = 'dark' | 'light' | 'system';
export type FontMode = { mode: 'follow' } | { mode: 'pinned'; pt: number };
export type Hotkey = 'cmd-k' | 'cmd-shift-p' | 'ctrl-shift-p';
/** By the pixel, so the rows follow the hand, or a whole row at a time. */
export type ScrollMode = 'stepped' | 'smooth';

export type WebSettings = {
  /** `"system"` or a locale tag. */
  language: string;
  theme: Theme;
  font: FontMode;
  'hover-reveal': boolean;
  'agents-panel': boolean;
  'palette-hotkey': Hotkey;
  'sidebar-width': number;
  'scroll-mode': ScrollMode;
  /** `"desktop"` follows the server's configuration; else a scheme's name. */
  'terminal-scheme': string;
};

/** One entry of `schemes.json`, built by `thinkterm cli color-schemes
    --json`. Every colour is a `#rrggbb` string. */
export type Scheme = {
  name: string;
  foreground: string;
  background: string;
  cursor_bg: string;
  cursor_fg: string;
  cursor_border: string;
  selection_bg: string;
  selection_fg: string;
  ansi: string[];
  brights: string[];
};

/** One row of `client.languages()`. */
export type LanguageOption = { preference: string; label: string };
