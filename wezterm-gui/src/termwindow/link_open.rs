//! Opening a link a terminal program printed. Any program can print one,
//! and OSC 8 lets it put any target behind any text, so a click only goes
//! straight through to a web page. A link that would hand itself to another
//! application asks first, and one carrying characters that do not show is
//! not opened at all: what the user saw is not what would open.

use crate::termwindow::ContextMenuApplicationAction;
use mux::pane::{Pane, PaneId};
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use window::{ContextMenuIcon, ContextMenuItem};

/// How long a link may be in the menu that asks about it, in characters.
/// The rest is elided from the middle, keeping the scheme and the end.
const SHOWN_LINK_CHARS: usize = 72;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum LinkOpen {
    /// A web page: the browser opens it, as it always has.
    Direct,
    /// Hands itself to another application: ask first.
    Confirm,
    /// Carries hidden characters, or could be an opener option.
    Refuse,
}

pub(crate) fn classify(uri: &str) -> LinkOpen {
    if wezterm_open_url::is_opener_option(uri) || uri.chars().any(is_hidden) {
        return LinkOpen::Refuse;
    }
    match uri.split_once(':') {
        Some((scheme, _))
            if scheme.eq_ignore_ascii_case("http") || scheme.eq_ignore_ascii_case("https") =>
        {
            LinkOpen::Direct
        }
        _ => LinkOpen::Confirm,
    }
}

/// Characters that take no room, steer the text's direction, or break the
/// line. A plain space shows, so it is left to the menu to display.
fn is_hidden(c: char) -> bool {
    c.is_control()
        || (c.is_whitespace() && c != ' ')
        || matches!(
            c,
            '\u{00AD}'
                | '\u{034F}'
                | '\u{061C}'
                | '\u{115F}'
                | '\u{1160}'
                | '\u{17B4}'
                | '\u{17B5}'
                | '\u{180B}'..='\u{180F}'
                | '\u{200B}'..='\u{200F}'
                | '\u{202A}'..='\u{202E}'
                | '\u{2060}'..='\u{206F}'
                | '\u{3164}'
                | '\u{FE00}'..='\u{FE0F}'
                | '\u{FEFF}'
                | '\u{FFA0}'
                | '\u{FFF9}'..='\u{FFFB}'
                | '\u{1D173}'..='\u{1D17A}'
                | '\u{E0000}'..='\u{E0FFF}'
        )
}

/// `uri` as the menu shows it: every hidden character spelled out as its
/// code point, and the middle elided past [`SHOWN_LINK_CHARS`].
fn shown_link(uri: &str) -> String {
    let mut shown = String::new();
    for c in uri.chars() {
        if is_hidden(c) {
            shown.push_str(&format!("<U+{:04X}>", c as u32));
        } else {
            shown.push(c);
        }
    }
    let count = shown.chars().count();
    if count <= SHOWN_LINK_CHARS {
        return shown;
    }
    let head = SHOWN_LINK_CHARS * 2 / 3;
    let tail = SHOWN_LINK_CHARS - head - 1;
    let mut elided: String = shown.chars().take(head).collect();
    elided.push('…');
    elided.extend(shown.chars().skip(count - tail));
    elided
}

/// A link that names a file on the host that owns the pane: an absolute
/// path or a `file://` URL without a host. Anything with another scheme is
/// either a web page or an application's own URL.
fn link_file_path(uri: &str) -> Option<PathBuf> {
    if let Ok(url) = url::Url::parse(uri) {
        if url.scheme() == "file" {
            return if url.host_str().is_some() {
                None
            } else {
                url.to_file_path().ok()
            };
        }
        if !url.scheme().is_empty() {
            return None;
        }
    }
    let path = PathBuf::from(uri);
    path.is_absolute().then_some(path)
}

fn pane_is_local(pane_id: PaneId) -> bool {
    mux::Mux::get()
        .get_pane(pane_id)
        .is_some_and(|pane| pane.downcast_ref::<mux::localpane::LocalPane>().is_some())
}

