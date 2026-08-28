//! Why a folder ThinkTerm was pointed at cannot be opened. Shared by the
//! Notes vault, the Files tree and Thread opening so all three tell the same
//! story about one refusal. Classify on whichever thread touched the
//! filesystem: painting may never do IO.

use crate::termwindow::ui::icons::SvgIcon;
use std::io::ErrorKind;

/// What is wrong with the folder itself. Only about the directory: a consumer
/// with failure modes of its own keeps those in its own type.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FolderProblem {
    /// The folder is gone: renamed, deleted, or on a volume that is not
    /// mounted.
    MissingRoot,
    /// The folder is there but the system refuses to open it; on macOS this
    /// is almost always TCC.
    UnreadableRoot,
    /// Any other IO failure.
    Other,
}

impl FolderProblem {
    /// Classify the error from a `read_dir` on the folder. Pure, so the
    /// mapping is testable without a filesystem.
    pub(crate) fn from_read_dir_kind(kind: ErrorKind) -> Self {
        match kind {
            ErrorKind::NotFound => Self::MissingRoot,
            // Covers both EACCES and EPERM; a macOS TCC denial arrives as the
            // latter.
            ErrorKind::PermissionDenied => Self::UnreadableRoot,
            _ => Self::Other,
        }
    }

    pub(crate) fn icon(self) -> SvgIcon {
        match self {
            // A folder we cannot get into reads as a folder problem, not a
            // fault; the rest are genuine faults.
            Self::UnreadableRoot => SvgIcon::FolderOpen,
            Self::MissingRoot | Self::Other => SvgIcon::CircleAlert,
        }
    }

    /// The headline, for a consumer with nothing more specific to say. A panel
    /// that can name what it was trying to open ("Notes folder not found")
    /// should say that instead; this is the folder-generic wording.
    pub(crate) fn title(self) -> String {
        crate::i18n::tr(match self {
            Self::MissingRoot => "folder-missing",
            Self::UnreadableRoot => "folder-unreadable",
            Self::Other => "folder-open-error",
        })
    }

    /// What to do about it, with one sentence naming where this platform
    /// hides the switch. Each branch spells out its own `tr("...")` literal:
    /// the i18n scan test only sees literals.
    pub(crate) fn hint(self) -> Option<String> {
        match self {
            Self::MissingRoot => Some(crate::i18n::tr("folder-missing-hint")),
            Self::UnreadableRoot => {
                let mut hint = crate::i18n::tr("folder-unreadable-hint");
                hint.push(' ');
                hint.push_str(&if cfg!(target_os = "macos") {
                    crate::i18n::tr("folder-unreadable-macos")
                } else if cfg!(windows) {
                    crate::i18n::tr("folder-unreadable-windows")
                } else {
                    // Every remaining unix. Worded without naming a distro so
                    // it stays true on the BSDs too.
                    crate::i18n::tr("folder-unreadable-unix")
                });
                Some(hint)
            }
            Self::Other => None,
        }
    }

    /// Whether a native-picker selection could change this answer: only a
    /// permission refusal, and only on macOS, where an open-panel selection
    /// grants access (TCC never re-prompts by itself). Elsewhere a picker
    /// confers nothing.
    pub(crate) fn can_reauthorize(self) -> bool {
        cfg!(target_os = "macos") && matches!(self, Self::UnreadableRoot)
    }
}

/// A Thread could not be opened because its Project directory is unusable.
/// Travels inside `anyhow` and is recovered with `downcast_ref` by callers
/// that have a window to show it in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ProjectRootUnavailable {
    pub(crate) path: std::path::PathBuf,
    pub(crate) problem: FolderProblem,
    /// The raw OS refusal, kept for the detail line. It names the path and the
    /// errno, which is what makes a bug report actionable.
    pub(crate) detail: String,
}

impl std::fmt::Display for ProjectRootUnavailable {
    fn fmt(&self, fmt: &mut std::fmt::Formatter) -> std::fmt::Result {
        write!(
            fmt,
            "Project directory {} cannot be opened: {}",
            self.path.display(),
            self.detail
        )
    }
}

impl std::error::Error for ProjectRootUnavailable {}

impl ProjectRootUnavailable {
    /// Classify a failure to list the Project directory.
    pub(crate) fn from_io(path: impl Into<std::path::PathBuf>, err: &std::io::Error) -> Self {
        Self {
            path: path.into(),
            problem: FolderProblem::from_read_dir_kind(err.kind()),
            detail: format!("{err}"),
        }
    }

    /// Recover a Project-root refusal from an error chain, whichever layer
    /// produced it: the GUI's pre-spawn probe throws this type directly, and
    /// the mux-level `require_cwd` backstop throws `RequiredCwdUnavailable`.
    pub(crate) fn from_error_chain(err: &anyhow::Error) -> Option<Self> {
        if let Some(failure) = err.downcast_ref::<Self>() {
            return Some(failure.clone());
        }
        err.downcast_ref::<mux::domain::RequiredCwdUnavailable>()
            .map(|refused| Self {
                path: refused.dir.clone(),
                problem: FolderProblem::from_read_dir_kind(refused.kind),
                detail: refused.detail.clone(),
            })
    }
}

#[cfg(test)]
mod test {
    use super::*;

    #[test]
    fn read_dir_errors_are_classified_by_kind() {
        assert_eq!(
            FolderProblem::from_read_dir_kind(ErrorKind::NotFound),
            FolderProblem::MissingRoot
        );
        assert_eq!(
            FolderProblem::from_read_dir_kind(ErrorKind::PermissionDenied),
            FolderProblem::UnreadableRoot
        );
        assert_eq!(
            FolderProblem::from_read_dir_kind(ErrorKind::InvalidData),
            FolderProblem::Other
        );
    }

    /// Every key each variant can reach has to exist in every shipped locale.
    /// A missing one renders as the raw key, which is how an untranslated
    /// string reaches a user.
    #[test]
    fn every_problem_has_translations() {
        for problem in [
            FolderProblem::MissingRoot,
            FolderProblem::UnreadableRoot,
            FolderProblem::Other,
        ] {
            let title = problem.title();
            assert!(!title.is_empty(), "{problem:?} has no title");
            assert!(
                !title.starts_with("folder-"),
                "{problem:?} title fell back to its key: {title}"
            );
            if let Some(hint) = problem.hint() {
                assert!(
                    !hint.contains("folder-unreadable-") && !hint.starts_with("folder-"),
                    "{problem:?} hint fell back to a key: {hint}"
                );
            }
        }
    }

    /// The picker button is macOS-only and permission-only: a missing folder
    /// is not handed back by selecting it, even there.
    #[test]
    fn only_macos_permission_denial_can_be_reauthorized() {
        assert!(!FolderProblem::MissingRoot.can_reauthorize());
        assert!(!FolderProblem::Other.can_reauthorize());
        assert_eq!(
            FolderProblem::UnreadableRoot.can_reauthorize(),
            cfg!(target_os = "macos")
        );
    }
}
