//! How the running copy of ThinkTerm got onto this machine, and therefore
//! who is allowed to replace it.
//!
//! Only a script install is ours to update: it is the one case where
//! nothing else tracks the files. Everything else has an owner -- a package
//! manager, Homebrew, the AppImage's own updater, the person who ran
//! `cargo build` -- and the right move is to name that owner, not to write
//! over its files.

use crate::InstallManifest;
use std::path::Path;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InstallMethod {
    /// Put here by `install.sh`; the manifest says where and which variant.
    Script(InstallManifest),
    /// Running from an AppImage, which updates through AppImageUpdate.
    AppImage,
    /// A Homebrew formula or cask (macOS or Linuxbrew).
    Homebrew,
    /// From the Nix store.
    Nix,
    /// Under `/usr/bin` or another system directory without a manifest:
    /// a deb, rpm, apk or distribution package.
    SystemPackage,
    /// The Windows installer.
    WindowsInstaller,
    /// A ThinkTerm.app that no manifest accounts for: unzipped from the
    /// release by hand and dragged into place. Carries the bundle path, so
    /// the installer can be pointed at the same place.
    MacAppBundle(std::path::PathBuf),
    /// Built from a checkout: a commit-stamp version, or a binary sitting in
    /// a cargo `target/` directory.
    SourceBuild,
    /// Nothing recognisable; the person who put it here knows.
    Unknown,
}

impl InstallMethod {
    /// Classify the running executable.
    pub fn detect() -> Self {
        let exe = std::env::current_exe().ok();
        let version = crate::running_release_version();
        Self::classify(
            exe.as_deref(),
            InstallManifest::for_running_executable(),
            std::env::var_os("APPIMAGE").is_some(),
            &version,
        )
    }

    fn classify(
        exe: Option<&Path>,
        manifest: Option<InstallManifest>,
        appimage: bool,
        version: &str,
    ) -> Self {
        if let Some(manifest) = manifest {
            return Self::Script(manifest);
        }
        if appimage {
            return Self::AppImage;
        }
        let path = exe
            .map(|p| p.to_string_lossy().replace('\\', "/"))
            .unwrap_or_default();
        let has = |needle: &str| path.contains(needle);
        if has("/Cellar/") || has("/Caskroom/") || has("/homebrew/") || has("/linuxbrew/") {
            return Self::Homebrew;
        }
        if path.starts_with("/nix/store/") {
            return Self::Nix;
        }
        if cfg!(windows) && (has("/Program Files") || has("/AppData/Local/Programs/")) {
            return Self::WindowsInstaller;
        }
        if has("/ThinkTerm.app/Contents/MacOS/") {
            if let Some(app) = exe.and_then(|e| e.ancestors().find(|a| a.ends_with("ThinkTerm.app"))) {
                return Self::MacAppBundle(app.to_path_buf());
            }
        }
        if has("/target/") || !crate::is_release_version(version) {
            return Self::SourceBuild;
        }
        if path.starts_with("/usr/bin/")
            || path.starts_with("/usr/lib/")
            || path.starts_with("/usr/libexec/")
            || path.starts_with("/opt/")
        {
            return Self::SystemPackage;
        }
        Self::Unknown
    }

    /// True when `thinkterm update` may replace the files itself: a script
    /// install, or a bundle it can take over by running the script at it.
    pub fn self_updatable(&self) -> bool {
        matches!(self, Self::Script(_) | Self::MacAppBundle(_))
    }

    /// The manifest to run the installer with. A hand-installed bundle has
    /// none, so one is made up for it: desktop variant, the default prefix
    /// for the command-line links, and the bundle where it already is. The
    /// installer then writes the real manifest, and the next update finds
    /// it the ordinary way.
    pub fn manifest_for_install(&self) -> Option<InstallManifest> {
        match self {
            Self::Script(m) => Some(m.clone()),
            Self::MacAppBundle(app) => {
                let home = std::env::var_os("HOME").map(std::path::PathBuf::from)?;
                Some(InstallManifest {
                    variant: "desktop".into(),
                    version: crate::running_release_version(),
                    os: "macos".into(),
                    arch: std::env::consts::ARCH.into(),
                    prefix: home.join(".local").to_string_lossy().into_owned(),
                    app: Some(app.to_string_lossy().into_owned()),
                    installed_at: None,
                    path: home.join(".local/share/thinkterm/install-manifest"),
                })
            }
            _ => None,
        }
    }

