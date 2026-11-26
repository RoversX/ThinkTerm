//! A file the conversation named, as a client asked to see it
//! ([`crate::Op::Open`]): read here, beside it, so that only what is shown
//! crosses the connection -- the start of its text and how it is
//! coloured, or a picture of it no larger than a phone shows.

use crate::{FileShown, FILE_TEXT_SHOWN};
use base64::Engine;
use std::io::Read;
use std::path::{Path, PathBuf};
use thinkterm_syntax::HighlightKind;

/// The longest side of a picture shown whole, in pixels.
const PICTURE_SIDE: u32 = 2048;
/// Pictures larger than this, in bytes, are not opened.
const PICTURE_LARGEST: u64 = 40 << 20;
/// What is read to tell text from what is not.
const SNIFFED: usize = 8 << 10;
/// The largest picture sent, before base64 (which makes it a third larger):
/// well inside the line a client takes ([`crate::MAX_LINE`]). A PNG past it
/// goes as a JPEG; a JPEG past it, less fine or smaller.
const PICTURE_SENT_LARGEST: usize = 4 << 20;
/// The most coloured spans told: past them, the text goes plain.
const SPANS_KEPT: usize = 60_000;

/// `path` as the conversation named it, where it is on this machine: one
/// from home (`~/`), or from where the agent was started.
pub(crate) fn resolve(path: &str, cwd: Option<&Path>) -> PathBuf {
    if let Some(rest) = path.strip_prefix("~/") {
        if let Some(home) = std::env::var_os("HOME") {
            return PathBuf::from(home).join(rest);
        }
    }
    let given = Path::new(path);
    match cwd {
        Some(cwd) if given.is_relative() => cwd.join(given),
        _ => given.to_path_buf(),
    }
}

/// The file at `path` as a client shows it.
pub(crate) fn open(path: &str, cwd: Option<&Path>) -> FileShown {
    let mut shown = FileShown {
        path: path.to_string(),
        ..FileShown::default()
    };
    let at = resolve(path, cwd);
    let meta = match std::fs::metadata(&at) {
        Ok(meta) => meta,
        Err(err) => {
            shown.error = Some(if err.kind() == std::io::ErrorKind::NotFound { "missing" } else { "unreadable" }.into());
            return shown;
        }
    };
    if meta.is_dir() {
        shown.error = Some("folder".into());
        return shown;
    }
    // A pipe, a socket, a device: nothing to show, and opening a pipe with
    // no writer would wait for one for ever.
    if !meta.is_file() {
        shown.error = Some("unreadable".into());
        return shown;
    }
    shown.size = meta.len();
    let Ok(mut file) = std::fs::File::open(&at) else {
        shown.error = Some("unreadable".into());
        return shown;
    };
    let mut head = Vec::with_capacity(SNIFFED);
    if (&mut file).take(SNIFFED as u64).read_to_end(&mut head).is_err() {
        shown.error = Some("unreadable".into());
        return shown;
    }
    // Only a picture's strong mark, or a picture's name: text can begin as
    // a weaker one does ("BM", "P1").
    let format = image::guess_format(&head).ok().filter(|format| {
        use image::ImageFormat::{Gif, Jpeg, Png, WebP};
        matches!(format, Png | Jpeg | Gif | WebP) || super::thumb::is_picture_path(path)
    });
    if let Some(format) = format {
        // The rest from the file opened, no more than a picture may be:
        // one grown or put in its place since is not read whole.
        let mut bytes = head;
        let read = (&mut file).take(PICTURE_LARGEST + 1 - bytes.len() as u64).read_to_end(&mut bytes);
        if bytes.len() as u64 > PICTURE_LARGEST {
            shown.error = Some("too_big".into());
        } else {
            match read.ok().and_then(|_| picture(&bytes, format)) {
                Some(picture) => shown.picture = Some(picture),
                None => shown.error = Some("unreadable".into()),
            }
        }
        return shown;
    }
    if head.contains(&0) {
        shown.error = Some("binary".into());
        return shown;
    }
    let mut bytes = head;
    if (&mut file)
        .take((FILE_TEXT_SHOWN - bytes.len().min(FILE_TEXT_SHOWN)) as u64)
        .read_to_end(&mut bytes)
        .is_err()
    {
        shown.error = Some("unreadable".into());
        return shown;
    }
    shown.cut = meta.len() > bytes.len() as u64;
    let text = whole_text(bytes, shown.cut);
    let language = thinkterm_syntax::detect_path(&at, &text);
    shown.language = language.map(|l| l.as_str().to_string());
    // Markdown is laid out, not coloured.
    if let Some(language) = language.filter(|l| *l != thinkterm_syntax::LanguageId::Markdown) {
        if let Ok(found) = thinkterm_syntax::highlight(language, &text, None) {
            shown.spans = found
                .spans
                .into_iter()
                .take(SPANS_KEPT)
                .filter(|span| span.range.end > span.range.start)
                .map(|span| (span.range.start as u32, span.range.end as u32, kind_word(span.kind).to_string()))
                .collect();
        }
    }
    shown.text = Some(text);
    shown
}

