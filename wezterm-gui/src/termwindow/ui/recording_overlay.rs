//! Pane-owned recording masks, shared by terminal and preview rendering.
//! normal input passes through unless the user explicitly enters edit mode.
use crate::quad::{QuadTrait, TripleLayerQuadAllocator};
use crate::ui::{DrawContext, SvgIcon};
use crate::utilsprites::RenderMetrics;
use config::{ConfigHandle, HsbTransform, TextStyle};
use fluent_bundle::FluentArgs;
use mux::pane::PaneId;
use mux::tab::PositionedPane;
use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, LazyLock, Mutex};
use termwiz::cell::CellAttributes;
use wezterm_term::color::ColorPalette;
use wezterm_term::StableRowIndex;
use window::color::LinearRgba;
use window::{
    KeyCode, KeyEvent, Modifiers, MouseCursor, MouseEvent, MouseEventKind, MousePress, WindowOps,
};

const MAX_MASKS: usize = 64;
const MIN_SIZE: f32 = 8.0;
// A selected mask's edges can be caught this far outside it and this far
// inside, and its handles are drawn this big. UI units, like everything here:
// 2x backing pixels (see `ui_scale_for_dpi`).
const HANDLE_REACH: f32 = 12.0;
const HANDLE_INWARD: f32 = 8.0;
const HANDLE_SIZE: f32 = 14.0;
// Edits that can be undone; kept only while editing.
const UNDO_LIMIT: usize = 100;
// What an arrow key moves the selection by, and with Shift: a point, or ten.
const NUDGE: f32 = 2.0;
const NUDGE_FAR: f32 = 20.0;
// Above content transitions, tooltips and the command palette (currently 11).
const OVERLAY_ZINDEX: i8 = 20;

// The floating toolbar, before it is squeezed to fit.
const BAR_HEIGHT: f32 = 80.0;
const BAR_PAD: f32 = 16.0;
const GRIP_WIDTH: f32 = 20.0;
const SWATCH: f32 = 30.0;
const SWATCH_GAP: f32 = 10.0;
const DIVIDER_GAP: f32 = 16.0;
const ICON: f32 = 28.0;
const ICON_GAP: f32 = 10.0;
const BUTTON_HEIGHT: f32 = 56.0;
const BUTTON_PAD: f32 = 18.0;

#[derive(Clone, Copy, Debug, Default, PartialEq)]
struct Rect {
    x: f32,
    y: f32,
    w: f32,
    h: f32,
}

/// What a mask is filled with. `Background` is the pane's own ground, so a
/// mask over a prompt stays out of sight when the theme changes.
#[derive(Clone, Copy, Debug, PartialEq)]
enum MaskColor {
    Background,
    Srgb(u8, u8, u8),
}

impl Default for MaskColor {
    fn default() -> Self {
        BLACK
    }
}

impl MaskColor {
    /// Always opaque: a mask is drawn over the text it hides, so a ground
    /// with any transparency in it would let that text through.
    fn linear(self, ground: LinearRgba) -> LinearRgba {
        match self {
            Self::Background => LinearRgba::with_components(ground.0, ground.1, ground.2, 1.0),
            Self::Srgb(r, g, b) => LinearRgba::with_srgba(r, g, b, 0xff),
        }
    }
}

const BLACK: MaskColor = MaskColor::Srgb(0, 0, 0);

// The toolbar's colours, in order. The pipette takes any other.
const SWATCHES: [MaskColor; 10] = [
    MaskColor::Background,
    BLACK,
    MaskColor::Srgb(0x63, 0x63, 0x66),
    MaskColor::Srgb(0xf2, 0xf2, 0xf7),
    MaskColor::Srgb(0xff, 0x45, 0x3a),
    MaskColor::Srgb(0xff, 0x9f, 0x0a),
    MaskColor::Srgb(0xe5, 0xb4, 0x00),
    MaskColor::Srgb(0x30, 0xd1, 0x58),
    MaskColor::Srgb(0x0a, 0x84, 0xff),
    MaskColor::Srgb(0xbf, 0x5a, 0xf2),
];

#[derive(Clone, Copy, Debug, PartialEq)]
struct Mask {
    rect: Rect,
    color: MaskColor,
}

// GUI-local state follows a pane between windows; no terminal or mux data changes.
// Rectangles are normalized to the pane frame, so previews reuse the same geometry.
#[derive(Clone, Debug, Default)]
pub(crate) struct PaneRecordingLayer(Arc<Mutex<PaneRecordingState>>);

#[derive(Debug, Default)]
struct PaneRecordingState {
    masks: Vec<Mask>,
    // The actual text grid, relative to the editable pane frame. This includes
    // the renderer's padding, nav bar and effective per-pane font metrics.
    grid: Option<Rect>,
    revision: u64,
}

static MASK_REVISION: AtomicU64 = AtomicU64::new(0);

pub(crate) fn recording_mask_revision() -> u64 {
    MASK_REVISION.load(Ordering::Relaxed)
}

fn next_revision() -> u64 {
    MASK_REVISION.fetch_add(1, Ordering::Relaxed) + 1
}

impl PaneRecordingLayer {
    pub(crate) fn revision(&self) -> u64 {
        self.0.lock().unwrap().revision
    }

    /// The masks over a preview of this pane, `Background` being the
    /// preview's own `ground`.
    pub(crate) fn preview_rects(
        &self,
        target_grid: window::RectF,
        clip: window::RectF,
        ground: LinearRgba,
    ) -> Vec<(window::RectF, LinearRgba)> {
        let state = self.0.lock().unwrap();
        if state.masks.is_empty() {
            return Vec::new();
        }
        let Some(grid) = state.grid.filter(|grid| grid.w > 0.0 && grid.h > 0.0) else {
            // A source pane that has not painted since its masks were created
            // has no trustworthy grid mapping yet. Keep its preview covered.
            return vec![(clip, BLACK.linear(ground))];
        };
        state
            .masks
            .iter()
            .filter_map(|mask| {
                let rect = Rect {
                    x: (mask.rect.x - grid.x) / grid.w,
                    y: (mask.rect.y - grid.y) / grid.h,
                    w: mask.rect.w / grid.w,
                    h: mask.rect.h / grid.h,
                }
                .in_frame(target_grid)
                .intersection(&clip)?;
                Some((rect, mask.color.linear(ground)))
            })
            .collect()
    }

    fn grid(&self) -> Option<Rect> {
        self.0.lock().unwrap().grid
    }

    fn set_grid(&self, frame: window::RectF, grid: window::RectF) -> bool {
        let grid = Rect {
            x: (grid.min_x() - frame.min_x()) / frame.width().max(1.0),
            y: (grid.min_y() - frame.min_y()) / frame.height().max(1.0),
            w: grid.width() / frame.width().max(1.0),
            h: grid.height() / frame.height().max(1.0),
        };
        let mut state = self.0.lock().unwrap();
        if state.grid != Some(grid) {
            state.grid = Some(grid);
            if !state.masks.is_empty() {
                state.revision = next_revision();
                return true;
            }
        }
        false
    }
    fn is_empty(&self) -> bool {
        self.0.lock().unwrap().masks.is_empty()
    }
}

static PANE_MASKS: LazyLock<Mutex<HashMap<PaneId, PaneRecordingLayer>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

fn pane_masks(pane_id: PaneId) -> Option<PaneRecordingLayer> {
    PANE_MASKS.lock().unwrap().get(&pane_id).cloned()
}

pub(crate) fn forget_pane(pane_id: PaneId) {
    PANE_MASKS.lock().unwrap().remove(&pane_id);
}

pub(crate) fn pane_layer(pane_id: PaneId) -> PaneRecordingLayer {
    PANE_MASKS
        .lock()
        .unwrap()
        .entry(pane_id)
        .or_default()
        .clone()
}

fn save_pane_masks(pane_id: PaneId, masks: Vec<Mask>) {
    let layer = pane_layer(pane_id);
    let mut state = layer.0.lock().unwrap();
    if state.masks != masks {
        state.masks = masks;
        state.revision = next_revision();
    }
}

/// The colour a cell's background is drawn in, as a mask colour: the pane's
/// own ground for a cell that has none of its own. Resolved as the line
/// renderer resolves it -- reverse video, bold brightening and all -- so the
/// pipette takes what is on screen.
fn cell_mask_color(
    attrs: &CellAttributes,
    palette: &ColorPalette,
    config: &ConfigHandle,
    style: &TextStyle,
    reverse_video: bool,
) -> MaskColor {
    let (_, drawn, own_ground) =
        crate::termwindow::render::cell_fg_bg(attrs, palette, config, style, reverse_video);
    if own_ground {
        return MaskColor::Background;
    }
    let (r, g, b, _) = drawn.to_srgb_u8();
    MaskColor::Srgb(r, g, b)
}

/// ⌘Z on macOS; Ctrl+Z elsewhere, where Super belongs to the window manager.
fn undo_modifiers() -> Modifiers {
    if cfg!(target_os = "macos") {
        Modifiers::SUPER
    } else {
        Modifiers::CTRL
    }
}

fn undo_shortcut() -> &'static str {
    if cfg!(target_os = "macos") {
        "⌘Z"
    } else {
        "Ctrl+Z"
    }
}

impl Rect {
    fn normalized(self, size: (f32, f32)) -> Self {
        Self {
            x: self.x / size.0.max(1.0),
            y: self.y / size.1.max(1.0),
            w: self.w / size.0.max(1.0),
            h: self.h / size.1.max(1.0),
        }
        .fitted((1.0, 1.0))
    }

    fn in_frame(self, frame: window::RectF) -> window::RectF {
        euclid::rect(
            frame.min_x() + self.x * frame.width(),
            frame.min_y() + self.y * frame.height(),
            self.w * frame.width(),
            self.h * frame.height(),
        )
    }

    fn between(a: (f32, f32), b: (f32, f32)) -> Self {
        Self {
            x: a.0.min(b.0),
            y: a.1.min(b.1),
            w: (a.0 - b.0).abs(),
            h: (a.1 - b.1).abs(),
        }
    }