    /// One or two sentences telling a person how this install is updated,
    /// for the methods that are not ours to touch.
    pub fn how_to_update(&self) -> String {
        match self {
            Self::Script(m) => format!(
                "installed by install.sh ({} variant, {}); `thinkterm update` can replace it",
                m.variant, m.version
            ),
            Self::AppImage => "this is an AppImage: update it with AppImageUpdate, or download the \
                              new AppImage from the releases page"
                .into(),
            Self::Homebrew => "installed with Homebrew: run `brew upgrade thinkterm`".into(),
            Self::Nix => "installed from the Nix store: update it through your Nix configuration".into(),
            Self::SystemPackage => "installed by the system package manager: update it with apt, dnf, \
                                   pacman or whatever installed it, using the new package from the \
                                   releases page"
                .into(),
            Self::WindowsInstaller => "installed by the Windows installer; `thinkterm update` downloads \
                                      the new installer and runs it"
                .into(),
            Self::MacAppBundle(app) => format!(
                "a ThinkTerm.app installed by hand at {}; `thinkterm update` can replace it in \
                 place and put the command-line links under ~/.local/bin",
                app.display()
            ),
            Self::SourceBuild => "this is a build from source: pull the repository and build again".into(),
            Self::Unknown => "this copy was not installed by install.sh, so `thinkterm update` will \
                             not touch it; replace it the way it was installed"
                .into(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn manifest() -> InstallManifest {
        InstallManifest {
            variant: "server".into(),
            version: "0.1.0".into(),
            os: "linux".into(),
            arch: "x86_64".into(),
            prefix: "/home/u/.local".into(),
            app: None,
            installed_at: None,
            path: PathBuf::from("/home/u/.local/share/thinkterm/install-manifest"),
        }
    }

    #[test]
    fn a_manifest_wins_over_everything_else() {
        let m = InstallMethod::classify(
            Some(Path::new("/opt/homebrew/Cellar/thinkterm/0.1.0/bin/thinkterm")),
            Some(manifest()),
            true,
            "0.1.0",
        );
        assert!(m.self_updatable());
    }

    #[test]
    fn classifies_the_common_layouts() {
        let c = |p: &str, v: &str| InstallMethod::classify(Some(Path::new(p)), None, false, v);
        assert_eq!(
            c("/opt/homebrew/Cellar/thinkterm/0.1.0/bin/thinkterm", "0.1.0"),
            InstallMethod::Homebrew
        );
        assert_eq!(
            c("/Applications/ThinkTerm.app/Contents/MacOS/thinkterm", "0.1.0"),
            InstallMethod::MacAppBundle(PathBuf::from("/Applications/ThinkTerm.app"))
        );
        assert_eq!(c("/usr/bin/thinkterm", "0.1.0"), InstallMethod::SystemPackage);
        assert_eq!(
            c("/nix/store/abc-thinkterm-0.1.0/bin/thinkterm", "0.1.0"),
            InstallMethod::Nix
        );
        assert_eq!(
            c("/home/u/src/thinkterm/target/release/thinkterm", "0.1.0"),
            InstallMethod::SourceBuild
        );
        assert_eq!(
            c("/usr/bin/thinkterm", "20260904-120000-abcdef12"),
            InstallMethod::SourceBuild
        );
        assert_eq!(
            InstallMethod::classify(Some(Path::new("/usr/bin/thinkterm")), None, true, "0.1.0"),
            InstallMethod::AppImage
        );
    }
}