/// `bytes` as text: cut short where the read stopped, not inside a
/// character; anything that is not UTF-8 shown as the replacement mark.
fn whole_text(mut bytes: Vec<u8>, cut: bool) -> String {
    if cut {
        // A character the cut went through is left out whole.
        let keep = match std::str::from_utf8(&bytes) {
            Ok(_) => bytes.len(),
            Err(err) if err.error_len().is_none() => err.valid_up_to(),
            Err(_) => bytes.len(),
        };
        bytes.truncate(keep);
    }
    match String::from_utf8(bytes) {
        Ok(text) => text,
        Err(err) => String::from_utf8_lossy(err.as_bytes()).into_owned(),
    }
}

/// A picture no larger than a phone shows whole, as a JPEG (a PNG when it
/// has see-through parts and that stays small), base64.
fn picture(bytes: &[u8], format: image::ImageFormat) -> Option<String> {
    let mut reader = image::ImageReader::with_format(std::io::Cursor::new(bytes), format);
    let mut limits = image::Limits::default();
    limits.max_image_width = Some(20_000);
    limits.max_image_height = Some(20_000);
    limits.max_alloc = Some(512 << 20);
    reader.limits(limits);
    let decoded = reader.decode().ok()?;
    let fitted = if decoded.width().max(decoded.height()) > PICTURE_SIDE {
        decoded.resize(PICTURE_SIDE, PICTURE_SIDE, image::imageops::FilterType::Triangle)
    } else {
        decoded
    };
    let encode = |bytes: Vec<u8>| base64::engine::general_purpose::STANDARD.encode(bytes);
    if fitted.color().has_alpha() {
        let mut png = std::io::Cursor::new(Vec::new());
        if fitted.write_to(&mut png, image::ImageFormat::Png).is_ok() && png.get_ref().len() <= PICTURE_SENT_LARGEST {
            return Some(encode(png.into_inner()));
        }
    }
    // A picture of fine detail can make a large JPEG: less fine, then
    // smaller, until it fits in a line with room to spare.
    let mut rgb = fitted.to_rgb8();
    for quality in [85, 70, 55, 55, 55] {
        let mut jpeg = std::io::Cursor::new(Vec::new());
        image::codecs::jpeg::JpegEncoder::new_with_quality(&mut jpeg, quality)
            .encode_image(&rgb)
            .ok()?;
        if jpeg.get_ref().len() <= PICTURE_SENT_LARGEST {
            return Some(encode(jpeg.into_inner()));
        }
        if quality == 55 {
            let (w, h) = (rgb.width() * 3 / 4, rgb.height() * 3 / 4);
            rgb = image::imageops::resize(&rgb, w.max(1), h.max(1), image::imageops::FilterType::Triangle);
        }
    }
    None
}

