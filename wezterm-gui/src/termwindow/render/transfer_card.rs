//! The floating card that follows a remote download or upload.
//!
//! Transfers used to report themselves in the Files panel's transfer strip:
//! one narrow row at the bottom of the panel, where a long file name could
//! push the percentage off the end. Downloads and uploads are no longer
//! painted there at all; this card is the one place they show progress and
//! result. It sits in the window's own bottom-right corner — over the right
//! sidebar when that is open.
//!
//! Downloads end with Open File/Folder and Open Download Folder; uploads end
//! with Copy Path. Running cards are derived from the transfer records,
//! which already own the live progress. A finished card is a snapshot, kept
//! separately so it can outlive the transfer row it came from.

use crate::termwindow::remote_files::{
    RemoteTransferKind, RemoteTransferProgress, RemoteTransferStatus,
};
use config::keyassignment::ClipboardCopyDestination;
use crate::termwindow::ui::icons::SvgIcon;
use crate::termwindow::{UIItem, UIItemType};
use crate::ui::draw::DrawContext;
use crate::utilsprites::RenderMetrics;
use anyhow::Context;
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::{Duration, Instant};
use window::color::LinearRgba;

/// Above the chrome and the right sidebar, below content-view flights,
/// tooltips and menus. A layer of its own: two surfaces sharing one draw each
/// other's text through their backgrounds.
const ZINDEX: i8 = 6;

/// Points, not the chrome's half-point design pixels: the update capsule uses
/// the same unit for its floating pill, and these two sit in the same corner.
const POINT: f32 = 2.0;
const CARD_WIDTH: f32 = 360.0;
const PAD: f32 = 14.0;
const GAP: f32 = 8.0;
const MARGIN: f32 = 10.0;
const RADIUS: f32 = 12.0;
const ICON_DISC: f32 = 24.0;
const ICON: f32 = 14.0;
const BAR_HEIGHT: f32 = 6.0;
const BUTTON_HEIGHT: f32 = 28.0;
const BUTTON_PAD: f32 = 10.0;
const BUTTON_ICON: f32 = 14.0;
const BUTTON_ICON_GAP: f32 = 5.0;
const CLOSE_HIT: f32 = 24.0;
const CLOSE_GLYPH: f32 = 12.0;
const STACK_GAP: f32 = 8.0;
/// Leave room above the update capsule when it is in the same corner.
const CAPSULE_LIFT: f32 = 42.0;
const MAX_CARDS: usize = 3;
const DONE_TTL: Duration = Duration::from_secs(12);
const FADE_IN: Duration = Duration::from_millis(180);
const FADE_OUT: Duration = Duration::from_millis(180);
const FRAME: Duration = Duration::from_millis(16);

/// A transfer that has stopped, kept long enough to be read. It carries a
/// snapshot of what the card needs because the strip may trim the row it came
/// from before this card is done; the id only ties it to the transfer that
/// produced it.
#[derive(Clone, Debug)]
pub(crate) struct TransferToast {
    pub kind: RemoteTransferKind,
    pub id: u64,
    pub name: String,
    /// Where a download landed. `None` for an upload.
    pub path: Option<PathBuf>,
    /// The remote path an upload occupies, for Copy Path. `None` for a
    /// download.
    pub remote_path: Option<String>,
    /// The remote directory an upload landed in, for the detail line.
    pub remote_dir: Option<String>,
    pub is_folder: bool,
    pub size: Option<u64>,
    pub item_count: Option<u64>,
    /// A failed transfer can be run again from its transfer row, provided the
    /// row kept its source and host. Successes never offer Retry.
    pub retryable: bool,
    pub outcome: TransferToastOutcome,
    pub shown_at: Instant,
    pub paused_at: Option<Instant>,
    pub closing_at: Option<Instant>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum TransferToastOutcome {
    Done,
    Failed(String),
}

impl TransferToast {
    pub(crate) fn download_done(
        id: u64,
        name: String,
        path: PathBuf,
        is_folder: bool,
        size: Option<u64>,
        item_count: Option<u64>,
        now: Instant,
    ) -> Self {
        Self {
            kind: RemoteTransferKind::Download,
            id,
            name,
            path: Some(path),
            remote_path: None,
            remote_dir: None,
            is_folder,
            size,
            item_count,
            retryable: false,
            outcome: TransferToastOutcome::Done,
            shown_at: now,
            paused_at: None,
            closing_at: None,
        }
    }

    pub(crate) fn upload_done(
        id: u64,
        name: String,
        remote_path: Option<String>,
        remote_dir: Option<String>,
        is_folder: bool,
        size: Option<u64>,
        item_count: Option<u64>,
        now: Instant,
    ) -> Self {
        Self {
            kind: RemoteTransferKind::Upload,
            id,
            name,
            path: None,
            remote_path,
            remote_dir,
            is_folder,
            size,
            item_count,
            retryable: false,
            outcome: TransferToastOutcome::Done,
            shown_at: now,
            paused_at: None,
            closing_at: None,
        }
    }

    pub(crate) fn download_failed(
        id: u64,
        name: String,
        path: Option<PathBuf>,
        is_folder: bool,
        message: String,
        retryable: bool,
        now: Instant,
    ) -> Self {
        Self {
            kind: RemoteTransferKind::Download,
            id,
            name,
            path,
            remote_path: None,
            remote_dir: None,
            is_folder,
            size: None,
            item_count: None,
            retryable,
            outcome: TransferToastOutcome::Failed(message),
            shown_at: now,
            paused_at: None,
            closing_at: None,
        }
    }

    pub(crate) fn upload_failed(
        id: u64,
        name: String,
        remote_path: Option<String>,
        remote_dir: Option<String>,
        is_folder: bool,
        message: String,
        retryable: bool,
        now: Instant,
    ) -> Self {
        Self {
            kind: RemoteTransferKind::Upload,
            id,
            name,
            path: None,
            remote_path,
            remote_dir,
            is_folder,
            size: None,
            item_count: None,
            retryable,
            outcome: TransferToastOutcome::Failed(message),
            shown_at: now,
            paused_at: None,
            closing_at: None,
        }
    }

