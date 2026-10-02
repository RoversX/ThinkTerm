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
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

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
         cd thinkterm && ./get-deps && cargo build --release -p wezterm -p wezterm-mux-server -p thinkterm-plugin-server\n  \
         install -Dm755 target/release/thinkterm target/release/wezterm target/release/thinkterm-mux-server target/release/thinkterm-plugin-server -t ~/.local/bin\n\
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

/// How far an install has got, for whoever is showing it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InstallProgress {
    /// Fetching the release archive or the Windows installer: bytes so far,
    /// of the size GitHub recorded for the asset.
    Downloading { done: u64, total: u64 },
    /// The download is in and verified; the installer has taken over.
    Installing,
}

/// The longest a download goes without telling its watcher how far it is.
const PROGRESS_INTERVAL: Duration = Duration::from_millis(100);

/// "63.1 MB": binary megabytes, or gigabytes past 1024 of them, the way
/// Settings shows every size.
pub fn format_size(bytes: u64) -> String {
    let mib = bytes as f64 / 1024.0 / 1024.0;
    if mib >= 1024.0 {
        format!("{:.2} GB", mib / 1024.0)
    } else {
        format!("{mib:.1} MB")
    }
}

/// Where a GET's body goes. A redirect's own body is thrown away, so the
/// sink has to be able to start over.
trait BodySink: Write {
    fn restart(&mut self) -> std::io::Result<()>;
}

impl BodySink for Vec<u8> {
    fn restart(&mut self) -> std::io::Result<()> {
        self.clear();
        Ok(())
    }
}

/// A download on its way to disk, hashed and counted as it arrives, with
/// the count reported at most every `interval`.
struct DownloadSink<'a> {
    file: std::fs::File,
    hasher: sha2::Sha256,
    done: u64,
    total: u64,
    progress: &'a mut dyn FnMut(u64, u64),
    interval: Duration,
    reported: Option<Instant>,
}

impl Write for DownloadSink<'_> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        use sha2::Digest;
        self.file.write_all(buf)?;
        self.hasher.update(buf);
        self.done += buf.len() as u64;
        if self
            .reported
            .map_or(true, |at| at.elapsed() >= self.interval)
        {
            self.reported = Some(Instant::now());
            (self.progress)(self.done, self.total);
        }
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.file.flush()
    }
}

impl BodySink for DownloadSink<'_> {
    fn restart(&mut self) -> std::io::Result<()> {
        use sha2::Digest;
        use std::io::{Seek, SeekFrom};
        self.file.set_len(0)?;
        self.file.seek(SeekFrom::Start(0))?;
        self.hasher = sha2::Sha256::new();
        self.done = 0;
        self.reported = None;
        Ok(())
    }
}

/// How many times a download is tried before giving up, as curl's --retry
/// did when install.sh fetched the archive itself.
const DOWNLOAD_ATTEMPTS: u64 = 3;