/// What a coloured span is, as a client colours it.
fn kind_word(kind: HighlightKind) -> &'static str {
    match kind {
        HighlightKind::Attribute => "attribute",
        HighlightKind::Comment => "comment",
        HighlightKind::Constant => "constant",
        HighlightKind::Constructor => "constructor",
        HighlightKind::Embedded => "embedded",
        HighlightKind::Function => "function",
        HighlightKind::Keyword => "keyword",
        HighlightKind::Label => "label",
        HighlightKind::Module => "module",
        HighlightKind::Number => "number",
        HighlightKind::Operator => "operator",
        HighlightKind::Property => "property",
        HighlightKind::Punctuation => "punctuation",
        HighlightKind::String => "string",
        HighlightKind::Tag => "tag",
        HighlightKind::Type => "type",
        HighlightKind::Variable => "variable",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("tt-chat-files-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn code_comes_coloured_and_from_where_the_agent_started() {
        let dir = scratch("code");
        std::fs::write(dir.join("main.rs"), "fn main() {\n    let x = \"hi\";\n}\n").unwrap();
        let shown = open("main.rs", Some(&dir));
        assert_eq!(shown.error, None);
        assert_eq!(shown.language.as_deref(), Some("rust"));
        assert_eq!(shown.text.as_deref(), Some("fn main() {\n    let x = \"hi\";\n}\n"));
        assert!(!shown.cut);
        assert_eq!(shown.size, 32);
        let text = shown.text.as_deref().unwrap();
        let word = |kind: &str| -> Vec<&str> {
            shown.spans.iter().filter(|s| s.2 == kind).map(|s| &text[s.0 as usize..s.1 as usize]).collect()
        };
        assert!(word("keyword").contains(&"fn"), "{:?}", shown.spans);
        assert!(word("string").iter().any(|s| s.contains("hi")), "{:?}", shown.spans);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn markdown_goes_plain_for_the_client_to_lay_out() {
        let dir = scratch("md");
        std::fs::write(dir.join("notes.md"), "# Title\n\nSome *words*.\n").unwrap();
        let shown = open(dir.join("notes.md").to_str().unwrap(), None);
        assert_eq!(shown.language.as_deref(), Some("markdown"));
        assert!(shown.spans.is_empty());
        assert_eq!(shown.text.as_deref(), Some("# Title\n\nSome *words*.\n"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_long_file_is_cut_between_characters() {
        let dir = scratch("long");
        // Three bytes each: the cut lands inside one unless it is moved.
        let text = "汉".repeat(FILE_TEXT_SHOWN / 3 + 10);
        std::fs::write(dir.join("long.txt"), &text).unwrap();
        let shown = open(dir.join("long.txt").to_str().unwrap(), None);
        assert!(shown.cut);
        let got = shown.text.unwrap();
        assert!(got.len() <= FILE_TEXT_SHOWN && got.chars().all(|c| c == '汉'), "{}", got.len());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn pictures_come_as_pictures_and_others_say_why_not() {
        let dir = scratch("other");
        let picture = image::RgbImage::from_pixel(3000, 1000, image::Rgb([200, 10, 10]));
        picture.save(dir.join("wide.png")).unwrap();
        let shown = open(dir.join("wide.png").to_str().unwrap(), None);
        assert_eq!(shown.error, None);
        let bytes = base64::engine::general_purpose::STANDARD.decode(shown.picture.unwrap()).unwrap();
        let back = image::load_from_memory(&bytes).unwrap();
        assert_eq!((back.width(), back.height()), (2048, 683));

        std::fs::write(dir.join("blob.bin"), [1u8, 0, 2, 3]).unwrap();
        assert_eq!(open(dir.join("blob.bin").to_str().unwrap(), None).error.as_deref(), Some("binary"));
        assert_eq!(open(dir.to_str().unwrap(), None).error.as_deref(), Some("folder"));
        assert_eq!(open("nothing-here.txt", Some(&dir)).error.as_deref(), Some("missing"));
        // Text that begins as a weak picture mark does is text.
        std::fs::write(dir.join("plan.txt"), "P1: ship the fix\nBM later\n").unwrap();
        assert_eq!(open("plan.txt", Some(&dir)).text.as_deref(), Some("P1: ship the fix\nBM later\n"));
        // A pipe is not waited on: opening one with no writer never ends.
        let pipe = dir.join("pipe");
        assert!(std::process::Command::new("mkfifo").arg(&pipe).status().unwrap().success());
        assert_eq!(open(pipe.to_str().unwrap(), None).error.as_deref(), Some("unreadable"));
        // A name is taken as it is, its spaces too.
        std::fs::write(dir.join("report "), "with a space").unwrap();
        std::fs::write(dir.join("report"), "without").unwrap();
        assert_eq!(open("report ", Some(&dir)).text.as_deref(), Some("with a space"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_detailed_picture_still_fits_a_line() {
        // Noise compresses worst: at quality 85 a JPEG of it is far past
        // what is sent.
        let mut seed = 7u32;
        let noise = image::RgbImage::from_fn(2048, 2048, |_, _| {
            seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            image::Rgb([(seed >> 8) as u8, (seed >> 16) as u8, (seed >> 24) as u8])
        });
        let mut png = std::io::Cursor::new(Vec::new());
        noise.write_to(&mut png, image::ImageFormat::Png).unwrap();
        let sent = picture(png.get_ref(), image::ImageFormat::Png).expect("a picture");
        assert!(sent.len() * 3 / 4 <= PICTURE_SENT_LARGEST, "{}", sent.len());
    }
}