    /// Success notices make way for the next transfer. A failure stays until
    /// the user closes it or retries: it carries the only way back into a
    /// transfer whose strip row is deliberately not shown.
    fn deadline(&self) -> Option<Instant> {
        match self.outcome {
            TransferToastOutcome::Done => Some(self.shown_at + DONE_TTL),
            TransferToastOutcome::Failed(_) => None,
        }
    }

    /// 0 to 1, transparent while arriving and leaving, opaque in between.
    fn alpha(&self, now: Instant) -> f32 {
        let arrive = (now.duration_since(self.shown_at).as_secs_f32() / FADE_IN.as_secs_f32())
            .clamp(0.0, 1.0);
        let arrive = arrive * arrive * (3.0 - 2.0 * arrive);
        let leave = self.closing_at.map_or(1.0, |at| {
            1.0 - (now.duration_since(at).as_secs_f32() / FADE_OUT.as_secs_f32()).clamp(0.0, 1.0)
        });
        arrive * leave
    }

    fn fading(&self, now: Instant) -> bool {
        now.duration_since(self.shown_at) < FADE_IN
            || self
                .closing_at
                .is_some_and(|at| now.duration_since(at) < FADE_OUT)
    }
}

/// One card's content, either a live transfer or a finished snapshot.
enum CardView {
    Running {
        kind: RemoteTransferKind,
        id: u64,
        name: String,
        progress: RemoteTransferProgress,
        canceling: bool,
    },
    Toast(TransferToast),
}

impl CardView {
    fn id(&self) -> u64 {
        match self {
            Self::Running { id, .. } => *id,
            Self::Toast(toast) => toast.id,
        }
    }
}

impl crate::TermWindow {
    /// Paint the running and recently finished transfer cards. Called after
    /// the update capsule, so the two can share the corner without guessing
    /// at each other's height.
    pub(crate) fn paint_transfer_cards(&mut self) -> anyhow::Result<()> {
        let now = Instant::now();
        let focused = crate::termwindow::app_has_focus();
        let hidden = self.transfer_cards_hidden();
        let mut views = self.transfer_card_views();

        // A running card is the immediate answer to a drop from Finder, and
        // the window's focus flag can lag behind the drop by a frame or two.
        // Keep running cards visible in that gap; finished notices still wait
        // for the app to be looked at again.
        if !focused {
            views.retain(|view| matches!(view, CardView::Running { .. }));
        }

        if views.is_empty() {
            // Nothing to show: let finished cards start their clock from the
            // moment the app is next looked at, rather than expire unseen.
            if hidden || !focused {
                for toast in &mut self.transfer_toasts {
                    if toast.closing_at.is_none() && toast.paused_at.is_none() {
                        toast.paused_at = Some(now);
                    }
                }
            }
            return Ok(());
        }

        if hidden {
            // Pause rather than expire: a menu must not eat the completion
            // notice.
            for toast in &mut self.transfer_toasts {
                if toast.closing_at.is_none() && toast.paused_at.is_none() {
                    toast.paused_at = Some(now);
                }
            }
            return Ok(());
        }

        if !focused {
            // Running cards paint below; keep the finished ones' clocks held
            // until someone is looking at the window again.
            for toast in &mut self.transfer_toasts {
                if toast.closing_at.is_none() && toast.paused_at.is_none() {
                    toast.paused_at = Some(now);
                }
            }
        }

        let settings = crate::native_settings::load_shared();
        let font_size = crate::native_settings::home_font_size(&settings);
        let title_font = self
            .fonts
            .title_font_with_size(font_size)
            .context("download card title font")?;
        let body_font = self
            .fonts
            .title_font_with_size((font_size - 1.0).max(9.0))
            .context("download card body font")?;
        let title_metrics = RenderMetrics::with_font_metrics(&title_font.metrics());
        let body_metrics = RenderMetrics::with_font_metrics(&body_font.metrics());
        let title_line = title_metrics.cell_size.height as f32;
        let body_line = body_metrics.cell_size.height as f32;

        let scale = self.ui_f32(1.0);
        let px = move |value: f32| value * POINT * scale;
        let pad = px(PAD);
        let gap = px(GAP);
        let bar_h = px(BAR_HEIGHT).max(1.0);
        let button_h = px(BUTTON_HEIGHT).max(1.0);

        let border = self.get_os_border();
        let (_, padding_top) = self.padding_left_top();
        let (top_tab_height, bottom_tab_height) = self.pane_area_insets();
        let content_bottom =
            self.dimensions.pixel_height as f32 - border.bottom.get() as f32 - bottom_tab_height;
        let grid_top = border.top.get() as f32 + top_tab_height + padding_top;
        let grid_bottom = grid_top + self.terminal_size.pixel_height as f32;
        let floor = if settings.terminal.bottom_quote_enabled {
            grid_bottom
        } else {
            content_bottom
        };

        // The window's own bottom-right corner, not the terminal area's
        // corner: when the right sidebar is open the card sits over it,
        // because this card is now the only place a download reports itself.
        let right =
            self.dimensions.pixel_width as f32 - border.right.get() as f32 - px(MARGIN);
        // Over the sidebar when it is open, so the card does not reach into
        // the terminal; at the full width when the sidebar is put away.
        let sidebar_room = self.right_sidebar_width() as f32 - px(MARGIN) * 2.0;
        let mut bottom = floor - px(MARGIN);
        // The update capsule sits left of an open sidebar, so only the
        // sidebar-less corner needs the card lifted above it.
        if sidebar_room <= 0.0 && self.update_capsule_visible() {
            bottom -= px(CAPSULE_LIFT);
        }
        let available = (right - px(MARGIN)).max(px(240.0));
        let width = if sidebar_room > 0.0 {
            px(CARD_WIDTH).min(sidebar_room).max(px(240.0))
        } else {
            px(CARD_WIDTH)
        }
        .min(available);

        // One decision per finished card: two labelled buttons fit side by
        // side in most locales, but a long translation stacks them instead of
        // squeezing one into an ellipsis.
        let button_room = width - pad * 2.0;
        let stacked: Vec<bool> = views
            .iter()
            .map(|view| match view {
                CardView::Running { .. } => false,
                CardView::Toast(toast)
                    if toast.kind == RemoteTransferKind::Download
                        && matches!(toast.outcome, TransferToastOutcome::Done) =>
                {
                    self.transfer_toast_buttons_stack(toast, &body_font, button_room, px)
                        .unwrap_or(false)
                }
                CardView::Toast(_) => false,
            })
            .collect();

        // Never let the newest card, the one that matters, be the one pushed
        // off the top; older ones simply wait for room.
        let top_limit = border.top.get() as f32 + top_tab_height + px(20.0);
        let mut rects = Vec::with_capacity(views.len());
        for (index, view) in views.iter().enumerate().rev() {
            let height = card_height(
                view,
                stacked[index],
                pad,
                gap,
                title_line,
                body_line,
                bar_h,
                button_h,
            );
            if !rects.is_empty() && bottom - height < top_limit {
                break;
            }
            let rect = euclid::rect(right - width, bottom - height, width, height);
            rects.push((view.id(), rect));
            bottom -= height + px(STACK_GAP);
        }
        rects.reverse();

        self.advance_transfer_toasts(now, &rects);

        let gl_state = self
            .render_state
            .as_ref()
            .context("download card render state")?;
        let layer = gl_state
            .layer_for_zindex(ZINDEX)
            .context("download card layer")?;
        let mut layers = layer.quad_allocator();
        let ctx = DrawContext::new(gl_state, self.dimensions, &title_metrics);

        let chrome = self.chrome();
        let mut card_colors = chrome.as_menu_card();
        crate::native_settings::apply_ui_colors(&mut card_colors, self.config.ui_colors.as_ref());
        let dark = card_colors.is_dark();
        let shadow = LinearRgba::with_components(0.0, 0.0, 0.0, if dark { 0.58 } else { 0.43 });

        let mut running = false;
        let mut wants_frame = false;
        let mut card_items = Vec::new();

        for (index, view) in views.iter().enumerate() {
            let Some((_, base_rect)) = rects.iter().find(|(id, _)| *id == view.id()) else {
                continue;
            };
            let rect = *base_rect;

            let (alpha, rise) = match view {
                CardView::Running { .. } => (1.0, 0.0),
                CardView::Toast(toast) => {
                    let Some(current) = self
                        .transfer_toasts
                        .iter()
                        .find(|candidate| candidate.id == toast.id)
                    else {
                        // It finished fading between layout and paint.
                        continue;
                    };
                    let alpha = current.alpha(now);
                    if current.fading(now) {
                        wants_frame = true;
                    } else if current.paused_at.is_none() {
                        // While the pointer rests on it the deadline is held;
                        // a repaint every frame would only spin. A failure has
                        // no deadline: it waits for Close or Retry.
                        if let Some(deadline) = current.deadline() {
                            self.update_next_frame_time(Some(deadline));
                        }
                    }
                    let arrive = (now.duration_since(current.shown_at).as_secs_f32()
                        / FADE_IN.as_secs_f32())
                    .clamp(0.0, 1.0);
                    (alpha, (1.0 - arrive) * px(8.0))
                }
            };
            if alpha <= 0.001 {
                continue;
            }
            let rect = rect.translate(euclid::vec2(0.0, rise));

            ctx.draw_elevated_surface(
                &mut layers,
                0,
                rect,
                card_colors.control_bg.mul_alpha(alpha),
                card_colors.control_border.mul_alpha(alpha),
                shadow.mul_alpha(alpha),
                px(RADIUS),
            )
            .context("download card surface")?;

            // The whole surface swallows clicks, not just the buttons. The
            // buttons are pushed after this and win the reverse-order hit
            // test, so only empty parts of the card reach this item.
            card_items.push(ui_item(rect, UIItemType::TransferCardBackground));

            match view {
                CardView::Running {
                    kind,
                    id,
                    name,
                    progress,
                    canceling,
                } => {
                    running = true;
                    self.paint_running_transfer_card(
                        &mut layers,
                        &ctx,
                        &title_font,
                        title_metrics,
                        &body_font,
                        body_metrics,
                        &card_colors,
                        &chrome,
                        rect,
                        *kind,
                        *id,
                        name,
                        progress,
                        *canceling,
                        alpha,
                        pad,
                        gap,
                        title_line,
                        body_line,
                        bar_h,
                        &mut card_items,
                        px,
                    )?;
                }
                CardView::Toast(toast) => {
                    self.paint_finished_transfer_card(
                        &mut layers,
                        &ctx,
                        &title_font,
                        title_metrics,
                        &body_font,
                        body_metrics,
                        &card_colors,
                        &chrome,
                        rect,
                        toast,
                        stacked[index],
                        alpha,
                        pad,
                        gap,
                        title_line,
                        body_line,
                        button_h,
                        &mut card_items,
                        px,
                    )?;
                }
            }
        }

        drop(layers);
        drop(ctx);
        self.ui_items.extend(card_items);

        if running {
            // The percentage and the indeterminate bar both move; 100ms is the
            // same cadence the transfer strip uses.
            self.update_next_frame_time(Some(now + Duration::from_millis(100)));
        }
        if wants_frame {
            self.update_next_frame_time(Some(now + FRAME));
        }

        // Remove cards that finished fading this frame. They were painted at
        // (or very near) zero alpha, so the next frame's layout simply closes
        // the gap.
        self.transfer_toasts.retain(|toast| {
            toast
                .closing_at
                .map_or(true, |at| now.duration_since(at) < FADE_OUT)
        });

        Ok(())
    }

