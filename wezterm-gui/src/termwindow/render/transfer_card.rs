//! Remote downloads and uploads as the dynamic status shows them: the
//! notices finished transfers leave behind, what each card says about a
//! transfer, and what its buttons do. The dynamic status itself is painted in
//! [`super::dynamic_status`].
//!
//! Transfers used to report themselves in the Files panel's transfer strip,
//! where a long file name could push the percentage off the end; they are
//! not painted there any more. Running transfers are derived from the
//! transfer records, which own the live progress. A finished one is a
//! snapshot, kept separately so it can outlive the transfer row it came
//! from.

use super::dynamic_status::{CardContent, Mark, Tone};
use crate::termwindow::remote_files::{
    RemoteTransfer, RemoteTransferKind, RemoteTransferProgress, RemoteTransferSource,
    RemoteTransferStatus, REMOTE_TRANSFER_CANCELED,
};
use crate::termwindow::ui::icons::SvgIcon;
use crate::termwindow::UIItemType;
use config::keyassignment::ClipboardCopyDestination;
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// Finished transfer notices kept at once.
const MAX_CARDS: usize = 3;
const DONE_TTL: Duration = Duration::from_secs(5);

/// A transfer that has stopped, kept long enough to be read. It carries a
/// snapshot of what the card needs because the strip may trim the row it came
/// from before this card is done. Retry cards keep their source row alive
/// until they are dismissed or evicted.
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
    pub completion_detail: Option<String>,
    pub leftover: Option<String>,
    pub shown_at: Instant,
    pub paused_at: Option<Instant>,
    pub closing_at: Option<Instant>,
    /// A success whose time as itself is up: it stays, folded to its tick,
    /// until it is dismissed or a newer one takes its place.
    pub quiet: bool,
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
            completion_detail: None,
            leftover: None,
            shown_at: now,
            paused_at: None,
            closing_at: None,
            quiet: false,
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
            completion_detail: None,
            leftover: None,
            shown_at: now,
            paused_at: None,
            closing_at: None,
            quiet: false,
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
            completion_detail: None,
            leftover: None,
            shown_at: now,
            paused_at: None,
            closing_at: None,
            quiet: false,
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
            completion_detail: None,
            leftover: None,
            shown_at: now,
            paused_at: None,
            closing_at: None,
            quiet: false,
        }
    }

    pub(crate) fn from_transfer(transfer: &RemoteTransfer, now: Instant) -> Option<Self> {
        let (message, leftover) = match &transfer.status {
            RemoteTransferStatus::Running => return None,
            RemoteTransferStatus::Failed(message)
                if message.starts_with(REMOTE_TRANSFER_CANCELED) =>
            {
                return None;
            }
            RemoteTransferStatus::Failed(message) => (Some(message.clone()), None),
            RemoteTransferStatus::FailedWithLeftover { message, leftover } => {
                (Some(message.clone()), Some(leftover.clone()))
            }
            RemoteTransferStatus::Done(_) => (None, None),
        };
        let item_count = transfer.progress.items().map(|(done, _)| done);
        let size = if item_count.is_none() {
            transfer.progress.total()
        } else {
            None
        };
        let remote_path = transfer
            .remote_destination
            .as_ref()
            .map(|path| path.as_str().to_string());
        let remote_dir = transfer
            .remote_destination
            .as_ref()
            .and_then(|path| path.parent())
            .map(|path| path.as_str().to_string());
        let is_folder = item_count.is_some()
            || matches!(
                &transfer.source,
                Some(RemoteTransferSource::DownloadFolder { .. })
            )
            || matches!(&transfer.source, Some(RemoteTransferSource::Upload { local, .. }) if local.is_dir());
        let mut toast = match (transfer.kind, message) {
            (RemoteTransferKind::Download, Some(message)) => Self::download_failed(
                transfer.id,
                transfer.name.clone(),
                transfer.destination.clone(),
                is_folder,
                message,
                transfer.can_retry(),
                now,
            ),
            (RemoteTransferKind::Upload, Some(message)) => Self::upload_failed(
                transfer.id,
                transfer.name.clone(),
                remote_path,
                remote_dir,
                is_folder,
                message,
                transfer.can_retry(),
                now,
            ),
            (RemoteTransferKind::Download, None) => Self::download_done(
                transfer.id,
                transfer.name.clone(),
                transfer.destination.clone()?,
                is_folder,
                size,
                item_count,
                now,
            ),
            (RemoteTransferKind::Upload, None) => Self::upload_done(
                transfer.id,
                transfer.name.clone(),
                remote_path,
                remote_dir,
                is_folder,
                size,
                item_count,
                now,
            ),
            _ => return None,
        };
        if let RemoteTransferStatus::Done(detail) = &transfer.status {
            if is_folder {
                toast.completion_detail = Some(detail.clone());
            }
        }
        toast.leftover = leftover;
        Some(toast)
    }

    /// When a success goes quiet; it stays after, as a failure does, until
    /// the user closes it -- a failure carries the only way back into a
    /// transfer whose strip row is deliberately not shown.
    fn deadline(&self) -> Option<Instant> {
        match self.outcome {
            TransferToastOutcome::Done if !self.quiet => Some(self.shown_at + DONE_TTL),
            _ => None,
        }
    }
}

