//! Pane-owned recording masks, shared by terminal and preview rendering.
//! normal input passes through unless the user explicitly enters edit mode.
use crate::quad::TripleLayerQuadAllocator;
use crate::ui::{DrawContext, SvgIcon};
use crate::utilsprites::RenderMetrics;
use mux::pane::PaneId;
use std::collections::HashMap;
use std::sync::{Arc, LazyLock, Mutex};
use window::color::LinearRgba;
use window::{KeyCode, KeyEvent, MouseCursor, MouseEvent, MouseEventKind, MousePress, WindowOps};

const MAX_MASKS: usize = 64;
const MIN_SIZE: f32 = 8.0;
const HANDLE_SIZE: f32 = 16.0;
// Above content transitions, tooltips and the command palette (currently 11).
const OVERLAY_ZINDEX: i8 = 20;

#[derive(Clone, Copy, Debug, Default, PartialEq)]
struct Rect {
    x: f32,
    y: f32,
    w: f32,
    h: f32,
}

// GUI-local state follows a pane between windows; no terminal or mux data changes.
// Rectangles are normalized to the pane frame, so previews reuse the same geometry.
#[derive(Clone, Default)]
pub(crate) struct PaneRecordingLayer(Arc<Mutex<Vec<Rect>>>);

impl PaneRecordingLayer {
    fn is_empty(&self) -> bool {
        self.0.lock().unwrap().is_empty()
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

fn save_pane_masks(pane_id: PaneId, masks: Vec<Rect>) {
    *pane_layer(pane_id).0.lock().unwrap() = masks;
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

    fn contains(self, p: (f32, f32)) -> bool {
        p.0 >= self.x && p.0 <= self.x + self.w && p.1 >= self.y && p.1 <= self.y + self.h
    }

    fn handle(self) -> Self {
        Self {
            x: self.x + self.w - HANDLE_SIZE.min(self.w),
            y: self.y + self.h - HANDLE_SIZE.min(self.h),
            w: HANDLE_SIZE.min(self.w),
            h: HANDLE_SIZE.min(self.h),
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

#[derive(Clone, Copy, Debug)]
enum DragKind {
    Create,
    Move(usize, Rect),
    Resize(usize, Rect),
}

#[derive(Clone, Copy, Debug)]
struct Drag {
    kind: DragKind,
    anchor: (f32, f32),
    preview: Rect,
}

#[derive(Default)]
pub(crate) struct RecordingOverlay {
    // UI coordinates, independent of terminal font size and backing DPI.
    masks: Vec<Rect>,
    pane_id: Option<PaneId>,
    canvas_size: (f32, f32),
    pub(crate) editing: bool,
    selected: Option<usize>,
    drag: Option<Drag>,
    buttons: [Option<Rect>; 3],
    toolbar: Option<Rect>,
    toolbar_position: Option<(f32, f32)>,
    toolbar_drag: Option<((f32, f32), Rect)>,
    hovered_button: Option<usize>,
    pressed_button: Option<usize>,
    finish_key: Option<KeyCode>,
}

impl RecordingOverlay {
    fn bind(&mut self, pane_id: PaneId, size: (f32, f32)) {
        self.cancel_interaction();
        self.pane_id = Some(pane_id);
        self.canvas_size = size;
        self.selected = None;
        self.masks = pane_masks(pane_id)
            .map(|masks| {
                masks
                    .0
                    .lock()
                    .unwrap()
                    .iter()
                    .map(|r| Rect {
                        x: r.x * size.0,
                        y: r.y * size.1,
                        w: r.w * size.0,
                        h: r.h * size.1,
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
                    .map(|r| r.normalized(self.canvas_size))
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
        self.masks.clear();
        self.selected = None;
        self.drag = None;
        self.save();
    }

    pub(crate) fn cancel_interaction(&mut self) {
        self.drag = None;
        self.pressed_button = None;
        self.finish_key = None;
        self.toolbar_drag = None;
        self.hovered_button = None;
    }

    fn finish(&mut self) {
        self.editing = false;
        self.cancel_interaction();
        self.buttons = [None; 3];
        self.toolbar = None;
    }

    fn button_enabled(&self, index: usize) -> bool {
        match index {
            0 => true,
            1 => self.selected.is_some(),
            2 => !self.is_empty(),
            _ => false,
        }
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

    fn delete_selected(&mut self) {
        self.drag = None;
        if let Some(index) = self.selected.take() {
            self.masks.remove(index);
            self.save();
        }
    }

    fn hit(&self, point: (f32, f32), bounds: (f32, f32)) -> Option<usize> {
        self.masks
            .iter()
            .rposition(|rect| rect.fitted(bounds).contains(point))
    }

    fn begin(&mut self, point: (f32, f32), bounds: (f32, f32)) {
        self.selected = self.hit(point, bounds);
        let kind = match self.selected {
            Some(index) => {
                let rect = self.masks[index].fitted(bounds);
                if rect.handle().contains(point) {
                    DragKind::Resize(index, rect)
                } else {
                    DragKind::Move(index, rect)
                }
            }
            None if self.masks.len() < MAX_MASKS => DragKind::Create,
            None => return,
        };
        let preview = match kind {
            DragKind::Create => Rect::between(point, point),
            DragKind::Move(_, rect) | DragKind::Resize(_, rect) => rect,
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
        drag.preview = match drag.kind {
            DragKind::Create => Rect::between(drag.anchor, point),
            DragKind::Move(_, original) => Rect {
                x: original.x + point.0 - drag.anchor.0,
                y: original.y + point.1 - drag.anchor.1,
                ..original
            }
            .fitted(bounds),
            DragKind::Resize(_, original) => Rect {
                w: (original.w + point.0 - drag.anchor.0)
                    .max(MIN_SIZE)
                    .min(bounds.0 - original.x),
                h: (original.h + point.1 - drag.anchor.1)
                    .max(MIN_SIZE)
                    .min(bounds.1 - original.y),
                ..original
            },
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
                self.selected = Some(self.masks.len());
                self.masks.push(drag.preview);
            }
            DragKind::Move(index, _) | DragKind::Resize(index, _) => {
                self.masks[index] = drag.preview
            }
        }
        self.save();
    }

    fn displayed(&self, index: usize, bounds: (f32, f32)) -> Rect {
        if let Some(drag) = self.drag {
            match drag.kind {
                DragKind::Move(i, _) | DragKind::Resize(i, _) if i == index => return drag.preview,
                _ => {}
            }
        }
        self.masks[index].fitted(bounds)
    }
}

impl crate::TermWindow {
    fn recording_pane_frame(&self, pane_id: PaneId) -> Option<window::RectF> {
        self.get_panes_to_render()
            .iter()
            .find(|pos| pos.pane.pane_id() == pane_id)
            .and_then(|pos| self.pane_frame_rect(pos).ok())
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

    // This runs inside the terminal world, so a recorded transition includes it.
    pub(crate) fn paint_pane_recording_masks(
        &self,
        pane_id: PaneId,
        frame: window::RectF,
        layers: &mut TripleLayerQuadAllocator,
    ) -> anyhow::Result<()> {
        let black = LinearRgba::with_components(0.0, 0.0, 0.0, 1.0);
        let editing =
            self.recording_overlay.editing && self.recording_overlay.pane_id == Some(pane_id);
        if !editing {
            return self.paint_saved_recording_masks(pane_id, frame, frame, layers, 1.0);
        }
        let state = &self.recording_overlay;
        let accent = self.chrome().accent;
        for index in 0..state.masks.len() {
            let rect = state
                .displayed(index, state.canvas_size)
                .normalized(state.canvas_size)
                .in_frame(frame);
            self.filled_rectangle(layers, 2, rect, black)?;
            if state.selected == Some(index) {
                self.fill_rounded_rectangle_with_border(
                    layers,
                    2,
                    rect,
                    black,
                    accent,
                    0.0,
                    self.ui_f32(1.0),
                )?;
                let side = self.ui_f32(6.0).min(rect.width()).min(rect.height());
                self.fill_rounded_rectangle(
                    layers,
                    2,
                    euclid::rect(rect.max_x() - side, rect.max_y() - side, side, side),
                    accent,
                    self.ui_f32(2.0),
                )?;
            }
        }
        if let Some(Drag {
            kind: DragKind::Create,
            preview,
            ..
        }) = state.drag
        {
            self.filled_rectangle(
                layers,
                2,
                preview.normalized(state.canvas_size).in_frame(frame),
                black,
            )?;
        }
        Ok(())
    }

    pub(crate) fn paint_saved_recording_masks(
        &self,
        pane_id: PaneId,
        frame: window::RectF,
        clip: window::RectF,
        layers: &mut TripleLayerQuadAllocator,
        opacity: f32,
    ) -> anyhow::Result<()> {
        if let Some(masks) = pane_masks(pane_id) {
            self.paint_recording_mask_layer(&masks, frame, clip, layers, opacity)?;
        }
        Ok(())
    }

    pub(crate) fn paint_recording_mask_layer(
        &self,
        masks: &PaneRecordingLayer,
        frame: window::RectF,
        clip: window::RectF,
        layers: &mut TripleLayerQuadAllocator,
        opacity: f32,
    ) -> anyhow::Result<()> {
        for rect in masks.0.lock().unwrap().iter() {
            if let Some(rect) = rect.in_frame(frame).intersection(&clip) {
                self.filled_rectangle(
                    layers,
                    2,
                    rect,
                    LinearRgba::with_components(0.0, 0.0, 0.0, opacity),
                )?;
            }
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
        let scale = self.ui_f32(1.0).max(0.01);
        self.recording_overlay.bind(
            pane.pane_id(),
            (frame.width() / scale, frame.height() / scale),
        );
        self.recording_overlay.editing = true;
        self.recording_overlay.drag = None;
        self.recording_overlay.pressed_button = None;
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
            match key.key {
                KeyCode::Char('\u{1b}') | KeyCode::Char('\r') => {
                    self.recording_overlay.finish();
                    self.recording_overlay.finish_key = Some(key.key.clone());
                }
                KeyCode::Char('\u{7f}') | KeyCode::Char('\u{8}') => {
                    self.recording_overlay.delete_selected();
                }
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
        let point = (
            (event.coords.x as f32 / scale).clamp(0.0, bounds.0),
            (event.coords.y as f32 / scale).clamp(0.0, bounds.1),
        );
        let frame = self
            .recording_overlay
            .pane_id
            .and_then(|id| self.recording_pane_frame(id));
        let Some(frame) = frame else {
            self.recording_overlay.finish();
            context.invalidate();
            return true;
        };
        let canvas = self.recording_overlay.canvas_size;
        let local = (
            (event.coords.x as f32 - frame.min_x()) / frame.width().max(1.0) * canvas.0,
            (event.coords.y as f32 - frame.min_y()) / frame.height().max(1.0) * canvas.1,
        );
        let inside = local.0 >= 0.0 && local.1 >= 0.0 && local.0 <= canvas.0 && local.1 <= canvas.1;
        let local = (local.0.clamp(0.0, canvas.0), local.1.clamp(0.0, canvas.1));
        let button = self
            .recording_overlay
            .buttons
            .iter()
            .position(|rect| rect.is_some_and(|r| r.contains(point)));
        let over_toolbar = self
            .recording_overlay
            .toolbar
            .is_some_and(|rect| rect.contains(point));
        self.recording_overlay.hovered_button = button;
        match event.kind {
            MouseEventKind::Press(MousePress::Left) => {
                if over_toolbar {
                    if let Some(button) = button {
                        if self.recording_overlay.button_enabled(button) {
                            self.recording_overlay.pressed_button = Some(button);
                        }
                    } else if let Some(rect) = self.recording_overlay.toolbar {
                        self.recording_overlay.toolbar_drag = Some((point, rect));
                    }
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
                } else if let Some(pressed) = self.recording_overlay.pressed_button.take() {
                    if button == Some(pressed) {
                        match pressed {
                            0 => self.recording_overlay.finish(),
                            1 => self.recording_overlay.delete_selected(),
                            2 => self.recording_overlay.clear(),
                            _ => {}
                        }
                    }
                } else {
                    self.recording_overlay.update_drag(local, canvas);
                    self.recording_overlay.commit_drag();
                }
            }
            _ => {}
        }
        let cursor = if !self.recording_overlay.editing {
            MouseCursor::Arrow
        } else if over_toolbar {
            if button.is_none_or(|i| self.recording_overlay.button_enabled(i)) {
                MouseCursor::Hand
            } else {
                MouseCursor::Arrow
            }
        } else if let Some(index) = self.recording_overlay.hit(local, canvas).filter(|_| inside) {
            if self
                .recording_overlay
                .displayed(index, canvas)
                .handle()
                .contains(local)
            {
                MouseCursor::SizeNorthWestSouthEast
            } else {
                MouseCursor::Hand
            }
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
        if self
            .recording_overlay
            .pane_id
            .and_then(|id| self.recording_pane_frame(id))
            .is_none()
        {
            self.recording_overlay.finish();
            return Ok(());
        }
        let scale = self.ui_f32(1.0).max(0.01);
        let bounds = (
            self.dimensions.pixel_width as f32 / scale,
            self.dimensions.pixel_height as f32 / scale,
        );
        let gl = self.render_state.as_ref().unwrap();
        let layer = gl.layer_for_zindex(OVERLAY_ZINDEX)?;
        let mut layers = layer.quad_allocator();
        let black = LinearRgba::with_components(0.0, 0.0, 0.0, 1.0);
        let size = (crate::native_settings::home_font_size(&crate::native_settings::load_shared())
            * 0.8)
            .max(10.0);
        let font = self.fonts.title_font_with_size(size)?;
        let metrics = RenderMetrics::with_font_metrics(&font.metrics());
        let ctx = DrawContext::new(gl, self.dimensions, &metrics);
        let palette = self.chrome();
        let text_height = metrics.cell_size.height as f32 / scale;
        let labels = [
            crate::i18n::tr("recording-overlay-done"),
            crate::i18n::tr("recording-overlay-delete"),
            crate::i18n::tr("recording-overlay-clear"),
        ];
        let widths = labels
            .each_ref()
            .map(|label| (ctx.measure_text_width(&font, label) / scale + 36.0).max(136.0));
        let natural_width = widths.iter().sum::<f32>() + 64.0;
        let bar = self
            .recording_overlay
            .toolbar_rect((natural_width, text_height + 96.0), bounds);
        let compression = (bar.w / natural_width).min(1.0);
        self.recording_overlay.toolbar = Some(bar);

        // Small, bounded rounded quads: no blurred framebuffer or animation loop.
        for (spread, offset, alpha) in [(12.0, 8.0, 0.05), (6.0, 5.0, 0.10), (2.0, 3.0, 0.18)] {
            let shadow = Rect {
                x: bar.x - spread,
                y: bar.y - spread + offset,
                w: bar.w + spread * 2.0,
                h: bar.h + spread * 2.0,
            };
            self.fill_rounded_rectangle(
                &mut layers,
                2,
                shadow.pixels(scale),
                black.mul_alpha(alpha),
                (34.0 + spread) * scale,
            )?;
        }
        self.fill_rounded_rectangle_with_border(
            &mut layers,
            2,
            bar.pixels(scale),
            palette.control_bg,
            palette.control_border,
            34.0 * scale,
            scale.max(1.0),
        )?;
        // The whole gap around the buttons is draggable, not just this visual grip.
        self.fill_rounded_rectangle(
            &mut layers,
            2,
            Rect {
                x: bar.x + (bar.w - 48.0) / 2.0,
                y: bar.y + 12.0,
                w: 48.0,
                h: 5.0,
            }
            .pixels(scale),
            palette.muted_text.mul_alpha(0.45),
            2.5 * scale,
        )?;
        let mut x = bar.x + 20.0 * compression;
        let mut buttons = [None; 3];
        for i in [1, 2, 0] {
            let width = widths[i] * compression;
            let rect = Rect {
                x,
                y: bar.y + 28.0,
                w: width,
                h: bar.h - 42.0,
            };
            let enabled = self.recording_overlay.button_enabled(i);
            let hovered = self.recording_overlay.hovered_button == Some(i) && enabled;
            let pressed = self.recording_overlay.pressed_button == Some(i) && hovered;
            let bg = if pressed {
                palette.control_pressed_bg
            } else {
                palette.control_hover_bg
            };
            if hovered {
                self.fill_rounded_rectangle(&mut layers, 2, rect.pixels(scale), bg, 18.0 * scale)?;
            }
            let center_x = x + width / 2.0;
            let icon = match i {
                0 => SvgIcon::Check,
                1 => SvgIcon::Trash2,
                _ => SvgIcon::RotateCcw,
            };
            let tint = if enabled {
                palette.text
            } else {
                palette.muted_text.mul_alpha(0.4)
            };
            let icon_color = if i == 0 { palette.on_accent } else { tint };
            if i == 0 {
                self.fill_rounded_rectangle(
                    &mut layers,
                    2,
                    Rect {
                        x: center_x - 23.0,
                        y: rect.y + 2.0,
                        w: 46.0,
                        h: 46.0,
                    }
                    .pixels(scale),
                    palette.accent,
                    23.0 * scale,
                )?;
            }
            ctx.draw_svg_icon(
                &mut layers,
                icon,
                (center_x - 16.0) * scale,
                (rect.y + 9.0) * scale,
                32.0 * scale,
                icon_color,
            )?;
            let max_width = (width - 12.0).max(0.0) * scale;
            let label_width = ctx.measure_text_width(&font, &labels[i]).min(max_width);
            ctx.draw_text_on_layer(
                &mut layers,
                2,
                &font,
                center_x * scale - label_width / 2.0,
                (rect.y + 56.0) * scale,
                &labels[i],
                tint,
                max_width,
            )?;
            buttons[i] = Some(rect);
            x += width + 12.0 * compression;
        }
        let hint_key = if self.recording_overlay.masks.len() >= MAX_MASKS {
            "recording-overlay-limit"
        } else if self.recording_overlay.toolbar_drag.is_some() {
            "recording-overlay-drag-toolbar"
        } else {
            "recording-overlay-hint"
        };
        let hint = crate::i18n::tr(hint_key);
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
        self.recording_overlay.buttons = buttons;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn with_mask() -> RecordingOverlay {
        let mut state = RecordingOverlay::default();
        state.editing = true;
        state.begin((180.0, 160.0), (500.0, 400.0));
        state.update_drag((100.0, 80.0), (500.0, 400.0));
        state.commit_drag();
        state
    }

    #[test]
    fn reverse_drag_creates_mask_and_finish_keeps_it() {
        let mut state = with_mask();
        assert_eq!(
            state.masks,
            vec![Rect {
                x: 100.0,
                y: 80.0,
                w: 80.0,
                h: 80.0
            }]
        );
        state.finish();
        assert!(!state.owns_keyboard());
        assert_eq!(state.masks.len(), 1);
    }

    #[test]
    fn moving_and_resizing_stay_inside_window() {
        let mut state = with_mask();
        state.begin((110.0, 90.0), (500.0, 400.0));
        state.update_drag((900.0, 900.0), (500.0, 400.0));
        state.commit_drag();
        assert_eq!(
            state.masks[0],
            Rect {
                x: 420.0,
                y: 320.0,
                w: 80.0,
                h: 80.0
            }
        );
        state.begin((495.0, 395.0), (500.0, 400.0));
        state.update_drag((0.0, 0.0), (500.0, 400.0));
        state.commit_drag();
        assert_eq!(state.masks[0].w, MIN_SIZE);
        assert_eq!(state.masks[0].h, MIN_SIZE);
    }

    #[test]
    fn cancelled_drag_and_click_do_not_change_masks() {
        let mut state = with_mask();
        let original = state.masks.clone();
        state.begin((110.0, 90.0), (500.0, 400.0));
        state.update_drag((300.0, 200.0), (500.0, 400.0));
        state.finish();
        assert_eq!(state.masks, original);
        state.begin((10.0, 10.0), (500.0, 400.0));
        state.commit_drag();
        assert_eq!(state.masks, original);
    }

    #[test]
    fn delete_and_limit_are_bounded() {
        let mut state = with_mask();
        state.delete_selected();
        assert!(state.is_empty());
        state.masks = vec![
            Rect {
                x: 100.0,
                y: 100.0,
                w: 10.0,
                h: 10.0
            };
            MAX_MASKS
        ];
        state.begin((1.0, 1.0), (500.0, 400.0));
        assert!(state.drag.is_none());
        state.clear();
        assert!(state.is_empty());
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
        assert_eq!(state.displayed(0, (500.0, 400.0)), state.masks[0]);
    }

    #[test]
    fn panes_own_independent_masks_and_other_windows_share_them() {
        let a = usize::MAX - 10;
        let b = usize::MAX - 11;
        let mut editor = RecordingOverlay::default();
        editor.bind(a, (500.0, 400.0));
        editor.begin((100.0, 80.0), editor.canvas_size);
        editor.update_drag((200.0, 160.0), editor.canvas_size);
        editor.commit_drag();
        editor.bind(b, (500.0, 400.0));
        assert!(editor.is_empty());
        editor.clear();
        let mut other_window = RecordingOverlay::default();
        other_window.bind(a, (1000.0, 800.0));
        assert_eq!(
            other_window.masks,
            vec![Rect {
                x: 200.0,
                y: 160.0,
                w: 200.0,
                h: 160.0
            }]
        );
        forget_pane(a);
        forget_pane(b);
    }

    #[test]
    fn cached_preview_sees_edits_and_retains_masks_until_picture_is_dropped() {
        let id = usize::MAX - 12;
        let cached = pane_layer(id);
        assert!(cached.is_empty());
        let rect = Rect {
            x: 0.2,
            y: 0.3,
            w: 0.4,
            h: 0.1,
        };
        save_pane_masks(id, vec![rect]);
        assert_eq!(*cached.0.lock().unwrap(), vec![rect]);
        save_pane_masks(id, vec![]);
        assert!(cached.is_empty());
        save_pane_masks(id, vec![rect]);
        forget_pane(id);
        assert!(pane_masks(id).is_none());
        assert_eq!(*cached.0.lock().unwrap(), vec![rect]);
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
        state.begin((110.0, 90.0), (500.0, 400.0));
        state.update_drag((300.0, 200.0), (500.0, 400.0));
        state.pressed_button = Some(2);
        state.finish_key = Some(KeyCode::Char('\u{1b}'));
        state.cancel_interaction();
        state.commit_drag();
        assert_eq!(state.masks, original);
        assert!(state.drag.is_none());
        assert!(state.pressed_button.is_none());
        assert!(state.finish_key.is_none());
    }
}
