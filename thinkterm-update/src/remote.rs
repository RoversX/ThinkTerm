//! Release metadata and signed installer selection for SSH installs only.

use crate::{Asset, DownloadSource, GiveWay, Release};
use anyhow::{bail, ensure, Context};
use std::collections::HashSet;
use std::io::Write;
use std::time::{Duration, Instant};

pub const INSTALL_SCRIPT: &str = include_str!("../../install.sh");
pub const SIGNING_KEY: &str = include_str!("../../assets/release-signing.pub");
pub const MANIFEST_NAME: &str = "thinkterm-release.txt";
const NAMESPACE: &str = "thinkterm-release";
const INSTALLER_NAMESPACE: &str = "thinkterm-installer-v1";
const INSTALLER_HEADER: &str = "#!/bin/sh\n# thinkterm-installer-v1\n";
const INSTALLER_BUDGET: Duration = Duration::from_secs(4);
const METADATA_LIMIT: usize = 1024 * 1024;
const ARCHIVE_LIMIT: u64 = 8 * 1024 * 1024 * 1024;
const LOOKUP_TIMEOUT: GiveWay = GiveWay {
    connect: Duration::from_secs(4),
    stall: Duration::from_secs(8),
    min_rate: 128,
    window: Duration::from_secs(8),
};

fn safe_name(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 200
        && value.as_bytes()[0].is_ascii_alphanumeric()
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"._+-".contains(&b))
}

