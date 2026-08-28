//! Spotlight-style command palette.
//!
//! The data layer (command enumeration, frecency, shortcut formatting) lives
//! in `termwindow/palette.rs`; this module owns the state machine and the
//! chrome, drawn with the `ui/` toolkit like the fallback context menu.
//! Structure follows `context_menu.rs` (a field on `TermWindow`, a paint call
//! at the tail of the paint pass, and its own key/mouse dispatch that runs
//! before the terminal's).

use crate::commands::ExpandedCommand;
use crate::overlay::selector::{matcher_pattern, matcher_score};
use crate::termwindow::palette::{build_commands, format_key_label, frecency_scores, save_recent};
use crate::termwindow::ui::icons::SvgIcon;
use crate::termwindow::{GuiWin, TermWindowNotif, UIItem, UIItemType, COMMAND_PALETTE_ZINDEX};
use crate::ui::{
    apply_wheel_to_area, char_index_for_x, contains, text_width_to_char, DrawContext,
    EditModifiers, InteractionState, ScrollState, TextInputState, UiContext, UiPalette, UiTokens,
    WidgetKind,
};
use crate::utilsprites::RenderMetrics;
use anyhow::Context as _;
use config::keyassignment::{ClipboardCopyDestination, KeyAssignment, SpawnTabDomain};
use mux::Mux;
use mux_lua::MuxPane;
use std::collections::HashMap;
use std::rc::Rc;
use std::time::{Duration, Instant};
use termwiz::input::KeyCode;
use window::color::LinearRgba;
use window::{
    Appearance, Clipboard, Modifiers, MouseCursor, MouseEvent, MouseEventKind, MousePress, RectF,
    WindowOps,
};