    fn fitted(self, bounds: (f32, f32)) -> Self {
        let w = self.w.min(bounds.0).max(0.0);
        let h = self.h.min(bounds.1).max(0.0);
        Self {
            x: self.x.clamp(0.0, (bounds.0 - w).max(0.0)),
            y: self.y.clamp(0.0, (bounds.1 - h).max(0.0)),
            w,
            h,
        }
    }

    fn grown(self, by: f32) -> Self {
        Self {
            x: self.x - by,
            y: self.y - by,
            w: self.w + by * 2.0,
            h: self.h + by * 2.0,
        }
    }

    fn contains(self, p: (f32, f32)) -> bool {
        p.0 >= self.x && p.0 <= self.x + self.w && p.1 >= self.y && p.1 <= self.y + self.h
    }

    /// The edges a press at `p` catches on this mask, once it is selected: a
    /// band along each edge from HANDLE_REACH outside it to HANDLE_INWARD
    /// inside, but never more than a quarter of the mask, so that even a
    /// small one keeps a middle to be dragged by.
    fn handle_at(self, p: (f32, f32)) -> Option<Handle> {
        fn side(p: f32, start: f32, len: f32) -> Option<i8> {
            let inward = HANDLE_INWARD.min(len / 4.0);
            if p < start - HANDLE_REACH || p > start + len + HANDLE_REACH {
                None
            } else if p <= start + inward {
                Some(-1)
            } else if p >= start + len - inward {
                Some(1)
            } else {
                Some(0)
            }
        }
        let handle = Handle(side(p.0, self.x, self.w)?, side(p.1, self.y, self.h)?);
        (handle != Handle(0, 0)).then_some(handle)
    }

    /// This rectangle with the edges `handle` names moved by `delta`, kept
    /// inside `bounds` and no smaller than MIN_SIZE.
    fn resized(self, handle: Handle, delta: (f32, f32), bounds: (f32, f32)) -> Self {
        let (mut left, mut top) = (self.x, self.y);
        let (mut right, mut bottom) = (self.x + self.w, self.y + self.h);
        match handle.0 {
            -1 => left = (left + delta.0).min(right - MIN_SIZE).max(0.0),
            1 => right = (right + delta.0).max(left + MIN_SIZE).min(bounds.0),
            _ => {}
        }
        match handle.1 {
            -1 => top = (top + delta.1).min(bottom - MIN_SIZE).max(0.0),
            1 => bottom = (bottom + delta.1).max(top + MIN_SIZE).min(bounds.1),
            _ => {}
        }
        Self {
            x: left,
            y: top,
            w: (right - left).max(0.0),
            h: (bottom - top).max(0.0),
        }
    }

    fn pixels(self, scale: f32) -> window::RectF {
        euclid::rect(
            self.x * scale,
            self.y * scale,
            self.w * scale,
            self.h * scale,
        )
    }
}

/// The edges a resize moves: -1 the left or top one, 1 the right or bottom
/// one, 0 neither.
#[derive(Clone, Copy, Debug, PartialEq)]
struct Handle(i8, i8);

impl Handle {
    fn cursor(self) -> MouseCursor {
        match self {
            Handle(0, _) => MouseCursor::SizeUpDown,
            Handle(_, 0) => MouseCursor::SizeLeftRight,
            Handle(x, y) if x == y => MouseCursor::SizeNorthWestSouthEast,
            _ => MouseCursor::SizeNorthEastSouthWest,
        }
    }
}

#[derive(Clone, Copy, Debug)]
enum DragKind {
    Create,
    Move(usize, Rect),
    Resize(usize, Rect, Handle),
}

#[derive(Clone, Copy, Debug)]
struct Drag {
    kind: DragKind,
    anchor: (f32, f32),
    preview: Rect,
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum Control {
    Swatch(usize),
    Pipette,
    Delete,
    Clear,
    Done,
}

/// The toolbar laid out from its own top-left corner, in UI points.
struct ToolbarLayout {
    controls: Vec<(Control, Rect)>,
    grip: Rect,
    dividers: [f32; 2],
    width: f32,
}

/// `labels` are the widths of the Delete, Clear and Done labels. Delete and
/// Clear show theirs only `with_labels`; without, they are icons.
fn toolbar_layout(labels: [f32; 3], with_labels: bool) -> ToolbarLayout {
    let centred = |x: f32, w: f32, h: f32| Rect {
        x,
        y: (BAR_HEIGHT - h) / 2.0,
        w,
        h,
    };
    let mut controls = Vec::with_capacity(SWATCHES.len() + 4);
    let mut x = BAR_PAD;
    let grip = centred(x, GRIP_WIDTH, 32.0);
    x += GRIP_WIDTH + ICON_GAP;
    for index in 0..SWATCHES.len() {
        controls.push((Control::Swatch(index), centred(x, SWATCH, SWATCH)));
        x += SWATCH + SWATCH_GAP;
    }
    x += DIVIDER_GAP - SWATCH_GAP;
    let first = x;
    x += 1.0 + DIVIDER_GAP;
    controls.push((Control::Pipette, centred(x, BUTTON_HEIGHT, BUTTON_HEIGHT)));
    x += BUTTON_HEIGHT + DIVIDER_GAP;
    let second = x;
    x += 1.0 + DIVIDER_GAP;
    for (control, label) in [(Control::Delete, labels[0]), (Control::Clear, labels[1])] {
        let label = if with_labels { ICON_GAP + label } else { 0.0 };
        let w = BUTTON_PAD * 2.0 + ICON + label;
        controls.push((control, centred(x, w, BUTTON_HEIGHT)));
        x += w + 8.0;
    }
    x += 8.0;
    let w = BUTTON_PAD * 2.0 + ICON + ICON_GAP + labels[2];
    controls.push((Control::Done, centred(x, w, BUTTON_HEIGHT)));
    x += w + BAR_PAD;
    ToolbarLayout {
        controls,
        grip,
        dividers: [first, second],
        width: x,
    }
}

#[derive(Default)]
pub(crate) struct RecordingOverlay {
    // UI coordinates, independent of terminal font size and backing DPI.
    masks: Vec<Mask>,
    pane_id: Option<PaneId>,
    canvas_size: (f32, f32),
    pub(crate) editing: bool,
    selected: Option<usize>,
    drag: Option<Drag>,
    // What a new mask is filled with: the colour chosen last.
    color: MaskColor,
    // The masks as they were before each edit, newest last.
    history: VecDeque<Vec<Mask>>,
    // The mask the last edit nudged, while that is what it did: holding an
    // arrow key down is one step to undo, not one per repeat.
    nudging: Option<usize>,
    // The pipette is out: the next press in the pane takes the colour there.
    picking: bool,
    // While picking, where the pointer is and the colour under it.
    pick_preview: Option<((f32, f32), MaskColor)>,
    controls: Vec<(Control, Rect)>,
    toolbar: Option<Rect>,
    toolbar_position: Option<(f32, f32)>,
    toolbar_drag: Option<((f32, f32), Rect)>,
    hovered: Option<Control>,
    pressed: Option<Control>,
    finish_key: Option<KeyCode>,
}

impl RecordingOverlay {
    fn bind(&mut self, pane_id: PaneId, size: (f32, f32)) {
        self.cancel_interaction();
        self.pane_id = Some(pane_id);
        self.canvas_size = size;
        self.selected = None;
        self.history = VecDeque::new();
        self.nudging = None;
        self.masks = pane_masks(pane_id)
            .map(|masks| {
                masks
                    .0
                    .lock()
                    .unwrap()
                    .masks
                    .iter()
                    .map(|mask| Mask {
                        rect: Rect {
                            x: mask.rect.x * size.0,
                            y: mask.rect.y * size.1,
                            w: mask.rect.w * size.0,
                            h: mask.rect.h * size.1,
                        },
                        color: mask.color,
                    })
                    .collect()
            })
            .unwrap_or_default();
    }

    fn save(&self) {
        if let Some(pane_id) = self.pane_id {
            save_pane_masks(
                pane_id,
                self.masks
                    .iter()
                    .map(|mask| Mask {
                        rect: mask.rect.normalized(self.canvas_size),
                        ..*mask
                    })
                    .collect(),
            );
        }
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.masks.is_empty()
    }
    pub(crate) fn owns_keyboard(&self) -> bool {
        self.editing || self.finish_key.is_some()
    }

    pub(crate) fn clear(&mut self) {
        if !self.masks.is_empty() {
            self.remember();
        }
        self.masks.clear();
        self.selected = None;
        self.drag = None;
        self.save();
    }

    pub(crate) fn cancel_interaction(&mut self) {
        self.drag = None;
        self.pressed = None;
        self.finish_key = None;
        self.toolbar_drag = None;
        self.hovered = None;
        self.picking = false;
        self.pick_preview = None;
    }

    fn finish(&mut self) {
        self.editing = false;
        self.cancel_interaction();
        self.controls = Vec::new();
        self.toolbar = None;
        self.history = VecDeque::new();
        self.nudging = None;
    }

    fn control_enabled(&self, control: Control) -> bool {
        match control {
            Control::Delete => self.selected.is_some(),
            Control::Clear => !self.is_empty(),
            _ => true,
        }
    }

    fn activate(&mut self, control: Control) {
        match control {
            Control::Swatch(index) => {
                self.picking = false;
                self.pick_preview = None;
                self.apply_color(SWATCHES[index]);
            }
            Control::Pipette => {
                self.picking = !self.picking;
                self.pick_preview = None;
            }
            Control::Delete | Control::Clear => {
                // Whatever the pipette was out for, this is something else.
                self.picking = false;
                self.pick_preview = None;
                if control == Control::Delete {
                    self.delete_selected();
                } else {
                    self.clear();
                }
            }
            Control::Done => self.finish(),
        }
    }

