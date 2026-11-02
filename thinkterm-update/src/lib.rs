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
pub mod remote;

pub use manifest::InstallManifest;
pub use method::InstallMethod;

/// The repository every update link points at. The update check, the toast,
/// the Settings pages and the installer all build their URLs from this, so a
/// fork or a rename is one edit rather than a grep.
pub const REPO_URL: &str = "https://github.com/RoversX/thinkterm";

/// Raw address of the installer on the default branch. `thinkterm update`
/// fetches it from here rather than from the release it installs: the script
/// has to understand the *current* archive layout,
/// and a fix to it should reach every existing install at once. Remote
/// installs prefer a signed online script with a bundled fallback in `remote`.
pub const INSTALL_SCRIPT_URL: &str =
    "https://raw.githubusercontent.com/RoversX/thinkterm/main/install.sh";

/// Hosts that proxy github.com downloads, for where GitHub itself is slow
/// or unreachable, in the order `Auto` tries them. `install.sh` carries the
/// same list for `--mirror auto`.
pub const MIRRORS: [&str; 4] = ["ghfast.top", "gh-proxy.org", "ghproxy.net", "gh.llkk.cc"];

/// Where release archives and the Windows installer are downloaded from.
/// Signed SSH installer scripts and release inventories also use these
/// sources. The releases API stays on GitHub; mirrored archives require
/// a trusted checksum and mirrored scripts require a pinned-key signature.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum DownloadSource {
    /// GitHub, then each of `MIRRORS` in turn, then GitHub again: a link
    /// that is slow everywhere ends on GitHub, not on whichever mirror
    /// happens to be last.
    #[default]
    Auto,
    /// GitHub only.
    Direct,
    /// This mirror, then GitHub.
    Mirror(&'static str),
}

impl DownloadSource {
    /// Every choice, in the order Settings offers them. The first is what a
    /// process nobody told otherwise uses, as install.sh does.
    pub fn all() -> impl Iterator<Item = Self> {
        [Self::Auto, Self::Direct]
            .into_iter()
            .chain(MIRRORS.iter().map(|host| Self::Mirror(host)))
    }

    /// The name a settings file keeps: "direct", "auto" or a mirror's host.
    pub fn name(self) -> &'static str {
        match self {
            Self::Direct => "direct",
            Self::Auto => "auto",
            Self::Mirror(host) => host,
        }
    }

    /// The source a settings file names; anything this build does not know
    /// -- a mirror since dropped from `MIRRORS`, say -- is the default.
    pub fn from_name(name: &str) -> Self {
        Self::all()
            .find(|source| source.name() == name)
            .unwrap_or_default()
    }

    /// Where to fetch `github_url` from, in order.
    fn urls(self, github_url: &str) -> Vec<String> {
        let via = |host: &str| format!("https://{host}/{github_url}");
        match self {
            Self::Direct => vec![github_url.to_string()],
            Self::Auto => std::iter::once(github_url.to_string())
                .chain(MIRRORS.iter().map(|host| via(host)))
                .chain(std::iter::once(github_url.to_string()))
                .collect(),
            Self::Mirror(host) => vec![via(host), github_url.to_string()],
        }
    }

    fn index(self) -> usize {
        Self::all().position(|source| source == self).unwrap_or(0)
    }
}

/// `DOWNLOAD_SOURCE` before anything set it.
const SOURCE_UNSET: usize = usize::MAX;

static DOWNLOAD_SOURCE: std::sync::atomic::AtomicUsize =
    std::sync::atomic::AtomicUsize::new(SOURCE_UNSET);

/// The GUI sets this from its settings as they change.
pub fn set_download_source(source: DownloadSource) {
    DOWNLOAD_SOURCE.store(source.index(), std::sync::atomic::Ordering::SeqCst);
}

/// What the GUI set; in a process it did not set it in -- the CLI, the mux
/// server -- what its settings file says right now, so the choice made in
/// Settings holds there too.
pub fn download_source() -> DownloadSource {
    match DOWNLOAD_SOURCE.load(std::sync::atomic::Ordering::SeqCst) {
        SOURCE_UNSET => download_source_at(&config::native_settings_path()),
        index => DownloadSource::all().nth(index).unwrap_or_default(),
    }
}

/// The download source the GUI's settings file at `path` names, or the
/// default when it names none or cannot be read.
fn download_source_at(path: &Path) -> DownloadSource {
    #[derive(Default, Deserialize)]
    struct Settings {
        #[serde(default)]
        workspaces: Workspaces,
    }
    #[derive(Default, Deserialize)]
    struct Workspaces {
        #[serde(default)]
        update_download_source: String,
    }
    let settings = std::fs::read_to_string(path)
        .ok()
        .and_then(|text| serde_json::from_str::<Settings>(&text).ok())
        .unwrap_or_default();
    DownloadSource::from_name(&settings.workspaces.update_download_source)
}

const API_RELEASES: &str = "https://api.github.com/repos/RoversX/thinkterm/releases";
const API_COMMITS: &str = "https://api.github.com/repos/RoversX/thinkterm/commits";

/// The oldest glibc the Linux release binaries of `variant` ("server" or
/// "desktop", as `install.sh` takes it) load on: the newest glibc symbol
/// version any of them needs, which `ci/check-glibc.sh` holds every release
/// to. Only the GUI needs 2.35; the server reaches back to 2.31 by way of a
/// second build (see `MAIN_SERVER_GLIBC`). `install.sh` carries the same
/// numbers and refuses older hosts; this copy lets a client refuse before it
/// even offers.
pub fn min_glibc(variant: &str) -> (u32, u32) {
    match variant {
        "server" => (2, 31),
        _ => (2, 35),
    }
}

/// From this glibc on, a host gets the server built beside the desktop,
/// which links the host's OpenSSL 3. An older host has only OpenSSL 1.1 and
/// gets the build made on Ubuntu 20.04 with OpenSSL built in, its archive
/// named for the glibc it needs. `install.sh` calls this
/// `min_glibc_server_main`.
const MAIN_SERVER_GLIBC: (u32, u32) = (2, 34);

