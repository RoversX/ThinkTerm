use anyhow::Context;
use std::borrow::Cow;
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::PathBuf;

/// Simple heuristics to try to avoid obvious trickery with
/// the name provided by the remote system
fn neuter_name(name: &str) -> Option<&str> {
    let name = match name.rsplit_once(|c| c == '/' || c == '\\') {
        Some((_, base)) => base,
        None => name,
    };

    if name == "." || name == ".." {
        return None;
    }

    if name.contains(':') {
        return None;
    }

    Some(name)
}
/// Given a suggested name, make a few attempts to derive a local name
/// in the user's download folder that doesn't conflict with any other
/// files in that folder.
/// Returns the selected name and the opened File on success.
fn resolve_file_name(name: Option<&str>) -> anyhow::Result<(PathBuf, File)> {
    // `neuter_name` is left exactly as it is, refusal of every `:` included:
    // that is stricter than Linux needs, but relaxing it there would change
    // what this does on a platform that has no problem with the name. The
    // host's own filename rules are applied to what it returns -- and never
    // to the fallback, which needs nothing done to it.
    let name = name
        .and_then(neuter_name)
        .map(|name| crate::termwindow::remote_walk::DownloadNameRules::host().sanitize(name))
        .unwrap_or(Cow::Borrowed("downloaded-via-wezterm"));

    let download_dir = dirs_next::download_dir()
        .ok_or_else(|| anyhow::anyhow!("unable to locate download directory"))?;

    for n in 0..20 {
        let candidate = if n == 0 {
            download_dir.join(&*name)
        } else {
            download_dir.join(&format!("{}.{}", name, n))
        };

        if let Ok(file) = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&candidate)
        {
            return Ok((candidate, file));
        }
    }

    anyhow::bail!(
        "Unable to find non-conflicting download name for {} in {}",
        name,
        download_dir.display()
    );
}

pub fn save_to_downloads(orig_name: Option<String>, data: &[u8]) -> anyhow::Result<()> {
    let (name, mut file) = resolve_file_name(orig_name.as_deref())?;
    file.write_all(data)
        .with_context(|| format!("writing {} of data to {}", data.len(), name.display()))?;

    let url = format!("file://{}", name.display());
    wezterm_toast_notification::persistent_toast_notification_with_click_to_open_url(
        "Download completed",
        &format!("Downloaded {}", name.display()),
        &url,
    );

    log::info!("Downloaded {}", name.display());

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::neuter_name;

    /// `neuter_name` refuses a `:` on every platform, and must go on doing
    /// so. It is stricter than unix needs -- `a:b.txt` is an ordinary name
    /// there -- but it is what this has always done, and the host filename
    /// rules are layered on top of it rather than replacing it, so that
    /// tightening Windows leaves the other platforms exactly as they were.
    #[test]
    fn a_stream_supplied_name_never_keeps_a_colon() {
        assert_eq!(neuter_name("report:2024.txt"), None);
        assert_eq!(neuter_name("C:evil.txt"), None);
        assert_eq!(neuter_name("."), None);
        assert_eq!(neuter_name(".."), None);
        // Separators are cut, not refused -- both of them, on every
        // platform, which is stricter than `Path::file_name` on unix.
        assert_eq!(neuter_name("a/b/c.txt"), Some("c.txt"));
        assert_eq!(neuter_name("a\\b\\c.txt"), Some("c.txt"));
        assert_eq!(neuter_name("report.txt"), Some("report.txt"));
    }
}