    /// The colour the toolbar shows as current: the selection's, or else the
    /// one the next mask gets.
    fn current_color(&self) -> MaskColor {
        self.selected
            .and_then(|index| self.masks.get(index))
            .map_or(self.color, |mask| mask.color)
    }

    fn toolbar_rect(&self, size: (f32, f32), bounds: (f32, f32)) -> Rect {
        let w = size.0.min((bounds.0 - 32.0).max(0.0));
        let h = size.1.min(bounds.1);
        let (x, y) = self
            .toolbar_position
            .unwrap_or(((bounds.0 - w) / 2.0, bounds.1 - h - 48.0));
        Rect { x, y, w, h }.fitted(bounds)
    }

    fn move_toolbar(&mut self, point: (f32, f32), bounds: (f32, f32)) {
        if let Some((anchor, original)) = self.toolbar_drag {
            let rect = Rect {
                x: original.x + point.0 - anchor.0,
                y: original.y + point.1 - anchor.1,
                ..original
            }
            .fitted(bounds);
            self.toolbar_position = Some((rect.x, rect.y));
        }
    }

    /// Keep the masks as they are before an edit, for undo to return to. Only
    /// while editing, and only the last UNDO_LIMIT edits.
    fn remember(&mut self) {
        self.nudging = None;
        if !self.editing {
            return;
        }
        if self.history.len() == UNDO_LIMIT {
            self.history.pop_front();
        }
        self.history.push_back(self.masks.clone());
    }

    fn undo(&mut self) {
        self.drag = None;
        self.nudging = None;
        if let Some(masks) = self.history.pop_back() {
            self.masks = masks;
            self.selected = self.selected.filter(|&index| index < self.masks.len());
            self.save();
        }
    }

    fn delete_selected(&mut self) {
        self.drag = None;
        if let Some(index) = self.selected.take() {
            self.remember();
            self.masks.remove(index);
            self.save();
        }
    }

    /// The selection takes `color`, and so does every mask drawn after it.
    fn apply_color(&mut self, color: MaskColor) {
        self.color = color;
        if let Some(index) = self.selected {
            if self.masks[index].color != color {
                self.remember();
                self.masks[index].color = color;
                self.save();
            }
        }
    }

    fn nudge(&mut self, dx: f32, dy: f32) {
        let Some(index) = self.selected.filter(|_| self.drag.is_none()) else {
            return;
        };
        let bounds = self.canvas_size;
        let rect = self.masks[index].rect.fitted(bounds);
        let moved = Rect {
            x: rect.x + dx,
            y: rect.y + dy,
            ..rect
        }
        .fitted(bounds);
        if moved != rect {
            if self.nudging != Some(index) {
                self.remember();
                self.nudging = Some(index);
            }
            self.masks[index].rect = moved;
            self.save();
        }
    }

    fn hit(&self, point: (f32, f32), bounds: (f32, f32)) -> Option<usize> {
        self.masks
            .iter()
            .rposition(|mask| mask.rect.fitted(bounds).contains(point))
    }

    /// What a press at `point` takes hold of: the selection's edges first,
    /// even where they stick out over another mask; then the topmost mask
    /// under it, by an edge or else as a whole.
    fn grab_at(&self, point: (f32, f32), bounds: (f32, f32)) -> Option<DragKind> {
        let selected = self.selected.and_then(|index| {
            let rect = self.masks[index].rect.fitted(bounds);
            rect.handle_at(point)
                .map(|handle| DragKind::Resize(index, rect, handle))
        });
        selected.or_else(|| {
            let index = self.hit(point, bounds)?;
            let rect = self.masks[index].rect.fitted(bounds);
            Some(match rect.handle_at(point) {
                Some(handle) => DragKind::Resize(index, rect, handle),
                None => DragKind::Move(index, rect),
            })
        })
    }

    fn begin(&mut self, point: (f32, f32), bounds: (f32, f32)) {
        self.nudging = None;
        let kind = match self.grab_at(point, bounds) {
            Some(kind) => kind,
            None if self.masks.len() < MAX_MASKS => DragKind::Create,
            None => {
                self.selected = None;
                return;
            }
        };
        self.selected = match kind {
            DragKind::Move(index, _) | DragKind::Resize(index, _, _) => Some(index),
            DragKind::Create => None,
        };
        let preview = match kind {
            DragKind::Create => Rect::between(point, point),
            DragKind::Move(_, rect) | DragKind::Resize(_, rect, _) => rect,
        };
        self.drag = Some(Drag {
            kind,
            anchor: point,
            preview,
        });
    }

    fn update_drag(&mut self, point: (f32, f32), bounds: (f32, f32)) {
        let Some(drag) = &mut self.drag else {
            return;
        };
        let point = (point.0.clamp(0.0, bounds.0), point.1.clamp(0.0, bounds.1));
        let delta = (point.0 - drag.anchor.0, point.1 - drag.anchor.1);
        drag.preview = match drag.kind {
            DragKind::Create => Rect::between(drag.anchor, point),
            DragKind::Move(_, original) => Rect {
                x: original.x + delta.0,
                y: original.y + delta.1,
                ..original
            }
            .fitted(bounds),
            DragKind::Resize(_, original, handle) => original.resized(handle, delta, bounds),
        };
    }

    fn commit_drag(&mut self) {
        let Some(drag) = self.drag.take() else {
            return;
        };
        if drag.preview.w < MIN_SIZE || drag.preview.h < MIN_SIZE {
            return;
        }
        match drag.kind {
            DragKind::Create => {
                self.remember();
                self.selected = Some(self.masks.len());
                self.masks.push(Mask {
                    rect: drag.preview,
                    color: self.color,
                });
            }
            // A press that selects without moving changes nothing.
            DragKind::Move(_, original) | DragKind::Resize(_, original, _)
                if drag.preview == original => {}
            DragKind::Move(index, _) | DragKind::Resize(index, _, _) => {
                self.remember();
                self.masks[index].rect = drag.preview;
            }
        }
        self.save();
    }

    fn displayed(&self, index: usize, bounds: (f32, f32)) -> Rect {
        if let Some(drag) = self.drag {
            match drag.kind {
                DragKind::Move(i, _) | DragKind::Resize(i, _, _) if i == index => {
                    return drag.preview
                }
                _ => {}
            }
        }
        self.masks[index].rect.fitted(bounds)
    }

    /// The pointer over the pane while editing.
    fn cursor_at(&self, point: (f32, f32), bounds: (f32, f32)) -> MouseCursor {
        match self.drag.map(|drag| drag.kind) {
            Some(DragKind::Resize(_, _, handle)) => return handle.cursor(),
            Some(DragKind::Move(..)) => return MouseCursor::Hand,
            Some(DragKind::Create) => return MouseCursor::Arrow,
            None => {}
        }
        match self.grab_at(point, bounds) {
            Some(DragKind::Resize(_, _, handle)) => handle.cursor(),
            Some(_) => MouseCursor::Hand,
            None => MouseCursor::Arrow,
        }
    }
}

impl crate::TermWindow {
    fn recording_pane(&self, pane_id: PaneId) -> Option<PositionedPane> {
        self.get_panes_to_render()
            .into_iter()
            .find(|pos| pos.pane.pane_id() == pane_id)
    }

    fn recording_pane_frame(&self, pane_id: PaneId) -> Option<window::RectF> {
        self.recording_pane(pane_id)
            .and_then(|pos| self.pane_mask_frame(&pos).ok())
    }

    /// What `Background` is over this pane -- its ground as painted -- and
    /// the dimming an inactive pane gets, so that its masks dim with it.
    fn recording_mask_ground(&self, pos: &PositionedPane) -> (LinearRgba, Option<HsbTransform>) {
        let background = pos.pane.palette().background;
        let ground = self
            .dark_terminal_ground()
            .unwrap_or_else(|| background.to_linear());
        let hsv =
            (!pos.is_active).then(|| self.config.inactive_pane_hsb_for_background(background));
        (ground, hsv)
    }

    /// The colour a mask needs to disappear into the cell under `coords`
    /// (window pixels) of the pane at `pos`, whose mask frame is `frame`.
    /// Padding outside the grid is the pane's own ground.
    fn recording_color_at(
        &self,
        pos: &PositionedPane,
        frame: window::RectF,
        coords: (f32, f32),
    ) -> Option<MaskColor> {
        let pane_id = pos.pane.pane_id();
        let grid = pane_masks(pane_id)?.grid()?.in_frame(frame);
        let dims = pos.pane.get_dimensions();
        if dims.cols == 0 || dims.viewport_rows == 0 || grid.width() <= 0.0 || grid.height() <= 0.0
        {
            return Some(MaskColor::Background);
        }
        let viewport = self.get_viewport(pane_id);
        let scrolled = match viewport {
            Some(_) => self.drawn_viewport_px(&pos.pane),
            None => 0.0,
        };
        let col = ((coords.0 - grid.min_x()) / (grid.width() / dims.cols as f32)).floor();
        let row = ((coords.1 - grid.min_y() + scrolled)
            / (grid.height() / dims.viewport_rows as f32))
            .floor();
        // Scrolled part of the way through a row, one more is drawn below.
        let rows = dims.viewport_rows + usize::from(scrolled > 0.0);
        if col < 0.0 || row < 0.0 || col >= dims.cols as f32 || row >= rows as f32 {
            return Some(MaskColor::Background);
        }
        let stable = viewport.unwrap_or(dims.physical_top) + row as StableRowIndex;
        // Asked for a row it does not have, a pane answers with another one.
        let (first, lines) = pos.pane.get_lines(stable..stable + 1);
        let line = lines.first().filter(|_| first == stable);
        let palette = pos.pane.palette();
        Some(match line.and_then(|line| line.get_cell(col as usize)) {
            Some(cell) => {
                let attrs = cell.attrs();
                let style = self.fonts.match_style(&self.config, attrs);
                cell_mask_color(attrs, &palette, &self.config, style, dims.reverse_video)
            }
            None => MaskColor::Background,
        })
    }

