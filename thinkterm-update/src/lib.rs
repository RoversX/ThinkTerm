//! What the CLI, the GUI and the mux client share about updating ThinkTerm:
//! which release is out, which version this build is, how this copy was
//! installed, and how to hand the job to `install.sh`.
//!
//! The installer itself is deliberately not reimplemented here. `install.sh`
//! is the one place that knows the archive layouts and the swap between the
//! desktop and server variants, so `thinkterm update` and the remote update
//! both run it; this crate only decides whether that is the right thing to
//! do and with which arguments.

use anyhow::{anyhow, Context};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

pub mod manifest;
pub mod method;

pub use manifest::InstallManifest;
pub use method::InstallMethod;

/// The repository every update link points at. The update check, the toast,
/// the Settings pages and the installer all build their URLs from this, so a
/// fork or a rename is one edit rather than a grep.
pub const REPO_URL: &str = "https://github.com/RoversX/thinkterm";

/// Raw address of the installer on the default branch. `thinkterm update`
/// and the remote update fetch it from here rather than from the release
/// they install: the script has to understand the *current* archive layout,
/// and a fix to it should reach every existing install at once.
pub const INSTALL_SCRIPT_URL: &str =
    "https://raw.githubusercontent.com/RoversX/thinkterm/main/install.sh";

const API_RELEASES: &str = "https://api.github.com/repos/RoversX/thinkterm/releases";

/// The oldest glibc the Linux release binaries run on: the Ubuntu 22.04 base
/// they are built in. `install.sh` carries the same number and refuses
/// older hosts; this copy lets a client refuse before it even offers.
pub const MIN_GLIBC: (u32, u32) = (2, 35);

/// Parse `getconf GNU_LIBC_VERSION` output ("glibc 2.31") or a bare
/// "2.31" into (major, minor).
pub fn parse_glibc_version(text: &str) -> Option<(u32, u32)> {
    let token = text.split_whitespace().last()?;
    let mut parts = token.split('.');
    let major = parts.next()?.parse().ok()?;
    let minor = parts.next()?.parse().ok()?;
    Some((major, minor))
}

/// What to tell someone whose host cannot take a release binary (too old a
/// glibc, musl, an unsupported architecture): the build they have to do
/// themselves, pinned to `version` so it matches the client that asked.
pub fn build_from_source_instructions(version: &str) -> String {
    let checkout = if is_release_version(version) {
        format!("git clone --branch {version} {REPO_URL}.git")
    } else {
        format!("git clone {REPO_URL}.git   # then check out the commit this client was built from")
    };
    format!(
        "Build ThinkTerm {version} from source on that host instead:\n  \
         {checkout}\n  \
         cd thinkterm && ./get-deps && cargo build --release -p wezterm -p wezterm-mux-server\n  \
         install -Dm755 target/release/thinkterm target/release/wezterm target/release/thinkterm-mux-server -t ~/.local/bin\n\
         then stop the old thinkterm-mux-server there and reconnect."
    )
}

pub fn releases_url() -> String {
    format!("{REPO_URL}/releases")
}

