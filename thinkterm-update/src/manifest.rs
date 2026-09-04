//! The file `install.sh` leaves behind: `<prefix>/share/thinkterm/install-manifest`,
//! one `key=value` per line. It is the only evidence that this copy of
//! ThinkTerm was put here by the script -- and therefore that the script may
//! overwrite it. A deb, rpm, AppImage or Homebrew install writes no such
//! file and belongs to its own updater.

use anyhow::{anyhow, Context};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstallManifest {
    /// `desktop` or `server`.
    pub variant: String,
    pub version: String,
    pub os: String,
    pub arch: String,
    /// The `--prefix` the script installed under; binaries are in `bin/`.
    pub prefix: String,
    /// macOS only: where the app bundle went.
    pub app: Option<String>,
    pub installed_at: Option<String>,
    /// Where this manifest was read from.
    pub path: PathBuf,
}

impl InstallManifest {
    pub fn parse(text: &str, path: &Path) -> anyhow::Result<Self> {
        let mut variant = None;
        let mut version = None;
        let mut os = None;
        let mut arch = None;
        let mut prefix = None;
        let mut app = None;
        let mut installed_at = None;
        for line in text.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let Some((key, value)) = line.split_once('=') else {
                continue;
            };
            let value = value.trim().to_string();
            match key.trim() {
                "variant" => variant = Some(value),
                "version" => version = Some(value),
                "os" => os = Some(value),
                "arch" => arch = Some(value),
                "prefix" => prefix = Some(value),
                "app" => app = Some(value),
                "installed_at" => installed_at = Some(value),
                _ => {}
            }
        }
        let field = |name: &str, value: Option<String>| {
            value
                .filter(|v| !v.is_empty())
                .ok_or_else(|| anyhow!("{}: missing {name}=", path.display()))
        };
        let variant = field("variant", variant)?;
        if variant != "desktop" && variant != "server" {
            anyhow::bail!("{}: unknown variant '{variant}'", path.display());
        }
        Ok(Self {
            variant,
            version: field("version", version)?,
            os: os.unwrap_or_default(),
            arch: arch.unwrap_or_default(),
            prefix: field("prefix", prefix)?,
            app: app.filter(|a| !a.is_empty()),
            installed_at,
            path: path.to_path_buf(),
        })
    }

    pub fn load(path: &Path) -> anyhow::Result<Self> {
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("reading {}", path.display()))?;
        Self::parse(&text, path)
    }

    /// The manifest describing the running executable, if the script
    /// installed it.
    ///
    /// Two places are tried. `<exe>/../../share/thinkterm/install-manifest`
    /// covers a Linux install, where the binary sits in `<prefix>/bin`. On
    /// macOS the binary runs from inside the app bundle -- `~/.local/bin`
    /// holds only symlinks to it -- so the default prefix is tried as well,
    /// and accepted only when its manifest names the bundle we are running
    /// from. A manifest that describes some other copy of ThinkTerm must not
    /// license overwriting this one.
    pub fn for_running_executable() -> Option<Self> {
        let exe = std::env::current_exe().ok()?;
        Self::for_executable(&exe, dirs_home().as_deref())
    }

    fn for_executable(exe: &Path, home: Option<&Path>) -> Option<Self> {
        let exe = std::fs::canonicalize(exe).unwrap_or_else(|_| exe.to_path_buf());
        let mut candidates = Vec::new();
        if let Some(prefix) = exe.parent().and_then(Path::parent) {
            candidates.push(prefix.join("share/thinkterm/install-manifest"));
        }
        if let Some(home) = home {
            candidates.push(home.join(".local/share/thinkterm/install-manifest"));
        }
        for path in candidates {
            let Ok(manifest) = Self::load(&path) else {
                continue;
            };
            if manifest.describes(&exe) {
                return Some(manifest);
            }
        }
        None
    }

    /// Does this manifest cover the executable at `exe` (already canonical)?
    /// Either the binary lives in the manifest's `<prefix>/bin`, or on macOS
    /// inside the bundle the manifest recorded.
    fn describes(&self, exe: &Path) -> bool {
        let same = |a: &Path, b: &Path| {
            let a = std::fs::canonicalize(a).unwrap_or_else(|_| a.to_path_buf());
            let b = std::fs::canonicalize(b).unwrap_or_else(|_| b.to_path_buf());
            a == b
        };
        if let Some(dir) = exe.parent() {
            if same(dir, &Path::new(&self.prefix).join("bin")) {
                return true;
            }
        }
        if let Some(app) = &self.app {
            let app = std::fs::canonicalize(app).unwrap_or_else(|_| PathBuf::from(app));
            if exe.starts_with(&app) {
                return true;
            }
        }
        false
    }
}