    pub(crate) fn active_pane_recording_masks_empty(&self) -> bool {
        self.get_active_pane_or_overlay()
            .is_none_or(|pane| pane_masks(pane.pane_id()).is_none_or(|masks| masks.is_empty()))
    }

    pub(crate) fn clear_active_pane_recording_masks(&mut self) {
        if let Some(pane) = self.get_active_pane_or_overlay() {
            save_pane_masks(pane.pane_id(), Vec::new());
            if self.recording_overlay.pane_id == Some(pane.pane_id()) {
                self.recording_overlay.clear();
            }
            crate::frontend::front_end().invalidate_all_windows();
        }
    }

    pub(crate) fn update_recording_mask_grid(
        &self,
        pane_id: PaneId,
        frame: window::RectF,
        grid: window::RectF,
    ) {
        if let Some(masks) = pane_masks(pane_id) {
            if masks.set_grid(frame, grid) {
                crate::frontend::front_end().invalidate_all_windows();
            }
        }
    }

    pub(crate) fn occlude_recording_masks(
        &self,
        heap: &mut crate::quad::HeapQuadAllocator,
        masks: &[(window::RectF, LinearRgba)],
        hsv: Option<HsbTransform>,
    ) -> anyhow::Result<()> {
        let clips: Vec<_> = masks
            .iter()
            .map(|(rect, _)| {
                crate::quad::QuadClipRect::from_top_left_pixels(
                    rect.min_x(),
                    rect.min_y(),
                    rect.max_x(),
                    rect.max_y(),
                    &self.dimensions,
                )
            })
            .collect();
        heap.occlude(&clips);
        let mut layers = TripleLayerQuadAllocator::Heap(heap);
        for (rect, color) in masks {
            self.filled_rectangle(&mut layers, 2, *rect, *color)?
                .set_hsv(hsv);
        }
        Ok(())
    }

    // This runs inside the terminal world, so a recorded transition includes it.
    pub(crate) fn paint_pane_recording_masks(
        &self,
        pos: &PositionedPane,
        frame: window::RectF,
        layers: &mut TripleLayerQuadAllocator,
    ) -> anyhow::Result<()> {
        let pane_id = pos.pane.pane_id();
        let editing =
            self.recording_overlay.editing && self.recording_overlay.pane_id == Some(pane_id);
        let layer = match pane_masks(pane_id) {
            Some(layer) if editing || !layer.is_empty() => layer,
            _ => return Ok(()),
        };
        let (ground, hsv) = self.recording_mask_ground(pos);
        if let TripleLayerQuadAllocator::Heap(heap) = layers {
            let masks = layer
                .0
                .lock()
                .unwrap()
                .masks
                .iter()
                .filter_map(|mask| {
                    let rect = mask.rect.in_frame(frame).intersection(&frame)?;
                    Some((rect, mask.color.linear(ground)))
                })
                .collect::<Vec<_>>();
            return self.occlude_recording_masks(heap, &masks, hsv);
        }
        if !editing {
            for mask in layer.0.lock().unwrap().masks.iter() {
                if let Some(rect) = mask.rect.in_frame(frame).intersection(&frame) {
                    self.filled_rectangle(layers, 2, rect, mask.color.linear(ground))?
                        .set_hsv(hsv);
                }
            }
            return Ok(());
        }
        let state = &self.recording_overlay;
        let px = self.ui_f32(1.0).max(0.01);
        let in_pane = |rect: Rect| rect.normalized(state.canvas_size).in_frame(frame);
        for (index, mask) in state.masks.iter().enumerate() {
            let rect = in_pane(state.displayed(index, state.canvas_size));
            self.filled_rectangle(layers, 2, rect, mask.color.linear(ground))?
                .set_hsv(hsv);
            if state.selected != Some(index) {
                self.paint_mask_outline(layers, rect, px)?;
            }
        }
        if let Some(Drag {
            kind: DragKind::Create,
            preview,
            ..
        }) = state.drag
        {
            if preview.w > 0.0 && preview.h > 0.0 {
                let rect = in_pane(preview);
                self.filled_rectangle(layers, 2, rect, state.color.linear(ground))?
                    .set_hsv(hsv);
                self.paint_mask_outline(layers, rect, px)?;
            }
        }
        if let Some(index) = state.selected {
            let rect = in_pane(state.displayed(index, state.canvas_size));
            self.paint_mask_selection(layers, rect, px)?;
        }
        Ok(())
    }

    /// A mask's edge while editing, in two tones so that it shows on any
    /// ground: light just inside the mask, dark just outside it.
    fn paint_mask_outline(
        &self,
        layers: &mut TripleLayerQuadAllocator,
        rect: window::RectF,
        px: f32,
    ) -> anyhow::Result<()> {
        let light = LinearRgba::with_components(1.0, 1.0, 1.0, 0.7);
        let dark = LinearRgba::with_components(0.0, 0.0, 0.0, 0.5);
        let line = 2.0 * px;
        self.stroke_rectangle(layers, rect, line, light)?;
        self.stroke_rectangle(layers, rect.inflate(line, line), line, dark)
    }

    /// The selected mask: an accent ring just outside it, and a handle on
    /// each corner and, where there is room, in the middle of each edge.
    fn paint_mask_selection(
        &self,
        layers: &mut TripleLayerQuadAllocator,
        rect: window::RectF,
        px: f32,
    ) -> anyhow::Result<()> {
        let accent = self.chrome().accent;
        let ring = 4.0 * px;
        self.stroke_rectangle(layers, rect.inflate(ring, ring), ring, accent)?;
        let size = HANDLE_SIZE * px;
        let white = LinearRgba::with_components(1.0, 1.0, 1.0, 1.0);
        for (fx, fy) in [
            (0.0, 0.0),
            (0.5, 0.0),
            (1.0, 0.0),
            (1.0, 0.5),
            (1.0, 1.0),
            (0.5, 1.0),
            (0.0, 1.0),
            (0.0, 0.5),
        ] {
            if (fx == 0.5 && rect.width() < size * 3.0) || (fy == 0.5 && rect.height() < size * 3.0)
            {
                continue;
            }
            let x = rect.min_x() + rect.width() * fx - size / 2.0;
            let y = rect.min_y() + rect.height() * fy - size / 2.0;
            self.fill_rounded_rectangle_with_border(
                layers,
                2,
                euclid::rect(x, y, size, size),
                white,
                accent,
                3.0 * px,
                3.0 * px,
            )?;
        }
        Ok(())
    }

    fn stroke_rectangle(
        &self,
        layers: &mut TripleLayerQuadAllocator,
        rect: window::RectF,
        width: f32,
        color: LinearRgba,
    ) -> anyhow::Result<()> {
        let w = width.min(rect.width() / 2.0).min(rect.height() / 2.0);
        if w <= 0.0 {
            return Ok(());
        }
        for edge in [
            euclid::rect(rect.min_x(), rect.min_y(), rect.width(), w),
            euclid::rect(rect.min_x(), rect.max_y() - w, rect.width(), w),
            euclid::rect(rect.min_x(), rect.min_y() + w, w, rect.height() - w * 2.0),
            euclid::rect(
                rect.max_x() - w,
                rect.min_y() + w,
                w,
                rect.height() - w * 2.0,
            ),
        ] {
            self.filled_rectangle(layers, 2, edge, color)?;
        }
        Ok(())
    }

    pub(crate) fn start_recording_overlay_edit(&mut self) {
        let Some(pane) = self.get_active_pane_or_overlay() else {
            return;
        };
        let Some(frame) = self.recording_pane_frame(pane.pane_id()) else {
            return;
        };
        // The layer records where the pane's grid is from its next paint on,
        // which is what the pipette reads cells through.
        pane_layer(pane.pane_id());
        let scale = self.ui_f32(1.0).max(0.01);
        self.recording_overlay.bind(
            pane.pane_id(),
            (frame.width() / scale, frame.height() / scale),
        );
        self.recording_overlay.editing = true;
        self.recording_overlay.drag = None;
        self.recording_overlay.pressed = None;
        self.context_menu = None;
        self.dragging = None;
        self.current_mouse_capture = None;
        self.current_mouse_buttons.clear();
        if let Some(window) = &self.window {
            window.invalidate();
        }
    }

    pub(crate) fn recording_overlay_key(
        &mut self,
        key: &KeyEvent,
        context: &dyn WindowOps,
    ) -> bool {
        if self.recording_overlay.finish_key.as_ref() == Some(&key.key) {
            if !key.key_is_down {
                self.recording_overlay.finish_key = None;
            }
            return true;
        }
        if !self.recording_overlay.editing {
            return false;
        }
        if key.key_is_down {
            let mods = key.modifiers
                & (Modifiers::CTRL | Modifiers::SHIFT | Modifiers::ALT | Modifiers::SUPER);
            let step = if mods.contains(Modifiers::SHIFT) {
                NUDGE_FAR
            } else {
                NUDGE
            };
            let overlay = &mut self.recording_overlay;
            match &key.key {
                // Esc puts the pipette down before it ends editing.
                KeyCode::Char('\u{1b}') if overlay.picking => {
                    overlay.picking = false;
                    overlay.pick_preview = None;
                }
                KeyCode::Char('\u{1b}') | KeyCode::Char('\r') => {
                    overlay.finish();
                    overlay.finish_key = Some(key.key.clone());
                }
                KeyCode::Char('\u{7f}') | KeyCode::Char('\u{8}') => {
                    overlay.delete_selected();
                }
                KeyCode::Char(c)
                    if mods == undo_modifiers()
                        && (c.eq_ignore_ascii_case(&'z') || *c == '\u{1a}') =>
                {
                    overlay.undo();
                }
                KeyCode::LeftArrow => overlay.nudge(-step, 0.0),
                KeyCode::RightArrow => overlay.nudge(step, 0.0),
                KeyCode::UpArrow => overlay.nudge(0.0, -step),
                KeyCode::DownArrow => overlay.nudge(0.0, step),
                _ => {}
            }
            crate::frontend::front_end().invalidate_all_windows();
            context.invalidate();
        }
        true
    }