    /// Keep at most [`MAX_CARDS`] finished notices; a burst of transfers must
    /// not grow the corner without bound. The Files strip remains the log for
    /// everything that is not a download or upload.
    pub(crate) fn push_transfer_toast(&mut self, toast: TransferToast) {
        self.transfer_toasts
            .retain(|existing| existing.id != toast.id);
        self.transfer_toasts.push(toast);
        self.transfer_toasts.sort_by_key(|toast| toast.id);
        while self.transfer_toasts.len() > MAX_CARDS {
            self.transfer_toasts.remove(0);
        }
        self.invalidate_window();
    }

    pub(crate) fn dismiss_transfer_toast(&mut self, id: u64) {
        self.transfer_toasts.retain(|toast| toast.id != id);
        self.invalidate_window();
    }

    /// Put an uploaded item's remote path on the clipboard. Downloads use
    /// their Open buttons instead; this is only offered for uploads.
    pub(crate) fn copy_transfer_path(&mut self, id: u64) {
        let Some(path) = self
            .transfer_toasts
            .iter()
            .find(|toast| toast.id == id)
            .and_then(|toast| toast.remote_path.clone())
        else {
            return;
        };
        self.copy_to_clipboard(ClipboardCopyDestination::Clipboard, path);
    }

    /// Retry the failed transfer behind a card. If the recorded host is no
    /// longer the one the Files panel shows, the retry refuses; put that
    /// reason on the card instead of leaving the old error there.
    pub(crate) fn retry_transfer_from_card(&mut self, id: u64, anchor: ::window::Point) {
        if self.retry_remote_transfer(id, anchor) {
            self.dismiss_transfer_toast(id);
        } else if let Some(message) = self
            .right_sidebar_remote_transfers
            .iter()
            .find(|transfer| transfer.id == id)
            .and_then(|transfer| match &transfer.status {
                RemoteTransferStatus::Failed(message)
                | RemoteTransferStatus::FailedWithLeftover { message, .. } => {
                    Some(message.clone())
                }
                _ => None,
            })
        {
            if let Some(toast) = self
                .transfer_toasts
                .iter_mut()
                .find(|toast| toast.id == id)
            {
                if let TransferToastOutcome::Failed(existing) = &mut toast.outcome {
                    *existing = message;
                }
            }
        }
        self.invalidate_window();
    }