/// One card's content, either a live transfer or a finished snapshot.
#[derive(Clone)]
pub(crate) enum CardView {
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
    pub(crate) fn id(&self) -> u64 {
        match self {
            Self::Running { id, .. } => *id,
            Self::Toast(toast) => toast.id,
        }
    }
}

impl crate::TermWindow {
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
        self.trim_remote_transfers();
        self.invalidate_window();
    }

    pub(crate) fn dismiss_transfer_toast(&mut self, id: u64) {
        self.transfer_toasts.retain(|toast| toast.id != id);
        self.trim_remote_transfers();
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

    /// Show the download in its folder, selected where the file manager can
    /// do that; with nothing there any more, open the folder it landed in --
    /// the download's own, rather than the setting's current value, which
    /// may have changed since this file was fetched.
    pub(crate) fn open_download_toast_folder(&mut self, id: u64) {
        if let Some(path) = self
            .transfer_toasts
            .iter()
            .find(|toast| toast.id == id && toast.kind == RemoteTransferKind::Download)
            .and_then(|toast| toast.path.clone())
            .filter(|path| path.exists())
        {
            wezterm_open_url::reveal_path(&path);
            return;
        }
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

    pub(super) fn transfer_cards_hidden(&self) -> bool {
        self.context_menu.is_some()
            || self.command_palette.is_some()
            || self.get_modal().is_some()
    }

    /// Whether there is any transfer to show, without gathering them.
    pub(super) fn has_transfer_cards(&self) -> bool {
        !self.transfer_toasts.is_empty()
            || self.right_sidebar_remote_transfers.iter().any(|transfer| {
                matches!(
                    transfer.kind,
                    RemoteTransferKind::Download | RemoteTransferKind::Upload
                ) && transfer.is_running()
            })
    }

    pub(super) fn transfer_card_views(&self) -> Vec<CardView> {
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

    /// Run each success's clock while it can be seen; one whose time is up
    /// goes quiet. True when one did.
    pub(super) fn advance_transfer_toasts(&mut self, now: Instant, visible: &HashSet<u64>, held: bool) -> bool {
        let mut changed = false;
        for toast in &mut self.transfer_toasts {
            if toast.closing_at.is_some() {
                continue;
            }
            // Held under the pointer, and held while the dynamic status has no room
            // for it: a notice should not burn its five seconds where
            // nobody can see it.
            if held || !visible.contains(&toast.id) {
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
                    toast.quiet = true;
                    changed = true;
                }
            }
        }
        changed
    }

    /// When the next finished notice is due to go, so a frame is there to
    /// see it off.
    pub(crate) fn next_transfer_toast_deadline(&self) -> Option<Instant> {
        self.transfer_toasts
            .iter()
            .filter(|toast| toast.closing_at.is_none() && toast.paused_at.is_none())
            .filter_map(TransferToast::deadline)
            .min()
    }

    /// Drop the notices whose time is up.
    pub(super) fn drop_closed_transfer_toasts(&mut self, now: Instant) {
        let before = self.transfer_toasts.len();
        self.transfer_toasts.retain(|toast| toast.closing_at.is_none());
        if self.transfer_toasts.len() != before {
            self.trim_remote_transfers();
            // This frame already painted the expired snapshot.
            self.update_next_frame_time(Some(now));
        }
    }
}

/// Something a card offers: a round button with an icon.
pub(crate) struct CardAction {
    pub icon: SvgIcon,
    pub primary: bool,
    pub item: UIItemType,
}

pub(crate) fn card_content(view: &CardView) -> CardContent {
    match view {
        CardView::Running {
            kind,
            id,
            name,
            progress,
            canceling,
        } => CardContent {
            quiet: false,
            mark: Mark::Icon(match kind {
                RemoteTransferKind::Upload => SvgIcon::Upload,
                _ => SvgIcon::Download,
            }),
            tone: Tone::Progress,
            name: name.clone(),
            detail: running_detail(progress, *canceling, *kind),
            short: running_short(progress),
            bar: Some(progress.fraction()),
            actions: if *canceling {
                Vec::new()
            } else {
                vec![CardAction {
                    icon: SvgIcon::X,
                    primary: false,
                    item: UIItemType::TransferCardCancel(*id),
                }]
            },
        },
        CardView::Toast(toast) => {
            let close = CardAction {
                icon: SvgIcon::X,
                primary: false,
                item: UIItemType::TransferCardDismiss(toast.id),
            };
            match &toast.outcome {
                TransferToastOutcome::Failed(message) => {
                    let title = match toast.kind {
                        RemoteTransferKind::Upload => crate::i18n::tr("upload-card-failed"),
                        _ => crate::i18n::tr("download-card-failed"),
                    };
                    let mut actions = Vec::new();
                    if toast.retryable {
                        actions.push(CardAction {
                            icon: SvgIcon::RotateCw,
                            primary: true,
                            item: UIItemType::TransferCardRetry(toast.id),
                        });
                    }
                    actions.push(close);
                    let detail = match &toast.leftover {
                        Some(leftover) => format!("{title} · {leftover} was left behind — {message}"),
                        None => format!("{title} · {message}"),
                    };
                    CardContent {
                        quiet: false,
                        mark: Mark::Icon(SvgIcon::CircleAlert),
                        tone: Tone::Failed,
                        name: toast.name.clone(),
                        detail,
                        short: crate::i18n::tr("right-agents-state-error"),
                        bar: None,
                        actions,
                    }
                }
                TransferToastOutcome::Done => {
                    let (detail, actions) = match toast.kind {
                        RemoteTransferKind::Upload => (
                            upload_done_detail(toast),
                            if toast.remote_path.is_some() {
                                vec![CardAction {
                                    icon: SvgIcon::Copy,
                                    primary: false,
                                    item: UIItemType::TransferCardCopyPath(toast.id),
                                }]
                            } else {
                                vec![close]
                            },
                        ),
                        _ => (
                            download_done_detail(toast),
                            vec![
                                CardAction {
                                    icon: if toast.is_folder {
                                        SvgIcon::FolderOpen
                                    } else {
                                        SvgIcon::ExternalLink
                                    },
                                    primary: true,
                                    item: UIItemType::TransferCardOpen(toast.id),
                                },
                                CardAction {
                                    icon: SvgIcon::Folder,
                                    primary: false,
                                    item: UIItemType::TransferCardOpenDownloads(toast.id),
                                },
                            ],
                        ),
                    };
                    // Gone quiet, it can be put away.
                    let mut actions = actions;
                    let dismiss = UIItemType::TransferCardDismiss(toast.id);
                    if toast.quiet && !actions.iter().any(|action| action.item == dismiss) {
                        actions.push(CardAction {
                            icon: SvgIcon::X,
                            primary: false,
                            item: dismiss,
                        });
                    }
                    CardContent {
                        quiet: toast.quiet,
                        mark: Mark::Icon(SvgIcon::Check),
                        tone: Tone::Done,
                        name: toast.name.clone(),
                        detail,
                        short: String::new(),
                        bar: None,
                        actions,
                    }
                }
            }
        }
    }
}


fn download_done_detail(toast: &TransferToast) -> String {
    let mut parts = Vec::new();
    if let Some(detail) = &toast.completion_detail {
        parts.push(detail.clone());
    }
    if let Some(size) = toast.size {
        parts.push(format_size(size));
    }
    if let Some(count) = toast.item_count {
        let mut args = fluent_bundle::FluentArgs::new();
        args.set("count", count as i64);
        parts.push(crate::i18n::tr_args("transfer-card-items", &args));
    }
    if let Some(path) = toast
        .path
        .as_ref()
        .filter(|_| toast.completion_detail.is_none())
    {
        let folder = path.parent().unwrap_or(path);
        let mut args = fluent_bundle::FluentArgs::new();
        args.set("path", display_folder(folder));
        parts.push(crate::i18n::tr_args("download-card-saved-to", &args));
    }
    parts.join(" · ")
}

fn upload_done_detail(toast: &TransferToast) -> String {
    let mut parts = Vec::new();
    if let Some(detail) = &toast.completion_detail {
        parts.push(detail.clone());
    }
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

/// The folded pill's count: items for a folder, otherwise the percentage.
fn running_short(progress: &RemoteTransferProgress) -> String {
    if let Some((done, total)) = progress.items() {
        return format!("{done} / {total}");
    }
    progress
        .fraction()
        .map(|fraction| format!("{}%", (fraction * 100.0).floor() as u32))
        .unwrap_or_default()
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

#[cfg(test)]
mod tests {
    use super::*;

    fn folder_transfer(kind: RemoteTransferKind, status: RemoteTransferStatus) -> RemoteTransfer {
        let progress = RemoteTransferProgress::default();
        progress.set_item_total(12);
        let remote =
            crate::termwindow::remote_files::RemotePath::from_server_absolute("/srv/tree").unwrap();
        let source = match kind {
            RemoteTransferKind::Upload => RemoteTransferSource::Upload {
                local: PathBuf::from("/tmp/tree"),
                remote: remote.clone(),
                directory: remote.parent().unwrap(),
            },
            _ => RemoteTransferSource::DownloadFolder { remote },
        };
        RemoteTransfer {
            id: 1,
            kind,
            name: "tree".into(),
            progress,
            status,
            source: Some(source),
            origin: Some(crate::termwindow::remote_files::RemoteOperationOrigin::new(
                "server-a".into(),
                "server-a".into(),
            )),
            destination: Some(PathBuf::from("/tmp/tree")),
            remote_destination: None,
        }
    }

    #[test]
    fn canceled_or_failed_transfers_keep_their_leftover_notice() {
        for kind in [RemoteTransferKind::Download, RemoteTransferKind::Upload] {
            for message in [REMOTE_TRANSFER_CANCELED, "connection lost"] {
                let transfer = folder_transfer(
                    kind,
                    RemoteTransferStatus::FailedWithLeftover {
                        message: message.into(),
                        leftover: "/tmp/partial-tree".into(),
                    },
                );
                let toast = TransferToast::from_transfer(&transfer, Instant::now()).unwrap();
                assert_eq!(toast.deadline(), None);
                let content = card_content(&CardView::Toast(toast));
                assert!(content.detail.contains("/tmp/partial-tree"));
                assert!(content.detail.contains(message));
            }
        }
        let canceled = folder_transfer(
            RemoteTransferKind::Download,
            RemoteTransferStatus::Failed(REMOTE_TRANSFER_CANCELED.into()),
        );
        assert!(TransferToast::from_transfer(&canceled, Instant::now()).is_none());
    }

    #[test]
    fn a_finished_transfer_goes_quiet_and_can_be_put_away() {
        let now = Instant::now();
        let transfer = folder_transfer(RemoteTransferKind::Download, RemoteTransferStatus::Done(String::new()));
        let mut toast = TransferToast::from_transfer(&transfer, now).unwrap();
        assert_eq!(toast.deadline(), Some(now + DONE_TTL));
        let content = card_content(&CardView::Toast(toast.clone()));
        assert!(!content.quiet);
        assert!(!content
            .actions
            .iter()
            .any(|action| action.item == UIItemType::TransferCardDismiss(toast.id)));
        toast.quiet = true;
        assert_eq!(toast.deadline(), None);
        let content = card_content(&CardView::Toast(toast.clone()));
        assert!(content.quiet);
        assert!(content
            .actions
            .iter()
            .any(|action| action.item == UIItemType::TransferCardDismiss(toast.id)));
    }

    #[test]
    fn folder_completion_keeps_omissions_in_the_card() {
        for kind in [RemoteTransferKind::Download, RemoteTransferKind::Upload] {
            let detail = "2 symlink(s) skipped, 1 unreadable item(s)";
            let transfer = folder_transfer(kind, RemoteTransferStatus::Done(detail.into()));
            let toast = TransferToast::from_transfer(&transfer, Instant::now()).unwrap();
            let content = card_content(&CardView::Toast(toast));
            assert!(content.detail.contains(detail));
        }
    }

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
    fn a_finished_card_expires_after_its_ttl() {
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