    pub(crate) fn recording_overlay_mouse(
        &mut self,
        event: &MouseEvent,
        context: &dyn WindowOps,
    ) -> bool {
        if !self.recording_overlay.editing {
            return false;
        }
        let scale = self.ui_f32(1.0).max(0.01);
        let bounds = (
            self.dimensions.pixel_width as f32 / scale,
            self.dimensions.pixel_height as f32 / scale,
        );
        let coords = (event.coords.x as f32, event.coords.y as f32);
        let point = (
            (coords.0 / scale).clamp(0.0, bounds.0),
            (coords.1 / scale).clamp(0.0, bounds.1),
        );
        let pane = self
            .recording_overlay
            .pane_id
            .and_then(|id| self.recording_pane(id))
            .and_then(|pos| Some((self.pane_mask_frame(&pos).ok()?, pos)));
        let Some((frame, pos)) = pane else {
            self.recording_overlay.finish();
            context.invalidate();
            return true;
        };
        let canvas = self.recording_overlay.canvas_size;
        let local = (
            (coords.0 - frame.min_x()) / frame.width().max(1.0) * canvas.0,
            (coords.1 - frame.min_y()) / frame.height().max(1.0) * canvas.1,
        );
        let inside = local.0 >= 0.0 && local.1 >= 0.0 && local.0 <= canvas.0 && local.1 <= canvas.1;
        let local = (local.0.clamp(0.0, canvas.0), local.1.clamp(0.0, canvas.1));
        let control = self
            .recording_overlay
            .controls
            .iter()
            .find(|(_, rect)| rect.contains(point))
            .map(|(control, _)| *control);
        let over_toolbar = self
            .recording_overlay
            .toolbar
            .is_some_and(|rect| rect.contains(point));
        self.recording_overlay.hovered = control;
        if self.recording_overlay.picking {
            let color = if inside && !over_toolbar {
                self.recording_color_at(&pos, frame, coords)
            } else {
                None
            };
            self.recording_overlay.pick_preview = color.map(|color| (point, color));
        }
        match event.kind {
            MouseEventKind::Press(MousePress::Left) => {
                if over_toolbar {
                    if let Some(control) = control {
                        if self.recording_overlay.control_enabled(control) {
                            self.recording_overlay.pressed = Some(control);
                        }
                    } else if let Some(rect) = self.recording_overlay.toolbar {
                        self.recording_overlay.toolbar_drag = Some((point, rect));
                    }
                } else if self.recording_overlay.picking {
                    // A press in the pane takes the colour under it; one
                    // anywhere else just puts the pipette down.
                    if let Some((_, color)) = self.recording_overlay.pick_preview.take() {
                        self.recording_overlay.apply_color(color);
                    }
                    self.recording_overlay.picking = false;
                } else if inside {
                    self.recording_overlay.begin(local, canvas);
                }
            }
            MouseEventKind::Move => {
                self.recording_overlay.move_toolbar(point, bounds);
                self.recording_overlay.update_drag(local, canvas);
            }
            MouseEventKind::Release(MousePress::Left) => {
                if self.recording_overlay.toolbar_drag.is_some() {
                    self.recording_overlay.move_toolbar(point, bounds);
                    self.recording_overlay.toolbar_drag = None;
                } else if let Some(pressed) = self.recording_overlay.pressed.take() {
                    if control == Some(pressed) {
                        self.recording_overlay.activate(pressed);
                    }
                } else {
                    self.recording_overlay.update_drag(local, canvas);
                    self.recording_overlay.commit_drag();
                }
            }
            _ => {}
        }
        let overlay = &self.recording_overlay;
        let cursor = if !overlay.editing {
            MouseCursor::Arrow
        } else if over_toolbar || overlay.toolbar_drag.is_some() {
            if control.is_none_or(|control| overlay.control_enabled(control)) {
                MouseCursor::Hand
            } else {
                MouseCursor::Arrow
            }
        } else if overlay.drag.is_some() || (inside && !overlay.picking) {
            overlay.cursor_at(local, canvas)
        } else {
            MouseCursor::Arrow
        };
        if matches!(event.kind, MouseEventKind::Release(MousePress::Left)) {
            crate::frontend::front_end().invalidate_all_windows();
        }
        context.set_cursor(Some(cursor));
        context.invalidate();
        true
    }