    /// Open the downloaded file, or the downloaded folder. If it was moved or
    /// deleted since the card appeared, fall back to the folder that held it.
    pub(crate) fn open_download_toast(&mut self, id: u64) {
        let Some(path) = self
            .transfer_toasts
            .iter()
            .find(|toast| toast.id == id && toast.kind == RemoteTransferKind::Download)
            .and_then(|toast| toast.path.clone())
        else {
            return;
        };
        if path.exists() {
            wezterm_open_url::open_url(&path.to_string_lossy());
        } else if let Some(parent) = path.parent() {
            wezterm_open_url::open_url(&parent.to_string_lossy());
        }
    }

    /// Open the folder the download landed in. The button is deliberately the
    /// download's parent rather than the setting's current value: the setting
    /// may have changed since this file was fetched.
    pub(crate) fn open_download_toast_folder(&mut self, id: u64) {
        let folder = self
            .transfer_toasts
            .iter()
            .find(|toast| toast.id == id && toast.kind == RemoteTransferKind::Download)
            .and_then(|toast| toast.path.as_deref())
            .and_then(Path::parent)
            .map(Path::to_path_buf)
            .or_else(crate::native_settings::effective_remote_download_directory);
        if let Some(folder) = folder {
            wezterm_open_url::open_url(&folder.to_string_lossy());
        }
    }

    pub(crate) fn cancel_transfer(&mut self, id: u64) {
        if let Some(transfer) = self
            .right_sidebar_remote_transfers
            .iter()
            .find(|transfer| transfer.id == id && transfer.is_running())
        {
            transfer.progress.request_cancel();
            self.invalidate_window();
        }
    }

    fn transfer_cards_hidden(&self) -> bool {
        self.context_menu.is_some()
            || self.command_palette.is_some()
            || self.get_modal().is_some()
    }

    fn transfer_card_views(&self) -> Vec<CardView> {
        let mut running: Vec<CardView> = self
            .right_sidebar_remote_transfers
            .iter()
            .filter(|transfer| {
                matches!(
                    transfer.kind,
                    RemoteTransferKind::Download | RemoteTransferKind::Upload
                ) && transfer.is_running()
            })
            .map(|transfer| CardView::Running {
                kind: transfer.kind,
                id: transfer.id,
                name: transfer.name.clone(),
                progress: transfer.progress.clone(),
                canceling: transfer.progress.is_canceled(),
            })
            .collect();
        let mut finished: Vec<CardView> = self
            .transfer_toasts
            .iter()
            .cloned()
            .map(CardView::Toast)
            .collect();
        running.sort_by_key(CardView::id);
        finished.sort_by_key(CardView::id);

        // A live transfer outranks a notice: starting three small transfers
        // must never push the progress of the big one out of the corner.
        if running.len() >= MAX_CARDS {
            running.drain(..running.len() - MAX_CARDS);
            finished.clear();
        } else {
            let room = MAX_CARDS - running.len();
            if finished.len() > room {
                finished.drain(..finished.len() - room);
            }
        }

        let mut views = running;
        views.extend(finished);
        views.sort_by_key(CardView::id);
        views
    }

    fn transfer_toast_buttons_stack(
        &self,
        toast: &TransferToast,
        body_font: &std::rc::Rc<wezterm_font::LoadedFont>,
        room: f32,
        px: impl Fn(f32) -> f32,
    ) -> anyhow::Result<bool> {
        let primary = if toast.is_folder {
            crate::i18n::tr("download-open-folder")
        } else {
            crate::i18n::tr("download-open-file")
        };
        let secondary = crate::i18n::tr("download-open-downloads");
        let primary = self.sidebar_text_width(body_font, &primary)?;
        let secondary = self.sidebar_text_width(body_font, &secondary)?;
        let icon = px(BUTTON_ICON) + px(BUTTON_ICON_GAP);
        let padding = px(BUTTON_PAD) * 2.0;
        // Side by side means each gets half the room; the wider label is what
        // decides whether that half is enough.
        let half = (room - px(GAP)) / 2.0;
        Ok(primary.max(secondary) + icon + padding > half)
    }