fn dirs_home() -> Option<PathBuf> {
    std::env::var_os("HOME").map(PathBuf::from)
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = "variant=server\nversion=0.1.0\nos=linux\narch=x86_64\nprefix=/home/u/.local\ninstalled_at=2026-09-04T05:11:43Z\ninstaller=install.sh\n";

    #[test]
    fn parses_the_script_output() {
        let m = InstallManifest::parse(SAMPLE, Path::new("/x")).unwrap();
        assert_eq!(m.variant, "server");
        assert_eq!(m.version, "0.1.0");
        assert_eq!(m.prefix, "/home/u/.local");
        assert_eq!(m.app, None);
        assert_eq!(m.installed_at.as_deref(), Some("2026-09-04T05:11:43Z"));
    }

    #[test]
    fn rejects_an_unknown_variant() {
        let err = InstallManifest::parse("variant=gui\nversion=1\nprefix=/p\n", Path::new("/x"))
            .unwrap_err();
        assert!(err.to_string().contains("unknown variant"), "{err}");
    }

    #[test]
    fn finds_the_manifest_next_to_a_prefix_install() {
        let dir = tempfile::tempdir().unwrap();
        let prefix = dir.path().join("pfx");
        std::fs::create_dir_all(prefix.join("bin")).unwrap();
        std::fs::create_dir_all(prefix.join("share/thinkterm")).unwrap();
        let exe = prefix.join("bin/thinkterm");
        std::fs::write(&exe, b"").unwrap();
        std::fs::write(
            prefix.join("share/thinkterm/install-manifest"),
            format!("variant=desktop\nversion=0.1.0\nprefix={}\n", prefix.display()),
        )
        .unwrap();
        let m = InstallManifest::for_executable(&exe, None).unwrap();
        assert_eq!(m.variant, "desktop");
    }

    #[test]
    fn a_manifest_for_another_install_is_ignored() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join("home");
        std::fs::create_dir_all(home.join(".local/share/thinkterm")).unwrap();
        std::fs::create_dir_all(home.join(".local/bin")).unwrap();
        std::fs::write(
            home.join(".local/share/thinkterm/install-manifest"),
            format!(
                "variant=server\nversion=0.1.0\nprefix={}\n",
                home.join(".local").display()
            ),
        )
        .unwrap();
        // A system-wide binary elsewhere: the user's manifest does not cover it.
        let elsewhere = dir.path().join("usr/bin");
        std::fs::create_dir_all(&elsewhere).unwrap();
        let exe = elsewhere.join("thinkterm");
        std::fs::write(&exe, b"").unwrap();
        assert!(InstallManifest::for_executable(&exe, Some(&home)).is_none());
        // The same binary inside that prefix is covered.
        let exe = home.join(".local/bin/thinkterm");
        std::fs::write(&exe, b"").unwrap();
        assert!(InstallManifest::for_executable(&exe, Some(&home)).is_some());
    }

    #[test]
    fn a_macos_bundle_named_by_the_manifest_is_covered() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join("home");
        let app = dir.path().join("Applications/ThinkTerm.app");
        std::fs::create_dir_all(app.join("Contents/MacOS")).unwrap();
        std::fs::create_dir_all(home.join(".local/share/thinkterm")).unwrap();
        let exe = app.join("Contents/MacOS/thinkterm");
        std::fs::write(&exe, b"").unwrap();
        std::fs::write(
            home.join(".local/share/thinkterm/install-manifest"),
            format!(
                "variant=desktop\nversion=0.1.0\nprefix={}\napp={}\n",
                home.join(".local").display(),
                app.display()
            ),
        )
        .unwrap();
        let m = InstallManifest::for_executable(&exe, Some(&home)).unwrap();
        assert_eq!(m.app.as_deref(), Some(app.to_str().unwrap()));
    }
}