/// Fetch `path` from the host that owns `pane`, in chunks, so a picture
/// produced there can be opened by an application here.
async fn fetch_remote_file(pane: Arc<dyn Pane>, path: PathBuf) -> anyhow::Result<Vec<u8>> {
    const CHUNK: u32 = 4 * 1024 * 1024;
    const LIMIT: usize = 64 * 1024 * 1024;
    let display = path.display().to_string();
    let path = path.to_string_lossy().into_owned();
    let mut data = Vec::new();
    let mut offset = 0u64;
    loop {
        let response = pane
            .read_remote_file(codec::ReadRemoteFile {
                pane_id: pane.pane_id(),
                path: path.clone(),
                offset,
                length: CHUNK,
            })
            .await?;
        anyhow::ensure!(response.data.len() <= CHUNK as usize, "oversized file chunk");
        data.extend_from_slice(&response.data);
        offset += response.data.len() as u64;
        if response.eof {
            return Ok(data);
        }
        anyhow::ensure!(!response.data.is_empty(), "{display} ended early");
        anyhow::ensure!(
            data.len() <= LIMIT,
            "{display} is larger than the 64 MiB link limit"
        );
    }
}

/// Write the fetched bytes under the terminal cache so the local opener
/// sees the extension it needs, and return the copy's path.
fn write_link_copy(path: &Path, bytes: &[u8]) -> anyhow::Result<PathBuf> {
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("link");
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    path.hash(&mut hasher);
    let directory = std::env::temp_dir().join("thinkterm-links");
    std::fs::create_dir_all(&directory)?;
    let local = directory.join(format!("{:016x}-{name}", hasher.finish()));
    std::fs::write(&local, bytes)?;
    Ok(local)
}

impl crate::TermWindow {
    /// Open `uri`, a terminal link the user clicked that the `open-uri`
    /// event left to us: a web page at once, anything else after asking in
    /// a menu at `anchor`, and a link hiding characters not at all.
    pub(crate) fn open_terminal_link(
        &mut self,
        uri: String,
        anchor: ::window::Point,
        pane_id: PaneId,
    ) {
        let decision = classify(&uri);
        if decision == LinkOpen::Direct {
            wezterm_open_url::open_url(&uri);
            return;
        }
        let Some(window) = self.window.as_ref().cloned() else {
            return;
        };
        let header = ContextMenuItem::SectionHeader {
            label: shown_link(&uri),
        };
        let remote_file = link_file_path(&uri).is_some() && !pane_is_local(pane_id);
        self.begin_context_menu_application_actions();
        let items = if decision == LinkOpen::Refuse {
            log::warn!("not opening an unsafe terminal link");
            let refused = self.context_menu_application_item_with_icon(
                crate::i18n::tr("link-refused-unsafe"),
                ContextMenuIcon::Warning,
                ContextMenuApplicationAction::ResolveTerminalLink {
                    link: None,
                    pane_id: None,
                },
                false,
            );
            vec![header, refused]
        } else {
            let open = self.context_menu_application_item_with_icon(
                crate::i18n::tr(if remote_file {
                    "link-open-remote-app"
                } else {
                    "link-open-other-app"
                }),
                ContextMenuIcon::ExternalLink,
                ContextMenuApplicationAction::ResolveTerminalLink {
                    link: Some(uri),
                    pane_id: Some(pane_id),
                },
                true,
            );
            let cancel = self.context_menu_application_item_with_icon(
                crate::i18n::tr("link-open-cancel"),
                ContextMenuIcon::Close,
                ContextMenuApplicationAction::ResolveTerminalLink {
                    link: None,
                    pane_id: None,
                },
                true,
            );
            vec![header, open, cancel]
        };
        self.show_term_context_menu(&window, anchor, items);
    }