    fn advance_transfer_toasts(
        &mut self,
        now: Instant,
        rects: &[(u64, euclid::Rect<f32, window::PixelUnit>)],
    ) {
        let hovered: HashSet<u64> = rects
            .iter()
            .filter(|(_, rect)| pointer_over(self, *rect))
            .map(|(id, _)| *id)
            .collect();
        let visible: HashSet<u64> = rects.iter().map(|(id, _)| *id).collect();

        for toast in &mut self.transfer_toasts {
            if toast.closing_at.is_some() {
                continue;
            }
            // Held under the pointer, and held while the three-card cap has
            // no room for it: a notice should not burn its twelve seconds
            // where nobody can see it.
            if hovered.contains(&toast.id) || !visible.contains(&toast.id) {
                if toast.paused_at.is_none() {
                    toast.paused_at = Some(now);
                }
            } else if let Some(paused) = toast.paused_at.take() {
                // Push the deadline out by exactly the time the pointer (or a
                // menu) held it still.
                toast.shown_at += now.duration_since(paused);
            }
            if let Some(deadline) = toast.deadline() {
                if toast.paused_at.is_none() && now >= deadline {
                    toast.closing_at = Some(now);
                }
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn paint_running_transfer_card(
        &self,
        layers: &mut crate::quad::TripleLayerQuadAllocator,
        ctx: &DrawContext,
        title_font: &std::rc::Rc<wezterm_font::LoadedFont>,
        title_metrics: RenderMetrics,
        body_font: &std::rc::Rc<wezterm_font::LoadedFont>,
        body_metrics: RenderMetrics,
        card: &crate::ui::tokens::UiPalette,
        chrome: &crate::ui::tokens::UiPalette,
        rect: euclid::Rect<f32, window::PixelUnit>,
        kind: RemoteTransferKind,
        id: u64,
        name: &str,
        progress: &RemoteTransferProgress,
        canceling: bool,
        alpha: f32,
        pad: f32,
        gap: f32,
        title_line: f32,
        body_line: f32,
        bar_h: f32,
        ui_items: &mut Vec<UIItem>,
        px: impl Fn(f32) -> f32,
    ) -> anyhow::Result<()> {
        let title_y = rect.origin.y + pad;
        let title_x = rect.origin.x + pad + px(ICON_DISC) + gap * 0.75;
        let title_right = rect.max_x() - pad;
        let cancel = crate::i18n::tr("right-cancel");
        let cancel_width = if canceling {
            0.0
        } else {
            self.sidebar_text_width(body_font, &cancel)? + px(PAD)
        };
        let title_width = (title_right - title_x - cancel_width - gap * 0.5).max(0.0);
        let text_color = card.text.mul_alpha(alpha);
        let muted = card.muted_text.mul_alpha(alpha);

        self.paint_disc_icon(
            layers,
            ctx,
            rect.origin.x + pad + px(ICON_DISC) / 2.0,
            title_y + title_line / 2.0,
            match kind {
                RemoteTransferKind::Upload => SvgIcon::Upload,
                _ => SvgIcon::Download,
            },
            chrome.accent.mul_alpha(0.16 * alpha),
            chrome.accent.mul_alpha(alpha),
            &px,
        )?;

        self.paint_sidebar_text(
            layers,
            title_font,
            title_metrics,
            name,
            title_x.max(0.0) as usize,
            title_y.max(0.0) as usize,
            title_width.max(0.0) as usize,
            text_color,
        )?;

        if !canceling {
            let cancel_h = title_line.max(px(CLOSE_HIT));
            let cancel_rect = euclid::rect(
                title_right - cancel_width,
                title_y + (title_line - cancel_h) / 2.0,
                cancel_width,
                cancel_h,
            );
            let hovered = pointer_over(self, cancel_rect);
            self.paint_sidebar_text(
                layers,
                body_font,
                body_metrics,
                &cancel,
                (cancel_rect.origin.x + px(PAD) / 2.0).max(0.0) as usize,
                (cancel_rect.origin.y + (cancel_h - body_line) / 2.0).max(0.0) as usize,
                (cancel_width - px(PAD)).max(0.0) as usize,
                if hovered {
                    chrome.danger.mul_alpha(alpha)
                } else {
                    muted
                },
            )?;
            ui_items.push(ui_item(cancel_rect, UIItemType::TransferCardCancel(id)));
        }

        let detail = running_detail(progress, canceling, kind);
        let detail_y = title_y + title_line + gap * 0.5;
        self.paint_sidebar_text(
            layers,
            body_font,
            body_metrics,
            &detail,
            title_x.max(0.0) as usize,
            detail_y.max(0.0) as usize,
            (title_right - title_x).max(0.0) as usize,
            muted,
        )?;

        let track = euclid::rect(
            title_x,
            rect.max_y() - pad - bar_h,
            (title_right - title_x).max(0.0),
            bar_h,
        );
        let radius = bar_h / 2.0;
        self.fill_rounded_rectangle(
            layers,
            1,
            track,
            chrome.separator.mul_alpha(0.72 * alpha),
            radius,
        )
        .context("download card progress track")?;
        let (fill_x, fill_w) = match progress.fraction() {
            Some(fraction) => (track.origin.x, (track.size.width * fraction).max(bar_h)),
            None => {
                // No honest denominator yet: a segment crossing the track is
                // better than an empty one that reads as stuck.
                let epoch = ANIMATION_EPOCH.get_or_init(Instant::now);
                let phase = epoch.elapsed().as_secs_f32() % 1.2 / 1.2;
                let segment = (track.size.width * 0.3).max(bar_h * 2.0);
                let raw_start = phase * (track.size.width + segment) - segment;
                let start = raw_start.max(0.0);
                let end = (raw_start + segment).min(track.size.width);
                (track.origin.x + start, (end - start).max(0.0))
            }
        };
        if fill_w > 0.0 {
            self.fill_rounded_rectangle(
                layers,
                1,
                euclid::rect(fill_x, track.origin.y, fill_w, bar_h),
                chrome.accent.mul_alpha(alpha),
                radius,
            )
            .context("download card progress fill")?;
        }
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    fn paint_finished_transfer_card(
        &self,
        layers: &mut crate::quad::TripleLayerQuadAllocator,
        ctx: &DrawContext,
        title_font: &std::rc::Rc<wezterm_font::LoadedFont>,
        title_metrics: RenderMetrics,
        body_font: &std::rc::Rc<wezterm_font::LoadedFont>,
        body_metrics: RenderMetrics,
        card: &crate::ui::tokens::UiPalette,
        chrome: &crate::ui::tokens::UiPalette,
        rect: euclid::Rect<f32, window::PixelUnit>,
        toast: &TransferToast,
        stacked: bool,
        alpha: f32,
        pad: f32,
        gap: f32,
        title_line: f32,
        body_line: f32,
        button_h: f32,
        ui_items: &mut Vec<UIItem>,
        px: impl Fn(f32) -> f32,
    ) -> anyhow::Result<()> {
        let failed = matches!(toast.outcome, TransferToastOutcome::Failed(_));
        let title_y = rect.origin.y + pad;
        let title_x = rect.origin.x + pad + px(ICON_DISC) + gap * 0.75;
        let title_right = rect.max_x() - pad;
        let close_rect = euclid::rect(
            title_right - px(CLOSE_HIT),
            title_y + (title_line - px(CLOSE_HIT)) / 2.0,
            px(CLOSE_HIT),
            px(CLOSE_HIT),
        );
        let title_width = (close_rect.origin.x - title_x - gap * 0.5).max(0.0);
        let text_color = card.text.mul_alpha(alpha);
        let muted = card.muted_text.mul_alpha(alpha);
        let white = LinearRgba::with_srgba(255, 255, 255, 255);

        let (icon, disc_color, icon_color) = if failed {
            (
                SvgIcon::CircleAlert,
                chrome.danger.mul_alpha(alpha),
                white.mul_alpha(alpha),
            )
        } else {
            (
                SvgIcon::CircleCheck,
                crate::thread_marks::FINISHED_COLOR.mul_alpha(alpha),
                white.mul_alpha(alpha),
            )
        };
        self.paint_disc_icon(
            layers,
            ctx,
            rect.origin.x + pad + px(ICON_DISC) / 2.0,
            title_y + title_line / 2.0,
            icon,
            disc_color,
            icon_color,
            &px,
        )?;

        let title = match (toast.kind, failed) {
            (RemoteTransferKind::Upload, true) => crate::i18n::tr("upload-card-failed"),
            (RemoteTransferKind::Upload, false) => crate::i18n::tr("upload-completed"),
            (_, true) => crate::i18n::tr("download-card-failed"),
            (_, false) => crate::i18n::tr("download-completed"),
        };
        self.paint_sidebar_text(
            layers,
            title_font,
            title_metrics,
            &title,
            title_x.max(0.0) as usize,
            title_y.max(0.0) as usize,
            title_width.max(0.0) as usize,
            if failed {
                chrome.danger.mul_alpha(alpha)
            } else {
                text_color
            },
        )?;

        let name_y = title_y + title_line + gap * 0.5;
        self.paint_sidebar_text(
            layers,
            body_font,
            body_metrics,
            &toast.name,
            title_x.max(0.0) as usize,
            name_y.max(0.0) as usize,
            (title_right - title_x).max(0.0) as usize,
            text_color,
        )?;

        let detail_y = name_y + body_line + gap * 0.5;
        let detail = match &toast.outcome {
            TransferToastOutcome::Done => match toast.kind {
                RemoteTransferKind::Upload => upload_done_detail(toast),
                _ => download_done_detail(toast),
            },
            TransferToastOutcome::Failed(message) => message.clone(),
        };
        self.paint_sidebar_text(
            layers,
            body_font,
            body_metrics,
            &detail,
            title_x.max(0.0) as usize,
            detail_y.max(0.0) as usize,
            (title_right - title_x).max(0.0) as usize,
            muted,
        )?;

        let close_hovered = pointer_over(self, close_rect);
        ctx.draw_svg_icon(
            layers,
            SvgIcon::X,
            close_rect.origin.x + (px(CLOSE_HIT) - px(CLOSE_GLYPH)) / 2.0,
            close_rect.origin.y + (px(CLOSE_HIT) - px(CLOSE_GLYPH)) / 2.0,
            px(CLOSE_GLYPH),
            if close_hovered { text_color } else { muted },
        )
        .context("download card close icon")?;
        ui_items.push(ui_item(
            close_rect,
            UIItemType::TransferCardDismiss(toast.id),
        ));

        let room = rect.size.width - pad * 2.0;
        let buttons_y = detail_y + body_line + gap;

        if failed {
            if toast.retryable {
                let retry = crate::i18n::tr("right-retry");
                let retry_rect =
                    euclid::rect(rect.origin.x + pad, buttons_y, room, button_h);
                self.paint_card_button(
                    layers,
                    body_font,
                    body_metrics,
                    card,
                    chrome,
                    retry_rect,
                    SvgIcon::RotateCcw,
                    &retry,
                    true,
                    alpha,
                    &px,
                )?;
                ui_items.push(ui_item(
                    retry_rect,
                    UIItemType::TransferCardRetry(toast.id),
                ));
            }
            return Ok(());
        }

        if toast.kind == RemoteTransferKind::Upload {
            if toast.remote_path.is_some() {
                let copy = crate::i18n::tr("right-copy-path");
                let copy_rect =
                    euclid::rect(rect.origin.x + pad, buttons_y, room, button_h);
                self.paint_card_button(
                    layers,
                    body_font,
                    body_metrics,
                    card,
                    chrome,
                    copy_rect,
                    SvgIcon::Copy,
                    &copy,
                    false,
                    alpha,
                    &px,
                )?;
                ui_items.push(ui_item(
                    copy_rect,
                    UIItemType::TransferCardCopyPath(toast.id),
                ));
            }
            return Ok(());
        }

        let primary = if toast.is_folder {
            crate::i18n::tr("download-open-folder")
        } else {
            crate::i18n::tr("download-open-file")
        };
        let secondary = crate::i18n::tr("download-open-downloads");
        let (primary_rect, secondary_rect) = if stacked {
            (
                euclid::rect(rect.origin.x + pad, buttons_y, room, button_h),
                euclid::rect(
                    rect.origin.x + pad,
                    buttons_y + button_h + gap,
                    room,
                    button_h,
                ),
            )
        } else {
            let width = ((room - gap) / 2.0).max(0.0);
            (
                euclid::rect(rect.origin.x + pad, buttons_y, width, button_h),
                euclid::rect(
                    rect.origin.x + pad + width + gap,
                    buttons_y,
                    width,
                    button_h,
                ),
            )
        };

        self.paint_card_button(
            layers,
            body_font,
            body_metrics,
            card,
            chrome,
            primary_rect,
            if toast.is_folder {
                SvgIcon::FolderOpen
            } else {
                SvgIcon::File
            },
            &primary,
            true,
            alpha,
            &px,
        )?;
        ui_items.push(ui_item(
            primary_rect,
            UIItemType::TransferCardOpen(toast.id),
        ));

        self.paint_card_button(
            layers,
            body_font,
            body_metrics,
            card,
            chrome,
            secondary_rect,
            SvgIcon::FolderOpen,
            &secondary,
            false,
            alpha,
            &px,
        )?;
        ui_items.push(ui_item(
            secondary_rect,
            UIItemType::TransferCardOpenDownloads(toast.id),
        ));

        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    fn paint_disc_icon(
        &self,
        layers: &mut crate::quad::TripleLayerQuadAllocator,
        ctx: &DrawContext,
        cx: f32,
        cy: f32,
        icon: SvgIcon,
        disc: LinearRgba,
        color: LinearRgba,
        px: &impl Fn(f32) -> f32,
    ) -> anyhow::Result<()> {
        ctx.draw_disc(layers, 1, cx, cy, px(ICON_DISC), disc)
            .context("download card icon disc")?;
        ctx.draw_svg_icon(
            layers,
            icon,
            cx - px(ICON) / 2.0,
            cy - px(ICON) / 2.0,
            px(ICON),
            color,
        )
        .context("download card icon")?;
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    fn paint_card_button(
        &self,
        layers: &mut crate::quad::TripleLayerQuadAllocator,
        body_font: &std::rc::Rc<wezterm_font::LoadedFont>,
        body_metrics: RenderMetrics,
        card: &crate::ui::tokens::UiPalette,
        chrome: &crate::ui::tokens::UiPalette,
        rect: euclid::Rect<f32, window::PixelUnit>,
        icon: SvgIcon,
        label: &str,
        primary: bool,
        alpha: f32,
        px: &impl Fn(f32) -> f32,
    ) -> anyhow::Result<()> {
        let hovered = pointer_over(self, rect);
        let radius = px(6.0);
        let (fill, border, content) = if primary {
            (
                if hovered {
                    chrome.accent_hover
                } else {
                    chrome.accent
                }
                .mul_alpha(alpha),
                LinearRgba::with_components(0.0, 0.0, 0.0, 0.0),
                chrome.on_accent.mul_alpha(alpha),
            )
        } else {
            (
                if hovered {
                    chrome.control_hover_bg
                } else {
                    card.control_bg
                }
                .mul_alpha(alpha),
                card.control_border.mul_alpha(alpha),
                card.text.mul_alpha(alpha),
            )
        };
        if border.3 > 0.0 {
            self.fill_rounded_rectangle(layers, 1, rect, border, radius)
                .context("download card button border")?;
            let inner = euclid::rect(
                rect.origin.x + 1.0,
                rect.origin.y + 1.0,
                (rect.size.width - 2.0).max(0.0),
                (rect.size.height - 2.0).max(0.0),
            );
            self.fill_rounded_rectangle(layers, 1, inner, fill, (radius - 1.0).max(0.0))
                .context("download card button fill")?;
        } else {
            self.fill_rounded_rectangle(layers, 1, rect, fill, radius)
                .context("download card button fill")?;
        }

        let text_width = self.sidebar_text_width(body_font, label)?;
        let icon_size = px(BUTTON_ICON);
        let icon_gap = px(BUTTON_ICON_GAP);
        let content_width = icon_size + icon_gap + text_width;
        let start = rect.origin.x + ((rect.size.width - content_width) / 2.0).max(px(BUTTON_PAD));
        self.paint_ui_icon(
            layers,
            2,
            icon,
            start.max(0.0) as usize,
            (rect.origin.y + (rect.size.height - icon_size) / 2.0).max(0.0) as usize,
            icon_size.max(1.0) as usize,
            content,
        )?;
        self.paint_sidebar_text(
            layers,
            body_font,
            body_metrics,
            label,
            (start + icon_size + icon_gap).max(0.0) as usize,
            (rect.origin.y + (rect.size.height - body_metrics.cell_size.height as f32) / 2.0)
                .max(0.0) as usize,
            (rect.size.width - (start - rect.origin.x) - icon_size - icon_gap - px(BUTTON_PAD))
                .max(0.0) as usize,
            content,
        )?;
        Ok(())
    }
}

fn download_done_detail(toast: &TransferToast) -> String {
    let mut parts = Vec::new();
    if let Some(size) = toast.size {
        parts.push(format_size(size));
    }
    if let Some(count) = toast.item_count {
        let mut args = fluent_bundle::FluentArgs::new();
        args.set("count", count as i64);
        parts.push(crate::i18n::tr_args("transfer-card-items", &args));
    }
    if let Some(path) = &toast.path {
        let folder = path.parent().unwrap_or(path);
        let mut args = fluent_bundle::FluentArgs::new();
        args.set("path", display_folder(folder));
        parts.push(crate::i18n::tr_args("download-card-saved-to", &args));
    }
    parts.join(" · ")
}

fn upload_done_detail(toast: &TransferToast) -> String {
    let mut parts = Vec::new();
    if let Some(size) = toast.size {
        parts.push(format_size(size));
    }
    if let Some(count) = toast.item_count {
        let mut args = fluent_bundle::FluentArgs::new();
        args.set("count", count as i64);
        parts.push(crate::i18n::tr_args("transfer-card-items", &args));
    }
    if let Some(directory) = &toast.remote_dir {
        let mut args = fluent_bundle::FluentArgs::new();
        args.set("path", directory.clone());
        parts.push(crate::i18n::tr_args("upload-card-uploaded-to", &args));
    }
    parts.join(" · ")
}

/// The download folder as a person names it: `~/Downloads` rather than the
/// full home path, which otherwise pushes the useful end of the line out of
/// the card.
fn display_folder(path: &Path) -> String {
    if let Some(home) = dirs_next::home_dir() {
        if let Ok(rest) = path.strip_prefix(&home) {
            if rest.as_os_str().is_empty() {
                return "~".to_string();
            }
            return format!("~/{}", rest.display());
        }
    }
    path.display().to_string()
}

fn running_detail(
    progress: &RemoteTransferProgress,
    canceling: bool,
    kind: RemoteTransferKind,
) -> String {
    if canceling {
        return crate::i18n::tr("transfer-card-canceling");
    }
    if let Some((done, total)) = progress.items() {
        return format!("{done} / {total}");
    }
    if let Some(fraction) = progress.fraction() {
        let total = progress.total().unwrap_or(0);
        return format!(
            "{}% · {} / {}",
            (fraction * 100.0).round() as u32,
            format_size(progress.transferred()),
            format_size(total)
        );
    }
    if progress.transferred() > 0 {
        return format!(
            "{} · {}",
            match kind {
                RemoteTransferKind::Upload => crate::i18n::tr("upload-card-uploading"),
                _ => crate::i18n::tr("update-capsule-downloading"),
            },
            format_size(progress.transferred())
        );
    }
    crate::i18n::tr("transfer-card-preparing")
}

/// Binary units, one decimal past bytes, matching Settings' size language
/// without rounding a small file to "0.0 MB".
fn format_size(bytes: u64) -> String {
    const UNITS: [&str; 4] = ["B", "KB", "MB", "GB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit + 1 < UNITS.len() {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

#[allow(clippy::too_many_arguments)]
fn card_height(
    view: &CardView,
    stacked: bool,
    pad: f32,
    gap: f32,
    title_line: f32,
    body_line: f32,
    bar_h: f32,
    button_h: f32,
) -> f32 {
    match view {
        CardView::Running { .. } => {
            pad * 2.0 + title_line + gap * 0.5 + body_line + gap * 0.75 + bar_h
        }
        CardView::Toast(toast) => match toast.outcome {
            TransferToastOutcome::Done => {
                let (button_area, button_gap) = match toast.kind {
                    RemoteTransferKind::Upload => {
                        if toast.remote_path.is_some() {
                            (button_h, gap)
                        } else {
                            (0.0, 0.0)
                        }
                    }
                    _ => (
                        if stacked {
                            button_h * 2.0 + gap
                        } else {
                            button_h
                        },
                        gap,
                    ),
                };
                pad * 2.0
                    + title_line
                    + gap * 0.5
                    + body_line
                    + gap * 0.5
                    + body_line
                    + button_gap
                    + button_area
            }
            TransferToastOutcome::Failed(_) => {
                pad * 2.0
                    + title_line
                    + gap * 0.5
                    + body_line
                    + gap * 0.5
                    + body_line
                    + if toast.retryable {
                        gap + button_h
                    } else {
                        0.0
                    }
            }
        },
    }
}

fn pointer_over(window: &crate::TermWindow, rect: euclid::Rect<f32, window::PixelUnit>) -> bool {
    window.is_pointer_over_ui_rect(
        rect.origin.x.max(0.0) as usize,
        rect.origin.y.max(0.0) as usize,
        rect.size.width as usize,
        rect.size.height as usize,
    )
}

fn ui_item(rect: euclid::Rect<f32, window::PixelUnit>, item_type: UIItemType) -> UIItem {
    UIItem {
        x: rect.origin.x.max(0.0) as usize,
        y: rect.origin.y.max(0.0) as usize,
        width: rect.size.width as usize,
        height: rect.size.height as usize,
        item_type,
    }
}

static ANIMATION_EPOCH: OnceLock<Instant> = OnceLock::new();

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sizes_keep_a_small_file_out_of_megabytes() {
        assert_eq!(format_size(0), "0 B");
        assert_eq!(format_size(999), "999 B");
        assert_eq!(format_size(1024), "1.0 KB");
        assert_eq!(format_size(1536), "1.5 KB");
        assert_eq!(format_size(1024 * 1024), "1.0 MB");
        assert_eq!(format_size(3 * 1024 * 1024 * 1024), "3.0 GB");
    }

    #[test]
    fn a_finished_card_fades_in_and_expires_after_its_ttl() {
        let now = Instant::now();
        let toast = TransferToast::download_done(
            1,
            "a.txt".to_string(),
            PathBuf::from("/tmp/a.txt"),
            false,
            Some(1),
            None,
            now,
        );
        assert_eq!(toast.alpha(now), 0.0);
        assert!(toast.alpha(now + FADE_IN) > 0.99);
        assert_eq!(toast.alpha(now + DONE_TTL), 1.0);
        assert_eq!(toast.deadline(), Some(now + DONE_TTL));
    }

    #[test]
    fn a_failure_card_waits_for_retry_or_close() {
        let now = Instant::now();
        let toast = TransferToast::download_failed(
            2,
            "b.txt".to_string(),
            None,
            false,
            "connection lost".to_string(),
            true,
            now,
        );
        assert_eq!(
            toast.outcome,
            TransferToastOutcome::Failed("connection lost".to_string())
        );
        assert_eq!(toast.size, None);
        assert_eq!(toast.item_count, None);
        assert!(toast.retryable);
        assert_eq!(toast.deadline(), None);
    }

    #[test]
    fn a_finished_detail_names_the_folder_not_the_file_twice() {
        let now = Instant::now();
        let toast = TransferToast::download_done(
            3,
            "report.pdf".to_string(),
            PathBuf::from("/nonesuch/Downloads/report.pdf"),
            false,
            Some(2 * 1024 * 1024),
            None,
            now,
        );
        let detail = download_done_detail(&toast);
        assert!(detail.starts_with("2.0 MB"));
        assert!(detail.contains("/nonesuch/Downloads"));
        assert!(!detail.contains("report.pdf"));
    }

    #[test]
    fn a_folder_detail_counts_items_without_a_byte_total() {
        let now = Instant::now();
        let toast = TransferToast::download_done(
            5,
            "tree".to_string(),
            PathBuf::from("/nonesuch/Downloads/tree"),
            true,
            None,
            Some(12),
            now,
        );
        let detail = download_done_detail(&toast);
        assert!(detail.contains("12"));
        assert!(detail.contains("/nonesuch/Downloads"));
    }

    #[test]
    fn an_upload_detail_names_its_remote_folder() {
        let now = Instant::now();
        let toast = TransferToast::upload_done(
            6,
            "up.txt".to_string(),
            Some("/srv/project/up.txt".to_string()),
            Some("/srv/project".to_string()),
            false,
            Some(1234),
            None,
            now,
        );
        let detail = upload_done_detail(&toast);
        assert!(detail.starts_with("1.2 KB"));
        assert!(detail.contains("/srv/project"));
        assert_eq!(toast.kind, RemoteTransferKind::Upload);
    }
}