fn valid_digest(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// The same format is used for signed release metadata and for metadata
/// obtained from GitHub and delivered over the authenticated SSH connection.
/// `-` means GitHub supplied no digest; only the latter path accepts it.
pub fn release_info(release: &Release) -> anyhow::Result<String> {
    ensure!(safe_name(&release.tag_name), "invalid release tag");
    let mut text = format!("thinkterm-release-v1\nversion {}\n", release.tag_name);
    let mut names = HashSet::new();
    for asset in &release.assets {
        ensure!(safe_name(&asset.name), "invalid release asset name");
        ensure!(
            asset.size > 0 && asset.size as u64 <= ARCHIVE_LIMIT,
            "invalid release asset size"
        );
        ensure!(names.insert(&asset.name), "duplicate release asset name");
        let digest = asset.sha256().filter(|s| valid_digest(s)).unwrap_or("-");
        text.push_str(&format!("{digest} {} {}\n", asset.size, asset.name));
    }
    ensure!(
        text.len() <= METADATA_LIMIT,
        "release metadata is too large"
    );
    Ok(text)
}

fn parse_manifest(bytes: &[u8], tag: &str) -> anyhow::Result<Release> {
    ensure!(safe_name(tag), "invalid release tag");
    ensure!(
        bytes.len() <= METADATA_LIMIT,
        "release metadata is too large"
    );
    let text = std::str::from_utf8(bytes).context("release metadata is not UTF-8")?;
    ensure!(
        !text.contains('\r'),
        "invalid release metadata line endings"
    );
    let mut lines = text.lines();
    ensure!(
        lines.next() == Some("thinkterm-release-v1"),
        "unknown release metadata format"
    );
    ensure!(
        lines.next() == Some(format!("version {tag}").as_str()),
        "release metadata version mismatch"
    );
    let mut assets = Vec::new();
    let mut names = HashSet::new();
    for line in lines {
        let fields: Vec<_> = line.split(' ').collect();
        ensure!(fields.len() == 3, "invalid release metadata row");
        ensure!(valid_digest(fields[0]), "invalid release checksum");
        ensure!(
            !fields[1].is_empty() && fields[1].bytes().all(|b| b.is_ascii_digit()),
            "invalid release asset size"
        );
        let size: usize = fields[1].parse().context("invalid release asset size")?;
        ensure!(
            size > 0 && size as u64 <= ARCHIVE_LIMIT,
            "invalid release asset size"
        );
        let name = fields[2];
        ensure!(
            safe_name(name) && names.insert(name),
            "invalid or duplicate release asset name"
        );
        let url = format!("{}/releases/download/{tag}/{name}", crate::REPO_URL);
        assets.push(Asset {
            name: name.into(),
            size,
            url: url.clone(),
            browser_download_url: url,
            digest: Some(format!("sha256:{}", fields[0])),
        });
    }
    ensure!(!assets.is_empty(), "empty release metadata");
    Ok(Release {
        url: crate::release_tag_url(tag),
        html_url: crate::release_tag_url(tag),
        tag_name: tag.into(),
        body: String::new(),
        assets,
        thinkterm_commit_time: None,
    })
}

fn verified_manifest(
    bytes: &[u8],
    signature: &[u8],
    tag: &str,
    key: &str,
) -> anyhow::Result<Release> {
    let key =
        ssh_key::PublicKey::from_openssh(key).context("invalid release signing public key")?;
    let signature = ssh_key::SshSig::from_pem(signature).context("invalid release signature")?;
    key.verify(NAMESPACE, bytes, &signature)
        .context("release signature verification failed")?;
    parse_manifest(bytes, tag)
}

struct LimitedBody {
    bytes: Vec<u8>,
    limit: usize,
}

impl Write for LimitedBody {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        if bytes.len() > self.limit.saturating_sub(self.bytes.len()) {
            return Err(std::io::Error::other("release metadata exceeds size limit"));
        }
        self.bytes.extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

// Authentication is explicitly allowed only for the API lookup. In particular,
// a mirror is never the initial host to which an update token may be sent.
fn fetch(
    url: &str,
    limit: usize,
    auth: Option<&str>,
    transport: crate::Transport,
) -> anyhow::Result<Vec<u8>> {
    let mut current = url.to_string();
    for _ in 0..=crate::MAX_REDIRECTS {
        let mut body = LimitedBody {
            bytes: Vec::new(),
            limit,
        };
        let mut headers = vec![("User-Agent", "thinkterm-release".to_string())];
        if let Some(token) = auth.filter(|_| crate::same_host(url, &current)) {
            headers.push(("Authorization", format!("Bearer {token}")));
        }
        let answer = transport(&current, &headers, Some(LOOKUP_TIMEOUT), &mut body)?;
        if (300..400).contains(&answer.status) {
            current = crate::redirect_target(
                &current,
                &answer.location.context("redirect without Location")?,
            )?;
        } else {
            ensure!(
                (200..300).contains(&answer.status),
                "HTTP {} {}",
                answer.status,
                answer.reason
            );
            return Ok(body.bytes);
        }
    }
    bail!("too many release metadata redirects")
}

pub fn get_release(tag: &str) -> anyhow::Result<Release> {
    lookup(
        tag,
        crate::download_source(),
        crate::update_token().as_deref(),
        crate::transport(),
        SIGNING_KEY,
    )
}

fn lookup(
    tag: &str,
    source: DownloadSource,
    token: Option<&str>,
    transport: crate::Transport,
    key: &str,
) -> anyhow::Result<Release> {
    ensure!(safe_name(tag), "invalid release tag");
    let api_result = (|| -> anyhow::Result<Release> {
        let bytes = fetch(
            &format!("{}/tags/{tag}", crate::API_RELEASES),
            METADATA_LIMIT,
            token,
            transport,
        )?;
        let release: Release =
            serde_json::from_slice(&bytes).context("invalid GitHub release response")?;
        ensure!(release.tag_name == tag, "GitHub release version mismatch");
        release_info(&release)?;
        Ok(release)
    })();
    let api_error = match api_result {
        Ok(release) => return Ok(release),
        Err(err) => err,
    };
    let url = format!(
        "{}/releases/download/{tag}/{MANIFEST_NAME}",
        crate::REPO_URL
    );
    let source = if token.is_some() {
        DownloadSource::Direct
    } else {
        source
    };
    let mut visited = HashSet::new();
    let mut failures = Vec::new();
    for url in source.urls(&url) {
        if !visited.insert(url.clone()) {
            continue;
        }
        let result = (|| {
            let bytes = fetch(&url, METADATA_LIMIT, None, transport)?;
            let signature = fetch(&format!("{url}.sig"), 16 * 1024, None, transport)?;
            verified_manifest(&bytes, &signature, tag, key)
        })();
        match result {
            Ok(release) => return Ok(release),
            Err(err) => failures.push(format!(
                "{}: {err:#}",
                crate::host_of(&url).unwrap_or_default()
            )),
        }
    }
    bail!("could not obtain trusted metadata for {tag}; GitHub API: {api_error:#}; signed release metadata: {}", failures.join("; "))
}

pub struct InstallerScript {
    pub text: String,
    pub online: bool,
}

pub fn installer_script() -> InstallerScript {
    let source = if crate::update_token().is_some() {
        DownloadSource::Direct
    } else {
        crate::download_source()
    };
    select_installer(source, SIGNING_KEY, INSTALLER_BUDGET, installer_fetch)
}

fn installer_fetch(url: &str, limit: usize, deadline: Instant) -> anyhow::Result<Vec<u8>> {
    let mut current = url.to_owned();
    for _ in 0..=crate::MAX_REDIRECTS {
        let remaining = deadline.saturating_duration_since(Instant::now());
        ensure!(
            remaining >= Duration::from_millis(10),
            "installer lookup timed out"
        );
        let timeout = format!(
            "{:.3}",
            remaining.min(Duration::from_millis(1500)).as_secs_f64()
        );
        let mut body = LimitedBody {
            bytes: Vec::new(),
            limit,
        };
        // No unbounded http_req fallback when curl is absent. Each request,
        // including redirects and signatures, spends the same total budget.
        let answer = crate::curl_get_inner(
            &["--max-time", &timeout, "--proto", "=https", "--retry", "0"],
            &current,
            &[("User-Agent", "thinkterm-installer".into())],
            Some(LOOKUP_TIMEOUT),
            &mut body,
            false,
        )?;
        if (300..400).contains(&answer.status) {
            current = crate::redirect_target(
                &current,
                &answer.location.context("redirect without Location")?,
            )?;
        } else {
            ensure!(
                (200..300).contains(&answer.status),
                "installer HTTP {}",
                answer.status
            );
            return Ok(body.bytes);
        }
    }
    bail!("too many installer redirects")
}

fn select_installer(
    source: DownloadSource,
    key: &str,
    budget: Duration,
    mut fetch: impl FnMut(&str, usize, Instant) -> anyhow::Result<Vec<u8>>,
) -> InstallerScript {
    let deadline = Instant::now() + budget;
    let url = format!(
        "{}/releases/download/installer-v1/install.sh",
        crate::REPO_URL
    );
    let mut visited = HashSet::new();
    for url in source.urls(&url) {
        if Instant::now() >= deadline {
            break;
        }
        if !visited.insert(url.clone()) {
            continue;
        }
        let verified = (|| -> anyhow::Result<String> {
            let bytes = fetch(&url, METADATA_LIMIT, deadline)?;
            ensure!(
                bytes.len() <= METADATA_LIMIT,
                "installer exceeds size limit"
            );
            ensure!(Instant::now() < deadline, "installer lookup timed out");
            let signature = fetch(&format!("{url}.sig"), 16 * 1024, deadline)?;
            ensure!(
                signature.len() <= 16 * 1024,
                "installer signature exceeds size limit"
            );
            ensure!(Instant::now() < deadline, "installer lookup timed out");
            let key = ssh_key::PublicKey::from_openssh(key)?;
            key.verify(
                INSTALLER_NAMESPACE,
                &bytes,
                &ssh_key::SshSig::from_pem(&signature)?,
            )?;
            let script = String::from_utf8(bytes)?;
            ensure!(
                script.starts_with(INSTALLER_HEADER),
                "unsupported installer protocol"
            );
            Ok(script)
        })();
        if let Ok(text) = verified {
            return InstallerScript { text, online: true };
        }
    }
    InstallerScript {
        text: INSTALL_SCRIPT.into(),
        online: false,
    }
}

/// Receive exact lengths rather than waiting for EOF: closing exec stdin in
/// wezterm-ssh also closes the channel. The caller keeps stdin until exit.
pub fn install_payload(
    variant: &str,
    release: &Release,
    script: &str,
) -> anyhow::Result<(String, Vec<u8>)> {
    payload_for_script(variant, release, script)
}

fn payload_for_script(
    variant: &str,
    release: &Release,
    script: &str,
) -> anyhow::Result<(String, Vec<u8>)> {
    ensure!(
        matches!(variant, "server" | "desktop"),
        "invalid install variant"
    );
    let metadata = release_info(release)?;
    let version = &release.tag_name;
    let command = format!(
        "(export PATH=\"$HOME/.local/bin:$PATH\"; \
         work=$(mktemp -d \"${{TMPDIR:-/tmp}}/thinkterm-update.XXXXXX\") || exit 1; \
         trap 'rm -rf \"$work\"' EXIT; trap 'exit 130' INT; trap 'exit 143' TERM; \
         head -c {total_len} > \"$work/payload\" && \
         test \"$(wc -c < \"$work/payload\")\" -eq {total_len} && \
         head -c {script_len} \"$work/payload\" > \"$work/install.sh\" && \
         tail -c {info_len} \"$work/payload\" > \"$work/release-info\" && \
         THINKTERM_INSTALL_PROGRESS=1 sh \"$work/install.sh\" --{variant} --version {version} \
         --release-info \"$work/release-info\"{mirror} </dev/null)",
        script_len = script.len(),
        info_len = metadata.len(),
        total_len = script.len() + metadata.len(),
        mirror = crate::mirror_arg(if crate::update_token().is_some() {
            DownloadSource::Direct
        } else {
            crate::download_source()
        }),
    );
    let mut payload = script.as_bytes().to_vec();
    payload.extend_from_slice(metadata.as_bytes());
    Ok((command, payload))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::process::{Command, Stdio};

    fn metadata() -> Vec<u8> {
        format!("thinkterm-release-v1\nversion 9.9.9\n{} 42 thinkterm-server-9.9.9-linux-x86_64.tar.gz\n", "a".repeat(64)).into_bytes()
    }

    // All signing material used here is generated in an owned temporary
    // directory and removed with it. No developer key or ssh-agent is used.
    fn signed(bytes: &[u8], namespace: &str) -> (String, Vec<u8>) {
        let dir = tempfile::tempdir().unwrap();
        let key = dir.path().join("example-key");
        let data = dir.path().join("manifest");
        assert!(Command::new("ssh-keygen")
            .args(["-q", "-t", "ed25519", "-N", "", "-C", "example", "-f"])
            .arg(&key)
            .output()
            .unwrap()
            .status
            .success());
        std::fs::write(&data, bytes).unwrap();
        assert!(Command::new("ssh-keygen")
            .args(["-Y", "sign", "-n", namespace, "-f"])
            .arg(&key)
            .arg(&data)
            .output()
            .unwrap()
            .status
            .success());
        (
            std::fs::read_to_string(key.with_extension("pub")).unwrap(),
            std::fs::read(data.with_extension("sig")).unwrap(),
        )
    }

    #[test]
    fn rejects_wrong_versions_duplicates_paths_and_missing_checksums() {
        let bytes = metadata();
        assert!(parse_manifest(&bytes, "9.9.9").is_ok());
        assert!(parse_manifest(&bytes, "9.9.8").is_err());
        let text = String::from_utf8(bytes).unwrap();
        for bad in [
            text.replace("thinkterm-server-", "../thinkterm-server-"),
            text.replace(&"a".repeat(64), "-"),
            text.replace(" 42 ", " +42 "),
            text.replace(" 42 ", " 0 "),
            format!("{text}{}\n", text.lines().last().unwrap()),
        ] {
            assert!(parse_manifest(bad.as_bytes(), "9.9.9").is_err());
        }
    }

    #[test]
    fn openssh_signature_checks_content_key_namespace_and_version() {
        let bytes = metadata();
        let (key, signature) = signed(&bytes, NAMESPACE);
        assert!(verified_manifest(&bytes, &signature, "9.9.9", &key).is_ok());
        let mut tampered = bytes.clone();
        tampered[0] ^= 1;
        assert!(verified_manifest(&tampered, &signature, "9.9.9", &key).is_err());
        assert!(verified_manifest(&bytes, &signature, "9.9.8", &key).is_err());
        assert!(verified_manifest(&bytes, &signature, "9.9.9", SIGNING_KEY).is_err());
        let (key, signature) = signed(&bytes, "example-other-namespace");
        assert!(verified_manifest(&bytes, &signature, "9.9.9", &key).is_err());
    }

    thread_local! {
        static RESPONSES: RefCell<(Vec<u8>, Vec<u8>, Vec<String>)> = const { RefCell::new((Vec::new(), Vec::new(), Vec::new())) };
    }

    fn offline_github(
        url: &str,
        headers: &[(&str, String)],
        deadline: Option<GiveWay>,
        body: &mut dyn Write,
    ) -> anyhow::Result<crate::Answer> {
        assert!(deadline.is_some());
        RESPONSES.with(|state| {
            let mut state = state.borrow_mut();
            state.2.push(url.into());
            if url.starts_with("https://api.github.com/") || url.starts_with("https://github.com/")
            {
                bail!("connection timed out");
            }
            assert!(!headers.iter().any(|(name, _)| *name == "Authorization"));
            body.write_all(if url.ends_with(".sig") {
                &state.1
            } else {
                &state.0
            })?;
            Ok(crate::Answer {
                status: 200,
                reason: "OK".into(),
                location: None,
            })
        })
    }

    #[test]
    fn api_failure_uses_verified_mirror_metadata_but_direct_and_token_do_not() {
        let bytes = metadata();
        let (key, signature) = signed(&bytes, NAMESPACE);
        RESPONSES.with(|state| *state.borrow_mut() = (bytes, signature, Vec::new()));
        assert!(lookup("9.9.9", DownloadSource::Auto, None, offline_github, &key).is_ok());
        for token in [None, Some("example-token")] {
            RESPONSES.with(|state| state.borrow_mut().2.clear());
            assert!(lookup(
                "9.9.9",
                if token.is_some() {
                    DownloadSource::Auto
                } else {
                    DownloadSource::Direct
                },
                token,
                offline_github,
                &key
            )
            .is_err());
            RESPONSES.with(|state| {
                assert!(state
                    .borrow()
                    .2
                    .iter()
                    .all(|url| url.starts_with("https://api.github.com/")
                        || url.starts_with("https://github.com/")))
            });
        }
        RESPONSES.with(|state| state.borrow_mut().0[0] ^= 1);
        assert!(lookup("9.9.9", DownloadSource::Auto, None, offline_github, &key).is_err());
    }

    #[test]
    fn metadata_download_has_a_memory_bound() {
        let mut body = LimitedBody {
            bytes: vec![],
            limit: 4,
        };
        assert_eq!(body.write(b"abc").unwrap(), 3);
        assert!(body.write(b"de").is_err());
        assert_eq!(body.bytes, b"abc");
    }

    #[test]
    fn online_installer_requires_the_key_namespace_content_and_protocol() {
        let script = format!("{INSTALLER_HEADER}echo example\n");
        let (key, signature) = signed(script.as_bytes(), INSTALLER_NAMESPACE);
        let select = |bytes: &[u8], signature: &[u8], key: &str| {
            select_installer(
                DownloadSource::Direct,
                key,
                INSTALLER_BUDGET,
                |url, _, _| {
                    Ok(if url.ends_with(".sig") {
                        signature
                    } else {
                        bytes
                    }
                    .to_vec())
                },
            )
        };
        let good = select(script.as_bytes(), &signature, &key);
        assert!(good.online);
        assert_eq!(good.text, script);
        for bad in [
            select(b"tampered", &signature, &key),
            select(script.as_bytes(), b"invalid", &key),
            select(script.as_bytes(), &signature, SIGNING_KEY),
        ] {
            assert!(!bad.online);
            assert_eq!(bad.text, INSTALL_SCRIPT);
        }
        let (other_key, other_sig) = signed(script.as_bytes(), NAMESPACE);
        assert!(!select(script.as_bytes(), &other_sig, &other_key).online);
        let incompatible = b"#!/bin/sh\n# thinkterm-installer-v2\n";
        let (key, signature) = signed(incompatible, INSTALLER_NAMESPACE);
        assert!(!select(incompatible, &signature, &key).online);
    }

    #[test]
    fn installer_mirrors_and_signature_share_one_deadline() {
        let script = format!("{INSTALLER_HEADER}echo example\n");
        let (key, signature) = signed(script.as_bytes(), INSTALLER_NAMESPACE);
        let mut first_deadline = None;
        let mut calls = 0;
        let selected = select_installer(
            DownloadSource::Auto,
            &key,
            INSTALLER_BUDGET,
            |url, _, deadline| {
                calls += 1;
                assert_eq!(*first_deadline.get_or_insert(deadline), deadline);
                if url.starts_with(crate::REPO_URL) {
                    bail!("offline");
                }
                Ok(if url.ends_with(".sig") {
                    signature.clone()
                } else {
                    script.as_bytes().to_vec()
                })
            },
        );
        assert!(selected.online);
        assert_eq!(calls, 3);
        let mut calls = 0;
        let selected = select_installer(
            DownloadSource::Auto,
            &key,
            Duration::from_millis(20),
            |_, _, deadline| {
                calls += 1;
                std::thread::sleep(
                    deadline.saturating_duration_since(Instant::now()) + Duration::from_millis(5),
                );
                Ok(script.as_bytes().to_vec())
            },
        );
        assert_eq!(calls, 1);
        assert!(!selected.online);
        assert_eq!(selected.text, INSTALL_SCRIPT);
    }

    #[test]
    fn unavailable_and_oversized_online_installers_use_bundled_copy() {
        for oversized in [false, true] {
            let selected = select_installer(
                DownloadSource::Direct,
                SIGNING_KEY,
                INSTALLER_BUDGET,
                |_, limit, _| {
                    if oversized {
                        Ok(vec![b'a'; limit + 1])
                    } else {
                        bail!("offline")
                    }
                },
            );
            assert!(!selected.online);
            assert_eq!(selected.text, INSTALL_SCRIPT);
        }
    }

    #[test]
    fn curl_installer_deadline_interrupts_a_stalled_connection() {
        use std::net::TcpListener;
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let url = format!(
            "https://localhost:{}/install.sh",
            listener.local_addr().unwrap().port()
        );
        // Keep TCP open without completing TLS; curl must enforce its own
        // total deadline rather than wait for a response or a read timeout.
        let started = Instant::now();
        let mut body = Vec::new();
        let result = crate::curl_get_inner(
            &["--max-time", "0.1", "--noproxy", "*"],
            &url,
            &[],
            Some(LOOKUP_TIMEOUT),
            &mut body,
            false,
        );
        assert!(result.is_err());
        assert!(started.elapsed() < Duration::from_secs(2));
        drop(listener);
    }

    #[test]
    fn shell_and_client_pin_the_same_public_key() {
        assert!(INSTALL_SCRIPT.contains(&format!("release_signing_key='{}'", SIGNING_KEY.trim())));
    }

    #[cfg(unix)]
    #[test]
    fn ssh_payload_finishes_with_stdin_held_open_and_removes_staging() {
        let release = parse_manifest(&metadata(), "9.9.9").unwrap();
        let dir = tempfile::tempdir().unwrap();
        let (command, payload) = payload_for_script("server", &release,
            "test \"$1\" = --server || exit 1\ncat \"$5\"\nif read -r ignored; then exit 2; fi\necho done\n").unwrap();
        let mut child = Command::new("sh")
            .args(["-c", &command])
            .env("HOME", dir.path())
            .env("TMPDIR", dir.path())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let mut held_stdin = child.stdin.take().unwrap();
        held_stdin.write_all(&payload).unwrap();
        let started = std::time::Instant::now();
        while child.try_wait().unwrap().is_none() {
            if started.elapsed() > Duration::from_secs(5) {
                child.kill().unwrap();
                panic!("installer waited for SSH EOF");
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        let out = child.wait_with_output().unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        assert_eq!(
            String::from_utf8(out.stdout).unwrap(),
            format!("{}done\n", release_info(&release).unwrap())
        );
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0);
        drop(held_stdin);
    }
}
