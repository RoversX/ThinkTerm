//! Bounded, worker-thread-friendly loading for remote Markdown images.
//!
//! This module intentionally exposes a blocking API. Callers must invoke it
//! from a worker thread rather than the UI thread. Redirects are handled here
//! so that every destination can be resolved and checked before connecting.

use anyhow::{bail, Context};
use reqwest::blocking::{Client, Response};
use reqwest::header::{ACCEPT, ACCEPT_ENCODING, LOCATION};
use std::io::Read;
use std::net::{IpAddr, SocketAddr, ToSocketAddrs};
use std::time::{Duration, Instant};
use termwiz::image::ImageDataType;
use url::{Host, Url};

pub(crate) const REMOTE_IMAGE_MAX_ENCODED_BYTES: usize = 16 * 1024 * 1024;
pub(crate) const REMOTE_IMAGE_MAX_PIXELS: u64 = 16_000_000;
pub(crate) const REMOTE_IMAGE_MAX_REDIRECTS: usize = 5;
pub(crate) const REMOTE_IMAGE_TIMEOUT: Duration = Duration::from_secs(10);

/// Encoded image bytes plus header dimensions. Keeping the original encoding
/// allows `ImageDataType` to retain animated image data and defer full decode
/// until the renderer actually needs it.
#[derive(Debug)]
pub(crate) struct RemoteImage {
    pub(crate) bytes: Vec<u8>,
    pub(crate) width: u32,
    pub(crate) height: u32,
}

impl RemoteImage {
    pub(crate) fn into_image_data(self) -> ImageDataType {
        ImageDataType::EncodedFile(self.bytes)
    }
}

/// Fetch a remote image under strict network, byte-size and decoded-pixel
/// limits. This function blocks and is intended to be called by a worker.
pub(crate) fn load_remote_image(source: &str) -> anyhow::Result<RemoteImage> {
    let started = Instant::now();
    let mut current = parse_remote_url(source)?;

    for redirects_followed in 0..=REMOTE_IMAGE_MAX_REDIRECTS {
        let destination = resolve_public_destination(&current)
            .with_context(|| format!("resolve remote image host for {current}"))?;
        // Account for DNS time before starting the network request. The
        // platform resolver itself is synchronous, but a slow lookup must not
        // leave a fresh ten-second HTTP budget afterward.
        let remaining = REMOTE_IMAGE_TIMEOUT
            .checked_sub(started.elapsed())
            .context("remote image request timed out")?;
        let client = build_client(&current, &destination, remaining)?;
        let mut response = client
            .get(current.clone())
            .header(ACCEPT, "image/*")
            .header(ACCEPT_ENCODING, "identity")
            .send()
            .with_context(|| format!("request remote image {current}"))?;

        if let Some(remote) = response.remote_addr() {
            ensure_public_ip(remote.ip()).context("remote image connected to a blocked address")?;
        }

        if response.status().is_redirection() {
            if redirects_followed == REMOTE_IMAGE_MAX_REDIRECTS {
                bail!(
                    "remote image exceeded the limit of {} redirects",
                    REMOTE_IMAGE_MAX_REDIRECTS
                );
            }
            current = redirect_target(&current, &response)?;
            continue;
        }

        if !response.status().is_success() {
            bail!("remote image request returned HTTP {}", response.status());
        }

        if response
            .content_length()
            .is_some_and(|len| len > REMOTE_IMAGE_MAX_ENCODED_BYTES as u64)
        {
            bail!(
                "remote image is larger than the {} MiB encoded-data limit",
                REMOTE_IMAGE_MAX_ENCODED_BYTES / 1024 / 1024
            );
        }

        // The client's request timeout covers both receiving the headers and
        // reading the response body. It was set to the budget remaining at
        // the start of this redirect hop.
        let bytes = read_bounded(&mut response, REMOTE_IMAGE_MAX_ENCODED_BYTES)?;
        return validate_encoded_image(bytes);
    }

    unreachable!("redirect loop always returns or fails")
}

fn build_client(
    url: &Url,
    destinations: &[SocketAddr],
    timeout: Duration,
) -> anyhow::Result<Client> {
    let host = url.host_str().context("remote image URL has no host")?;
    let mut builder = Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .no_proxy()
        .timeout(timeout)
        .connect_timeout(timeout);

    // Pin DNS names to the addresses that passed the public-address policy.
    // IP literals need no resolver override.
    if matches!(url.host(), Some(Host::Domain(_))) {
        builder = builder.resolve_to_addrs(host, destinations);
    }

    builder.build().context("build remote image HTTP client")
}