/// The glibc this machine runs on; None off Linux, or where it cannot say.
pub fn local_glibc() -> Option<(u32, u32)> {
    static GLIBC: std::sync::OnceLock<Option<(u32, u32)>> = std::sync::OnceLock::new();
    if !cfg!(target_os = "linux") {
        return None;
    }
    *GLIBC.get_or_init(|| {
        let out = std::process::Command::new("getconf")
            .arg("GNU_LIBC_VERSION")
            .stdin(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .output()
            .ok()?;
        if !out.status.success() {
            return None;
        }
        parse_glibc_version(&String::from_utf8_lossy(&out.stdout))
    })
}

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
         cd thinkterm && ./get-deps && cargo build --release -p wezterm -p wezterm-mux-server -p thinkterm-plugin-server -p thinkterm-tailnet-node\n  \
         install -Dm755 target/release/thinkterm target/release/wezterm target/release/thinkterm-mux-server target/release/thinkterm-plugin-server target/release/thinkterm-tailnet -t ~/.local/bin\n\
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
    /// When the commit the release was made from was committed, in seconds
    /// since the epoch. Not part of GitHub's answer: looked up, and kept in
    /// the cached copy, only when it is what decides -- see `is_newer_build`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thinkterm_commit_time: Option<i64>,
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

/// When a source that has another after it gives way to the next one: it
/// takes longer than `connect` to connect, sends nothing for `stall`, or --
/// where the transport can drop a body part-way -- sends under `min_rate`
/// bytes a second over a `window`. From mainland China GitHub often hangs
/// or crawls rather than fails. install.sh asks curl for the same.
#[derive(Debug, Clone, Copy)]
pub struct GiveWay {
    pub connect: Duration,
    pub stall: Duration,
    pub min_rate: u64,
    pub window: Duration,
}

const GIVE_WAY: GiveWay = GiveWay {
    connect: Duration::from_secs(10),
    stall: Duration::from_secs(20),
    min_rate: 300 * 1024,
    window: Duration::from_secs(20),
};

/// What one GET came back with. A 2xx body has gone to the writer by then.
#[derive(Debug)]
pub struct Answer {
    pub status: u16,
    pub reason: String,
    pub location: Option<String>,
}

/// One GET of `url` sending `headers`, redirects not followed, the body of a
/// 2xx written out as it arrives; `give_way` as above, for a source that has
/// another after it.
pub type Transport = fn(
    url: &str,
    headers: &[(&str, String)],
    give_way: Option<GiveWay>,
    body: &mut dyn Write,
) -> anyhow::Result<Answer>;

static TRANSPORT: std::sync::OnceLock<Transport> = std::sync::OnceLock::new();

/// Make every GET here -- the releases API, install.sh, release assets --
/// through `transport` instead of http_req. The GUI hands over one built on
/// reqwest, which it links anyway: that goes through the system's proxy,
/// and can drop a body part-way, so a source that crawls gives way too,
/// where http_req can only leave one that stalls (its reader thread panics
/// when the body is dropped early). The CLI and the mux server, which do
/// not link reqwest, stay on http_req.
pub fn set_transport(transport: Transport) {
    let _ = TRANSPORT.set(transport);
}

fn transport() -> Transport {
    TRANSPORT.get().copied().unwrap_or(http_req_transport)
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
    // A mirror only when there is a digest to check what it sends.
    let (urls, accept) = match update_token() {
        Some(_) => (vec![asset.url.clone()], Some("application/octet-stream")),
        None if asset.sha256().is_none() => (vec![asset.browser_download_url.clone()], None),
        None => (download_source().urls(&asset.browser_download_url), None),
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
    let transport = transport();
    let mut fetched = Err(anyhow!("no source to download from"));
    for (index, url) in urls.iter().enumerate() {
        // Every source but the last gets one try and gives way as `GiveWay`
        // says; the last is the one there is no point leaving.
        let last = index + 1 == urls.len();
        let attempts = if last { DOWNLOAD_ATTEMPTS } else { 1 };
        let give_way = (!last).then_some(GIVE_WAY);
        if index > 0 {
            if let Err(err) = sink.restart() {
                fetched = Err(err.into());
                break;
            }
            // Back to nothing at once, not when the next source first sends.
            (sink.progress)(0, sink.total);
        }
        let mut attempt = 1;
        fetched = loop {
            let mut result = http_get_into(url, accept, transport, give_way, &mut sink);
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
                    if attempt < attempts
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
        .with_context(|| format!("downloading {} from {url}", asset.name));
        if !last {
            match &fetched {
                Err(err) => log::warn!("{err:#}; trying the next source"),
                Ok(()) if !digest_matches(asset, &sink.hasher) => {
                    log::warn!("{} from {url} does not match its digest; discarded", asset.name);
                    fetched = Err(anyhow!("checksum mismatch from {url}"));
                }
                Ok(()) => break,
            }
            continue;
        }
    }
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

fn digest_matches(asset: &Asset, hasher: &sha2::Sha256) -> bool {
    use sha2::Digest;
    asset
        .sha256()
        .map_or(true, |expected| hex::encode(hasher.clone().finalize()) == expected)
}

/// The installer ci/deploy.sh builds for `arch`. x86_64 keeps the name it
/// had before there was an arm64 one, which every copy installed since
/// looks for. An arm64 copy must not take that one: it installs over the
/// native build in the same place, and the machine then runs it emulated.
fn windows_installer_name(arch: &str, version: &str) -> String {
    match arch {
        "aarch64" => format!("ThinkTerm-{version}-arm64-setup.exe"),
        _ => format!("ThinkTerm-{version}-setup.exe"),
    }
}

/// Windows: fetch the release's installer and run it to the end. It moves
/// the files of a running ThinkTerm aside rather than close it (see
/// ci/windows-installer.iss), so this process -- one of those files --
/// keeps running the old build until it is restarted, as on the other
/// platforms, and can wait for the outcome.
///
/// `/SILENT` keeps the progress window and drops every question; `/SP-`
/// skips the "this will install" prompt; `/NOCLOSEAPPLICATIONS` keeps the
/// installer from ever closing a running ThinkTerm -- a file it cannot move
/// aside fails the install instead; `/LOG` keeps its log beside the
/// download. Elevation is the installer's own UAC prompt.
pub fn run_windows_installer(
    release: &Release,
    progress: &mut dyn FnMut(InstallProgress),
) -> anyhow::Result<()> {
    let version = release.tag_name.trim_start_matches('v');
    let wanted = windows_installer_name(std::env::consts::ARCH, version);
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

    let log = dir.join(format!("{}.log", asset.name));
    println!("running the installer: {}", path.display());
    let status = std::process::Command::new(&path)
        .args(["/SILENT", "/SP-", "/NOCLOSEAPPLICATIONS", "/NORESTART"])
        .arg(format!("/LOG={}", log.display()))
        .status()
        .with_context(|| format!("starting {}", path.display()))?;
    installer_outcome(status.code(), &log)
}

/// What the installer's exit code says about the install.
fn installer_outcome(code: Option<i32>, log: &std::path::Path) -> anyhow::Result<()> {
    match code {
        Some(0) => Ok(()),
        // Before the install began (a declined UAC prompt among them), or
        // during it.
        Some(2) | Some(5) => Err(anyhow!("the installer was cancelled")),
        Some(code) => Err(anyhow!(
            "the installer failed (exit code {code}); its log is {}",
            log.display()
        )),
        None => Err(anyhow!(
            "the installer ended without an exit code; its log is {}",
            log.display()
        )),
    }
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
    http_get_into(uri, None, transport(), None, &mut body)?;
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
    transport: Transport,
    give_way: Option<GiveWay>,
    sink: &mut S,
) -> anyhow::Result<()> {
    let auth = update_token().map(|token| format!("Bearer {token}"));

    let mut current = uri.to_string();
    for _ in 0..=MAX_REDIRECTS {
        // Name the host that answered when a redirect led to another one.
        let what = match host_of(&current).filter(|_| !same_host(uri, &current)) {
            Some(host) => format!("{uri} (redirected to {host})"),
            None => uri.to_string(),
        };
        let mut headers = vec![(
            "User-Agent",
            format!("thinkterm/{}", config::wezterm_version()),
        )];
        if let Some(accept) = accept {
            headers.push(("Accept", accept.to_string()));
        }
        // Only to the host the token was meant for: the storage a download
        // is redirected to is signed already and must not see it.
        if let Some(auth) = auth.as_ref().filter(|_| same_host(uri, &current)) {
            headers.push(("Authorization", auth.clone()));
        }
        let answer = transport(&current, &headers, give_way, &mut *sink)
            .map_err(|e| anyhow!("fetching {what}: {e:#}"))?;
        let status = answer.status;
        if (300..400).contains(&status) {
            let Some(location) = answer.location else {
                return Err(Refused(format!("fetching {what}: HTTP {status} without a Location")).into());
            };
            current = redirect_target(&current, &location)
                .map_err(|err| Refused(format!("fetching {what}: {err:#}")))?;
            sink.restart()?;
            continue;
        }
        if !(200..300).contains(&status) {
            let message = format!("fetching {what}: HTTP {status} {}", answer.reason);
            // A timeout or a rate limit is worth another try; no other 4xx is.
            let lasting = (400..500).contains(&status) && !matches!(status, 408 | 429);
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

/// http_req's GET: HTTP/1.0, no proxy, and a minute to connect and for
/// each read unless `give_way` asks for less. It cannot drop a body
/// part-way, so `give_way`'s rate goes unchecked.
fn http_req_transport(
    url: &str,
    headers: &[(&str, String)],
    give_way: Option<GiveWay>,
    mut body: &mut dyn Write,
) -> anyhow::Result<Answer> {
    use http_req::request::{HttpVersion, Request};
    use http_req::uri::Uri;
    use std::convert::TryFrom;

    let parsed = Uri::try_from(url)?;
    let mut request = Request::new(&parsed);
    request.version(HttpVersion::Http10);
    for (name, value) in headers {
        request.header(*name, value);
    }
    if let Some(give_way) = give_way {
        request
            .connect_timeout(Some(give_way.connect))
            .read_timeout(Some(give_way.stall));
    }
    let res = request.send(&mut body)?;
    Ok(Answer {
        status: res.status_code().into(),
        reason: res.reason().to_string(),
        location: res.headers().get("Location").cloned(),
    })
}

/// The GET through the system's curl, for a process that does not link
/// reqwest: curl goes through the proxy the environment names
/// (`https_proxy`, `all_proxy`, socks5 too), and gives up on a source that
/// crawls. The URL and the headers -- a token among them -- reach it on its
/// stdin, not its command line, which other users can read. With no curl
/// to run, http_req does it.
pub fn curl_transport(
    url: &str,
    headers: &[(&str, String)],
    give_way: Option<GiveWay>,
    body: &mut dyn Write,
) -> anyhow::Result<Answer> {
    curl_get(&[], url, headers, give_way, body)
}

/// `curl_transport`, with `extra` on curl's command line.
fn curl_get(
    extra: &[&str],
    url: &str,
    headers: &[(&str, String)],
    give_way: Option<GiveWay>,
    body: &mut dyn Write,
) -> anyhow::Result<Answer> {
    curl_get_inner(extra, url, headers, give_way, body, true)
}

fn curl_get_inner(
    extra: &[&str],
    url: &str,
    headers: &[(&str, String)],
    give_way: Option<GiveWay>,
    body: &mut dyn Write,
    allow_fallback: bool,
) -> anyhow::Result<Answer> {
    use std::io::{BufRead, BufReader, Read};
    use std::process::{Command, Stdio};

    // A line break would end curl's config line early and start another.
    let lines = std::iter::once(url).chain(headers.iter().map(|(_, value)| value.as_str()));
    for line in lines {
        anyhow::ensure!(!line.chars().any(char::is_control), "refusing {line:?}");
    }
    let quote = |value: &str| value.replace('\\', "\\\\").replace('"', "\\\"");
    let mut config = format!("url = \"{}\"\n", quote(url));
    for (name, value) in headers {
        config.push_str(&format!("header = \"{name}: {}\"\n", quote(value)));
    }

    // Under the rate for the window gives way, nothing at all included; the
    // last source gets http_req's minute to connect and to go without data.
    let (connect, rate, window) = match give_way {
        Some(give_way) => (give_way.connect, give_way.min_rate, give_way.window),
        None => (Duration::from_secs(60), 1, Duration::from_secs(60)),
    };
    let secs = |d: Duration| d.as_secs().max(1).to_string();
    let mut child = match Command::new("curl")
        // First, or curl reads ~/.curlrc, where an --include or a
        // --write-out would land in the body.
        .arg("--disable")
        .args(["--silent", "--show-error", "--suppress-connect-headers"])
        // The answer's head, then its body, both to stdout.
        .args(["--dump-header", "-", "--config", "-"])
        .args(["--connect-timeout", &secs(connect)])
        .args(["--speed-limit", &rate.to_string(), "--speed-time", &secs(window)])
        .args(extra)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
    {
        Ok(child) => child,
        Err(err) if allow_fallback && err.kind() == std::io::ErrorKind::NotFound => {
            return http_req_transport(url, headers, give_way, body);
        }
        Err(err) => return Err(err).context("running curl"),
    };
    child
        .stdin
        .take()
        .context("no stdin on curl")?
        .write_all(config.as_bytes())?;
    // What curl says when it fails, read aside so it cannot fill its pipe.
    let stderr = child.stderr.take().map(|mut stderr| {
        std::thread::spawn(move || {
            let mut text = String::new();
            let _ = stderr.read_to_string(&mut text);
            text
        })
    });
    let mut out = BufReader::new(child.stdout.take().context("no stdout on curl")?);

    let read = (|| -> anyhow::Result<Option<Answer>> {
        // "HTTP/1.1 302 Found" or "HTTP/2 200", then headers, then a blank
        // line; nothing at all if curl failed before an answer came.
        let mut answer: Option<Answer> = None;
        loop {
            let mut line = Vec::new();
            if out.read_until(b'\n', &mut line)? == 0 {
                return Ok(None);
            }
            let line = String::from_utf8_lossy(&line);
            let line = line.trim_end();
            match &mut answer {
                None if line.is_empty() => continue,
                None => {
                    let mut parts = line.splitn(3, ' ');
                    let status = parts
                        .nth(1)
                        .and_then(|status| status.parse().ok())
                        .with_context(|| format!("curl answered {line:?}"))?;
                    answer = Some(Answer {
                        status,
                        reason: parts.next().unwrap_or_default().to_string(),
                        location: None,
                    });
                }
                Some(_) if line.is_empty() => break,
                Some(answer) => {
                    if let Some((name, value)) = line.split_once(':') {
                        if name.trim().eq_ignore_ascii_case("location") {
                            answer.location = Some(value.trim().to_string());
                        }
                    }
                }
            }
        }
        let answer = answer.expect("the loop leaves only with an answer");
        if (200..300).contains(&answer.status) {
            std::io::copy(&mut out, body)?;
        }
        Ok(Some(answer))
    })();

    // Done with it either way: an answer that was not a 2xx leaves its body
    // unread, and curl stops when its pipe closes.
    drop(out);
    let complete = matches!(&read, Ok(Some(answer)) if (200..300).contains(&answer.status));
    if !complete {
        let _ = child.kill();
    }
    let exit = child.wait().context("waiting for curl")?;
    let said = stderr
        .and_then(|thread| thread.join().ok())
        .unwrap_or_default();
    match read? {
        Some(answer) if !complete || exit.success() => Ok(answer),
        _ => Err(anyhow!("curl: {} ({exit})", said.trim())),
    }
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
    parse_release(version).is_some()
}

/// A release version, with or without the `v` older tags carried.
fn parse_release(version: &str) -> Option<semver::Version> {
    semver::Version::parse(version.trim_start_matches('v')).ok()
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
    match (parse_release(latest), parse_release(current)) {
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

/// Is `latest` -- a release made from a commit committed at
/// `latest_commit_time` -- something the running build, `current` committed
/// at `current_commit_time`, should be told about?
///
/// The versions decide, as `is_newer_release` has them. Only a tie goes on to
/// the commits: a package made by hand from a checkout can carry the version
/// of a release that does not exist yet, and when that release comes out its
/// code is the later one. Committer times, in seconds since the epoch, so
/// time zones and daylight saving never reorder them. A rebase or a squash
/// dates its commits to when it ran, and a package made from a branch that
/// missed some of the release's commits looks as new as its last commit:
/// both only ever mislead a package made by hand, never a release's.
pub fn is_newer_build(
    latest: &str,
    latest_commit_time: Option<i64>,
    current: &str,
    current_commit_time: Option<i64>,
) -> bool {
    if is_newer_release(latest, current) {
        return true;
    }
    same_release_version(latest, current)
        && matches!(
            (latest_commit_time, current_commit_time),
            (Some(latest), Some(current)) if latest > current
        )
}

/// Whether two versions name the same release.
pub fn same_release_version(a: &str, b: &str) -> bool {
    matches!((parse_release(a), parse_release(b)), (Some(a), Some(b)) if a == b)
}

/// The commit time of the running build when that can tell it apart from a
/// release of the same version: a package made by hand, whose binary carries
/// the commit stamp of a checkout. None for a release package, which is the
/// release whatever commit its tag points at -- so its users are never told
/// about the very release they run.
pub fn running_build_commit_time() -> Option<i64> {
    #[cfg(target_os = "macos")]
    if macos_bundle_is_release_package() {
        return None;
    }
    build_commit_time(config::wezterm_version(), config::wezterm_commit_time())
}

fn build_commit_time(stamp: &str, time: Option<i64>) -> Option<i64> {
    if is_release_version(stamp) {
        None
    } else {
        time
    }
}

/// The Info.plist key a package signed for a release carries (ci/deploy.sh
/// writes it). A laptop-built one has a commit stamp in its binaries like
/// any hand-made package, so the stamp alone cannot tell them apart.
#[cfg(target_os = "macos")]
const RELEASE_PACKAGE_KEY: &str = "ThinkTermReleasePackage";

#[cfg(target_os = "macos")]
fn macos_bundle_is_release_package() -> bool {
    std::env::current_exe()
        .ok()
        .and_then(|exe| Some(exe.parent()?.parent()?.join("Info.plist")))
        .and_then(|path| plist::Value::from_file(path).ok())
        .and_then(|value| value.as_dictionary()?.get(RELEASE_PACKAGE_KEY)?.as_boolean())
        .unwrap_or(false)
}

/// What a release is to the running build.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct UpdateVerdict {
    /// Something to tell the person about.
    pub newer: bool,
    /// And it carries the running build's own version: the same release,
    /// made from later code than this hand-made package of it.
    pub new_build: bool,
}

/// Judge `latest` against the running build, `current`: by version, and on
/// a tie by commit time (see `is_newer_build`). The tag's commit time, which
/// only a tie needs, comes from `cached` -- the last check's copy of the
/// same release -- before GitHub is asked, and is kept in `latest` for the
/// next check: a published tag does not move, and every request counts
/// against the anonymous API limit.
pub fn judge_release(latest: &mut Release, current: &str, cached: Option<&Release>) -> UpdateVerdict {
    judge_release_with(latest, current, cached, running_build_commit_time(), get_tag_commit_time)
}

fn judge_release_with(
    latest: &mut Release,
    current: &str,
    cached: Option<&Release>,
    build_time: Option<i64>,
    lookup: impl FnOnce(&str) -> anyhow::Result<i64>,
) -> UpdateVerdict {
    let tie = same_release_version(&latest.tag_name, current);
    if tie && build_time.is_some() && latest.thinkterm_commit_time.is_none() {
        latest.thinkterm_commit_time = cached
            .filter(|cached| cached.tag_name == latest.tag_name)
            .and_then(|cached| cached.thinkterm_commit_time)
            .or_else(|| match lookup(&latest.tag_name) {
                Ok(time) => Some(time),
                Err(err) => {
                    log::debug!("looking up the commit of {}: {err:#}", latest.tag_name);
                    None
                }
            });
    }
    let newer = is_newer_build(
        &latest.tag_name,
        latest.thinkterm_commit_time,
        current,
        build_time,
    );
    UpdateVerdict {
        newer,
        new_build: newer && tie,
    }
}

/// When the commit tag `tag` points at was committed, from the commits API:
/// the list form, which leaves out the changed files, started from the tag
/// itself so that a branch of the same name cannot answer instead.
pub fn get_tag_commit_time(tag: &str) -> anyhow::Result<i64> {
    let uri = format!("{API_COMMITS}?sha=refs/tags/{}&per_page=1", percent_encode(tag));
    let body = http_get(&uri)?;
    let commits: serde_json::Value = serde_json::from_slice(&body)
        .with_context(|| format!("parsing the GitHub commit list from {uri}"))?;
    let date = commits
        .get(0)
        .and_then(|commit| commit.pointer("/commit/committer/date"))
        .and_then(|date| date.as_str())
        .ok_or_else(|| anyhow!("no commit date for {tag} in {uri}"))?;
    parse_github_utc(date).ok_or_else(|| anyhow!("unexpected commit date {date:?} for {tag}"))
}

/// `text` safe inside a query string: everything but unreserved characters
/// percent-encoded.
fn percent_encode(text: &str) -> String {
    text.bytes()
        .map(|byte| match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                (byte as char).to_string()
            }
            _ => format!("%{byte:02X}"),
        })
        .collect()
}

/// Seconds since the epoch for a GitHub timestamp, which is always UTC:
/// `2026-10-15T07:12:34Z`.
fn parse_github_utc(date: &str) -> Option<i64> {
    let time = humantime::parse_rfc3339(date).ok()?;
    let seconds = time.duration_since(std::time::UNIX_EPOCH).ok()?.as_secs();
    i64::try_from(seconds).ok()
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
         sh \"$installer\" --{variant} --version {version}{mirror})",
        mirror = mirror_arg(download_source()),
    )
}

/// What `install.sh` is told about where to download from: nothing for
/// `Auto`, the script's own default -- which also keeps the default working
/// with a copy of the script from before `--mirror`. The names are this
/// file's own, so they need no quoting.
fn mirror_arg(source: DownloadSource) -> String {
    match source {
        DownloadSource::Auto => String::new(),
        source => format!(" --mirror {}", source.name()),
    }
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
    asset_name_for(
        std::env::consts::OS,
        std::env::consts::ARCH,
        variant,
        version,
        local_glibc(),
    )
}

fn asset_name_for(
    os: &str,
    arch: &str,
    variant: &str,
    version: &str,
    glibc: Option<(u32, u32)>,
) -> Option<String> {
    match (os, arch, variant) {
        ("linux", "x86_64" | "aarch64", "desktop") => {
            Some(format!("thinkterm-{version}-linux-{arch}.tar.gz"))
        }
        // A glibc it cannot read gets the main build, as in install.sh.
        ("linux", "x86_64" | "aarch64", "server") => match glibc {
            Some(glibc) if glibc < MAIN_SERVER_GLIBC => {
                let (major, minor) = min_glibc("server");
                Some(format!(
                    "thinkterm-server-{version}-linux-{arch}-glibc{major}.{minor}.tar.gz"
                ))
            }
            _ => Some(format!("thinkterm-server-{version}-linux-{arch}.tar.gz")),
        },
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
    fn the_version_decides_and_only_a_tie_goes_on_to_the_commits() {
        // A higher or lower version settles it, whatever the commits.
        assert!(is_newer_build("0.2.1", None, "0.2.0", None));
        assert!(!is_newer_build("0.1.9", Some(900), "0.2.0", Some(100)));
        // The same version, made from a later commit: a newer build.
        assert!(is_newer_build("0.2.0", Some(200), "0.2.0", Some(100)));
        assert!(is_newer_build("v0.2.0", Some(200), "0.2.0", Some(100)));
        // The same commit, or an earlier one, is nothing to offer.
        assert!(!is_newer_build("0.2.0", Some(100), "0.2.0", Some(100)));
        assert!(!is_newer_build("0.2.0", Some(50), "0.2.0", Some(100)));
        // Either time unknown: the versions alone, as before.
        assert!(!is_newer_build("0.2.0", None, "0.2.0", Some(100)));
        assert!(!is_newer_build("0.2.0", Some(200), "0.2.0", None));
        // A commit stamp is not the release's version.
        assert!(!is_newer_build("0.2.0", Some(200), "20261015-023205-79a8bed2", Some(100)));
    }

    #[test]
    fn only_a_build_stamped_with_a_commit_has_a_commit_time_to_compare() {
        assert_eq!(build_commit_time("20261015-023205-79a8bed2", Some(100)), Some(100));
        assert_eq!(build_commit_time("20261015-023205-79a8bed2-dirty", Some(100)), Some(100));
        // A release package is the release: never told about itself.
        assert_eq!(build_commit_time("0.2.0", Some(100)), None);
    }

    fn release(tag: &str, commit_time: Option<i64>) -> Release {
        let mut release: Release = serde_json::from_value(serde_json::json!({
            "url": "", "body": "", "html_url": "", "tag_name": tag, "assets": []
        }))
        .unwrap();
        release.thinkterm_commit_time = commit_time;
        release
    }

    #[test]
    fn a_tie_is_settled_by_the_tag_s_commit_time_asked_once() {
        let asked = std::cell::Cell::new(0);
        let lookup = |time: i64| {
            let asked = &asked;
            move |_: &str| {
                asked.set(asked.get() + 1);
                Ok(time)
            }
        };
        // A later commit behind the same version: a new build, asked once.
        let mut latest = release("0.2.0", None);
        let verdict = judge_release_with(&mut latest, "0.2.0", None, Some(100), lookup(200));
        assert_eq!(verdict, UpdateVerdict { newer: true, new_build: true });
        assert_eq!((latest.thinkterm_commit_time, asked.get()), (Some(200), 1));
        // The next check takes it from the cached copy of the same tag.
        let cached = latest.clone();
        let mut latest = release("0.2.0", None);
        judge_release_with(&mut latest, "0.2.0", Some(&cached), Some(100), lookup(999));
        assert_eq!((latest.thinkterm_commit_time, asked.get()), (Some(200), 1));
        // A different version, or a release package: nothing to ask.
        let mut latest = release("0.2.1", None);
        let verdict = judge_release_with(&mut latest, "0.2.0", None, Some(100), lookup(50));
        assert_eq!(verdict, UpdateVerdict { newer: true, new_build: false });
        let mut latest = release("0.2.0", None);
        let verdict = judge_release_with(&mut latest, "0.2.0", None, None, lookup(200));
        assert_eq!(verdict, UpdateVerdict::default());
        assert_eq!(asked.get(), 1);
    }

    #[test]
    fn a_tag_commit_github_cannot_date_leaves_the_versions_to_decide() {
        let mut latest = release("0.2.0", None);
        let verdict = judge_release_with(&mut latest, "0.2.0", None, Some(100), |_: &str| {
            Err(anyhow!("rate limited"))
        });
        assert_eq!(verdict, UpdateVerdict::default());
        assert_eq!(latest.thinkterm_commit_time, None);
    }

    #[test]
    fn a_tag_goes_into_the_query_string_encoded() {
        assert_eq!(percent_encode("0.2.0"), "0.2.0");
        assert_eq!(percent_encode("1.0.0+build.5"), "1.0.0%2Bbuild.5");
        assert_eq!(percent_encode("a b/c"), "a%20b%2Fc");
    }

    #[test]
    fn github_timestamps_are_read_as_utc() {
        assert_eq!(parse_github_utc("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(parse_github_utc("2000-02-29T12:00:00Z"), Some(951_825_600));
        // A commit made at 02:32:05 +11:00 on the 15th.
        assert_eq!(parse_github_utc("2026-10-14T15:32:05Z"), Some(1_791_991_925));
        assert_eq!(parse_github_utc("2026-10-14T15:32:05+11:00"), None);
        assert_eq!(parse_github_utc("2026-13-01T00:00:00Z"), None);
        assert_eq!(parse_github_utc("not a date"), None);
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

    thread_local! {
        /// The URLs and header names `fake_transport` was asked for.
        static ASKED: std::cell::RefCell<Vec<(String, Vec<String>)>> = Default::default();
    }

    /// github.com redirects to the storage, which answers; anything else is
    /// not there.
    fn fake_transport(
        url: &str,
        headers: &[(&str, String)],
        _give_way: Option<GiveWay>,
        body: &mut dyn Write,
    ) -> anyhow::Result<Answer> {
        ASKED.with(|asked| {
            asked.borrow_mut().push((
                url.to_string(),
                headers.iter().map(|(name, _)| name.to_string()).collect(),
            ))
        });
        let answer = |status, location: Option<&str>| Answer {
            status,
            reason: String::new(),
            location: location.map(str::to_string),
        };
        if url.starts_with("https://github.com/") {
            body.write_all(b"<a redirect's own page>")?;
            return Ok(answer(302, Some("https://objects.example.com/a?sig=1")));
        }
        if url == "https://objects.example.com/a?sig=1" {
            body.write_all(b"the asset")?;
            return Ok(answer(200, None));
        }
        Ok(answer(404, None))
    }

    #[test]
    fn the_download_source_is_read_from_the_settings_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");
        // No file, no field, a name this build does not know: the default.
        assert_eq!(download_source_at(&path), DownloadSource::Auto);
        let write = |text: &str| std::fs::write(&path, text).unwrap();
        write(r#"{"workspaces":{"remote_update_keeps_sessions":true}}"#);
        assert_eq!(download_source_at(&path), DownloadSource::Auto);
        write(r#"{"workspaces":{"update_download_source":"direct"},"other":1}"#);
        assert_eq!(download_source_at(&path), DownloadSource::Direct);
        write(r#"{"workspaces":{"update_download_source":"ghproxy.net"}}"#);
        assert_eq!(download_source_at(&path), DownloadSource::Mirror("ghproxy.net"));
        write(r#"{"workspaces":{"update_download_source":"gone.example"}}"#);
        assert_eq!(download_source_at(&path), DownloadSource::Auto);
    }

    #[test]
    fn a_transport_is_driven_through_redirects() {
        let mut body = Vec::new();
        http_get_into(
            "https://github.com/o/r/releases/download/1.0/a.zip",
            Some("application/octet-stream"),
            fake_transport,
            None,
            &mut body,
        )
        .unwrap();
        // The redirect's own body is dropped; both asks carry the agent and
        // the Accept header.
        assert_eq!(body, b"the asset");
        let asked = ASKED.with(|asked| asked.take());
        assert_eq!(asked.len(), 2);
        assert_eq!(asked[1].0, "https://objects.example.com/a?sig=1");
        for (_, headers) in &asked {
            assert!(headers.iter().any(|name| name == "User-Agent"), "{headers:?}");
            assert!(headers.iter().any(|name| name == "Accept"), "{headers:?}");
        }
        // A 404 is not asked again.
        let err = http_get_into("https://example.com/missing", None, fake_transport, None, &mut Vec::new())
            .unwrap_err();
        assert!(err.chain().any(|cause| cause.is::<Refused>()), "{err:#}");
    }

    /// A server on localhost answering one request with `head`, then `len`
    /// bytes, `chunk` of them every `pause`; the URL to ask it at, and what
    /// it was asked.
    fn serve(
        head: String,
        len: usize,
        chunk: usize,
        pause: Duration,
    ) -> (String, std::sync::mpsc::Receiver<String>) {
        use std::io::Read;
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let (asked, request) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut buf = [0u8; 4096];
            let read = stream.read(&mut buf).unwrap_or(0);
            let _ = asked.send(String::from_utf8_lossy(&buf[..read]).to_string());
            if stream.write_all(head.as_bytes()).is_err() {
                return;
            }
            let mut sent = 0;
            while sent < len {
                let n = chunk.min(len - sent);
                // The client hanging up is how a test that gives way ends.
                if stream.write_all(&vec![b'x'; n]).is_err() {
                    return;
                }
                sent += n;
                std::thread::sleep(pause);
            }
        });
        (format!("http://{addr}/asset"), request)
    }

    fn ok_head(len: usize) -> String {
        format!("HTTP/1.1 200 OK\r\nContent-Length: {len}\r\nConnection: close\r\n\r\n")
    }

    /// Straight to the test server, whatever proxy this machine names.
    fn curl(
        url: &str,
        headers: &[(&str, String)],
        give_way: Option<GiveWay>,
        body: &mut Vec<u8>,
    ) -> anyhow::Result<Answer> {
        curl_get(&["--noproxy", "*"], url, headers, give_way, body)
    }

    const QUICK: GiveWay = GiveWay {
        connect: Duration::from_secs(5),
        stall: Duration::from_secs(5),
        min_rate: 64 * 1024,
        window: Duration::from_secs(1),
    };

    #[test]
    fn curl_reads_a_whole_body_and_sends_the_headers() {
        let (url, request) = serve(ok_head(200_000), 200_000, 50_000, Duration::from_millis(1));
        let mut body = Vec::new();
        let headers = [("Accept", "application/octet-stream".to_string())];
        let answer = curl(&url, &headers, Some(QUICK), &mut body).unwrap();
        assert_eq!(answer.status, 200);
        assert_eq!(body.len(), 200_000);
        let request = request.recv().unwrap();
        assert!(request.contains("Accept: application/octet-stream"), "{request}");
    }

    #[test]
    fn curl_hands_a_redirect_back_unfollowed() {
        let head = "HTTP/1.1 302 Found\r\nLocation: https://objects.example.com/a?sig=1\r\n\
                    Content-Length: 5\r\nConnection: close\r\n\r\n";
        let (url, _) = serve(head.to_string(), 5, 5, Duration::ZERO);
        let mut body = Vec::new();
        let answer = curl(&url, &[], None, &mut body).unwrap();
        assert_eq!(answer.status, 302);
        assert_eq!(answer.location.as_deref(), Some("https://objects.example.com/a?sig=1"));
        assert!(body.is_empty());
    }

    #[test]
    fn curl_gives_way_when_a_body_crawls() {
        // 1 KB every 50 ms is about 20 KB/s, under the 64 asked for.
        let (url, _) = serve(ok_head(1_000_000), 1_000_000, 1024, Duration::from_millis(50));
        let err = curl(&url, &[], Some(QUICK), &mut Vec::new()).unwrap_err();
        assert!(format!("{err:#}").contains("curl"), "{err:#}");
    }

    #[test]
    fn curl_waits_out_a_slow_last_source() {
        let (url, _) = serve(ok_head(20_000), 20_000, 1024, Duration::from_millis(20));
        let mut body = Vec::new();
        curl(&url, &[], None, &mut body).unwrap();
        assert_eq!(body.len(), 20_000);
    }

    #[test]
    fn curl_refuses_a_line_break_in_what_it_would_send() {
        let headers = [("Authorization", "Bearer a\nurl = \"http://example.com\"".to_string())];
        assert!(curl("https://example.com/a", &headers, None, &mut Vec::new()).is_err());
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
        // (Rust's os and arch, install.sh's os and arch, variant, glibc)
        let cases = [
            ("linux", "x86_64", "linux", "x86_64", "desktop", Some((2, 39))),
            ("linux", "aarch64", "linux", "aarch64", "server", Some((2, 39))),
            ("linux", "x86_64", "linux", "x86_64", "server", Some((2, 31))),
            ("linux", "aarch64", "linux", "aarch64", "server", None),
            ("macos", "aarch64", "macos", "arm64", "desktop", None),
            ("macos", "x86_64", "macos", "x86_64", "server", None),
        ];
        let (major, minor) = min_glibc("server");
        for (os, arch, sh_os, sh_arch, variant, glibc) in cases {
            // What install.sh's preflight sets on a host below the main
            // server build's glibc.
            let old = match glibc {
                Some(glibc) if glibc < MAIN_SERVER_GLIBC => {
                    format!("old_glibc=1; min_glibc_server={major}.{minor}; ")
                }
                _ => String::new(),
            };
            let out = std::process::Command::new("sh")
                .arg("-c")
                .arg(format!(
                    "{function}{old}os={sh_os}; arch={sh_arch}; variant={variant}; asset_name 1.2.3"
                ))
                .output()
                .unwrap();
            assert!(out.status.success());
            assert_eq!(
                asset_name_for(os, arch, variant, "1.2.3", glibc).as_deref(),
                Some(String::from_utf8_lossy(&out.stdout).trim()),
                "{os}/{arch}/{variant}/{glibc:?}"
            );
        }
        assert_eq!(asset_name_for("windows", "x86_64", "desktop", "1.2.3", None), None);
    }

    #[test]
    fn an_old_glibc_gets_the_server_with_openssl_built_in() {
        let name = |glibc| asset_name_for("linux", "x86_64", "server", "1.2.3", glibc);
        // Ubuntu 20.04, Debian 11.
        assert_eq!(
            name(Some((2, 31))).as_deref(),
            Some("thinkterm-server-1.2.3-linux-x86_64-glibc2.31.tar.gz")
        );
        // RHEL 9 and on, and a host that would not say.
        for glibc in [Some((2, 34)), Some((2, 39)), None] {
            assert_eq!(
                name(glibc).as_deref(),
                Some("thinkterm-server-1.2.3-linux-x86_64.tar.gz"),
                "{glibc:?}"
            );
        }
    }

    #[test]
    fn windows_installer_follows_the_architecture() {
        assert_eq!(windows_installer_name("x86_64", "1.2.3"), "ThinkTerm-1.2.3-setup.exe");
        assert_eq!(windows_installer_name("aarch64", "1.2.3"), "ThinkTerm-1.2.3-arm64-setup.exe");
    }

    #[test]
    fn only_a_clean_installer_exit_is_an_install() {
        let log = std::path::Path::new("setup.log");
        assert!(installer_outcome(Some(0), log).is_ok());
        for cancelled in [2, 5] {
            let err = installer_outcome(Some(cancelled), log).unwrap_err().to_string();
            assert!(err.contains("cancelled"), "{cancelled}: {err}");
        }
        // Preparing failed: a file in use that could not be moved aside.
        let err = installer_outcome(Some(7), log).unwrap_err().to_string();
        assert!(err.contains("exit code 7") && err.contains("setup.log"), "{err}");
        assert!(installer_outcome(None, log).is_err());
    }

    #[test]
    fn glibc_versions_parse_from_getconf_output() {
        assert_eq!(parse_glibc_version("glibc 2.31\n"), Some((2, 31)));
        assert_eq!(parse_glibc_version("2.35"), Some((2, 35)));
        assert_eq!(parse_glibc_version(""), None);
        assert!(parse_glibc_version("glibc 2.28").unwrap() < min_glibc("server"));
        assert!(parse_glibc_version("glibc 2.39").unwrap() >= min_glibc("desktop"));
    }

    #[test]
    fn a_server_takes_a_glibc_the_desktop_does_not() {
        // RHEL 9 and its kin, Amazon Linux 2023; Ubuntu 20.04, Debian 11.
        for text in ["glibc 2.34", "glibc 2.31"] {
            let glibc = parse_glibc_version(text).unwrap();
            assert!(glibc >= min_glibc("server"), "{text}");
            assert!(glibc < min_glibc("desktop"), "{text}");
        }
    }

    #[test]
    fn the_glibc_floors_are_install_sh_s() {
        let script = include_str!("../../install.sh");
        let floor = |name: &str| {
            let line = script
                .lines()
                .find(|line| line.starts_with(&format!("{name}=\"")))
                .unwrap_or_else(|| panic!("install.sh sets no {name}"));
            parse_glibc_version(line.split('"').nth(1).unwrap()).unwrap()
        };
        assert_eq!(floor("min_glibc_server"), min_glibc("server"));
        assert_eq!(floor("min_glibc_desktop"), min_glibc("desktop"));
        assert_eq!(floor("min_glibc_server_main"), MAIN_SERVER_GLIBC);
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
        assert!(text.contains("target/release/thinkterm-tailnet "), "{text}");
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

    #[test]
    fn download_sources_order_github_and_mirrors() {
        let url = "https://github.com/o/r/releases/download/1.0/a.tar.gz";
        assert_eq!(DownloadSource::Direct.urls(url), [url]);
        let auto = DownloadSource::Auto.urls(url);
        assert_eq!(auto.len(), MIRRORS.len() + 2);
        assert_eq!(auto[0], url);
        assert_eq!(auto[1], format!("https://{}/{url}", MIRRORS[0]));
        assert_eq!(auto.last().map(String::as_str), Some(url));
        assert_eq!(
            DownloadSource::Mirror("ghproxy.net").urls(url),
            [format!("https://ghproxy.net/{url}"), url.to_string()]
        );
        // What a settings file keeps reads back as the same source; what
        // this build does not know is GitHub.
        for source in DownloadSource::all() {
            assert_eq!(DownloadSource::from_name(source.name()), source);
            assert_eq!(DownloadSource::all().nth(source.index()), Some(source));
        }
        assert_eq!(DownloadSource::from_name("gone.example"), DownloadSource::Auto);
        assert_eq!(mirror_arg(DownloadSource::Direct), " --mirror direct");
        assert_eq!(mirror_arg(DownloadSource::Auto), "");
        assert_eq!(
            mirror_arg(DownloadSource::Mirror("ghfast.top")),
            " --mirror ghfast.top"
        );
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
