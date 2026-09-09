//! A certificate for the web listener when the configuration names none:
//! self-signed, kept in the data directory, good for the machine's names
//! and addresses. A browser warns once and is then in a secure context,
//! which is what WebGPU needs; nothing here is a trust anchor.

use anyhow::Context;
use rcgen::{Certificate, CertificateParams, DistinguishedName, DnType, SanType};
use sha2::{Digest, Sha256};
use std::net::IpAddr;
use std::path::{Path, PathBuf};

/// The certificate and key for the web listener, made when missing or
/// when the machine has an address the stored one does not name.
pub struct WebTls {
    pub cert: PathBuf,
    pub key: PathBuf,
    /// Hex SHA-256 of the certificate, for the log and the docs.
    pub fingerprint: String,
    pub generated: bool,
}

fn dir() -> PathBuf {
    config::DATA_DIR.join("web-tls")
}

fn names(addresses: &[IpAddr]) -> (Vec<String>, Vec<IpAddr>) {
    let mut dns = vec!["localhost".to_string()];
    if let Ok(host) = hostname::get() {
        if let Ok(host) = host.into_string() {
            if !host.is_empty() {
                dns.push(host.clone());
                if !host.contains('.') {
                    dns.push(format!("{host}.local"));
                }
            }
        }
    }
    dns.sort();
    dns.dedup();
    let mut ips: Vec<IpAddr> = vec!["127.0.0.1".parse().unwrap(), "::1".parse().unwrap()];
    ips.extend(addresses.iter().copied());
    ips.sort();
    ips.dedup();
    (dns, ips)
}

fn sans_text(dns: &[String], ips: &[IpAddr]) -> String {
    let mut lines: Vec<String> = dns.iter().cloned().collect();
    lines.extend(ips.iter().map(|ip| ip.to_string()));
    lines.join("\n")
}

fn fingerprint(der: &[u8]) -> String {
    let digest = Sha256::digest(der);
    digest.iter().map(|b| format!("{b:02x}")).collect::<Vec<_>>().join(":")
}

fn write_private(path: &Path, bytes: &[u8]) -> anyhow::Result<()> {
    let tmp = path.with_extension("tmp");
    {
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create(true).truncate(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options.open(&tmp).with_context(|| format!("writing {}", tmp.display()))?;
        std::io::Write::write_all(&mut file, bytes)?;
    }
    std::fs::rename(&tmp, path).with_context(|| format!("renaming into {}", path.display()))?;
    Ok(())
}

/// The listener's certificate: the stored one when it still names every
/// address the machine has, else a fresh one.
pub fn ensure(addresses: &[IpAddr]) -> anyhow::Result<WebTls> {
    let dir = dir();
    std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    let cert_path = dir.join("cert.pem");
    let key_path = dir.join("key.pem");
    let sans_path = dir.join("sans.txt");
    let (dns, ips) = names(addresses);
    let wanted = sans_text(&dns, &ips);
    let stored = std::fs::read_to_string(&sans_path).unwrap_or_default();
    let stale = stored != wanted;
    if cert_path.is_file() && key_path.is_file() && !stale {
        let pem = std::fs::read(&cert_path)?;
        let der = pem_to_der(&pem).unwrap_or_default();
        return Ok(WebTls { cert: cert_path, key: key_path, fingerprint: fingerprint(&der), generated: false });
    }
    let mut params = CertificateParams::new(dns.clone());
    for ip in &ips {
        params.subject_alt_names.push(SanType::IpAddress(*ip));
    }
    let mut dn = DistinguishedName::new();
    dn.push(DnType::CommonName, dns.iter().find(|n| *n != "localhost").cloned().unwrap_or_else(|| "ThinkTerm".into()));
    dn.push(DnType::OrganizationName, "ThinkTerm web listener");
    params.distinguished_name = dn;
    params.not_before = rcgen::date_time_ymd(2024, 1, 1);
    params.not_after = rcgen::date_time_ymd(2040, 1, 1);
    let cert = Certificate::from_params(params).context("generating the web certificate")?;
    let der = cert.serialize_der().context("encoding the web certificate")?;
    write_private(&key_path, cert.serialize_private_key_pem().as_bytes())?;
    write_private(&cert_path, cert.serialize_pem()?.as_bytes())?;
    write_private(&sans_path, wanted.as_bytes())?;
    Ok(WebTls { cert: cert_path, key: key_path, fingerprint: fingerprint(&der), generated: true })
}

/// The DER inside a PEM certificate, for the fingerprint of a stored one.
fn pem_to_der(pem: &[u8]) -> Option<Vec<u8>> {
    let text = std::str::from_utf8(pem).ok()?;
    let body: String = text
        .lines()
        .filter(|l| !l.starts_with("-----"))
        .collect::<Vec<_>>()
        .join("");
    base64_decode(&body)
}

fn base64_decode(s: &str) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(s.len() * 3 / 4);
    let mut buf = 0u32;
    let mut bits = 0;
    for c in s.bytes() {
        let v = match c {
            b'A'..=b'Z' => c - b'A',
            b'a'..=b'z' => c - b'a' + 26,
            b'0'..=b'9' => c - b'0' + 52,
            b'+' => 62,
            b'/' => 63,
            b'=' => break,
            _ => return None,
        } as u32;
        buf = (buf << 6) | v;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push(((buf >> bits) & 0xff) as u8);
        }
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_names_always_carry_loopback_and_the_addresses() {
        let (dns, ips) = names(&["192.168.1.7".parse().unwrap()]);
        assert!(dns.contains(&"localhost".to_string()));
        assert!(ips.contains(&"127.0.0.1".parse().unwrap()));
        assert!(ips.contains(&"192.168.1.7".parse().unwrap()));
        assert_eq!(base64_decode("aGVsbG8="), Some(b"hello".to_vec()));
    }

    #[test]
    fn a_pem_certificate_decodes_to_its_der() {
        let (dns, ips) = names(&[]);
        let mut params = CertificateParams::new(dns);
        for ip in &ips {
            params.subject_alt_names.push(SanType::IpAddress(*ip));
        }
        let cert = Certificate::from_params(params).unwrap();
        let pem = cert.serialize_pem().unwrap();
        let der = pem_to_der(pem.as_bytes()).unwrap();
        // A DER certificate is one outer SEQUENCE; the signature inside is
        // fresh on every serialisation, so only the shape is compared.
        assert_eq!(&der[..2], &[0x30, 0x82]);
        assert!((der.len() as i64 - cert.serialize_der().unwrap().len() as i64).abs() <= 4);
        assert_eq!(fingerprint(&der).len(), 32 * 3 - 1);
    }
}