fn parse_remote_url(source: &str) -> anyhow::Result<Url> {
    let url = Url::parse(source).context("parse remote image URL")?;
    validate_remote_url(&url)?;
    Ok(url)
}

fn validate_remote_url(url: &Url) -> anyhow::Result<()> {
    if !matches!(url.scheme(), "http" | "https") {
        bail!("remote images require an HTTP or HTTPS URL");
    }
    if url.host().is_none() {
        bail!("remote image URL has no host");
    }
    if !url.username().is_empty() || url.password().is_some() {
        bail!("remote image URL must not contain credentials");
    }
    Ok(())
}

fn redirect_target(base: &Url, response: &Response) -> anyhow::Result<Url> {
    let location = response
        .headers()
        .get(LOCATION)
        .context("remote image redirect has no Location header")?
        .to_str()
        .context("remote image redirect Location is not valid text")?;
    let target = base
        .join(location)
        .context("resolve remote image redirect URL")?;
    validate_remote_url(&target)?;
    Ok(target)
}

fn resolve_public_destination(url: &Url) -> anyhow::Result<Vec<SocketAddr>> {
    validate_remote_url(url)?;
    let port = url
        .port_or_known_default()
        .context("remote image URL has no usable port")?;

    let addresses: Vec<SocketAddr> = match url.host().context("remote image URL has no host")? {
        Host::Ipv4(ip) => vec![SocketAddr::new(IpAddr::V4(ip), port)],
        Host::Ipv6(ip) => vec![SocketAddr::new(IpAddr::V6(ip), port)],
        Host::Domain(host) => {
            if host.eq_ignore_ascii_case("localhost") || host.ends_with(".localhost") {
                bail!("remote image host is local");
            }
            (host, port)
                .to_socket_addrs()
                .with_context(|| format!("DNS lookup failed for {host}"))?
                .collect()
        }
    };

    if addresses.is_empty() {
        bail!("remote image host resolved to no addresses");
    }
    for address in &addresses {
        ensure_public_ip(address.ip())?;
    }
    Ok(addresses)
}

fn ensure_public_ip(ip: IpAddr) -> anyhow::Result<()> {
    if is_public_ip(ip) {
        Ok(())
    } else {
        bail!("remote image target address {ip} is not public")
    }
}

fn is_public_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(ip) => {
            let [a, b, c, _] = ip.octets();
            !(a == 0
                || a == 10
                || a == 127
                || (a == 100 && (64..=127).contains(&b))
                || (a == 169 && b == 254)
                || (a == 172 && (16..=31).contains(&b))
                || (a == 192 && b == 0 && c == 0)
                || (a == 192 && b == 0 && c == 2)
                || (a == 192 && b == 168)
                || (a == 198 && (b == 18 || b == 19))
                || (a == 198 && b == 51 && c == 100)
                || (a == 203 && b == 0 && c == 113)
                || a >= 224)
        }
        IpAddr::V6(ip) => {
            // Cover both IPv4-mapped (::ffff:a.b.c.d) and the historic
            // IPv4-compatible (::a.b.c.d) representation.
            if let Some(embedded) = ip.to_ipv4() {
                return is_public_ip(IpAddr::V4(embedded));
            }
            let segments = ip.segments();
            !(ip.is_unspecified()
                || ip.is_loopback()
                || ip.is_multicast()
                || (segments[0] & 0xfe00) == 0xfc00 // unique-local fc00::/7
                || (segments[0] & 0xffc0) == 0xfe80 // link-local fe80::/10
                || (segments[0] & 0xffc0) == 0xfec0 // historic site-local fec0::/10
                || (segments[0] == 0x2001 && segments[1] == 0x0db8))
        }
    }
}

fn read_bounded(reader: &mut impl Read, max_bytes: usize) -> anyhow::Result<Vec<u8>> {
    let mut bytes = Vec::new();
    let mut buffer = [0u8; 16 * 1024];
    loop {
        let read = reader
            .read(&mut buffer)
            .context("read remote image response")?;
        if read == 0 {
            break;
        }
        if bytes.len().saturating_add(read) > max_bytes {
            bail!(
                "remote image is larger than the {} MiB encoded-data limit",
                max_bytes / 1024 / 1024
            );
        }
        bytes.extend_from_slice(&buffer[..read]);
    }
    Ok(bytes)
}