    /// Open a terminal link whose target lives on the pane's host: fetch a
    /// copy for this machine and hand that to the local opener.
    pub(crate) fn open_terminal_link_now(&self, link: String, pane_id: Option<PaneId>) {
        let Some(pane_id) = pane_id else {
            wezterm_open_url::open_url(&link);
            return;
        };
        let Some(path) = link_file_path(&link) else {
            wezterm_open_url::open_url(&link);
            return;
        };
        if pane_is_local(pane_id) {
            wezterm_open_url::open_url(&link);
            return;
        }
        let Some(pane) = mux::Mux::get().get_pane(pane_id) else {
            wezterm_open_url::open_url(&link);
            return;
        };
        promise::spawn::spawn(async move {
            let result = async {
                let bytes = fetch_remote_file(pane, path.clone()).await?;
                promise::spawn::spawn_into_new_thread(move || {
                    let local = write_link_copy(&path, &bytes)?;
                    wezterm_open_url::open_url(&local.to_string_lossy());
                    Ok(())
                })
                .await
            }
            .await;
            if let Err(err) = result {
                log::error!("unable to open a remote terminal link: {err:#}");
                wezterm_toast_notification::persistent_toast_notification(
                    "ThinkTerm",
                    &format!("Unable to open the linked file: {err}"),
                );
            }
        })
        .detach();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn web_pages_open_directly() {
        assert_eq!(classify("https://example.com/a?b=c"), LinkOpen::Direct);
        assert_eq!(classify("http://192.0.2.1:8080/"), LinkOpen::Direct);
        assert_eq!(classify("HTTPS://example.com"), LinkOpen::Direct);
    }

    #[test]
    fn opener_options_are_refused_before_confirmation() {
        for target in [
            "-a/home/user/Downloads/example.app",
            "--args",
            "--help",
            "-",
        ] {
            assert_eq!(classify(target), LinkOpen::Refuse, "{}", target);
        }
        assert_eq!(classify("./-notes.txt"), LinkOpen::Confirm);
        assert_eq!(classify("file:///home/user/-notes.txt"), LinkOpen::Confirm);
        assert_eq!(classify("https://example.com/-a"), LinkOpen::Direct);
    }

    #[test]
    fn other_schemes_ask_first() {
        for uri in [
            "file:///Applications/Example.app",
            "file:///home/user/notes.txt",
            "mailto:user@example.com",
            "vscode://file/home/user/main.rs:10",
            "ssh://devbox",
            "x-example-app:run",
            "httpx://example.com",
            "no scheme at all",
        ] {
            assert_eq!(classify(uri), LinkOpen::Confirm, "{uri}");
        }
    }

    #[test]
    fn hidden_characters_are_refused_whatever_the_scheme() {
        for uri in [
            "https://exa\u{200B}mple.com",
            "https://example.com/\u{202E}gpj.exe",
            "https://example.com\n/path",
            "file:///home/user/a\u{2028}b",
            "https://example.com/\u{00A0}",
            "https://example.com/\u{FEFF}",
            "https://example.com/\u{1b}[31m",
            "https://example.com/\u{E0041}",
        ] {
            assert_eq!(classify(uri), LinkOpen::Refuse, "{uri:?}");
        }
        assert_eq!(
            classify("file:///home/user/my notes.txt"),
            LinkOpen::Confirm
        );
    }

    #[test]
    fn file_links_are_paths_but_urls_are_not() {
        assert_eq!(
            link_file_path("/tmp/icon/preview.jpg"),
            Some(PathBuf::from("/tmp/icon/preview.jpg"))
        );
        assert_eq!(
            link_file_path("file:///tmp/icon/preview.jpg"),
            Some(PathBuf::from("/tmp/icon/preview.jpg"))
        );
        // The host in a file URL is some other machine's, not the pane's.
        assert_eq!(link_file_path("file://elsewhere/tmp/x.jpg"), None);
        for link in [
            "https://example.com/x.jpg",
            "mailto:user@example.com",
            "vscode://file/tmp/x.jpg",
            "notes.txt",
            "./notes.txt",
        ] {
            assert_eq!(link_file_path(link), None, "{link}");
        }
    }

    #[test]
    fn the_menu_spells_out_hidden_characters_and_elides_the_middle() {
        assert_eq!(
            shown_link("https://exa\u{200B}mple.com"),
            "https://exa<U+200B>mple.com"
        );
        let long = format!("file:///home/user/{}/main.rs", "deep/".repeat(40));
        let shown = shown_link(&long);
        assert_eq!(shown.chars().count(), SHOWN_LINK_CHARS);
        assert!(shown.starts_with("file:///home/user/deep/"));
        assert!(shown.ends_with("/main.rs"));
        assert!(shown.contains('…'));
    }
}