/// Download one release asset to `dest`, reporting `(bytes so far, size)`
/// as it arrives, and verify it against GitHub's digest when there is one.
/// Returns whether it was verified: a missing digest is reported, not
/// treated as a failure. A failed or mismatched download leaves no file.
pub fn download_asset_to(
    asset: &Asset,
    dest: &Path,
    progress: &mut dyn FnMut(u64, u64),
) -> anyhow::Result<bool> {
    use sha2::Digest;
    // With a token the repository may be private, and then only the API
    // serves the bytes; install.sh downloads from there in that case too.
    let (url, accept) = match update_token() {
        Some(_) => (asset.url.as_str(), Some("application/octet-stream")),
        None => (asset.browser_download_url.as_str(), None),
    };
    let file =
        std::fs::File::create(dest).with_context(|| format!("creating {}", dest.display()))?;
    let mut sink = DownloadSink {
        file,
        hasher: sha2::Sha256::new(),
        done: 0,
        total: asset.size as u64,
        progress,
        interval: PROGRESS_INTERVAL,
        reported: None,
    };
    let mut attempt = 1;
    let fetched = loop {
        let mut result = http_get_into(url, accept, &mut sink);
        if result.is_ok() {
            result = sink.flush().map_err(anyhow::Error::from);
        }
        // http_req ends a body at the first read error instead of failing,
        // so a dropped connection arrives as a short file: count the bytes.
        if result.is_ok() && sink.total > 0 && sink.done != sink.total {
            result = Err(anyhow!(
                "the download stopped at {} of {} bytes",
                sink.done,
                sink.total
            ));
        }
        match result {
            Err(err)
                if attempt < DOWNLOAD_ATTEMPTS
                    && !err.chain().any(|cause| cause.is::<Refused>()) =>
            {
                log::warn!("downloading {} (attempt {attempt}): {err:#}", asset.name);
                std::thread::sleep(Duration::from_secs(attempt));
                attempt += 1;
                if let Err(err) = sink.restart() {
                    break Err(err.into());
                }
            }
            result => break result,
        }
    }
    .with_context(|| format!("downloading {}", asset.name));
    let DownloadSink {
        hasher,
        done,
        total,
        progress,
        ..
    } = sink;
    if let Err(err) = fetched {
        let _ = std::fs::remove_file(dest);
        return Err(err);
    }
    progress(done, total);
    match asset.sha256() {
        Some(expected) => {
            let got = hex::encode(hasher.finalize());
            if got != expected {
                let _ = std::fs::remove_file(dest);
                anyhow::bail!(
                    "checksum mismatch for {}: expected {expected}, got {got}; \
                     the download is corrupt or has been tampered with",
                    asset.name
                );
            }
            Ok(true)
        }
        None => Ok(false),
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
pub fn run_windows_installer(
    release: &Release,
    progress: &mut dyn FnMut(InstallProgress),
) -> anyhow::Result<()> {
    let version = release.tag_name.trim_start_matches('v');
    let wanted = format!("ThinkTerm-{version}-setup.exe");
    let asset = release
        .assets
        .iter()
        .find(|a| a.name.eq_ignore_ascii_case(&wanted))
        .ok_or_else(|| anyhow!("release {version} has no {wanted}; see {}", release.html_url))?;

    let dir = std::env::temp_dir().join("thinkterm-update");
    std::fs::create_dir_all(&dir)?;
    let path = dir.join(&asset.name);
    let verified = download_asset_to(asset, &path, &mut |done, total| {
        progress(InstallProgress::Downloading { done, total })
    })?;
    if !verified {
        println!("note: GitHub recorded no checksum for this asset; the download was not verified");
    }
    progress(InstallProgress::Installing);

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
/// and the installer script is a few dozen kilobytes; downloads of release
/// assets go through `download_asset_to` instead.
pub fn http_get(uri: &str) -> anyhow::Result<Vec<u8>> {
    let mut body = Vec::new();
    http_get_into(uri, None, &mut body)?;
    Ok(body)
}

/// A token lifts the anonymous API rate limit and is what makes a private
/// repository reachable at all. It has to be one meant for this: the GUI
/// runs its check on its own, and a GITHUB_TOKEN the user exported for
/// other tools must not be sent anywhere by it.
fn update_token() -> Option<String> {
    std::env::var("THINKTERM_UPDATE_TOKEN")
        .ok()
        .map(|token| token.trim().to_string())
        .filter(|token| !token.is_empty())
}

/// How many redirects a GET follows. GitHub sends every asset download
/// through one, to signed storage; http_req does not follow them itself.
const MAX_REDIRECTS: usize = 5;

fn http_get_into<S: BodySink>(
    uri: &str,
    accept: Option<&str>,
    sink: &mut S,
) -> anyhow::Result<()> {
    use http_req::request::{HttpVersion, Request};
    use http_req::uri::Uri;
    use std::convert::TryFrom;

    let auth = update_token().map(|token| format!("Bearer {token}"));

    let mut current = uri.to_string();
    for _ in 0..=MAX_REDIRECTS {
        // Name the host that answered when a redirect led to another one.
        let what = match host_of(&current).filter(|_| !same_host(uri, &current)) {
            Some(host) => format!("{uri} (redirected to {host})"),
            None => uri.to_string(),
        };
        let parsed = Uri::try_from(current.as_str())?;
        let mut request = Request::new(&parsed);
        request
            .version(HttpVersion::Http10)
            .header("User-Agent", &format!("thinkterm/{}", config::wezterm_version()));
        if let Some(accept) = accept {
            request.header("Accept", accept);
        }
        // Only to the host the token was meant for: the storage a download
        // is redirected to is signed already and must not see it.
        if let Some(auth) = auth.as_ref().filter(|_| same_host(uri, &current)) {
            request.header("Authorization", auth);
        }
        let res = request
            .send(sink)
            .map_err(|e| anyhow!("fetching {what}: {e}"))?;
        let status = res.status_code();
        if status.is_redirect() {
            let Some(location) = res.headers().get("Location") else {
                return Err(Refused(format!("fetching {what}: HTTP {status} without a Location")).into());
            };
            current = redirect_target(&current, location)
                .map_err(|err| Refused(format!("fetching {what}: {err:#}")))?;
            sink.restart()?;
            continue;
        }
        if !status.is_success() {
            let message = format!("fetching {what}: HTTP {status} {}", res.reason());
            // A timeout or a rate limit is worth another try; no other 4xx is.
            let lasting = status.is_client_err() && !matches!(u16::from(status), 408 | 429);
            return Err(if lasting {
                Refused(message).into()
            } else {
                anyhow!(message)
            });
        }
        return Ok(());
    }
    Err(Refused(format!("fetching {uri}: more than {MAX_REDIRECTS} redirects")).into())
}

/// An answer that asking again will not change: a 4xx other than a timeout
/// or a rate limit, or a redirect this does not follow. Anything else -- the
/// network, a 5xx, a short body -- may come out differently the next time.
#[derive(Debug)]
struct Refused(String);

impl std::fmt::Display for Refused {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for Refused {}

fn host_of(uri: &str) -> Option<String> {
    use http_req::uri::Uri;
    use std::convert::TryFrom;
    Uri::try_from(uri)
        .ok()
        .and_then(|uri| uri.host().map(str::to_ascii_lowercase))
}

fn same_host(a: &str, b: &str) -> bool {
    matches!((host_of(a), host_of(b)), (Some(a), Some(b)) if a == b)
}

/// Where a redirect from `current` to `location` leads. Only https is
/// followed: a download that finished over plain http could have been
/// swapped on the way.
fn redirect_target(current: &str, location: &str) -> anyhow::Result<String> {
    let location = location.trim();
    if location.starts_with("https://") {
        return Ok(location.to_string());
    }
    if location.starts_with('/') && !location.starts_with("//") && current.starts_with("https://") {
        let rest = &current["https://".len()..];
        let origin_len = "https://".len() + rest.find('/').unwrap_or(rest.len());
        return Ok(format!("{}{location}", &current[..origin_len]));
    }
    anyhow::bail!("refusing to follow a redirect to {location:?}")
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

/// The command that upgrades a running mux server on the host in place:
/// the freshly installed binary takes the running server's panes over and
/// the running server exits, so nothing in its sessions ends. Exits 0 once
/// the new server owns everything, and 1 with the reason on stderr when
/// the running server cannot hand over (a build from before the feature)
/// or the takeover failed and the running server carried on.
pub fn takeover_command() -> String {
    "export PATH=\"$HOME/.local/bin:$PATH\"; thinkterm-mux-server --daemonize --takeover"
        .to_string()
}

/// The release archive `install.sh` installs for `variant` on this machine,
/// named exactly as its `asset_name` names it; None where it has no build.
pub fn install_asset_name(variant: &str, version: &str) -> Option<String> {
    asset_name_for(std::env::consts::OS, std::env::consts::ARCH, variant, version)
}

fn asset_name_for(os: &str, arch: &str, variant: &str, version: &str) -> Option<String> {
    match (os, arch, variant) {
        ("linux", "x86_64" | "aarch64", "desktop") => {
            Some(format!("thinkterm-{version}-linux-{arch}.tar.gz"))
        }
        ("linux", "x86_64" | "aarch64", "server") => {
            Some(format!("thinkterm-server-{version}-linux-{arch}.tar.gz"))
        }
        // One bundle serves both variants, named the way uname -m names
        // the architecture there.
        ("macos", "aarch64", _) => Some(format!("ThinkTerm-macos-arm64-{version}.zip")),
        ("macos", "x86_64", _) => Some(format!("ThinkTerm-macos-x86_64-{version}.zip")),
        _ => None,
    }
}

/// Run `install.sh` on this machine for the install the manifest describes,
/// installing `release`. The script is fetched fresh and fed to `sh` on
/// stdin, exactly as the documented `curl | sh` does, so there is one
/// installer to keep correct rather than two. The release archive is
/// downloaded here first, so `progress` can follow it, and handed to the
/// script with `--from`.
///
/// Returns once the script has finished; its own output goes straight to the
/// terminal, which is where a person running `thinkterm update` is looking.
pub fn run_local_installer(
    manifest: &InstallManifest,
    release: &Release,
    progress: &mut dyn FnMut(InstallProgress),
) -> anyhow::Result<()> {
    run_installer(manifest, release, false, progress).map(|_| ())
}

/// The same, with the installer's output collected and returned instead of
/// written to the terminal: what a GUI that has no terminal shows in its
/// own window. On failure the output is in the error's message.
pub fn run_local_installer_captured(
    manifest: &InstallManifest,
    release: &Release,
    progress: &mut dyn FnMut(InstallProgress),
) -> anyhow::Result<String> {
    run_installer(manifest, release, true, progress)
}

fn run_installer(
    manifest: &InstallManifest,
    release: &Release,
    capture: bool,
    progress: &mut dyn FnMut(InstallProgress),
) -> anyhow::Result<String> {
    let script = http_get(INSTALL_SCRIPT_URL)
        .with_context(|| format!("downloading the installer from {INSTALL_SCRIPT_URL}"))?;

    // Only an archive GitHub recorded a checksum for is fetched here: it is
    // checked against that before the script sees it, and the script checks
    // nothing it is handed with --from. Without one, or without an archive
    // under the name the script would fetch, the script finds, downloads and
    // judges it itself, as it always has -- just without a progress report.
    let version = release.tag_name.trim_start_matches('v');
    let asset = install_asset_name(&manifest.variant, version)
        .and_then(|name| release.assets.iter().find(|asset| asset.name == name))
        .filter(|asset| asset.sha256().is_some());
    // The directory, and the archive in it, go when this returns.
    let staged = match asset {
        Some(asset) => {
            let dir = tempfile::Builder::new()
                .prefix("thinkterm-update-")
                .tempdir()
                .context("creating a directory to download into")?;
            let path = dir.path().join(&asset.name);
            download_asset_to(asset, &path, &mut |done, total| {
                progress(InstallProgress::Downloading { done, total })
            })?;
            Some((dir, path))
        }
        None => None,
    };
    progress(InstallProgress::Installing);
    run_install_script(
        manifest,
        &release.tag_name,
        staged.as_ref().map(|(_, path)| path.as_path()),
        &script,
        capture,
    )
}

fn run_install_script(
    manifest: &InstallManifest,
    tag: &str,
    from: Option<&Path>,
    script: &[u8],
    capture: bool,
) -> anyhow::Result<String> {
    use std::io::Read;
    use std::process::{Command, Stdio};

    let mut cmd = Command::new("sh");
    cmd.arg("-s")
        .arg("--")
        .arg(format!("--{}", manifest.variant));
    if let Some(archive) = from {
        cmd.arg("--from").arg(archive);
    }
    // With --from the script looks nothing up; the version still names the
    // install in the manifest it writes.
    cmd.arg("--version").arg(tag).arg("--prefix").arg(&manifest.prefix);
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
    fn redirects_are_followed_only_to_https() {
        assert_eq!(
            redirect_target(
                "https://github.com/o/r/releases/download/1.0/a.zip",
                "https://objects.example.com/a?sig=1"
            )
            .unwrap(),
            "https://objects.example.com/a?sig=1"
        );
        assert_eq!(
            redirect_target("https://api.github.com/repos/o/r/releases/latest", "/repositories/1/releases/latest")
                .unwrap(),
            "https://api.github.com/repositories/1/releases/latest"
        );
        assert!(redirect_target("https://github.com/a", "http://example.com/a").is_err());
        assert!(redirect_target("https://github.com/a", "//example.com/a").is_err());
        assert!(redirect_target("https://github.com/a", "a.zip").is_err());
    }

    #[test]
    fn the_token_stays_on_its_own_host() {
        assert!(same_host(
            "https://api.github.com/repos/o/r/releases/latest",
            "https://API.github.com/repositories/1/releases/latest"
        ));
        assert!(!same_host(
            "https://github.com/o/r/releases/download/1.0/a.zip",
            "https://objects.example.com/a?sig=1"
        ));
    }

    #[test]
    fn a_download_counts_hashes_and_starts_over() {
        use sha2::Digest;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("asset");
        let mut reports = Vec::new();
        let mut progress = |done: u64, total: u64| reports.push((done, total));
        let mut sink = DownloadSink {
            file: std::fs::File::create(&path).unwrap(),
            hasher: sha2::Sha256::new(),
            done: 0,
            total: 11,
            progress: &mut progress,
            // Long enough that no pause in a busy test run reaches it.
            interval: Duration::from_secs(3600),
            reported: None,
        };
        sink.write_all(b"redirected").unwrap();
        sink.restart().unwrap();
        sink.write_all(b"hello ").unwrap();
        sink.write_all(b"world").unwrap();
        sink.flush().unwrap();
        let DownloadSink { hasher, done, .. } = sink;
        assert_eq!(done, 11);
        assert_eq!(std::fs::read(&path).unwrap(), b"hello world");
        assert_eq!(
            hex::encode(hasher.finalize()),
            hex::encode(sha2::Sha256::digest(b"hello world"))
        );
        // The first write reports at once, and so does the first after
        // starting over; the rest wait out the interval.
        assert_eq!(reports, vec![(10, 11), (6, 11)]);
    }

    /// The archive downloaded here is handed to install.sh by name, so the
    /// names have to be the ones its own `asset_name` would fetch.
    #[cfg(unix)]
    #[test]
    fn asset_names_match_the_installer() {
        let script = std::fs::read_to_string(
            Path::new(env!("CARGO_MANIFEST_DIR")).join("../install.sh"),
        )
        .unwrap();
        let start = script.find("asset_name() {").expect("install.sh defines asset_name");
        let end = start + script[start..].find("\n}\n").expect("asset_name ends") + 3;
        let function = &script[start..end];
        // (Rust's os and arch, install.sh's os and arch, variant)
        let cases = [
            ("linux", "x86_64", "linux", "x86_64", "desktop"),
            ("linux", "aarch64", "linux", "aarch64", "server"),
            ("macos", "aarch64", "macos", "arm64", "desktop"),
            ("macos", "x86_64", "macos", "x86_64", "server"),
        ];
        for (os, arch, sh_os, sh_arch, variant) in cases {
            let out = std::process::Command::new("sh")
                .arg("-c")
                .arg(format!(
                    "{function}os={sh_os}; arch={sh_arch}; variant={variant}; asset_name 1.2.3"
                ))
                .output()
                .unwrap();
            assert!(out.status.success());
            assert_eq!(
                asset_name_for(os, arch, variant, "1.2.3").as_deref(),
                Some(String::from_utf8_lossy(&out.stdout).trim()),
                "{os}/{arch}/{variant}"
            );
        }
        assert_eq!(asset_name_for("windows", "x86_64", "desktop", "1.2.3"), None);
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
        assert!(
            text.contains("target/release/thinkterm-plugin-server"),
            "{text}"
        );
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
    fn the_takeover_command_runs_the_installed_server_in_place() {
        let cmd = takeover_command();
        assert!(cmd.starts_with("export PATH=\"$HOME/.local/bin:$PATH\";"), "{cmd}");
        assert!(cmd.ends_with("thinkterm-mux-server --daemonize --takeover"), "{cmd}");
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