pub fn release_tag_url(tag: &str) -> String {
    format!("{REPO_URL}/releases/tag/{tag}")
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct Release {
    pub url: String,
    pub body: String,
    pub html_url: String,
    pub tag_name: String,
    pub assets: Vec<Asset>,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct Asset {
    pub name: String,
    pub size: usize,
    pub url: String,
    pub browser_download_url: String,
    /// `sha256:<hex>`, recorded by GitHub at upload. Absent on assets
    /// uploaded before GitHub started computing it, and on cached release
    /// JSON written by older builds.
    #[serde(default)]
    pub digest: Option<String>,
}

impl Asset {
    /// The hex sha256 GitHub recorded, if any.
    pub fn sha256(&self) -> Option<&str> {
        self.digest.as_deref()?.strip_prefix("sha256:")
    }
}

/// Download one release asset and verify it against GitHub's digest when
/// there is one. A missing digest is reported, not treated as a failure.
pub fn download_asset(asset: &Asset) -> anyhow::Result<(Vec<u8>, bool)> {
    use sha2::Digest;
    let body = http_get(&asset.browser_download_url)
        .with_context(|| format!("downloading {}", asset.name))?;
    match asset.sha256() {
        Some(expected) => {
            let got = hex::encode(sha2::Sha256::digest(&body));
            if got != expected {
                anyhow::bail!(
                    "checksum mismatch for {}: expected {expected}, got {got}; \
                     the download is corrupt or has been tampered with",
                    asset.name
                );
            }
            Ok((body, true))
        }
        None => Ok((body, false)),
    }
}

/// Windows: fetch the release's installer and hand over to it. Inno Setup
/// closes the running ThinkTerm, replaces the files and restarts it, so
/// this process must not wait for it -- it is one of the files.
///
/// `/SILENT` keeps the progress window and drops every question; `/SP-`
/// skips the "this will install" prompt; the two APPLICATIONS switches let
/// the installer stop and restart a running ThinkTerm rather than fail on a
/// locked file. Elevation is the installer's own UAC prompt.
pub fn run_windows_installer(release: &Release) -> anyhow::Result<()> {
    let version = release.tag_name.trim_start_matches('v');
    let wanted = format!("ThinkTerm-{version}-setup.exe");
    let asset = release
        .assets
        .iter()
        .find(|a| a.name.eq_ignore_ascii_case(&wanted))
        .ok_or_else(|| anyhow!("release {version} has no {wanted}; see {}", release.html_url))?;

    println!("downloading {} ({} bytes)...", asset.name, asset.size);
    let (body, verified) = download_asset(asset)?;
    if !verified {
        println!("note: GitHub recorded no checksum for this asset; the download was not verified");
    }

    let dir = std::env::temp_dir().join("thinkterm-update");
    std::fs::create_dir_all(&dir)?;
    let path = dir.join(&asset.name);
    std::fs::write(&path, &body).with_context(|| format!("writing {}", path.display()))?;

    println!("starting the installer: {}", path.display());
    std::process::Command::new(&path)
        .args(["/SILENT", "/SP-", "/CLOSEAPPLICATIONS", "/RESTARTAPPLICATIONS", "/NORESTART"])
        .spawn()
        .with_context(|| format!("starting {}", path.display()))?;
    Ok(())
}

fn get_github_release_info(uri: &str) -> anyhow::Result<Release> {
    let body = http_get(uri)?;
    let release: Release = serde_json::from_slice(&body)
        .with_context(|| format!("parsing the GitHub release JSON from {uri}"))?;
    Ok(release)
}

/// One HTTP GET, whole body in memory. The releases API answers are small
/// and the installer script is a few dozen kilobytes; nothing here streams.
pub fn http_get(uri: &str) -> anyhow::Result<Vec<u8>> {
    use http_req::request::{HttpVersion, Request};
    use http_req::uri::Uri;
    use std::convert::TryFrom;

    let parsed = Uri::try_from(uri)?;
    let mut body = Vec::new();
    let mut request = Request::new(&parsed);
    request
        .version(HttpVersion::Http10)
        .header("User-Agent", &format!("thinkterm/{}", config::wezterm_version()));
    // The same convention as install.sh: a token lifts the anonymous API
    // rate limit and is what makes a private repository reachable at all.
    let auth = std::env::var("GITHUB_TOKEN")
        .ok()
        .filter(|t| !t.trim().is_empty())
        .map(|t| format!("Bearer {}", t.trim()));
    if let Some(auth) = &auth {
        request.header("Authorization", auth);
    }
    let res = request
        .send(&mut body)
        .map_err(|e| anyhow!("fetching {uri}: {e}"))?;
    if !res.status_code().is_success() {
        anyhow::bail!(
            "fetching {uri}: HTTP {} {}",
            res.status_code(),
            res.reason()
        );
    }
    Ok(body)
}

pub fn get_latest_release_info() -> anyhow::Result<Release> {
    get_github_release_info(&format!("{API_RELEASES}/latest"))
}

pub fn get_release_by_tag(tag: &str) -> anyhow::Result<Release> {
    get_github_release_info(&format!("{API_RELEASES}/tags/{tag}"))
}

#[allow(unused)]
pub fn get_nightly_release_info() -> anyhow::Result<Release> {
    get_release_by_tag("nightly")
}

/// The release version of the running build, for comparison against the tag
/// of the latest GitHub release.
///
/// `wezterm_version()` is baked in at compile time from `.tag`, which only the
/// release workflow writes -- and macOS is the one platform whose packages are
/// built by hand rather than in CI, so its binaries carry a commit stamp that
/// `is_newer_release` deliberately refuses to compare against a `v*` tag. The
/// bundle's Info.plist does carry the release version, is read at runtime
/// rather than compile time, and is already what Finder and the About panel
/// show, so prefer it when we are running from inside an app bundle.
pub fn running_release_version() -> String {
    #[cfg(target_os = "macos")]
    {
        if let Some(version) = macos_bundle_version() {
            return version;
        }
    }
    config::wezterm_version().to_string()
}

/// `<bundle>.app/Contents/MacOS/<exe>` puts Info.plist one level up from the
/// executable's directory. Returns None for a bare `cargo build` binary, which
/// has no bundle and should keep reporting its commit stamp.
#[cfg(target_os = "macos")]
fn macos_bundle_version() -> Option<String> {
    let exe = std::env::current_exe().ok()?;
    version_from_info_plist(exe.parent()?.parent()?.join("Info.plist"))
}

#[cfg(target_os = "macos")]
fn version_from_info_plist(path: PathBuf) -> Option<String> {
    let value = plist::Value::from_file(path).ok()?;
    let version = value
        .as_dictionary()?
        .get("CFBundleShortVersionString")?
        .as_string()?
        .trim()
        .to_string();
    if version.is_empty() {
        None
    } else {
        Some(version)
    }
}

/// Whether a version string names a release, as opposed to the
/// `<date>-<hash>` commit stamp a plain checkout builds with. Only a release
/// has a tag on GitHub, so only a release can be fetched by name.
pub fn is_release_version(version: &str) -> bool {
    semver::Version::parse(version.trim_start_matches('v')).is_ok()
}

/// Is `latest` a release the running build should be told about?
///
/// The two strings only compare meaningfully when they use the same scheme.
/// A CI build carries the release tag (the workflow writes `.tag`, which
/// wezterm-version's build.rs prefers); anything built from a plain checkout
/// carries a `<date>-<hash>` commit stamp instead. Comparing across the two
/// with `>` is what made every `v*` release look permanently newer than every
/// local build: 'v' sorts above every digit, so the banner never went away.
pub fn is_newer_release(latest: &str, current: &str) -> bool {
    let parse = |s: &str| semver::Version::parse(s.trim_start_matches('v')).ok();
    match (parse(latest), parse(current)) {
        (Some(latest), Some(current)) => latest > current,
        // Both on the commit-stamp scheme, which upstream still uses for its
        // own tags. It starts with a zero-padded date, so lexicographic order
        // is chronological order.
        (None, None) => latest > current,
        // One of each: there is no ordering between the schemes, and a build
        // that never came from a release is not something to nag about.
        _ => false,
    }
}

/// The shell one-liner that installs release `version` of `variant` for the
/// invoking user, as `install.sh` documents it. Used verbatim over ssh for
/// the remote update and quoted in messages that tell a person what to run.
///
/// `~/.local/bin` goes on PATH first because a non-interactive ssh shell does
/// not source the files that would add it, and the script's PATH reminder is
/// aimed at a person reading a terminal.
/// Download to a private temporary file first: a failed or partial download
/// must never execute or look like a successful install to the restart UI.
pub fn install_command(variant: &str, version: &str) -> String {
    format!(
        "(export PATH=\"$HOME/.local/bin:$PATH\"; \
         installer=$(mktemp \"${{TMPDIR:-/tmp}}/thinkterm-update.XXXXXX\") || exit 1; \
         trap 'rm -f \"$installer\"' EXIT; \
         trap 'exit 130' INT; trap 'exit 143' TERM; \
         curl -fsSL -o \"$installer\" {INSTALL_SCRIPT_URL} && \
         sh \"$installer\" --{variant} --version {version})"
    )
}

/// Run `install.sh` on this machine for the install the manifest describes,
/// asking it for release `tag`. The script is fetched fresh and fed to `sh`
/// on stdin, exactly as the documented `curl | sh` does, so there is one
/// installer to keep correct rather than two.
///
/// Returns once the script has finished; its own output goes straight to the
/// terminal, which is where a person running `thinkterm update` is looking.
pub fn run_local_installer(manifest: &InstallManifest, tag: &str) -> anyhow::Result<()> {
    run_installer(manifest, tag, false).map(|_| ())
}

/// The same, with the installer's output collected and returned instead of
/// written to the terminal: what a GUI that has no terminal shows in its
/// own window. On failure the output is in the error's message.
pub fn run_local_installer_captured(manifest: &InstallManifest, tag: &str) -> anyhow::Result<String> {
    run_installer(manifest, tag, true)
}

fn run_installer(manifest: &InstallManifest, tag: &str, capture: bool) -> anyhow::Result<String> {
    use std::io::{Read, Write};
    use std::process::{Command, Stdio};

    let script = http_get(INSTALL_SCRIPT_URL)
        .with_context(|| format!("downloading the installer from {INSTALL_SCRIPT_URL}"))?;

    let mut cmd = Command::new("sh");
    cmd.arg("-s")
        .arg("--")
        .arg(format!("--{}", manifest.variant))
        .arg("--version")
        .arg(tag)
        .arg("--prefix")
        .arg(&manifest.prefix);
    if let Some(app) = &manifest.app {
        if let Some(dir) = Path::new(app).parent() {
            cmd.arg("--app-dir").arg(dir);
        }
    }
    let (out, err) = if capture {
        (Stdio::piped(), Stdio::piped())
    } else {
        (Stdio::inherit(), Stdio::inherit())
    };
    let mut child = cmd
        .stdin(Stdio::piped())
        .stdout(out)
        .stderr(err)
        .spawn()
        .context("running sh for the installer")?;
    {
        let mut stdin = child
            .stdin
            .take()
            .ok_or_else(|| anyhow!("no stdin on the installer process"))?;
        stdin.write_all(&script)?;
    }
    // stderr on its own thread, so a chatty installer cannot fill one pipe
    // while this end waits on the other.
    let stderr_thread = child.stderr.take().map(|mut stderr| {
        std::thread::spawn(move || {
            let mut buf = Vec::new();
            let _ = stderr.read_to_end(&mut buf);
            buf
        })
    });
    let mut output = Vec::new();
    if let Some(mut stdout) = child.stdout.take() {
        stdout.read_to_end(&mut output)?;
    }
    if let Some(thread) = stderr_thread {
        if let Ok(mut err) = thread.join() {
            output.append(&mut err);
        }
    }
    let status = child.wait()?;
    let output = String::from_utf8_lossy(&output).into_owned();
    if !status.success() {
        if capture && !output.trim().is_empty() {
            anyhow::bail!("the installer exited with {status}:\n{}", output.trim());
        }
        anyhow::bail!("the installer exited with {status}");
    }
    Ok(output)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn release_tags_order_by_semver() {
        assert!(is_newer_release("0.2.0", "0.1.9"));
        assert!(is_newer_release("v0.2.0", "0.1.9"));
        assert!(!is_newer_release("0.1.9", "0.2.0"));
        assert!(!is_newer_release("0.2.0", "0.2.0"));
    }

    #[test]
    fn commit_stamps_order_by_date_and_never_against_tags() {
        assert!(is_newer_release("20260904-120000-abcdef12", "20260903-120000-abcdef12"));
        assert!(!is_newer_release("0.2.0", "20260903-120000-abcdef12"));
        assert!(!is_newer_release("20260903-120000-abcdef12", "0.2.0"));
    }

    #[test]
    fn only_semver_is_a_release_version() {
        assert!(is_release_version("0.1.0"));
        assert!(is_release_version("v0.1.0"));
        assert!(!is_release_version("20260904-120000-abcdef12"));
    }

    /// The bundle template is the version a hand-built macOS package reports,
    /// so a release that forgets to bump it silently stops notifying users.
    #[cfg(target_os = "macos")]
    #[test]
    fn bundle_template_carries_a_comparable_version() {
        let plist = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../assets/macos/ThinkTerm.app/Contents/Info.plist");
        let version = version_from_info_plist(plist)
            .expect("the shipped Info.plist must declare CFBundleShortVersionString");
        assert!(
            is_release_version(&version),
            "Info.plist version {version:?} must parse as semver, or macOS \
             builds cannot be compared against a release tag"
        );
    }

    #[test]
    fn glibc_versions_parse_from_getconf_output() {
        assert_eq!(parse_glibc_version("glibc 2.31\n"), Some((2, 31)));
        assert_eq!(parse_glibc_version("2.35"), Some((2, 35)));
        assert_eq!(parse_glibc_version(""), None);
        assert!(parse_glibc_version("glibc 2.31").unwrap() < MIN_GLIBC);
        assert!(parse_glibc_version("glibc 2.39").unwrap() >= MIN_GLIBC);
    }

    #[test]
    fn source_instructions_pin_a_release_tag() {
        let text = build_from_source_instructions("0.2.0");
        assert!(text.contains("--branch 0.2.0"), "{text}");
        assert!(text.contains("thinkterm-mux-server"), "{text}");
    }

    #[test]
    fn asset_digest_is_read_with_and_without_the_field() {
        let with: Asset = serde_json::from_str(
            r#"{"name":"a","size":1,"url":"u","browser_download_url":"b","digest":"sha256:abcd"}"#,
        )
        .unwrap();
        assert_eq!(with.sha256(), Some("abcd"));
        let without: Asset =
            serde_json::from_str(r#"{"name":"a","size":1,"url":"u","browser_download_url":"b"}"#)
                .unwrap();
        assert_eq!(without.sha256(), None);
    }

    #[test]
    fn install_command_names_variant_and_version() {
        let cmd = install_command("server", "0.2.0");
        assert!(cmd.contains("--server --version 0.2.0"), "{cmd}");
        assert!(cmd.contains(INSTALL_SCRIPT_URL), "{cmd}");
    }

    #[cfg(unix)]
    #[test]
    fn remote_installer_requires_a_complete_download_and_preserves_exit_status() {
        let dir = tempfile::tempdir().unwrap();
        let marker = dir.path().join("executed");
        for (download, expected_status, executed) in [
            ("return 22;", 22, false),
            (
                r#"printf '%s\n' 'printf executed > "$THINKTERM_UPDATE_TEST_MARKER"; exit 7' > "$3"; return 22;"#,
                22,
                false,
            ),
            (
                r#"printf '%s\n' 'printf executed > "$THINKTERM_UPDATE_TEST_MARKER"; exit 7' > "$3"; return 0;"#,
                7,
                true,
            ),
        ] {
            let status = std::process::Command::new("sh")
                .arg("-c")
                .arg(format!(
                    "curl() {{ {download} }}\n{}",
                    install_command("server", "0.2.0")
                ))
                .env("THINKTERM_UPDATE_TEST_MARKER", &marker)
                .status()
                .unwrap();
            assert_eq!(status.code(), Some(expected_status));
            assert_eq!(marker.exists(), executed);
        }
    }
}