// Geometry in design pixels (the 2x macOS grid); convert with `ctx.px` at the
// point of use, exactly like the context menu and the sidebars.
const CARD_RADIUS: f32 = 28.0;
const CARD_MIN_W: f32 = 760.0;
const CARD_MAX_W: f32 = 1360.0;
const CARD_MARGIN: f32 = 48.0;
const CARD_TOP_MIN: f32 = 72.0;
const CARD_BOTTOM_PAD: f32 = 10.0;
/// Breathing room between the search separator and the first row; without it
/// the selected row's highlight kisses the separator line.
const LIST_TOP_PAD: f32 = 12.0;
/// How long the scrollbar stays fully visible after the last scroll…
const SCROLLBAR_LINGER: f32 = 0.7;
/// …and how long it takes to fade out after that.
const SCROLLBAR_FADE: f32 = 0.25;
const SEARCH_EXTRA_H: f32 = 48.0;
const SEARCH_MIN_H: f32 = 96.0;
const SEARCH_ICON_SIZE: f32 = 34.0;
const ROW_EXTRA_H: f32 = 28.0;
const ROW_MIN_H: f32 = 72.0;
const ROW_RADIUS: f32 = 16.0;
const ROW_INSET_X: f32 = 10.0;
const ROW_INSET_Y: f32 = 3.0;
const PAD_X: f32 = 28.0;
const ICON_SIZE: f32 = 30.0;
const ICON_SLOT: f32 = 46.0;
const LABEL_GAP: f32 = 16.0;
const CHEVRON_SIZE: f32 = 26.0;
const CARET_WIDTH: f32 = 3.0;
const BACK_CHIP_ICON: f32 = 26.0;
const BACK_CHIP_PAD_X: f32 = 14.0;
const BACK_CHIP_GAP: f32 = 12.0;
/// Fraction of the window height the list may occupy.
const LIST_MAX_FRACTION: f32 = 0.62;
/// Search penetrates group children once the query is this long...
const PENETRATION_MIN_CHARS: usize = 2;
/// ...surfacing at most this many children per group.
const PENETRATION_PER_GROUP: usize = 3;
/// Families smaller than this stay flat; a submenu wrapping one or two
/// entries is slower than just showing them.
const GROUP_FOLD_THRESHOLD: usize = 3;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PaletteAction {
    Search,
    Back,
    /// Index into the current match order, not into the item list.
    Row(usize),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum PaletteGroupKind {
    ColorScheme,
    Space,
    Workspace,
    DomainNewTab,
    DomainAttach,
    DomainDetach,
    LaunchMenu,
}

pub(crate) enum PaletteItem {
    Leaf(ExpandedCommand),
    Group(PaletteGroup),
}

pub(crate) struct PaletteGroup {
    label: String,
    doc: String,
    icon: SvgIcon,
    /// Right-hand accessory, e.g. the current theme name.
    current_value: Option<String>,
    /// Extra English terms folded into the fuzzy haystack so localized group
    /// labels stay findable by their conventional names.
    aliases: &'static str,
    items: Rc<Vec<PaletteItem>>,
    /// One fuzzy haystack per child, precomputed at build time: formatting
    /// ~1000 scheme haystacks (each with a `{:?}` of the action) on every
    /// keystroke is measurable; doing it once at open is not.
    haystacks: Rc<Vec<String>>,
}

struct PaletteFrame {
    /// Breadcrumb chip label, e.g. "Change Theme".
    label: String,
    items: Rc<Vec<PaletteItem>>,
    haystacks: Rc<Vec<String>>,
    /// Restored verbatim when the user backs out.
    parent_query: String,
    parent_selected: usize,
    parent_scroll: f32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MatchEntry {
    /// Index into the current frame's items.
    Item(usize),
    /// A group child surfaced by top-level search: (group item idx, child idx).
    Nested(usize, usize),
}

struct MatchCache {
    query: String,
    depth: usize,
    order: Vec<MatchEntry>,
}

#[derive(Clone, Copy)]
pub(crate) struct PaletteLayout {
    card: RectF,
    list: RectF,
    row_height: f32,
    visible_rows: usize,
    /// Leading pad inside the scrollable content, before row 0.
    top_pad: f32,
}

impl Default for PaletteLayout {
    fn default() -> Self {
        let zero = euclid::rect(0.0, 0.0, 0.0, 0.0);
        Self {
            card: zero,
            list: zero,
            row_height: 1.0,
            visible_rows: 1,
            top_pad: 0.0,
        }
    }
}

enum EntryKind {
    Group(usize),
    Leaf,
}

pub(crate) struct CommandPaletteState {
    root: Vec<PaletteItem>,
    /// Drill-down stack; empty means the root list is showing.
    stack: Vec<PaletteFrame>,
    search: TextInputState,
    /// Index into the match order, NOT into the item list.
    selected: usize,
    matches: Option<MatchCache>,
    scroll: ScrollState,
    widgets: UiContext<PaletteAction>,
    interaction: InteractionState<PaletteAction>,
    /// Absolute x of a click in the search field (and whether it extends the
    /// selection), resolved into a caret index at the next paint — mouse
    /// handlers have no `DrawContext` to measure glyphs with. Same deferral
    /// as `ssh_hosts_view`.
    pending_caret_click: Option<(f32, bool)>,
    dragging_search: bool,
    layout: PaletteLayout,
    /// ui scale captured at paint so mouse handlers can convert pixels.
    last_ui_scale: f32,
    /// Scroll offset to restore at the next paint, applied after
    /// `set_extents`: restoring it immediately in `pop_frame` would be
    /// clamped against the child frame's stale extents.
    pending_scroll: Option<f32>,
    /// Offset at the previous paint; a change marks scroll activity for the
    /// auto-hiding scrollbar.
    last_scroll_offset: f32,
    /// When the scrollbar last saw activity; None = never shown.
    scroll_active_at: Option<Instant>,
    /// Distinguishes palette generations so an async clipboard paste started
    /// against one palette cannot land in a later one.
    generation: u64,
}

impl CommandPaletteState {
    fn new(root: Vec<PaletteItem>) -> Self {
        Self {
            root,
            stack: Vec::new(),
            search: TextInputState::new(),
            selected: 0,
            matches: None,
            scroll: ScrollState::new(),
            widgets: UiContext::default(),
            interaction: InteractionState::default(),
            pending_caret_click: None,
            dragging_search: false,
            layout: PaletteLayout::default(),
            last_ui_scale: 1.0,
            pending_scroll: None,
            last_scroll_offset: 0.0,
            scroll_active_at: None,
            generation: {
                use std::sync::atomic::{AtomicU64, Ordering};
                static NEXT: AtomicU64 = AtomicU64::new(1);
                NEXT.fetch_add(1, Ordering::Relaxed)
            },
        }
    }

    fn current_items(&self) -> &[PaletteItem] {
        self.stack
            .last()
            .map(|frame| frame.items.as_slice())
            .unwrap_or(self.root.as_slice())
    }

    fn ensure_matches(&mut self) {
        let depth = self.stack.len();
        let stale = match &self.matches {
            Some(cache) => cache.query != self.search.text() || cache.depth != depth,
            None => true,
        };
        if stale {
            let query = self.search.text().to_string();
            let order = {
                let penetrate = crate::native_settings::load_shared()
                    .command_palette
                    .search_penetrates_groups;
                let haystacks = self.stack.last().map(|frame| frame.haystacks.as_slice());
                compute_order(&query, self.current_items(), haystacks, penetrate)
            };
            self.matches = Some(MatchCache {
                query,
                depth,
                order,
            });
            self.selected = 0;
            self.scroll.reset();
        }
        let len = self.order_len();
        if len == 0 {
            self.selected = 0;
        } else if self.selected >= len {
            self.selected = len - 1;
        }
    }

    fn invalidate_matches(&mut self) {
        self.matches = None;
        self.selected = 0;
        self.scroll.reset();
    }

    /// Whether the cached match order still describes the current query and
    /// frame. Mouse hits index into the order of the *last paint*; acting on
    /// them through a stale cache would execute the wrong row.
    fn matches_fresh(&self) -> bool {
        match &self.matches {
            Some(cache) => cache.query == self.search.text() && cache.depth == self.stack.len(),
            None => false,
        }
    }

    fn order_len(&self) -> usize {
        self.matches.as_ref().map(|m| m.order.len()).unwrap_or(0)
    }

    fn entry(&self, order_idx: usize) -> Option<MatchEntry> {
        self.matches
            .as_ref()
            .and_then(|m| m.order.get(order_idx))
            .copied()
    }

    fn classify_entry(&self, order_idx: usize) -> Option<EntryKind> {
        match self.entry(order_idx)? {
            MatchEntry::Item(idx) => match self.current_items().get(idx)? {
                PaletteItem::Group(_) => Some(EntryKind::Group(idx)),
                PaletteItem::Leaf(_) => Some(EntryKind::Leaf),
            },
            MatchEntry::Nested(..) => Some(EntryKind::Leaf),
        }
    }

    fn command_for_entry(&self, order_idx: usize) -> Option<ExpandedCommand> {
        match self.entry(order_idx)? {
            MatchEntry::Item(idx) => match self.current_items().get(idx)? {
                PaletteItem::Leaf(cmd) => Some(cmd.clone()),
                PaletteItem::Group(_) => None,
            },
            MatchEntry::Nested(group_idx, child_idx) => {
                match self.current_items().get(group_idx)? {
                    PaletteItem::Group(group) => match group.items.get(child_idx)? {
                        PaletteItem::Leaf(cmd) => Some(cmd.clone()),
                        PaletteItem::Group(_) => None,
                    },
                    PaletteItem::Leaf(_) => None,
                }
            }
        }
    }

    fn enter_group(&mut self, item_idx: usize) {
        let (label, items, haystacks) = match self.current_items().get(item_idx) {
            Some(PaletteItem::Group(group)) => (
                group.label.clone(),
                Rc::clone(&group.items),
                Rc::clone(&group.haystacks),
            ),
            _ => return,
        };
        self.stack.push(PaletteFrame {
            label,
            items,
            haystacks,
            parent_query: self.search.text().to_string(),
            parent_selected: self.selected,
            parent_scroll: self.scroll.offset,
        });
        self.search.clear();
        self.invalidate_matches();
    }

    fn pop_frame(&mut self) -> bool {
        let Some(frame) = self.stack.pop() else {
            return false;
        };
        self.search.set_text_end(frame.parent_query);
        self.invalidate_matches();
        self.ensure_matches();
        self.selected = frame.parent_selected.min(self.order_len().saturating_sub(1));
        self.pending_scroll = Some(frame.parent_scroll);
        true
    }

    /// Backspace doubles as "back" only when it cannot possibly be editing:
    /// empty query, caret at the start, nothing selected, and a frame to
    /// return to.
    fn backspace_pops(&self) -> bool {
        !self.stack.is_empty()
            && self.search.is_empty()
            && self.search.cursor == 0
            && self.search.caret_selection_range().is_none()
    }

    fn move_selection_by(&mut self, delta: isize) {
        let len = self.order_len();
        if len == 0 {
            return;
        }
        let current = self.selected as isize;
        self.selected = (current + delta).clamp(0, len as isize - 1) as usize;
        self.ensure_selected_visible();
    }

    fn set_selection(&mut self, idx: usize) {
        let len = self.order_len();
        if len == 0 {
            return;
        }
        self.selected = idx.min(len - 1);
        self.ensure_selected_visible();
    }

    fn ensure_selected_visible(&mut self) {
        let row_h = self.layout.row_height.max(1.0);
        let pad = self.layout.top_pad;
        let viewport = pad + self.layout.visible_rows.max(1) as f32 * row_h;
        // Content coordinates: the leading pad sits before row 0.
        let top = pad + self.selected as f32 * row_h;
        let bottom = top + row_h;
        if top < self.scroll.offset {
            // Land the row a pad below the separator (and at offset 0 for
            // row 0, so the resting gap comes back).
            self.scroll
                .scroll_by(self.selected as f32 * row_h - self.scroll.offset);
        } else if bottom > self.scroll.offset + viewport {
            self.scroll
                .scroll_by(bottom - viewport - self.scroll.offset);
        }
    }
}

fn item_label(item: &PaletteItem) -> &str {
    match item {
        PaletteItem::Leaf(cmd) => &cmd.brief,
        PaletteItem::Group(group) => &group.label,
    }
}

fn item_haystack(item: &PaletteItem) -> String {
    match item {
        PaletteItem::Leaf(cmd) => format!(
            "{}: {}. {} {:?}",
            cmd.menubar.join(" "),
            cmd.brief,
            cmd.doc,
            cmd.action
        ),
        PaletteItem::Group(group) => {
            format!("{} {} {}", group.label, group.doc, group.aliases)
        }
    }
}

/// Precomputed haystacks for a list of items, one per index, so per-keystroke
/// matching never re-formats them.
fn build_haystacks(items: &[PaletteItem]) -> Vec<String> {
    items.iter().map(item_haystack).collect()
}

fn compute_order(
    query: &str,
    items: &[PaletteItem],
    haystacks: Option<&[String]>,
    penetrate_groups: bool,
) -> Vec<MatchEntry> {
    if query.is_empty() {
        return (0..items.len()).map(MatchEntry::Item).collect();
    }

    let pattern = matcher_pattern(query);
    let mut scored: Vec<(u32, MatchEntry)> = items
        .iter()
        .enumerate()
        .filter_map(|(idx, item)| {
            let owned;
            let haystack: &str = match haystacks.and_then(|all| all.get(idx)) {
                Some(precomputed) => precomputed,
                None => {
                    owned = item_haystack(item);
                    &owned
                }
            };
            matcher_score(&pattern, haystack).map(|score| {
                // Pump up an exact label match, otherwise the order may be
                // undesirable when many candidates share a score.
                let score = if item_label(item).eq_ignore_ascii_case(query) {
                    u32::MAX
                } else {
                    score
                };
                (score, MatchEntry::Item(idx))
            })
        })
        .collect();
    scored.sort_by(|a, b| b.0.cmp(&a.0));
    let mut order: Vec<MatchEntry> = scored.into_iter().map(|(_, entry)| entry).collect();

    // Direct matches first, then a capped peek into each group so a theme or
    // workspace name typed at the top level is still reachable without
    // drilling in — while 1000 scheme names can never bury the real commands.
    if penetrate_groups && query.chars().count() >= PENETRATION_MIN_CHARS {
        let mut nested: Vec<(u32, MatchEntry)> = Vec::new();
        for (group_idx, item) in items.iter().enumerate() {
            let PaletteItem::Group(group) = item else {
                continue;
            };
            let mut hits: Vec<(u32, usize)> = group
                .items
                .iter()
                .enumerate()
                .filter_map(|(child_idx, child)| {
                    let score = match group.haystacks.get(child_idx) {
                        Some(haystack) => matcher_score(&pattern, haystack),
                        None => matcher_score(&pattern, &item_haystack(child)),
                    };
                    score.map(|score| (score, child_idx))
                })
                .collect();
            hits.sort_by(|a, b| b.0.cmp(&a.0));
            hits.truncate(PENETRATION_PER_GROUP);
            nested.extend(
                hits.into_iter()
                    .map(|(score, child_idx)| (score, MatchEntry::Nested(group_idx, child_idx))),
            );
        }
        nested.sort_by(|a, b| b.0.cmp(&a.0));
        order.extend(nested.into_iter().map(|(_, entry)| entry));
    }

    order
}

/// Which family a command belongs to, when that family is worth folding into
/// a drill-down group. Matched on the action's shape, not its label.
fn classify_family(
    cmd: &ExpandedCommand,
    launch_menu: &[config::keyassignment::SpawnCommand],
) -> Option<PaletteGroupKind> {
    match &cmd.action {
        KeyAssignment::SwitchToWorkspace { name: Some(_), .. } => Some(PaletteGroupKind::Workspace),
        KeyAssignment::AttachDomain(_) => Some(PaletteGroupKind::DomainAttach),
        KeyAssignment::DetachDomain(SpawnTabDomain::DomainName(_)) => {
            Some(PaletteGroupKind::DomainDetach)
        }
        // launch_menu membership is checked before the domain shape: a
        // launch_menu entry may itself name a domain, and folding it into
        // "New Tab in Domain" would replace the user's label with the bare
        // domain name.
        KeyAssignment::SpawnCommandInNewTab(spawn)
            if launch_menu.iter().any(|item| item == spawn) =>
        {
            Some(PaletteGroupKind::LaunchMenu)
        }
        KeyAssignment::SpawnCommandInNewTab(spawn) => match &spawn.domain {
            SpawnTabDomain::DomainName(_) => Some(PaletteGroupKind::DomainNewTab),
            _ => None,
        },
        _ => None,
    }
}

/// Inside its group the child sheds the family boilerplate: the group header
/// already says "Attach Domain", so the row just says which one.
fn family_child(mut cmd: ExpandedCommand, kind: PaletteGroupKind) -> ExpandedCommand {
    let short = match (&cmd.action, kind) {
        (KeyAssignment::SwitchToWorkspace { name: Some(name), .. }, _) => Some(name.clone()),
        (KeyAssignment::AttachDomain(name), _) => Some(name.clone()),
        (KeyAssignment::DetachDomain(SpawnTabDomain::DomainName(name)), _) => Some(name.clone()),
        (KeyAssignment::SpawnCommandInNewTab(spawn), PaletteGroupKind::DomainNewTab) => {
            match &spawn.domain {
                SpawnTabDomain::DomainName(name) => Some(name.clone()),
                _ => None,
            }
        }
        (_, PaletteGroupKind::LaunchMenu) => cmd
            .brief
            .strip_suffix(" (New Tab)")
            .map(|label| label.to_string()),
        _ => None,
    };
    if let Some(short) = short {
        cmd.brief = short.into();
    }
    cmd
}

fn group_meta(kind: PaletteGroupKind) -> (&'static str, SvgIcon, &'static str) {
    match kind {
        PaletteGroupKind::ColorScheme => (
            "command-palette-group-theme",
            SvgIcon::Palette,
            "theme color scheme colours appearance",
        ),
        PaletteGroupKind::Space => (
            "command-palette-group-space",
            SvgIcon::SquareStack,
            "space switch spaces",
        ),
        PaletteGroupKind::Workspace => (
            "command-palette-group-workspace",
            SvgIcon::Layers,
            "workspace session switch",
        ),
        PaletteGroupKind::DomainNewTab => (
            "command-palette-group-domain-new-tab",
            SvgIcon::Plus,
            "new tab domain spawn",
        ),
        PaletteGroupKind::DomainAttach => (
            "command-palette-group-attach",
            SvgIcon::Link2,
            "attach domain connect mux",
        ),
        PaletteGroupKind::DomainDetach => (
            "command-palette-group-detach",
            SvgIcon::Unlink2,
            "detach domain disconnect mux",
        ),
        PaletteGroupKind::LaunchMenu => (
            "command-palette-group-launch",
            SvgIcon::SquareTerminal,
            "launch menu run program",
        ),
    }
}

/// `ExpandedCommand.icon` carries Nerd Font names, but the chrome draws
/// Lucide SVGs; the title/palette font has no guaranteed nerd glyph fallback,
/// so the names are mapped rather than drawn. Unmapped names leave the slot
/// empty (labels stay aligned either way).
fn nerd_icon_to_svg(name: Option<&str>) -> Option<SvgIcon> {
    Some(match name? {
        "cod_clear_all" => SvgIcon::Trash2,
        "cod_debug" => SvgIcon::Info,
        "cod_empty_window" => SvgIcon::Square,
        "cod_list_flat" => SvgIcon::ListChecks,
        "cod_multiple_windows" => SvgIcon::SquareStack,
        "cod_settings_gear" => SvgIcon::Settings,
        "cod_split_horizontal" => SvgIcon::SplitHorizontal,
        "cod_split_vertical" => SvgIcon::SplitVertical,
        "fa_long_arrow_down" => SvgIcon::ArrowDown,
        "fa_long_arrow_left" => SvgIcon::ArrowLeft,
        "fa_long_arrow_right" => SvgIcon::ArrowRight,
        "fa_long_arrow_up" => SvgIcon::ArrowUp,
        "fa_ticket" => SvgIcon::Layers,
        "md_add" => SvgIcon::Plus,
        "md_close_box_outline" => SvgIcon::X,
        "md_content_copy" => SvgIcon::Copy,
        "md_content_paste" => SvgIcon::ClipboardPaste,
        "md_delete" => SvgIcon::Trash2,
        "md_drag" => SvgIcon::ArrowRight,
        "md_edit" => SvgIcon::Pencil,
        "md_folder_open" => SvgIcon::FolderOpen,
        "md_format_align_bottom" => SvgIcon::ArrowDown,
        "md_format_align_top" => SvgIcon::ArrowUp,
        "md_format_size" => SvgIcon::SlidersHorizontal,
        "md_fullscreen" => SvgIcon::Maximize2,
        "md_help" => SvgIcon::Info,
        "md_keyboard_variant" => SvgIcon::Keyboard,
        "md_open_in_new" => SvgIcon::ExternalLink,
        "md_palette" => SvgIcon::Palette,
        "md_pipe" => SvgIcon::Link2,
        "md_pipe_disconnected" => SvgIcon::Unlink2,
        "md_reload" => SvgIcon::RotateCcw,
        "md_server_network" => SvgIcon::Server,
        "md_sticker_emoji" => SvgIcon::MessageCircle,
        "md_tab_plus" => SvgIcon::Plus,
        "md_window_minimize" => SvgIcon::Minimize2,
        "md_window_restore" => SvgIcon::Maximize2,
        "oct_browser" => SvgIcon::Globe,
        "oct_comment_discussion" => SvgIcon::MessageCircle,
        "oct_search" => SvgIcon::Search,
        "oct_stop" => SvgIcon::CircleAlert,
        "oct_terminal" => SvgIcon::Terminal,
        _ => return None,
    })
}

/// The floating-menu palette, tuned like `context_menu_palette` so the card
/// reads as native chrome in both appearances.
fn command_palette_colors(appearance: Appearance) -> UiPalette {
    let mut palette = UiPalette::for_appearance(appearance);
    match appearance {
        Appearance::Dark | Appearance::DarkHighContrast => {
            palette.control_bg = LinearRgba::with_srgba(30, 30, 32, 255);
            palette.control_hover_bg = LinearRgba::with_srgba(255, 255, 255, 255).mul_alpha(0.08);
            palette.control_border = LinearRgba::with_srgba(118, 118, 128, 255).mul_alpha(0.34);
            palette.separator = LinearRgba::with_srgba(84, 84, 88, 255).mul_alpha(0.36);
            palette.text = LinearRgba::with_srgba(242, 242, 247, 255);
            palette.secondary_text = LinearRgba::with_srgba(226, 226, 232, 255);
            palette.muted_text = LinearRgba::with_srgba(150, 150, 156, 255);
            palette.selected_bg = LinearRgba::with_srgba(70, 70, 74, 255);
        }
        Appearance::Light | Appearance::LightHighContrast => {
            palette.control_bg = LinearRgba::with_srgba(246, 246, 248, 255);
            palette.control_hover_bg = LinearRgba::with_srgba(60, 60, 67, 255).mul_alpha(0.08);
            palette.control_border = LinearRgba::with_srgba(60, 60, 67, 255).mul_alpha(0.22);
            palette.separator = LinearRgba::with_srgba(60, 60, 67, 255).mul_alpha(0.20);
        }
    }
    palette
}

enum PaletteOutcome {
    Keep,
    Close,
    Execute(ExpandedCommand),
}

/// Whether the pressed chord is the palette hotkey picked in Settings.
/// Every choice — the default included — matches directly, so the chord
/// keeps working when default key bindings are disabled or remapped, and a
/// pick like ⌘K wins over its stock clear-scrollback binding (the caller
/// intercepts ahead of both the raw and the cooked keymap lookups).
pub(crate) fn settings_hotkey_matches(key: &::window::KeyCode, mods: Modifiers) -> bool {
    use crate::native_settings::NativeCommandPaletteHotkey as Hotkey;
    let hotkey = crate::native_settings::load_shared().command_palette.hotkey;
    let (want_mods, want_char) = match hotkey {
        // The platform default: ⌘⇧P on macOS, Ctrl+Shift+P elsewhere
        // (SUPER belongs to the window manager off macOS).
        Hotkey::CmdShiftP => {
            if cfg!(target_os = "macos") {
                (Modifiers::SUPER | Modifiers::SHIFT, 'p')
            } else {
                (Modifiers::CTRL | Modifiers::SHIFT, 'p')
            }
        }
        Hotkey::CmdP => (Modifiers::SUPER, 'p'),
        Hotkey::CmdK => (Modifiers::SUPER, 'k'),
        Hotkey::CtrlShiftP => (Modifiers::CTRL | Modifiers::SHIFT, 'p'),
    };
    let chord =
        mods & (Modifiers::CTRL | Modifiers::SHIFT | Modifiers::ALT | Modifiers::SUPER);
    if chord != want_mods {
        return false;
    }
    matches!(key, ::window::KeyCode::Char(c) if c.to_ascii_lowercase() == want_char)
}

impl crate::TermWindow {
    pub(crate) fn toggle_command_palette(&mut self) {
        if self.command_palette.is_some() {
            self.close_command_palette();
        } else {
            self.open_command_palette();
        }
    }

    pub(crate) fn open_command_palette(&mut self) {
        self.close_fallback_context_menu();
        self.cancel_modal();
        // An inline tab rename owns the keyboard ahead of the palette in
        // key_event_impl; opening from the menubar while renaming would draw
        // a palette whose keystrokes all land in the tab name.
        self.finish_inline_tab_rename(true);
        self.hover_tooltip = None;
        let root = self.build_command_palette_root();
        self.command_palette = Some(CommandPaletteState::new(root));
        self.invalidate_window();
    }

    pub(crate) fn close_command_palette(&mut self) {
        if self.command_palette.take().is_some() {
            self.remove_command_palette_ui_items();
            self.invalidate_window();
        }
    }

    fn remove_command_palette_ui_items(&mut self) {
        self.last_ui_item = None;
        self.ui_items
            .retain(|item| !matches!(item.item_type, UIItemType::CommandPalette));
    }

    fn build_command_palette_root(&mut self) -> Vec<PaletteItem> {
        let start = Instant::now();

        // Showing CopyMode actions is useless unless the copy overlay is
        // active; same filter the old modal applied.
        let active_pane = self.get_active_pane_or_overlay();
        let filter_copy_mode = active_pane
            .as_ref()
            .map(|pane| {
                pane.downcast_ref::<crate::termwindow::CopyOverlay>()
                    .is_none()
            })
            .unwrap_or(true);
        let mux_pane = active_pane.map(|pane| MuxPane(pane.pane_id()));
        let commands = build_commands(GuiWin::new(self), mux_pane, filter_copy_mode);

        let launch_menu = self.config.launch_menu.clone();
        let scores = frecency_scores();

        // First pass: how large is each foldable family?
        let mut family_counts: HashMap<PaletteGroupKind, usize> = HashMap::new();
        for cmd in &commands {
            if let Some(kind) = classify_family(cmd, &launch_menu) {
                *family_counts.entry(kind).or_default() += 1;
            }
        }

        // Second pass: route large families into their group, keep the rest
        // flat in the order build_commands produced (frecency first).
        let mut families: HashMap<PaletteGroupKind, Vec<PaletteItem>> = HashMap::new();
        let mut leaves: Vec<PaletteItem> = Vec::new();
        for cmd in commands {
            match classify_family(&cmd, &launch_menu) {
                Some(kind) if family_counts.get(&kind).copied().unwrap_or(0) >= GROUP_FOLD_THRESHOLD => {
                    families
                        .entry(kind)
                        .or_default()
                        .push(PaletteItem::Leaf(family_child(cmd, kind)));
                }
                _ => leaves.push(PaletteItem::Leaf(cmd)),
            }
        }

        let mut groups: Vec<PaletteItem> = Vec::new();

        // Change Theme: synthesized, no flat equivalent exists.
        groups.push(PaletteItem::Group(self.build_color_scheme_group(&scores)));

        // Switch Space: synthesized from this window's Spaces.
        if let Some(group) = self.build_space_group() {
            groups.push(PaletteItem::Group(group));
        }

        for kind in [
            PaletteGroupKind::Workspace,
            PaletteGroupKind::DomainNewTab,
            PaletteGroupKind::DomainAttach,
            PaletteGroupKind::DomainDetach,
            PaletteGroupKind::LaunchMenu,
        ] {
            let Some(items) = families.remove(&kind) else {
                continue;
            };
            let (label_key, icon, aliases) = group_meta(kind);
            let current_value = match kind {
                PaletteGroupKind::Workspace => {
                    Mux::try_get().map(|mux| mux.active_workspace().to_string())
                }
                _ => None,
            };
            let haystacks = build_haystacks(&items);
            groups.push(PaletteItem::Group(PaletteGroup {
                label: crate::i18n::tr(label_key),
                doc: String::new(),
                icon,
                current_value,
                aliases,
                items: Rc::new(items),
                haystacks: Rc::new(haystacks),
            }));
        }

        groups.extend(leaves);
        log::trace!(
            "command palette root built in {:?} ({} entries)",
            start.elapsed(),
            groups.len()
        );
        groups
    }

    fn build_color_scheme_group(&self, scores: &HashMap<String, f64>) -> PaletteGroup {
        let mut names: Vec<String> = config::COLOR_SCHEMES.keys().cloned().collect();
        for name in self.config.color_schemes.keys() {
            if !config::COLOR_SCHEMES.contains_key(name) {
                names.push(name.clone());
            }
        }
        // Schwartzian sort: computing lowercase inside the comparator would
        // allocate 2·n·log n Strings over ~1000 names.
        let mut keyed: Vec<(f64, String, String)> = names
            .into_iter()
            .map(|name| {
                let score = scores.get(&name).copied().unwrap_or(0.0);
                let lower = name.to_lowercase();
                (score, lower, name)
            })
            .collect();
        keyed.sort_by(|a, b| {
            b.0.partial_cmp(&a.0)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| a.1.cmp(&b.1))
        });
        let names: Vec<String> = keyed.into_iter().map(|(_, _, name)| name).collect();

        let mut items: Vec<PaletteItem> = Vec::with_capacity(names.len() + 1);
        items.push(PaletteItem::Leaf(ExpandedCommand {
            brief: crate::i18n::tr("command-palette-default-scheme").into(),
            doc: String::new().into(),
            action: KeyAssignment::SetColorScheme(None),
            keys: vec![],
            menubar: &[],
            icon: None,
        }));
        for name in names {
            items.push(PaletteItem::Leaf(ExpandedCommand {
                brief: name.clone().into(),
                doc: String::new().into(),
                action: KeyAssignment::SetColorScheme(Some(name)),
                keys: vec![],
                menubar: &[],
                icon: None,
            }));
        }

        let (label_key, icon, aliases) = group_meta(PaletteGroupKind::ColorScheme);
        let haystacks = build_haystacks(&items);
        PaletteGroup {
            label: crate::i18n::tr(label_key),
            doc: String::new(),
            icon,
            current_value: Some(
                self.config
                    .color_scheme
                    .clone()
                    .unwrap_or_else(|| crate::i18n::tr("command-palette-value-default")),
            ),
            aliases,
            items: Rc::new(items),
            haystacks: Rc::new(haystacks),
        }
    }

    fn build_space_group(&self) -> Option<PaletteGroup> {
        let spaces = crate::workspace_threads::spaces_for_window(self.space_owner_id);
        if spaces.len() < GROUP_FOLD_THRESHOLD {
            return None;
        }
        let current = spaces
            .iter()
            .find(|space| space.is_active)
            .map(|space| space.name.clone());
        let items: Vec<PaletteItem> = spaces
            .into_iter()
            .map(|space| {
                PaletteItem::Leaf(ExpandedCommand {
                    brief: space.name.into(),
                    doc: String::new().into(),
                    action: KeyAssignment::SwitchSpace(space.id),
                    keys: vec![],
                    menubar: &[],
                    icon: None,
                })
            })
            .collect();
        let (label_key, icon, aliases) = group_meta(PaletteGroupKind::Space);
        let haystacks = build_haystacks(&items);
        Some(PaletteGroup {
            label: crate::i18n::tr(label_key),
            doc: String::new(),
            icon,
            current_value: current,
            aliases,
            items: Rc::new(items),
            haystacks: Rc::new(haystacks),
        })
    }

    fn execute_palette_command(&mut self, cmd: ExpandedCommand) {
        if let Err(err) = save_recent(&cmd) {
            log::error!("Error while saving recents: {err:#}");
        }
        if let Some(pane) = self.get_active_pane_or_overlay() {
            if let Err(err) = self.perform_key_assignment(&pane, &cmd.action) {
                log::error!("Error while performing {:?}: {err:#}", cmd.action);
            }
        }
    }

    fn command_palette_paste(&mut self) {
        let Some(window) = self.window.as_ref().cloned() else {
            return;
        };
        let Some(generation) = self.command_palette.as_ref().map(|state| state.generation)
        else {
            return;
        };
        let future = window.get_clipboard(Clipboard::Clipboard);
        promise::spawn::spawn(async move {
            if let Ok(text) = future.await {
                window.notify(TermWindowNotif::Apply(Box::new(move |term_window| {
                    if let Some(state) = term_window.command_palette.as_mut() {
                        // Only the palette this paste was started against; a
                        // close-and-reopen before the clipboard resolved must
                        // not receive the text.
                        if state.generation == generation {
                            let cleaned: String =
                                text.chars().filter(|c| !c.is_control()).collect();
                            state.search.caret_insert(&cleaned, false);
                            state.invalidate_matches();
                        }
                    }
                    term_window.invalidate_window();
                })));
            }
        })
        .detach();
    }

    pub(crate) fn command_palette_key(
        &mut self,
        key: KeyCode,
        mods: Modifiers,
        context: &dyn WindowOps,
    ) {
        let Some(mut state) = self.command_palette.take() else {
            return;
        };
        let mut outcome = PaletteOutcome::Keep;
        let mut wants_paste = false;
        let mut copy_text: Option<String> = None;

        let edit = EditModifiers::from(mods);
        let shift = edit.shift;
        let macos = cfg!(target_os = "macos");
        let chord_mods = mods
            & (Modifiers::CTRL | Modifiers::SHIFT | Modifiers::ALT | Modifiers::SUPER);
        let is_toggle_chord = matches!(key, KeyCode::Char('p') | KeyCode::Char('P'))
            && (chord_mods == Modifiers::CTRL | Modifiers::SHIFT
                || chord_mods == Modifiers::SUPER | Modifiers::SHIFT);

        // Sequential blocks with fall-through, like handle_inline_tab_rename_key:
        // off macOS a bare Ctrl is BOTH the command and the word chord, so a
        // key the command block does not claim (Ctrl+←/→/⌫) must still reach
        // the word block. An else-if chain would swallow it.
        'dispatch: {
            if is_toggle_chord
                || matches!(key, KeyCode::Escape)
                || (matches!(key, KeyCode::Char('g')) && chord_mods == Modifiers::CTRL)
            {
                outcome = PaletteOutcome::Close;
                break 'dispatch;
            }
            if chord_mods == Modifiers::CTRL
                && matches!(
                    key,
                    KeyCode::Char('u') | KeyCode::Char('p') | KeyCode::Char('n')
                )
            {
                // Checked before the command chord: off macOS, Ctrl doubles as
                // the command modifier and would swallow these.
                match key {
                    KeyCode::Char('u') => {
                        state.search.clear();
                        state.invalidate_matches();
                    }
                    KeyCode::Char('p') => {
                        state.ensure_matches();
                        state.move_selection_by(-1);
                    }
                    KeyCode::Char('n') => {
                        state.ensure_matches();
                        state.move_selection_by(1);
                    }
                    _ => {}
                }
                break 'dispatch;
            }
            if edit.command {
                let mut handled = true;
                match key {
                    KeyCode::Char('a') | KeyCode::Char('A') => state.search.caret_select_all(),
                    KeyCode::Char('c') | KeyCode::Char('C') => {
                        copy_text = state.search.caret_selected_text();
                    }
                    KeyCode::Char('x') | KeyCode::Char('X') => {
                        copy_text = state.search.caret_take_selected_text();
                        state.invalidate_matches();
                    }
                    KeyCode::Char('v') | KeyCode::Char('V') => wants_paste = true,
                    KeyCode::LeftArrow if macos => state.search.caret_move_home(shift),
                    KeyCode::RightArrow if macos => state.search.caret_move_end(shift),
                    KeyCode::Backspace if macos => {
                        state.search.caret_delete_to_start();
                        state.invalidate_matches();
                    }
                    _ => handled = false,
                }
                if handled {
                    break 'dispatch;
                }
            }
            if edit.word
                && matches!(
                    key,
                    KeyCode::LeftArrow | KeyCode::RightArrow | KeyCode::Backspace
                )
            {
                match key {
                    KeyCode::LeftArrow => state.search.caret_word_left(shift),
                    KeyCode::RightArrow => state.search.caret_word_right(shift),
                    KeyCode::Backspace => {
                        state.search.caret_delete_word_back();
                        state.invalidate_matches();
                    }
                    _ => {}
                }
                break 'dispatch;
            }
            if !edit.plain() {
                break 'dispatch;
            }
            match key {
                KeyCode::UpArrow => {
                    state.ensure_matches();
                    state.move_selection_by(-1);
                }
                KeyCode::DownArrow => {
                    state.ensure_matches();
                    state.move_selection_by(1);
                }
                KeyCode::PageUp => {
                    state.ensure_matches();
                    state.move_selection_by(-(state.layout.visible_rows.max(1) as isize));
                }
                KeyCode::PageDown => {
                    state.ensure_matches();
                    state.move_selection_by(state.layout.visible_rows.max(1) as isize);
                }
                KeyCode::Home if state.search.is_empty() => {
                    state.ensure_matches();
                    state.set_selection(0);
                }
                KeyCode::End if state.search.is_empty() => {
                    state.ensure_matches();
                    state.set_selection(state.order_len().saturating_sub(1));
                }
                KeyCode::Home => state.search.caret_move_home(shift),
                KeyCode::End => state.search.caret_move_end(shift),
                KeyCode::LeftArrow => state.search.caret_move_left(shift),
                KeyCode::RightArrow => state.search.caret_move_right(shift),
                KeyCode::Enter => {
                    state.ensure_matches();
                    let selected = state.selected;
                    match state.classify_entry(selected) {
                        Some(EntryKind::Group(item_idx)) => state.enter_group(item_idx),
                        Some(EntryKind::Leaf) => {
                            if let Some(cmd) = state.command_for_entry(selected) {
                                outcome = PaletteOutcome::Execute(cmd);
                            }
                        }
                        None => {}
                    }
                }
                KeyCode::Tab => {
                    state.ensure_matches();
                    let selected = state.selected;
                    if let Some(EntryKind::Group(item_idx)) = state.classify_entry(selected) {
                        state.enter_group(item_idx);
                    }
                }
                KeyCode::Backspace => {
                    if state.backspace_pops() {
                        state.pop_frame();
                    } else {
                        state.search.caret_backspace();
                        state.invalidate_matches();
                    }
                }
                KeyCode::Delete => {
                    state.search.caret_delete_forward();
                    state.invalidate_matches();
                }
                KeyCode::Char(c) if !c.is_control() => {
                    state.search.caret_insert(&c.to_string(), false);
                    state.invalidate_matches();
                }
                _ => {}
            }
        }

        match outcome {
            PaletteOutcome::Keep => {
                self.command_palette = Some(state);
            }
            PaletteOutcome::Close => {
                self.remove_command_palette_ui_items();
            }
            PaletteOutcome::Execute(cmd) => {
                self.remove_command_palette_ui_items();
                self.execute_palette_command(cmd);
            }
        }
        if let Some(text) = copy_text {
            if !text.is_empty() {
                self.copy_to_clipboard(ClipboardCopyDestination::Clipboard, text);
            }
        }
        if wants_paste {
            self.command_palette_paste();
        }
        context.invalidate();
    }

    /// IME / composed input (CJK etc.) lands here instead of as a Char key.
    pub(crate) fn command_palette_text(&mut self, text: &str, context: &dyn WindowOps) {
        if let Some(state) = self.command_palette.as_mut() {
            let cleaned: String = text.chars().filter(|c| !c.is_control()).collect();
            if !cleaned.is_empty() {
                state.search.caret_insert(&cleaned, false);
                state.invalidate_matches();
                context.invalidate();
            }
        }
    }

    /// The palette owns all pointer traffic while open, exactly like the
    /// fallback context menu. Every branch mutates state and invalidates —
    /// the old modal's silently-empty `mouse_event` is the failure mode this
    /// replaces.
    pub(crate) fn mouse_event_command_palette(
        &mut self,
        event: &MouseEvent,
        context: &dyn WindowOps,
    ) -> bool {
        let Some(mut state) = self.command_palette.take() else {
            return false;
        };
        let x = event.coords.x as f32;
        let y = event.coords.y as f32;
        let mut outcome = PaletteOutcome::Keep;

        match event.kind {
            MouseEventKind::Move => {
                let hit = state.widgets.hit_test(x, y).map(|target| target.action);
                let cursor = match hit {
                    Some(PaletteAction::Row(_)) | Some(PaletteAction::Back) => MouseCursor::Hand,
                    Some(PaletteAction::Search) => MouseCursor::Text,
                    None => MouseCursor::Arrow,
                };
                context.set_cursor(Some(cursor));
                if state.interaction.hovered != hit {
                    state.interaction.hovered = hit;
                    context.invalidate();
                }
                if state.dragging_search {
                    state.pending_caret_click = Some((x, true));
                    context.invalidate();
                }
            }
            MouseEventKind::Press(MousePress::Left) => {
                let hit = state.widgets.hit_test(x, y).map(|target| target.action);
                if hit.is_none() && !contains(state.layout.card, x, y) {
                    // Click-away closes, like Spotlight.
                    outcome = PaletteOutcome::Close;
                } else {
                    state.interaction.pressed = hit;
                    if hit == Some(PaletteAction::Search) {
                        state.dragging_search = true;
                        state.pending_caret_click =
                            Some((x, event.modifiers.contains(Modifiers::SHIFT)));
                    }
                    context.invalidate();
                }
            }
            MouseEventKind::Release(MousePress::Left) => {
                state.dragging_search = false;
                let hit = state.widgets.hit_test(x, y).map(|target| target.action);
                let pressed = state.interaction.pressed.take();
                if hit.is_some() && hit == pressed {
                    match hit.unwrap() {
                        PaletteAction::Row(order_idx) => {
                            // Only act while the painted order is still
                            // current; recomputing here would renumber the
                            // rows out from under the click.
                            if !state.matches_fresh() {
                                self.command_palette = Some(state);
                                context.invalidate();
                                return true;
                            }
                            match state.classify_entry(order_idx) {
                                Some(EntryKind::Group(item_idx)) => state.enter_group(item_idx),
                                Some(EntryKind::Leaf) => {
                                    if let Some(cmd) = state.command_for_entry(order_idx) {
                                        outcome = PaletteOutcome::Execute(cmd);
                                    }
                                }
                                None => {}
                            }
                        }
                        PaletteAction::Back => {
                            state.pop_frame();
                        }
                        PaletteAction::Search => {}
                    }
                }
                context.invalidate();
            }
            MouseEventKind::VertWheel(_) | MouseEventKind::HorzWheel(_) => {
                if apply_wheel_to_area(
                    event,
                    state.layout.list,
                    &mut state.scroll,
                    state.last_ui_scale,
                ) {
                    context.invalidate();
                }
            }
            MouseEventKind::Press(MousePress::Right) | MouseEventKind::Press(MousePress::Middle) => {
            }
            MouseEventKind::Release(_) => {
                state.interaction.pressed = None;
            }
        }

        match outcome {
            PaletteOutcome::Keep => {
                self.command_palette = Some(state);
            }
            PaletteOutcome::Close => {
                self.remove_command_palette_ui_items();
                context.set_cursor(Some(MouseCursor::Arrow));
                context.invalidate();
            }
            PaletteOutcome::Execute(cmd) => {
                self.remove_command_palette_ui_items();
                self.execute_palette_command(cmd);
                context.set_cursor(Some(MouseCursor::Arrow));
                context.invalidate();
            }
        }
        true
    }

    fn command_palette_cursor_on(&self) -> bool {
        // cursor_blink_rate = 0 means "no blinking": a steady caret and no
        // repaint timer.
        if self.config.cursor_blink_rate == 0 {
            return true;
        }
        let blink_ms = (self.config.cursor_blink_rate as u64).max(100);
        self.update_next_frame_time(Some(Instant::now() + Duration::from_millis(blink_ms)));
        let ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis())
            .unwrap_or(0);
        (ms / blink_ms as u128) % 2 == 0
    }

    pub(crate) fn paint_command_palette(&mut self) -> anyhow::Result<()> {
        if self.command_palette.is_none() {
            return Ok(());
        }
        // A modal (CharSelect, PromptInputLine, …) opened over the palette —
        // via Lua or the menubar — would be unreachable behind the palette's
        // input takeover. The modal wins; the palette bows out.
        if self.modal.borrow().is_some() {
            self.close_command_palette();
            return Ok(());
        }
        let mut state = self.command_palette.take().unwrap();
        let result = self.paint_command_palette_impl(&mut state);
        self.command_palette = Some(state);
        result
    }

    fn paint_command_palette_impl(
        &mut self,
        state: &mut CommandPaletteState,
    ) -> anyhow::Result<()> {
        state.ensure_matches();
        state.widgets.clear();

        if state.scroll.advance_animation(Instant::now()) {
            self.update_next_frame_time(Some(Instant::now() + Duration::from_millis(8)));
        }
        let cursor_on = self.command_palette_cursor_on();

        let native = crate::native_settings::load_shared();
        let base_font_size = native
            .command_palette
            .font_size
            .unwrap_or(self.config.command_palette_font_size)
            .clamp(8.0, 32.0);
        let list_font = self
            .fonts
            .command_palette_font_with_size(base_font_size)
            .context("command palette font")?;
        let search_font = self
            .fonts
            .command_palette_font_with_size(base_font_size * 1.35)
            .context("command palette search font")?;
        let list_metrics = RenderMetrics::with_font_metrics(&list_font.metrics());
        let search_metrics = RenderMetrics::with_font_metrics(&search_font.metrics());

        let mut palette =
            command_palette_colors(crate::native_settings::effective_appearance());
        // The legacy config colors default to a dark-only pairing that would
        // wreck light mode; honour them only when the user changed them.
        let fg = self.config.command_palette_fg_color.to_linear();
        let bg = self.config.command_palette_bg_color.to_linear();
        if fg != config::RgbaColor::from(termwiz::color::SrgbaTuple(0.75, 0.75, 0.75, 1.0))
            .to_linear()
        {
            // Rows draw with secondary/muted, so a configured foreground
            // (e.g. a high-contrast palette) must reach those too, like the
            // old modal painted its whole command list in this color.
            palette.text = fg;
            palette.secondary_text = fg;
            palette.muted_text = fg.mul_alpha(0.7);
        }
        if bg != config::RgbaColor::from((0x33u8, 0x33u8, 0x33u8)).to_linear() {
            palette.control_bg = bg;
        }

        // The backdrop item is what routes every pointer event to the palette
        // dispatcher and keeps a click from leaking into the pane underneath.
        // Pushed before the render-state borrow below, because `ui_items` is
        // on the same struct. Only the stale items are dropped here —
        // clearing `last_ui_item` too (as the close path does) would break
        // enter/leave bookkeeping on every frame.
        self.ui_items
            .retain(|item| !matches!(item.item_type, UIItemType::CommandPalette));
        self.ui_items.push(UIItem {
            x: 0,
            y: 0,
            width: self.dimensions.pixel_width,
            height: self.dimensions.pixel_height,
            item_type: UIItemType::CommandPalette,
        });

        let order_len = state.order_len();
        let dark = matches!(
            crate::native_settings::effective_appearance(),
            Appearance::Dark | Appearance::DarkHighContrast
        );

        let gl_state = self.render_state.as_ref().unwrap();
        let layer = gl_state
            .layer_for_zindex(COMMAND_PALETTE_ZINDEX)
            .context("command palette layer")?;
        let mut layers = layer.quad_allocator();
        let ctx = DrawContext::new(gl_state, self.dimensions, &list_metrics);
        let search_ctx = DrawContext::new(gl_state, self.dimensions, &search_metrics);

        let win_w = self.dimensions.pixel_width as f32;
        let win_h = self.dimensions.pixel_height as f32;

        // Scrim over the whole window.
        let scrim_alpha = if dark { 0.28 } else { 0.16 };
        ctx.draw_rect(
            &mut layers,
            0,
            0.0,
            0.0,
            win_w,
            win_h,
            LinearRgba::with_components(0.0, 0.0, 0.0, scrim_alpha),
        )?;

        // Card geometry. Spotlight sits high, not centred.
        let mut card_w = (win_w * 0.5).max(ctx.px(CARD_MIN_W)).min(ctx.px(CARD_MAX_W));
        card_w = card_w.min((win_w - ctx.px(CARD_MARGIN)).max(ctx.px(320.0)));
        let card_x = ((win_w - card_w) / 2.0).round().max(0.0);
        let card_y = (win_h * 0.16).max(ctx.px(CARD_TOP_MIN)).round();

        let search_h = (search_metrics.cell_size.height as f32 + ctx.px(SEARCH_EXTRA_H))
            .max(ctx.px(SEARCH_MIN_H));
        let row_h =
            (list_metrics.cell_size.height as f32 + ctx.px(ROW_EXTRA_H)).max(ctx.px(ROW_MIN_H));

        let mut max_visible =
            (((win_h * LIST_MAX_FRACTION - search_h) / row_h).floor() as usize).max(1);
        if native.command_palette.rows > 0 {
            max_visible = max_visible.min(native.command_palette.rows as usize);
        } else if let Some(rows) = self.config.command_palette_rows {
            max_visible = max_visible.min(rows.max(1));
        }
        let shown_rows = order_len.min(max_visible);
        // The card shrinks to its contents; no empty rows, but always enough
        // room for the empty-state line.
        let list_h = shown_rows.max(1) as f32 * row_h;
        let card_h =
            search_h + 1.0 + ctx.px(LIST_TOP_PAD) + list_h + ctx.px(CARD_BOTTOM_PAD);
        let card = euclid::rect(card_x, card_y, card_w, card_h);

        ctx.draw_elevated_surface(
            &mut layers,
            0,
            card,
            palette.control_bg,
            palette.control_border,
            LinearRgba::with_components(0.0, 0.0, 0.0, 1.0),
            ctx.px(CARD_RADIUS),
        )?;

        // ---- Search row -----------------------------------------------------
        // Registered before the breadcrumb chip: hit_test picks the most
        // recently pushed target, so the chip must land after the full-row
        // search field to stay clickable.
        let search_field = euclid::rect(card_x, card_y, card_w, search_h);
        state
            .widgets
            .push(search_field, WidgetKind::TextInput, PaletteAction::Search);

        let mut cursor_x = card_x + ctx.px(PAD_X);

        // Breadcrumb chip while drilled into a group.
        if let Some(frame) = state.stack.last() {
            let chip_font = &list_font;
            let label_w = self.sidebar_text_width(chip_font, &frame.label)?;
            let chip_h = (list_metrics.cell_size.height as f32 + ctx.px(18.0))
                .min(search_h - ctx.px(16.0));
            let chip_w =
                ctx.px(BACK_CHIP_PAD_X) * 2.0 + ctx.px(BACK_CHIP_ICON) + ctx.px(8.0) + label_w;
            let chip_y = card_y + (search_h - chip_h) / 2.0;
            let chip = euclid::rect(cursor_x, chip_y, chip_w, chip_h);
            let chip_bg = if state.interaction.hovered == Some(PaletteAction::Back) {
                palette.control_hover_bg
            } else {
                palette.sidebar_row_hover_bg
            };
            ctx.draw_rounded_rect(
                &mut layers,
                0,
                chip.origin.x,
                chip.origin.y,
                chip.size.width,
                chip.size.height,
                chip_bg,
                chip_h / 2.0,
            )?;
            ctx.draw_svg_icon(
                &mut layers,
                SvgIcon::ArrowLeft,
                chip.origin.x + ctx.px(BACK_CHIP_PAD_X),
                chip.origin.y + (chip_h - ctx.px(BACK_CHIP_ICON)) / 2.0,
                ctx.px(BACK_CHIP_ICON),
                palette.secondary_text,
            )?;
            self.paint_sidebar_text(
                &mut layers,
                chip_font,
                list_metrics,
                &frame.label,
                (chip.origin.x + ctx.px(BACK_CHIP_PAD_X) + ctx.px(BACK_CHIP_ICON) + ctx.px(8.0))
                    as usize,
                (chip.origin.y + (chip_h - list_metrics.cell_size.height as f32) / 2.0).round()
                    as usize,
                (label_w + ctx.px(4.0)) as usize,
                palette.secondary_text,
            )?;
            state.widgets.push(chip, WidgetKind::Button, PaletteAction::Back);
            cursor_x += chip_w + ctx.px(BACK_CHIP_GAP);
        } else {
            ctx.draw_svg_icon(
                &mut layers,
                SvgIcon::Search,
                cursor_x,
                card_y + (search_h - ctx.px(SEARCH_ICON_SIZE)) / 2.0,
                ctx.px(SEARCH_ICON_SIZE),
                palette.muted_text,
            )?;
            cursor_x += ctx.px(SEARCH_ICON_SIZE) + ctx.px(LABEL_GAP);
        }

        let text_left = cursor_x;
        let text_width = (card_x + card_w - ctx.px(PAD_X) - text_left).max(0.0);

        // A recorded click becomes a caret position now that the font and the
        // text origin exist.
        if let Some((click_x, extend)) = state.pending_caret_click.take() {
            let idx = char_index_for_x(
                &search_ctx,
                &search_font,
                state.search.text(),
                click_x - text_left,
            );
            state.search.caret_set(idx, extend);
        }

        // System accent blue in both modes: the dark palette's selected_bg is
        // a grey that vanishes against the dark card.
        let accent = if dark {
            LinearRgba::with_srgba(10, 132, 255, 255)
        } else {
            LinearRgba::with_srgba(0, 122, 255, 255)
        };

        let query = state.search.text().to_string();
        let search_text_y = card_y + (search_h - search_metrics.cell_size.height as f32) / 2.0;
        if let Some((sel_start, sel_end)) = state.search.caret_selection_range() {
            let start_x =
                text_width_to_char(&search_ctx, &search_font, &query, sel_start).min(text_width);
            let end_x =
                text_width_to_char(&search_ctx, &search_font, &query, sel_end).min(text_width);
            let inset_y = ctx.px(10.0);
            ctx.draw_rounded_rect(
                &mut layers,
                0,
                text_left + start_x - ctx.px(3.0),
                card_y + inset_y,
                (end_x - start_x) + ctx.px(6.0),
                search_h - inset_y * 2.0,
                accent.mul_alpha(0.40),
                ctx.px(6.0),
            )?;
        }
        if query.is_empty() {
            search_ctx.draw_text(
                &mut layers,
                &search_font,
                text_left,
                search_text_y,
                &crate::i18n::tr("command-palette-placeholder"),
                palette.muted_text,
                text_width,
            )?;
        } else {
            search_ctx.draw_text(
                &mut layers,
                &search_font,
                text_left,
                search_text_y,
                &query,
                palette.text,
                text_width,
            )?;
        }
        if cursor_on && state.search.caret_selection_range().is_none() {
            let caret_dx = text_width_to_char(&search_ctx, &search_font, &query, state.search.cursor)
                .min(text_width);
            let inset_y = ctx.px(12.0);
            ctx.draw_rect(
                &mut layers,
                1,
                text_left + caret_dx,
                card_y + inset_y,
                ctx.px(CARET_WIDTH),
                search_h - inset_y * 2.0,
                accent,
            )?;
        }

        // Separator between the search row and the list.
        ctx.draw_rect(
            &mut layers,
            0,
            card_x + 1.0,
            card_y + search_h,
            card_w - 2.0,
            1.0,
            palette.separator,
        )?;

        // ---- Rows -----------------------------------------------------------
        // The top pad belongs to the scrollable CONTENT, not the viewport:
        // the clip line sits right under the separator, at rest the first
        // row starts a pad below it, and a scrolled row is cut flush at the
        // separator instead of at a floating line a pad below it.
        let top_pad = ctx.px(LIST_TOP_PAD);
        let list_y = card_y + search_h + 1.0;
        let viewport_h = top_pad + list_h;
        let list = euclid::rect(card_x, list_y, card_w, viewport_h);

        state
            .scroll
            .set_extents(viewport_h, top_pad + order_len as f32 * row_h);
        if let Some(offset) = state.pending_scroll.take() {
            state.scroll.scroll_by(offset - state.scroll.offset);
        }
        // Continuous (stepless) scrolling: rows render at fractional offsets
        // into a heap buffer that is then clipped to the list viewport, the
        // same mechanism the rounded terminal previews use. The first row
        // whose bottom can reach the viewport starts the loop.
        let offset = state.scroll.offset;
        let first = (((offset - top_pad) / row_h).floor().max(0.0)) as usize;

        if order_len == 0 {
            let label = crate::i18n::tr("command-palette-empty");
            let label_w = ctx.measure_text_width(&list_font, &label);
            ctx.draw_text(
                &mut layers,
                &list_font,
                card_x + (card_w - label_w).max(0.0) / 2.0,
                list_y + top_pad + (row_h - list_metrics.cell_size.height as f32) / 2.0,
                &label,
                palette.muted_text,
                card_w,
            )?;
        }

        let items = if let Some(frame) = state.stack.last() {
            frame.items.as_slice()
        } else {
            state.root.as_slice()
        };
        let order = state
            .matches
            .as_ref()
            .map(|m| m.order.as_slice())
            .unwrap_or(&[]);

        let mut row_heap = crate::quad::HeapQuadAllocator::default();
        let mut row_layers = crate::quad::TripleLayerQuadAllocator::Heap(&mut row_heap);

        // Runs until a row starts past the viewport; the clip pass trims the
        // partial rows at both edges.
        for order_idx in first.. {
            let Some(entry) = order.get(order_idx).copied() else {
                break;
            };
            let row_y = list_y + top_pad + order_idx as f32 * row_h - offset;
            if row_y >= list_y + viewport_h {
                break;
            }
            let row_rect = euclid::rect(card_x, row_y, card_w, row_h);
            let selected = order_idx == state.selected;
            let hovered =
                state.interaction.hovered == Some(PaletteAction::Row(order_idx)) && !selected;

            if selected || hovered {
                let highlight = if selected {
                    palette.selected_bg
                } else {
                    palette.control_hover_bg
                };
                ctx.draw_rounded_rect(
                    &mut row_layers,
                    0,
                    card_x + ctx.px(ROW_INSET_X),
                    row_y + ctx.px(ROW_INSET_Y),
                    card_w - ctx.px(ROW_INSET_X) * 2.0,
                    row_h - ctx.px(ROW_INSET_Y) * 2.0,
                    highlight,
                    ctx.px(ROW_RADIUS),
                )?;
            }

            let text_color = if selected {
                palette.selected_text
            } else {
                palette.secondary_text
            };
            let dim_color = if selected {
                palette.selected_text.mul_alpha(0.7)
            } else {
                palette.muted_text
            };

            // Resolve the row's parts. Total lookups: the order was computed
            // from these same items, but a panic on a future drift is not
            // worth the two saved branches.
            let (icon, prefix, label, accessory_value, chevron, key_label) = match entry {
                MatchEntry::Item(item_idx) => match items.get(item_idx) {
                    Some(PaletteItem::Leaf(cmd)) => {
                        let prefix = if cmd.menubar.is_empty() {
                            None
                        } else {
                            Some(format!("{} › ", cmd.menubar.join(" › ")))
                        };
                        (
                            nerd_icon_to_svg(cmd.icon.as_deref()),
                            prefix,
                            cmd.brief.to_string(),
                            None,
                            false,
                            format_key_label(cmd, &self.config),
                        )
                    }
                    Some(PaletteItem::Group(group)) => (
                        Some(group.icon),
                        None,
                        group.label.clone(),
                        group.current_value.clone(),
                        true,
                        None,
                    ),
                    None => continue,
                },
                MatchEntry::Nested(group_idx, child_idx) => {
                    let Some(PaletteItem::Group(group)) = items.get(group_idx) else {
                        continue;
                    };
                    let Some(PaletteItem::Leaf(cmd)) = group.items.get(child_idx) else {
                        continue;
                    };
                    (
                        Some(group.icon),
                        Some(format!("{} › ", group.label)),
                        cmd.brief.to_string(),
                        None,
                        false,
                        format_key_label(cmd, &self.config),
                    )
                }
            };

            // Icon slot (always reserved so labels align).
            let icon_x = card_x + ctx.px(PAD_X);
            if let Some(icon) = icon {
                ctx.draw_svg_icon(
                    &mut row_layers,
                    icon,
                    icon_x,
                    row_y + (row_h - ctx.px(ICON_SIZE)) / 2.0,
                    ctx.px(ICON_SIZE),
                    text_color,
                )?;
            }
            let label_x = icon_x + ctx.px(ICON_SLOT);

            // Right-aligned accessory: shortcut, or current value + chevron.
            let mut accessory_right = card_x + card_w - ctx.px(PAD_X);
            if chevron {
                let chev = ctx.px(CHEVRON_SIZE);
                ctx.draw_svg_icon(
                    &mut row_layers,
                    SvgIcon::ChevronRight,
                    accessory_right - chev,
                    row_y + (row_h - chev) / 2.0,
                    chev,
                    dim_color,
                )?;
                accessory_right -= chev + ctx.px(8.0);
            }
            let accessory_text = accessory_value.or(key_label);
            if let Some(text) = &accessory_text {
                let max_acc = (card_w * 0.35).max(ctx.px(80.0));
                // Shaping goes through the shared UI shape cache
                // (sidebar_text_width / paint_ui_title_text_cached): the
                // DrawContext painters re-run HarfBuzz on every call, which
                // at rows × frames is exactly the per-frame shaping cost
                // that made the sidebar search list crawl.
                let shown = self.ellipsize_ui_text(&list_font, text, max_acc as usize)?;
                let acc_w = self.sidebar_text_width(&list_font, &shown)?;
                self.paint_ui_title_text_cached(
                    &mut row_layers,
                    &list_font,
                    &list_metrics,
                    &shown,
                    (accessory_right - acc_w).max(0.0) as usize,
                    (row_y + (row_h - list_metrics.cell_size.height as f32) / 2.0).round()
                        as usize,
                    (acc_w + ctx.px(2.0)) as usize,
                    dim_color,
                )?;
                accessory_right -= acc_w;
            }

            // Label, with an optional dimmed menu-path prefix.
            let label_max = (accessory_right - ctx.px(LABEL_GAP) - label_x).max(0.0);
            let text_y = row_y + (row_h - list_metrics.cell_size.height as f32) / 2.0;
            let mut label_cursor = label_x;
            if let Some(prefix) = &prefix {
                let prefix_w = self.sidebar_text_width(&list_font, prefix)?;
                if prefix_w < label_max * 0.6 {
                    self.paint_sidebar_text(
                        &mut row_layers,
                        &list_font,
                        list_metrics,
                        prefix,
                        label_cursor as usize,
                        text_y.round() as usize,
                        label_max as usize,
                        dim_color,
                    )?;
                    label_cursor += prefix_w;
                }
            }
            self.paint_sidebar_text(
                &mut row_layers,
                &list_font,
                list_metrics,
                &label,
                label_cursor as usize,
                text_y.round() as usize,
                (label_max - (label_cursor - label_x)).max(0.0) as usize,
                text_color,
            )?;

            // Hit target clamped to the viewport: the sliver of a partial row
            // hidden under the search bar or the bottom edge must not be
            // clickable there.
            if let Some(hit_rect) = row_rect.intersection(&list) {
                state
                    .widgets
                    .push(hit_rect, WidgetKind::SidebarRow, PaletteAction::Row(order_idx));
            }
        }

        // Emit the buffered rows clipped to the list viewport; a row crossing
        // either edge keeps exactly its visible part.
        drop(row_layers);
        let clip = crate::quad::QuadClipRect::from_top_left_pixels(
            list.min_x(),
            list.min_y(),
            list.max_x(),
            list.max_y(),
            &self.dimensions,
        );
        row_heap.apply_to_clipped(&mut layers, clip, 1.0)?;

        // Auto-hiding scrollbar: appears on scroll activity, lingers, fades.
        if (state.scroll.offset - state.last_scroll_offset).abs() > 0.5 {
            state.scroll_active_at = Some(Instant::now());
        }
        state.last_scroll_offset = state.scroll.offset;
        if state.scroll.has_overflow() {
            let alpha = match state.scroll_active_at {
                Some(active_at) => {
                    let idle = active_at.elapsed().as_secs_f32();
                    if idle <= SCROLLBAR_LINGER {
                        1.0
                    } else {
                        (1.0 - (idle - SCROLLBAR_LINGER) / SCROLLBAR_FADE).clamp(0.0, 1.0)
                    }
                }
                None => 0.0,
            };
            if alpha > 0.0 {
                let mut bar_palette = palette;
                bar_palette.scrollbar_thumb = palette.scrollbar_thumb.mul_alpha(alpha);
                let tokens = UiTokens::for_dpi(self.dimensions.dpi);
                crate::ui::draw_scrollbar(
                    &ctx,
                    &mut layers,
                    bar_palette,
                    tokens,
                    list,
                    state.scroll,
                )?;
                // Keep frames coming until the fade completes.
                self.update_next_frame_time(Some(
                    Instant::now() + Duration::from_millis(50),
                ));
            }
        }

        drop(layers);

        state.layout = PaletteLayout {
            card,
            list,
            row_height: row_h,
            visible_rows: shown_rows.max(1),
            top_pad,
        };
        state.last_ui_scale = ctx.scale();

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn leaf(brief: &str) -> PaletteItem {
        PaletteItem::Leaf(ExpandedCommand {
            brief: brief.to_string().into(),
            doc: "".into(),
            action: KeyAssignment::Nop,
            keys: vec![],
            menubar: &[],
            icon: None,
        })
    }

    fn group(label: &str, children: Vec<PaletteItem>) -> PaletteItem {
        let haystacks = build_haystacks(&children);
        PaletteItem::Group(PaletteGroup {
            label: label.to_string(),
            doc: String::new(),
            icon: SvgIcon::Palette,
            current_value: None,
            aliases: "",
            items: Rc::new(children),
            haystacks: Rc::new(haystacks),
        })
    }

    fn state_with(items: Vec<PaletteItem>) -> CommandPaletteState {
        let mut state = CommandPaletteState::new(items);
        state.ensure_matches();
        state
    }

    #[test]
    fn backspace_only_pops_when_the_query_is_empty() {
        let mut state = state_with(vec![group("Change Theme", vec![leaf("Merino Light")])]);
        state.enter_group(0);
        assert_eq!(state.stack.len(), 1);

        state.search.caret_insert("mer", false);
        assert!(!state.backspace_pops());

        state.search.clear();
        assert!(state.backspace_pops());
    }

    #[test]
    fn backing_out_restores_the_parent_query_and_selection() {
        let mut state = state_with(vec![
            leaf("New Tab"),
            group("Change Theme", vec![leaf("Merino Light"), leaf("Dracula")]),
        ]);
        state.search.caret_insert("theme", false);
        state.ensure_matches();
        state.selected = 0;
        state.enter_group(1);
        assert!(state.search.is_empty());

        state.search.caret_insert("dra", false);
        state.ensure_matches();
        assert!(state.pop_frame());
        assert_eq!(state.search.text(), "theme");
        assert!(state.stack.is_empty());
    }

    /// The parent's scroll offset must survive the round trip through a
    /// group even though the child frame left different scroll extents
    /// behind: restoring it immediately would be clamped to the child's
    /// max_offset, so it is deferred to the next paint via pending_scroll.
    #[test]
    fn backing_out_defers_the_scroll_restore_past_stale_extents() {
        let mut state = state_with(vec![
            leaf("Alpha"),
            leaf("Beta"),
            group("Attach Domain", vec![leaf("unix"), leaf("ssh")]),
        ]);
        // Parent list scrolled down; extents as a large list would have them.
        state.scroll.set_extents(100.0, 1000.0);
        state.scroll.scroll_by(400.0);
        state.enter_group(2);
        // The small child frame clamps the live offset...
        state.scroll.set_extents(100.0, 100.0);
        assert_eq!(state.scroll.offset, 0.0);
        assert!(state.pop_frame());
        // ...but the parent's position is parked for the next paint, not
        // squeezed through the stale extents.
        assert_eq!(state.pending_scroll, Some(400.0));
    }

    #[test]
    fn exact_match_outranks_a_fuzzy_one() {
        let items = vec![leaf("New Tab Something Long"), leaf("New Tab")];
        let order = compute_order("New Tab", &items, None, true);
        assert_eq!(order.first().copied(), Some(MatchEntry::Item(1)));
    }

    #[test]
    fn penetration_is_capped_per_group() {
        let children: Vec<PaletteItem> = (0..20)
            .map(|i| leaf(&format!("Solarized Variant {i}")))
            .collect();
        let items = vec![group("Change Theme", children)];
        let order = compute_order("solarized", &items, None, true);
        let nested = order
            .iter()
            .filter(|entry| matches!(entry, MatchEntry::Nested(..)))
            .count();
        assert!(nested <= PENETRATION_PER_GROUP, "{nested} nested entries");
    }

    #[test]
    fn short_queries_do_not_penetrate_groups() {
        let items = vec![group("Change Theme", vec![leaf("Zenburn")])];
        let order = compute_order("z", &items, None, true);
        assert!(order
            .iter()
            .all(|entry| matches!(entry, MatchEntry::Item(_))));
    }

    #[test]
    fn a_family_of_two_is_not_folded_into_a_group() {
        // classify_family names the family; the fold itself is gated on
        // GROUP_FOLD_THRESHOLD in build_command_palette_root. Guard the
        // threshold value so a refactor doesn't silently fold pairs.
        assert!(GROUP_FOLD_THRESHOLD >= 3);
    }

    #[test]
    fn selection_stays_within_the_filtered_order() {
        let mut state = state_with(vec![leaf("Alpha"), leaf("Beta"), leaf("Gamma")]);
        state.layout.row_height = 10.0;
        state.layout.visible_rows = 2;
        state.move_selection_by(10);
        assert_eq!(state.selected, 2);
        state.move_selection_by(-10);
        assert_eq!(state.selected, 0);
    }

    #[test]
    fn every_command_icon_name_maps_to_an_svg() {
        // The names commands.rs uses today; a new icon name that lacks a
        // mapping silently loses its glyph, so keep this list in sync.
        for name in [
            "cod_clear_all",
            "cod_debug",
            "cod_empty_window",
            "cod_list_flat",
            "cod_multiple_windows",
            "cod_settings_gear",
            "cod_split_horizontal",
            "cod_split_vertical",
            "fa_long_arrow_down",
            "fa_long_arrow_left",
            "fa_long_arrow_right",
            "fa_long_arrow_up",
            "fa_ticket",
            "md_add",
            "md_close_box_outline",
            "md_content_copy",
            "md_content_paste",
            "md_delete",
            "md_drag",
            "md_edit",
            "md_folder_open",
            "md_format_align_bottom",
            "md_format_align_top",
            "md_format_size",
            "md_fullscreen",
            "md_help",
            "md_keyboard_variant",
            "md_open_in_new",
            "md_palette",
            "md_pipe",
            "md_pipe_disconnected",
            "md_reload",
            "md_server_network",
            "md_sticker_emoji",
            "md_tab_plus",
            "md_window_minimize",
            "md_window_restore",
            "oct_browser",
            "oct_comment_discussion",
            "oct_search",
            "oct_stop",
            "oct_terminal",
        ] {
            assert!(
                nerd_icon_to_svg(Some(name)).is_some(),
                "{name} has no SvgIcon mapping"
            );
        }
    }
}