fn validate_encoded_image(bytes: Vec<u8>) -> anyhow::Result<RemoteImage> {
    if bytes.len() > REMOTE_IMAGE_MAX_ENCODED_BYTES {
        bail!("remote image exceeds the encoded-data limit");
    }
    let encoded = ImageDataType::EncodedFile(bytes);
    let (width, height) = encoded
        .dimensions()
        .context("decode remote image dimensions")?;
    validate_dimensions(width, height)?;
    let ImageDataType::EncodedFile(bytes) = encoded else {
        unreachable!();
    };
    Ok(RemoteImage {
        bytes,
        width,
        height,
    })
}

fn validate_dimensions(width: u32, height: u32) -> anyhow::Result<()> {
    let pixels = u64::from(width) * u64::from(height);
    if width == 0 || height == 0 {
        bail!("remote image has zero width or height");
    }
    if pixels > REMOTE_IMAGE_MAX_PIXELS {
        bail!(
            "remote image is too large to decode ({width}x{height}, over {} megapixels)",
            REMOTE_IMAGE_MAX_PIXELS / 1_000_000
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[test]
    fn url_policy_allows_only_credential_free_http_urls() {
        assert!(validate_remote_url(&Url::parse("https://example.com/image.png").unwrap()).is_ok());
        assert!(validate_remote_url(&Url::parse("http://example.com/image.png").unwrap()).is_ok());
        assert!(validate_remote_url(&Url::parse("file:///tmp/image.png").unwrap()).is_err());
        assert!(validate_remote_url(&Url::parse("ftp://example.com/image.png").unwrap()).is_err());
        assert!(
            validate_remote_url(&Url::parse("https://user:secret@example.com/a").unwrap()).is_err()
        );
    }

    #[test]
    fn address_policy_rejects_non_public_ranges() {
        for address in [
            "0.0.0.0",
            "10.1.2.3",
            "100.64.0.1",
            "127.0.0.1",
            "169.254.4.2",
            "172.16.0.1",
            "192.168.1.1",
            "198.18.0.1",
            "224.0.0.1",
            "::",
            "::1",
            "::127.0.0.1",
            "::ffff:127.0.0.1",
            "fc00::1",
            "fe80::1",
            "2001:db8::1",
        ] {
            let ip: IpAddr = address.parse().unwrap();
            assert!(!is_public_ip(ip), "unexpectedly allowed {}", address);
        }
        for address in ["1.1.1.1", "8.8.8.8", "2606:4700:4700::1111"] {
            let ip: IpAddr = address.parse().unwrap();
            assert!(is_public_ip(ip), "unexpectedly blocked {}", address);
        }
    }

    #[test]
    fn literal_private_destination_is_rejected_without_network() {
        for url in [
            "http://127.0.0.1/image.png",
            "http://[::1]/image.png",
            "http://192.168.1.1/image.png",
            "http://localhost/image.png",
        ] {
            let url = Url::parse(url).unwrap();
            assert!(resolve_public_destination(&url).is_err(), "allowed {}", url);
        }
    }

    #[test]
    fn byte_guard_stops_after_limit() {
        let data = vec![0u8; 33];
        let error = read_bounded(&mut Cursor::new(data), 32).unwrap_err();
        assert!(error.to_string().contains("encoded-data limit"));
    }

    #[test]
    fn pixel_guard_is_overflow_safe_and_rejects_oversized_images() {
        assert!(validate_dimensions(4_000, 4_000).is_ok());
        assert!(validate_dimensions(4_001, 4_000).is_err());
        assert!(validate_dimensions(u32::MAX, u32::MAX).is_err());
        assert!(validate_dimensions(0, 100).is_err());
    }

    #[test]
    fn valid_png_retains_encoded_bytes_and_dimensions() {
        // Header-only dimensions are sufficient; this tiny valid PNG is 1x1.
        let bytes = vec![
            137, 80, 78, 71, 13, 10, 26, 10, 0, 0, 0, 13, 73, 72, 68, 82, 0, 0, 0, 1, 0, 0, 0, 1,
            8, 6, 0, 0, 0, 31, 21, 196, 137, 0, 0, 0, 13, 73, 68, 65, 84, 8, 215, 99, 248, 207,
            192, 240, 31, 0, 5, 0, 1, 255, 137, 153, 61, 29, 0, 0, 0, 0, 73, 69, 78, 68, 174, 66,
            96, 130,
        ];
        let expected = bytes.clone();
        let image = validate_encoded_image(bytes).unwrap();
        assert_eq!((image.width, image.height), (1, 1));
        assert_eq!(image.bytes, expected);
    }
}
