//! text/uri-list decoding, shared by drag-and-drop (XDND) and clipboard
//! reads on the X11 and Wayland backends.
use std::path::PathBuf;
use url::Url;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedUriList {
    pub paths: Vec<PathBuf>,
    /// A normalized textual rendition for non-file URI lists. Comments and
    /// blank lines are omitted so this can safely become the paste fallback.
    pub fallback_text: String,
}

pub fn decode_texturi_list(url_list: &[u8]) -> ParsedUriList {
    let mut paths = Vec::new();
    let mut uri_lines = Vec::new();

    for line in String::from_utf8_lossy(url_list).lines() {
        let line = line.trim();
        if line.starts_with('#') || line.is_empty() {
            // text/uri-list: comment and empty lines carry no payload.
            continue;
        }
        uri_lines.push(line.to_string());
        let Ok(url) = Url::parse(line).map_err(|err| {
            log::error!("Error parsing dropped file line {line} as url: {err:#}");
        }) else {
            continue;
        };
        if let Ok(path) = url.to_file_path().map_err(|_| {
            log::debug!("URI-list entry {url:?} is not a local file");
        }) {
            paths.push(path);
        }
    }

    ParsedUriList {
        paths,
        fallback_text: uri_lines.join("\n"),
    }
}

pub fn parse_texturi_list(url_list: &[u8]) -> Vec<PathBuf> {
    decode_texturi_list(url_list).paths
}

pub fn clipboard_contents_from_uri_list(
    uri_list: &[u8],
    advertised_text: Option<&[u8]>,
) -> crate::ClipboardContents {
    let parsed = decode_texturi_list(uri_list);
    if !parsed.paths.is_empty() {
        crate::ClipboardContents::FilePaths(parsed.paths)
    } else if let Some(text) = advertised_text {
        crate::ClipboardContents::Text(String::from_utf8_lossy(text).replace("\r\n", "\n"))
    } else {
        crate::ClipboardContents::Text(parsed.fallback_text)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_uri_list_decodes_paths_and_skips_noise() {
        let list = b"# comment line\r\nfile:///home/x/a%20b.txt\r\n\r\nhttps://example.com/nope\r\nfile:///tmp/plain.png\r\n";
        assert_eq!(
            parse_texturi_list(list),
            vec![
                PathBuf::from("/home/x/a b.txt"),
                PathBuf::from("/tmp/plain.png"),
            ]
        );
    }

    #[test]
    fn uri_without_files_uses_advertised_text() {
        assert_eq!(
            clipboard_contents_from_uri_list(
                b"https://example.com/report\r\n",
                Some(b"Quarterly report\r\n")
            ),
            crate::ClipboardContents::Text("Quarterly report\n".to_string())
        );
    }

    #[test]
    fn uri_without_text_preserves_normalized_uri_text() {
        assert_eq!(
            clipboard_contents_from_uri_list(
                b"# generated\r\nhttps://example.com/a\r\nhttps://example.com/b\r\n",
                None,
            ),
            crate::ClipboardContents::Text(
                "https://example.com/a\nhttps://example.com/b".to_string()
            )
        );
    }
}