    pub(crate) fn paint_recording_overlay(&mut self) -> anyhow::Result<()> {
        if !self.recording_overlay.editing {
            return Ok(());
        }
        let pos = self
            .recording_overlay
            .pane_id
            .and_then(|id| self.recording_pane(id))
            .filter(|pos| self.pane_mask_frame(pos).is_ok());
        let Some(pos) = pos else {
            self.recording_overlay.finish();
            return Ok(());
        };
        let (ground, _) = self.recording_mask_ground(&pos);
        let scale = self.ui_f32(1.0).max(0.01);
        let bounds = (
            self.dimensions.pixel_width as f32 / scale,
            self.dimensions.pixel_height as f32 / scale,
        );
        let gl = self.render_state.as_ref().unwrap();
        let layer = gl.layer_for_zindex(OVERLAY_ZINDEX)?;
        let mut layers = layer.quad_allocator();
        let black = LinearRgba::with_components(0.0, 0.0, 0.0, 1.0);
        let white = LinearRgba::with_components(1.0, 1.0, 1.0, 1.0);
        let size = (crate::native_settings::home_font_size(&crate::native_settings::load_shared())
            * 0.8)
            .max(10.0);
        let font = self.fonts.title_font_with_size(size)?;
        let metrics = RenderMetrics::with_font_metrics(&font.metrics());
        let ctx = DrawContext::new(gl, self.dimensions, &metrics);
        let palette = self.chrome();
        let text_height = metrics.cell_size.height as f32 / scale;
        let labels = [
            crate::i18n::tr("recording-overlay-delete"),
            crate::i18n::tr("recording-overlay-clear"),
            crate::i18n::tr("recording-overlay-done"),
        ];
        let widths = labels
            .each_ref()
            .map(|label| ctx.measure_text_width(&font, label) / scale);
        // Labels go first when the window is narrow, then everything shrinks.
        let available = (bounds.0 - 32.0).max(0.0);
        let mut layout = toolbar_layout(widths, true);
        let with_labels = layout.width <= available;
        if !with_labels {
            layout = toolbar_layout(widths, false);
        }
        let squeeze = (available / layout.width).min(1.0);
        let bar = self
            .recording_overlay
            .toolbar_rect((layout.width * squeeze, BAR_HEIGHT * squeeze), bounds);
        self.recording_overlay.toolbar = Some(bar);
        let place = |rect: Rect| Rect {
            x: bar.x + rect.x * squeeze,
            y: bar.y + rect.y * squeeze,
            w: rect.w * squeeze,
            h: rect.h * squeeze,
        };
        let controls = layout
            .controls
            .iter()
            .map(|(control, rect)| (*control, place(*rect)))
            .collect::<Vec<_>>();

        // The command palette's surface: soft shadows from the atlas under a
        // bordered fill.
        let radius = bar.h / 2.0;
        ctx.draw_elevated_surface(
            &mut layers,
            2,
            bar.pixels(scale),
            palette.control_bg,
            palette.control_border,
            black,
            radius * scale,
        )?;
        // The whole gap around the controls is draggable, not just this grip.
        let grip = place(layout.grip);
        let dot = 5.0 * squeeze;
        for (column, row) in [(0, 0), (1, 0), (0, 1), (1, 1), (0, 2), (1, 2)] {
            let dot_rect = Rect {
                x: grip.x + (grip.w - dot * 3.0) / 2.0 + column as f32 * dot * 2.0,
                y: grip.y + (grip.h - dot * 5.0) / 2.0 + row as f32 * dot * 2.0,
                w: dot,
                h: dot,
            };
            self.fill_rounded_rectangle(
                &mut layers,
                2,
                dot_rect.pixels(scale),
                palette.muted_text.mul_alpha(0.6),
                dot / 2.0 * scale,
            )?;
        }
        for x in layout.dividers {
            let divider = place(Rect {
                x,
                y: (BAR_HEIGHT - 36.0) / 2.0,
                w: 2.0,
                h: 36.0,
            });
            self.filled_rectangle(
                &mut layers,
                2,
                divider.pixels(scale),
                palette.control_border,
            )?;
        }

        let overlay = &self.recording_overlay;
        let current = overlay.current_color();
        let edge = palette.text.mul_alpha(0.18);
        let icon = ICON * squeeze;
        for (control, rect) in &controls {
            let enabled = overlay.control_enabled(*control);
            let hovered = enabled && overlay.hovered == Some(*control);
            let pressed = hovered && overlay.pressed == Some(*control);
            let button_bg = if pressed {
                palette.control_pressed_bg
            } else {
                palette.control_hover_bg
            };
            let icon_y = (rect.y + (rect.h - icon) / 2.0) * scale;
            let text_y = (rect.y + (rect.h - text_height) / 2.0) * scale;
            match *control {
                Control::Swatch(index) => {
                    let color = SWATCHES[index];
                    let ring = rect.grown(6.0 * squeeze);
                    if color == current {
                        self.fill_rounded_rectangle_with_border(
                            &mut layers,
                            2,
                            ring.pixels(scale),
                            palette.control_bg,
                            palette.accent,
                            ring.w / 2.0 * scale,
                            3.0 * squeeze * scale,
                        )?;
                    } else if hovered {
                        self.fill_rounded_rectangle(
                            &mut layers,
                            2,
                            ring.pixels(scale),
                            button_bg,
                            ring.w / 2.0 * scale,
                        )?;
                    }
                    // The pane's own ground is meant to blend in, so it gets
                    // the stronger edge.
                    let (border, width) = match color {
                        MaskColor::Background => (palette.muted_text, 3.0),
                        _ => (edge, 2.0),
                    };
                    self.fill_rounded_rectangle_with_border(
                        &mut layers,
                        2,
                        rect.pixels(scale),
                        color.linear(ground),
                        border,
                        rect.w / 2.0 * scale,
                        width * squeeze * scale,
                    )?;
                }
                Control::Pipette => {
                    if overlay.picking || hovered {
                        let bg = if overlay.picking {
                            palette.control_pressed_bg
                        } else {
                            button_bg
                        };
                        self.fill_rounded_rectangle(
                            &mut layers,
                            2,
                            rect.pixels(scale),
                            bg,
                            16.0 * squeeze * scale,
                        )?;
                    }
                    let tint = if overlay.picking {
                        palette.accent
                    } else {
                        palette.text
                    };
                    ctx.draw_svg_icon(
                        &mut layers,
                        SvgIcon::Pipette,
                        (rect.x + (rect.w - icon) / 2.0) * scale,
                        icon_y,
                        icon * scale,
                        tint,
                    )?;
                    if !SWATCHES.contains(&current) {
                        // A colour the pipette took, which no swatch shows.
                        let chip = Rect {
                            x: rect.x + rect.w - 20.0 * squeeze,
                            y: rect.y + rect.h - 20.0 * squeeze,
                            w: 16.0 * squeeze,
                            h: 16.0 * squeeze,
                        };
                        self.fill_rounded_rectangle_with_border(
                            &mut layers,
                            2,
                            chip.pixels(scale),
                            current.linear(ground),
                            palette.accent,
                            chip.w / 2.0 * scale,
                            3.0 * squeeze * scale,
                        )?;
                    }
                }
                Control::Delete | Control::Clear => {
                    if hovered {
                        self.fill_rounded_rectangle(
                            &mut layers,
                            2,
                            rect.pixels(scale),
                            button_bg,
                            rect.h / 2.0 * scale,
                        )?;
                    }
                    let tint = if enabled {
                        palette.text
                    } else {
                        palette.muted_text.mul_alpha(0.4)
                    };
                    let (svg, label) = match control {
                        Control::Delete => (SvgIcon::Trash2, &labels[0]),
                        _ => (SvgIcon::RotateCcw, &labels[1]),
                    };
                    let x = rect.x + BUTTON_PAD * squeeze;
                    ctx.draw_svg_icon(&mut layers, svg, x * scale, icon_y, icon * scale, tint)?;
                    if with_labels {
                        let text_x = x + icon + ICON_GAP * squeeze;
                        let max_width = (rect.x + rect.w - text_x).max(0.0) * scale;
                        ctx.draw_text_on_layer(
                            &mut layers,
                            2,
                            &font,
                            text_x * scale,
                            text_y,
                            label,
                            tint,
                            max_width,
                        )?;
                    }
                }
                Control::Done => {
                    let fill = if pressed {
                        palette.accent.mul_alpha(0.8)
                    } else {
                        palette.accent
                    };
                    self.fill_rounded_rectangle(
                        &mut layers,
                        2,
                        rect.pixels(scale),
                        fill,
                        rect.h / 2.0 * scale,
                    )?;
                    let x = rect.x + BUTTON_PAD * squeeze;
                    ctx.draw_svg_icon(
                        &mut layers,
                        SvgIcon::Check,
                        x * scale,
                        icon_y,
                        icon * scale,
                        palette.on_accent,
                    )?;
                    let text_x = x + icon + ICON_GAP * squeeze;
                    let max_width = (rect.x + rect.w - text_x).max(0.0) * scale;
                    ctx.draw_text_on_layer(
                        &mut layers,
                        2,
                        &font,
                        text_x * scale,
                        text_y,
                        &labels[2],
                        palette.on_accent,
                        max_width,
                    )?;
                }
            }
        }

        let hint = if overlay.picking {
            crate::i18n::tr("recording-overlay-picking")
        } else if overlay.masks.len() >= MAX_MASKS {
            crate::i18n::tr("recording-overlay-limit")
        } else if overlay.toolbar_drag.is_some() {
            crate::i18n::tr("recording-overlay-drag-toolbar")
        } else {
            match overlay.hovered {
                Some(Control::Swatch(index)) if SWATCHES[index] == MaskColor::Background => {
                    crate::i18n::tr("recording-overlay-follow-background")
                }
                Some(Control::Pipette) => crate::i18n::tr("recording-overlay-pick"),
                Some(Control::Delete) if !with_labels => labels[0].clone(),
                Some(Control::Clear) if !with_labels => labels[1].clone(),
                _ => {
                    let mut args = FluentArgs::new();
                    args.set("undo", undo_shortcut());
                    crate::i18n::tr_args("recording-overlay-hint", &args)
                }
            }
        };
        let max_width = (bounds.0 - 48.0).max(0.0) * scale;
        let hint_width = ctx.measure_text_width(&font, &hint).min(max_width);
        let hint_x = ((bar.x + bar.w / 2.0) * scale - hint_width / 2.0)
            .clamp(0.0, (bounds.0 * scale - hint_width).max(0.0));
        let hint_y = if bar.y >= text_height + 28.0 {
            bar.y - text_height - 20.0
        } else {
            bar.y + bar.h + 16.0
        };
        if hint_y + text_height <= bounds.1 {
            let hint_bg = euclid::rect(
                hint_x - 10.0 * scale,
                (hint_y - 5.0) * scale,
                hint_width + 20.0 * scale,
                (text_height + 10.0) * scale,
            );
            self.fill_rounded_rectangle(&mut layers, 2, hint_bg, palette.control_bg, 10.0 * scale)?;
            ctx.draw_text_on_layer(
                &mut layers,
                2,
                &font,
                hint_x,
                hint_y * scale,
                &hint,
                palette.muted_text,
                max_width,
            )?;
        }

        // Beside the pointer, the colour a press would take.
        if let Some((point, color)) = overlay.pick_preview {
            let chip = Rect {
                x: point.0 + 28.0,
                y: point.1 - 68.0,
                w: 44.0,
                h: 44.0,
            }
            .fitted(bounds);
            ctx.draw_shadow(
                &mut layers,
                2,
                chip.pixels(scale),
                chip.w / 2.0 * scale,
                ctx.px(4.0),
                ctx.px(2.0),
                black.mul_alpha(0.35),
            )?;
            self.fill_rounded_rectangle_with_border(
                &mut layers,
                2,
                chip.pixels(scale),
                color.linear(ground),
                white,
                chip.w / 2.0 * scale,
                4.0 * scale,
            )?;
        }
        self.recording_overlay.controls = controls;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use wezterm_term::color::ColorAttribute;

    const CANVAS: (f32, f32) = (500.0, 400.0);

    fn mask(x: f32, y: f32, w: f32, h: f32) -> Mask {
        Mask {
            rect: Rect { x, y, w, h },
            color: BLACK,
        }
    }

    fn with_mask() -> RecordingOverlay {
        let mut state = RecordingOverlay::default();
        state.editing = true;
        state.canvas_size = CANVAS;
        state.begin((180.0, 160.0), CANVAS);
        state.update_drag((100.0, 80.0), CANVAS);
        state.commit_drag();
        state
    }

    #[test]
    fn reverse_drag_creates_mask_and_finish_keeps_it() {
        let mut state = with_mask();
        assert_eq!(state.masks, vec![mask(100.0, 80.0, 80.0, 80.0)]);
        state.finish();
        assert!(!state.owns_keyboard());
        assert_eq!(state.masks.len(), 1);
    }

    #[test]
    fn moving_and_resizing_stay_inside_window() {
        let mut state = with_mask();
        state.begin((110.0, 90.0), CANVAS);
        state.update_drag((900.0, 900.0), CANVAS);
        state.commit_drag();
        assert_eq!(
            state.masks[0].rect,
            Rect {
                x: 420.0,
                y: 320.0,
                w: 80.0,
                h: 80.0
            }
        );
        state.begin((495.0, 395.0), CANVAS);
        state.update_drag((0.0, 0.0), CANVAS);
        state.commit_drag();
        assert_eq!(state.masks[0].rect.w, MIN_SIZE);
        assert_eq!(state.masks[0].rect.h, MIN_SIZE);
    }

    #[test]
    fn edges_and_corners_resize_and_a_small_mask_still_moves() {
        let mut state = with_mask();
        // The left edge, caught just outside the mask.
        state.begin((97.0, 120.0), CANVAS);
        state.update_drag((60.0, 300.0), CANVAS);
        state.commit_drag();
        // The edge follows the pointer, caught 3 points out.
        assert_eq!(
            state.masks[0].rect,
            Rect {
                x: 63.0,
                y: 80.0,
                w: 117.0,
                h: 80.0
            }
        );
        // The top-right corner.
        state.begin((181.0, 79.0), CANVAS);
        state.update_drag((200.0, 40.0), CANVAS);
        state.commit_drag();
        assert_eq!(
            state.masks[0].rect,
            Rect {
                x: 63.0,
                y: 41.0,
                w: 136.0,
                h: 119.0
            }
        );
        // A mask smaller than the handles still moves from its middle...
        state.masks.push(mask(300.0, 300.0, 10.0, 10.0));
        state.selected = Some(1);
        state.begin((305.0, 305.0), CANVAS);
        assert!(matches!(state.drag.unwrap().kind, DragKind::Move(1, _)));
        state.update_drag((325.0, 315.0), CANVAS);
        state.commit_drag();
        assert_eq!(state.masks[1].rect, mask(320.0, 310.0, 10.0, 10.0).rect);
        // ...and resizes from just outside it.
        state.begin((332.0, 315.0), CANVAS);
        assert!(matches!(
            state.drag.unwrap().kind,
            DragKind::Resize(1, _, Handle(1, 0))
        ));
        assert_eq!(
            state.cursor_at((332.0, 315.0), CANVAS),
            MouseCursor::SizeLeftRight
        );
    }

    #[test]
    fn cancelled_drag_and_click_do_not_change_masks() {
        let mut state = with_mask();
        let original = state.masks.clone();
        let history = state.history.len();
        state.begin((110.0, 90.0), CANVAS);
        state.update_drag((300.0, 200.0), CANVAS);
        state.finish();
        assert_eq!(state.masks, original);
        state.editing = true;
        state.begin((10.0, 10.0), CANVAS);
        state.commit_drag();
        assert_eq!(state.masks, original);
        // Selecting without moving is not an edit either.
        state.begin((110.0, 90.0), CANVAS);
        state.commit_drag();
        assert_eq!(state.masks, original);
        assert!(state.history.len() <= history);
    }

    #[test]
    fn delete_and_limit_are_bounded() {
        let mut state = with_mask();
        state.delete_selected();
        assert!(state.is_empty());
        state.masks = vec![mask(100.0, 100.0, 10.0, 10.0); MAX_MASKS];
        state.begin((1.0, 1.0), CANVAS);
        assert!(state.drag.is_none());
        state.clear();
        assert!(state.is_empty());
    }

    #[test]
    fn undo_steps_back_through_edits_and_is_bounded() {
        let mut state = with_mask();
        let created = state.masks.clone();
        state.begin((120.0, 100.0), CANVAS);
        state.update_drag((220.0, 100.0), CANVAS);
        state.commit_drag();
        state.apply_color(SWATCHES[4]);
        state.clear();
        assert!(state.is_empty());
        state.undo(); // the clear
        assert_eq!(state.masks.len(), 1);
        assert_eq!(state.masks[0].color, SWATCHES[4]);
        state.undo(); // the colour
        assert_eq!(state.masks[0].color, BLACK);
        state.undo(); // the move
        assert_eq!(state.masks, created);
        state.undo(); // the mask itself
        assert!(state.is_empty());
        state.undo(); // nothing left to undo
        assert!(state.is_empty());
        for _ in 0..UNDO_LIMIT + 20 {
            state.remember();
        }
        assert_eq!(state.history.len(), UNDO_LIMIT);
        state.finish();
        assert!(state.history.is_empty());
        // Outside editing nothing is kept.
        state.remember();
        assert!(state.history.is_empty());
    }

    #[test]
    fn nudges_move_the_selection_inside_the_pane() {
        let mut state = with_mask();
        state.nudge(1.0, 0.0);
        assert_eq!(state.masks[0].rect.x, 101.0);
        state.nudge(0.0, -1000.0);
        assert_eq!(state.masks[0].rect.y, 0.0);
        state.undo();
        assert_eq!(state.masks[0].rect.y, 80.0);
        let before = state.masks.clone();
        state.selected = None;
        state.nudge(10.0, 10.0);
        assert_eq!(state.masks, before);
    }

    #[test]
    fn a_run_of_nudges_is_one_step_to_undo() {
        let mut state = with_mask();
        let before = state.masks.clone();
        let steps = state.history.len();
        for _ in 0..50 {
            state.nudge(NUDGE, 0.0);
        }
        assert_eq!(state.history.len(), steps + 1);
        assert_eq!(state.masks[0].rect.x, 100.0 + 50.0 * NUDGE);
        // Any other edit ends the run.
        state.apply_color(SWATCHES[4]);
        state.nudge(NUDGE, 0.0);
        assert_eq!(state.history.len(), steps + 3);
        state.undo();
        state.undo();
        state.undo();
        assert_eq!(state.masks, before);
    }

    #[test]
    fn an_unselected_mask_resizes_from_its_edges_too() {
        let mut state = with_mask();
        state.selected = None;
        // Inside its lower-right corner.
        state.begin((176.0, 156.0), CANVAS);
        assert!(matches!(
            state.drag.unwrap().kind,
            DragKind::Resize(0, _, Handle(1, 1))
        ));
        assert_eq!(state.selected, Some(0));
        state.update_drag((196.0, 176.0), CANVAS);
        state.commit_drag();
        assert_eq!(state.masks[0].rect, mask(100.0, 80.0, 100.0, 100.0).rect);
        // Just outside a mask that is not selected is empty space.
        state.selected = None;
        state.begin((205.0, 120.0), CANVAS);
        assert!(matches!(state.drag.unwrap().kind, DragKind::Create));
    }

    #[test]
    fn a_translucent_ground_still_hides_what_is_under_it() {
        let ground = LinearRgba::with_components(0.2, 0.3, 0.4, 0.6);
        assert_eq!(
            MaskColor::Background.linear(ground),
            LinearRgba::with_components(0.2, 0.3, 0.4, 1.0)
        );
        assert_eq!(SWATCHES[4].linear(ground).3, 1.0);
    }

    #[test]
    fn colours_go_to_the_selection_and_to_new_masks() {
        let mut state = with_mask();
        state.apply_color(MaskColor::Background);
        assert_eq!(state.masks[0].color, MaskColor::Background);
        state.selected = None;
        state.apply_color(SWATCHES[7]);
        assert_eq!(state.masks[0].color, MaskColor::Background);
        state.begin((300.0, 300.0), CANVAS);
        state.update_drag((340.0, 340.0), CANVAS);
        state.commit_drag();
        assert_eq!(state.masks[1].color, SWATCHES[7]);
        assert_eq!(state.current_color(), SWATCHES[7]);
        // The pipette is a toggle, and a swatch puts it down.
        state.activate(Control::Pipette);
        assert!(state.picking);
        state.activate(Control::Swatch(1));
        assert!(!state.picking);
        assert_eq!(state.masks[1].color, BLACK);
    }

    fn srgb(color: wezterm_term::color::SrgbaTuple) -> MaskColor {
        let (r, g, b, _) = color.to_srgb_u8();
        MaskColor::Srgb(r, g, b)
    }

    #[test]
    fn cells_become_mask_colours() {
        let palette = ColorPalette::default();
        let config = ConfigHandle::default_config();
        let style = TextStyle::default();
        let color = |attrs: &CellAttributes, reverse_video| {
            cell_mask_color(attrs, &palette, &config, &style, reverse_video)
        };
        let mut attrs = CellAttributes::default();
        assert_eq!(color(&attrs, false), MaskColor::Background);
        attrs.set_background(ColorAttribute::PaletteIndex(2));
        assert_eq!(
            color(&attrs, false),
            srgb(palette.resolve_bg(ColorAttribute::PaletteIndex(2)))
        );
        // Reverse video draws the foreground behind the text.
        attrs.set_reverse(true);
        assert_eq!(
            color(&attrs, false),
            srgb(palette.resolve_fg(ColorAttribute::Default))
        );
    }

    #[test]
    fn the_pipette_picks_what_the_renderer_draws() {
        let palette = ColorPalette::default();
        let config = ConfigHandle::default_config();
        let style = TextStyle::default();
        // A bold reversed red cell is drawn in bright red, entry 9.
        let mut attrs = CellAttributes::default();
        attrs.set_foreground(ColorAttribute::PaletteIndex(1));
        attrs.set_intensity(wezterm_term::Intensity::Bold);
        attrs.set_reverse(true);
        assert_eq!(
            cell_mask_color(&attrs, &palette, &config, &style, false),
            srgb(palette.resolve_fg(ColorAttribute::PaletteIndex(9)))
        );
        // The whole screen reversed: a plain cell is drawn in the foreground,
        // and a reversed one is back on the pane's own ground.
        let plain = CellAttributes::default();
        assert_eq!(
            cell_mask_color(&plain, &palette, &config, &style, true),
            srgb(palette.resolve_fg(ColorAttribute::Default))
        );
        let mut reversed = CellAttributes::default();
        reversed.set_reverse(true);
        assert_eq!(
            cell_mask_color(&reversed, &palette, &config, &style, true),
            MaskColor::Background
        );
    }

    #[test]
    fn toolbar_layout_keeps_controls_apart_and_drops_labels_first() {
        let full = toolbar_layout([60.0, 50.0, 70.0], true);
        let compact = toolbar_layout([60.0, 50.0, 70.0], false);
        assert!(compact.width < full.width);
        for layout in [&full, &compact] {
            assert_eq!(layout.controls.len(), SWATCHES.len() + 4);
            let mut right = layout.grip.x + layout.grip.w;
            for (_, rect) in &layout.controls {
                assert!(rect.x >= right && rect.x + rect.w <= layout.width);
                assert!(rect.y >= 0.0 && rect.y + rect.h <= BAR_HEIGHT);
                right = rect.x + rect.w;
            }
        }
    }

    #[test]
    fn smaller_window_keeps_masks_editable_without_changing_saved_geometry() {
        let state = with_mask();
        let rect = state.displayed(0, (60.0, 50.0));
        assert_eq!(
            rect,
            Rect {
                x: 0.0,
                y: 0.0,
                w: 60.0,
                h: 50.0
            }
        );
        assert_eq!(state.displayed(0, CANVAS), state.masks[0].rect);
    }

    #[test]
    fn panes_own_independent_masks_and_other_windows_share_them() {
        let a = usize::MAX - 10;
        let b = usize::MAX - 11;
        let mut editor = RecordingOverlay::default();
        editor.bind(a, CANVAS);
        editor.begin((100.0, 80.0), editor.canvas_size);
        editor.update_drag((200.0, 160.0), editor.canvas_size);
        editor.commit_drag();
        editor.bind(b, CANVAS);
        assert!(editor.is_empty());
        editor.clear();
        let mut other_window = RecordingOverlay::default();
        other_window.bind(a, (1000.0, 800.0));
        assert_eq!(other_window.masks, vec![mask(200.0, 160.0, 200.0, 160.0)]);
        forget_pane(a);
        forget_pane(b);
    }

    #[test]
    fn cached_preview_sees_edits_and_retains_masks_until_picture_is_dropped() {
        let id = usize::MAX - 12;
        let cached = pane_layer(id);
        assert!(cached.is_empty());
        let saved = mask(0.2, 0.3, 0.4, 0.1);
        save_pane_masks(id, vec![saved]);
        assert_eq!(cached.0.lock().unwrap().masks, vec![saved]);
        save_pane_masks(id, vec![]);
        assert!(cached.is_empty());
        save_pane_masks(id, vec![saved]);
        forget_pane(id);
        assert!(pane_masks(id).is_none());
        assert_eq!(cached.0.lock().unwrap().masks, vec![saved]);
    }

    #[test]
    fn normalized_mask_maps_into_terminal_and_overview_frames() {
        let mask = Rect {
            x: 100.0,
            y: 80.0,
            w: 200.0,
            h: 40.0,
        }
        .normalized((1000.0, 400.0));
        let terminal = mask.in_frame(euclid::rect(300.0, 100.0, 1000.0, 400.0));
        assert_eq!(terminal, euclid::rect(400.0, 180.0, 200.0, 40.0));
        let overview = mask.in_frame(euclid::rect(20.0, 50.0, 250.0, 100.0));
        assert_eq!(overview, euclid::rect(45.0, 70.0, 50.0, 10.0));
        assert_eq!(
            overview.intersection(&euclid::rect(60.0, 50.0, 100.0, 100.0)),
            Some(euclid::rect(60.0, 70.0, 35.0, 10.0))
        );
    }

    #[test]
    fn floating_toolbar_moves_independently_and_stays_reachable() {
        let mut state = with_mask();
        let original_masks = state.masks.clone();
        let bar = state.toolbar_rect((300.0, 100.0), (800.0, 600.0));
        assert_eq!((bar.x, bar.y), (250.0, 452.0));
        state.toolbar_drag = Some(((bar.x + 20.0, bar.y + 10.0), bar));
        state.move_toolbar((1200.0, -200.0), (800.0, 600.0));
        let moved = state.toolbar_rect((300.0, 100.0), (800.0, 600.0));
        assert_eq!((moved.x, moved.y), (500.0, 0.0));
        let small = state.toolbar_rect((300.0, 100.0), (180.0, 90.0));
        assert!(small.x >= 0.0 && small.y >= 0.0);
        assert!(small.x + small.w <= 180.0 && small.y + small.h <= 90.0);
        state.cancel_interaction();
        assert!(state.toolbar_drag.is_none());
        assert_eq!(state.masks, original_masks);
    }

    #[test]
    fn focus_loss_cancels_interaction_without_removing_masks() {
        let mut state = with_mask();
        let original = state.masks.clone();
        state.begin((110.0, 90.0), CANVAS);
        state.update_drag((300.0, 200.0), CANVAS);
        state.pressed = Some(Control::Clear);
        state.finish_key = Some(KeyCode::Char('\u{1b}'));
        state.picking = true;
        state.cancel_interaction();
        state.commit_drag();
        assert_eq!(state.masks, original);
        assert!(state.drag.is_none());
        assert!(state.pressed.is_none());
        assert!(state.finish_key.is_none());
        assert!(!state.picking);
    }

    fn preview_rects(
        layer: &PaneRecordingLayer,
        target: window::RectF,
        clip: window::RectF,
    ) -> Vec<window::RectF> {
        let ground = LinearRgba::with_components(0.1, 0.1, 0.1, 1.0);
        layer
            .preview_rects(target, clip, ground)
            .into_iter()
            .map(|(rect, _)| rect)
            .collect()
    }

    #[test]
    fn preview_masks_follow_text_grid_padding_splits_and_pane_font_metrics() {
        for (frame, grid, cell) in [
            // Default horizontal one-cell and vertical half-cell padding plus nav.
            (
                euclid::rect(280.0, 40.0, 900.0, 600.0),
                euclid::rect(294.0, 84.0, 868.0, 532.0),
                (14.0, 28.0),
            ),
            // Split pane begins half a root cell before its own content grid.
            (
                euclid::rect(701.0, 40.0, 479.0, 600.0),
                euclid::rect(708.0, 84.0, 448.0, 532.0),
                (14.0, 28.0),
            ),
            // Independent pane zoom and a different window/sidebar origin.
            (
                euclid::rect(61.0, 200.0, 1200.0, 900.0),
                euclid::rect(82.0, 260.0, 1155.0, 798.0),
                (21.0, 42.0),
            ),
        ] {
            let layer = PaneRecordingLayer::default();
            let rect = Rect {
                x: grid.min_x() - frame.min_x(),
                y: grid.min_y() - frame.min_y(),
                w: cell.0 * 4.0,
                h: cell.1,
            }
            .normalized((frame.width(), frame.height()));
            layer.0.lock().unwrap().masks = vec![Mask { rect, color: BLACK }];
            layer.set_grid(frame, grid);
            let target = euclid::rect(25.0, 80.0, grid.width() / 4.0, grid.height() / 4.0);
            let result = preview_rects(&layer, target, euclid::rect(0.0, 0.0, 1000.0, 1000.0));
            assert_eq!(result.len(), 1);
            for (got, wanted) in [
                (result[0].min_x(), target.min_x()),
                (result[0].min_y(), target.min_y()),
                (result[0].width(), cell.0),
                (result[0].height(), cell.1 / 4.0),
            ] {
                assert!((got - wanted).abs() < 0.0001, "{} != {}", got, wanted);
            }
        }
    }

    #[test]
    fn preview_masks_take_their_colours_and_the_preview_ground() {
        let layer = PaneRecordingLayer::default();
        let frame = euclid::rect(0.0, 0.0, 100.0, 100.0);
        let ground = LinearRgba::with_components(0.2, 0.3, 0.4, 1.0);
        layer.0.lock().unwrap().masks = vec![
            Mask {
                rect: Rect {
                    x: 0.1,
                    y: 0.1,
                    w: 0.2,
                    h: 0.2,
                },
                color: MaskColor::Background,
            },
            mask(0.5, 0.5, 0.2, 0.2),
        ];
        layer.set_grid(frame, frame);
        let colours: Vec<_> = layer
            .preview_rects(frame, frame, ground)
            .into_iter()
            .map(|(_, colour)| colour)
            .collect();
        assert_eq!(colours, vec![ground, BLACK.linear(ground)]);
    }

    #[test]
    fn missing_grid_only_covers_masked_panes_and_geometry_changes_invalidate() {
        let layer = PaneRecordingLayer::default();
        let frame = euclid::rect(0.0, 0.0, 100.0, 100.0);
        assert!(preview_rects(&layer, frame, frame).is_empty());
        layer.0.lock().unwrap().masks.push(mask(0.1, 0.1, 0.2, 0.2));
        assert_eq!(preview_rects(&layer, frame, frame), vec![frame]);
        layer.set_grid(frame, euclid::rect(10.0, 10.0, 80.0, 80.0));
        let first = layer.revision();
        layer.set_grid(frame, euclid::rect(10.0, 10.0, 80.0, 80.0));
        assert_eq!(layer.revision(), first);
        layer.set_grid(frame, euclid::rect(20.0, 10.0, 70.0, 80.0));
        assert!(layer.revision() > first);
    }

    #[test]
    fn snapshot_keeps_masks_for_unrendered_or_removed_panes() {
        use crate::termwindow::content_view::{
            TerminalPreviewPaneSnapshot, TerminalPreviewSnapshot,
        };
        let id = usize::MAX - 111;
        let saved = mask(0.1, 0.1, 0.2, 0.2);
        save_pane_masks(id, vec![saved]);
        let snapshot = TerminalPreviewSnapshot {
            tab_size: Default::default(),
            splits: Vec::new(),
            panes: vec![TerminalPreviewPaneSnapshot {
                pane_id: id,
                recording_layer: pane_layer(id),
                is_active: true,
                left: 0,
                top: 0,
                width: 0,
                height: 0,
                cols: 0,
                rows: 0,
                resolved_top: 0,
                lines: Vec::new(),
                box_pixel_height: 0,
                dimensions: Default::default(),
                palette: Default::default(),
                cursor: Default::default(),
            }],
        };
        let first = snapshot.recording_revision();
        assert!(first > 0); // zero-sized/off-card panes participate in both key checks
        save_pane_masks(id, vec![saved, saved]);
        let edited = snapshot.recording_revision();
        assert!(edited > first); // retires both a cached picture and its old partial
        forget_pane(id);
        assert_eq!(snapshot.recording_revision(), edited);
        assert_eq!(
            snapshot.panes[0]
                .recording_layer
                .0
                .lock()
                .unwrap()
                .masks
                .len(),
            2
        );
        let frame = euclid::rect(0.0, 0.0, 100.0, 100.0);
        assert_eq!(
            preview_rects(&snapshot.panes[0].recording_layer, frame, frame),
            vec![frame]
        );
    }
    #[test]
    fn preview_uses_full_source_grid_when_the_window_clips_rows_or_columns() {
        let layer = PaneRecordingLayer::default();
        let frame = euclid::rect(100.0, 20.0, 800.0, 600.0);
        let grid = euclid::rect(112.0, 60.0, 1200.0, 960.0);
        // A visible cell near the right edge; the snapshot also contains
        // source columns/rows outside the current window's clipped viewport.
        let rect = Rect {
            x: 12.0 + 60.0 * 12.0,
            y: 40.0 + 20.0 * 24.0,
            w: 12.0,
            h: 24.0,
        }
        .normalized((frame.width(), frame.height()));
        layer.0.lock().unwrap().masks = vec![Mask { rect, color: BLACK }];
        layer.set_grid(frame, grid);
        let preview_grid = euclid::rect(20.0, 30.0, 300.0, 240.0);
        let result = preview_rects(&layer, preview_grid, preview_grid);
        assert_eq!(result.len(), 1);
        for (got, wanted) in [
            (result[0].min_x(), 200.0),
            (result[0].min_y(), 150.0),
            (result[0].width(), 3.0),
            (result[0].height(), 6.0),
        ] {
            assert!((got - wanted).abs() < 0.0001, "{} != {}", got, wanted);
        }
    }
}
