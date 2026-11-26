//! Small pictures of the pictures sent in a chat, or looked at in it, for
//! a phone to show beside what was said and done: made here, beside the
//! files, so that only they cross the connection.

use crate::{Attachment, Ev, Item};
use base64::Engine;
use serde_json::Value;
use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::sync::{Mutex, OnceLock};

/// The longest side of a small picture, in pixels.
const SIDE: u32 = 480;
/// Files larger than this are not opened to make one.
const LARGEST: u64 = 40 << 20;
/// The most small pictures added to one line: with the history the Pi
/// extension fits in 4 MiB, the line stays well under the 8 MiB a client
/// takes in. Older pictures past it go without one.
const ROOM_PER_LINE: usize = 2 << 20;

/// A file as it was when its small picture was made: when it was last
/// written, and its size.
type FileAsIs = (Option<std::time::SystemTime>, u64);
/// The small picture made of each file, by path, with the file as it was.
type MadeOf = HashMap<String, (FileAsIs, Option<String>)>;

/// A JPEG of the picture in the file at `path`, base64; none for what is
/// no picture, or cannot be read. Each is made once for the file as it is:
/// one written over since (a screenshot taken again under its name) is
/// made anew.
pub(crate) fn of_file(path: &str) -> Option<String> {
    // The latest of each file only: one written over a thousand times is
    // held once.
    static MADE: OnceLock<Mutex<MadeOf>> = OnceLock::new();
    if path.is_empty() {
        return None;
    }
    let meta = std::fs::metadata(path).ok().filter(|meta| meta.is_file() && meta.len() <= LARGEST)?;
    let as_is = (meta.modified().ok(), meta.len());
    let made = MADE.get_or_init(Default::default);
    if let Some((then, known)) = made.lock().ok()?.get(path) {
        if *then == as_is {
            return known.clone();
        }
    }
    let small = std::fs::read(path).ok().and_then(|bytes| of_bytes(&bytes));
    made.lock().ok()?.insert(path.to_string(), (as_is, small.clone()));
    small
}

/// A `data:` URL's picture.
pub(crate) fn of_data_url(url: &str) -> Option<String> {
    let (_, data) = url.strip_prefix("data:")?.split_once(";base64,")?;
    let bytes = base64::engine::general_purpose::STANDARD.decode(data.trim()).ok()?;
    of_bytes(&bytes)
}

fn of_bytes(bytes: &[u8]) -> Option<String> {
    let mut reader = image::ImageReader::new(std::io::Cursor::new(bytes)).with_guessed_format().ok()?;
    let mut limits = image::Limits::default();
    limits.max_image_width = Some(20_000);
    limits.max_image_height = Some(20_000);
    limits.max_alloc = Some(512 << 20);
    reader.limits(limits);
    let picture = reader.decode().ok()?;
    let small = picture.thumbnail(SIDE, SIDE).to_rgb8();
    let mut out = std::io::Cursor::new(Vec::new());
    small.write_to(&mut out, image::ImageFormat::Jpeg).ok()?;
    Some(base64::engine::general_purpose::STANDARD.encode(out.into_inner()))
}

/// The most pictures one call is told to have looked at.
const PICTURES_PER_CALL: usize = 8;
/// A picture in a call's result larger than this, base64, is not kept.
const PICTURE_DATA_LARGEST: usize = 30 << 20;

/// A picture file a call looked at, by where it is: its small picture is
/// made as it is sent ([`fill_line`]).
pub(crate) fn picture_file(path: &str) -> Attachment {
    Attachment {
        name: path.rsplit('/').next().unwrap_or(path).to_string(),
        image: true,
        path: Some(path.to_string()),
        thumb: None,
    }
}

/// Whether `path` names a picture, by its ending.
pub(crate) fn is_picture_path(path: &str) -> bool {
    let lower = path.to_ascii_lowercase();
    [".png", ".jpg", ".jpeg", ".gif", ".webp", ".bmp"].iter().any(|ext| lower.ends_with(ext))
}

/// The pictures in what a call gave back, in any of the shapes agents give
/// them: Anthropic's (`{type: image, source: {type: base64, media_type,
/// data}}`), MCP's and Pi's (`{type: image, data, mimeType}`), a file part
/// (`{type: file, mime, url}`, a `data:` or `file://` URL), an OpenAI
/// image (`{type: image_url, image_url: {url}}`). Each one only there as
/// data is kept as a file of its own ([`kept_picture`]), so that it can be
/// opened whole.
pub(crate) fn pictures_in(value: &Value) -> Vec<Attachment> {
    pictures_kept_in(value, &super::pi::chat_dir().join("pictures"))
}

fn pictures_kept_in(value: &Value, dir: &std::path::Path) -> Vec<Attachment> {
    let mut found = Vec::new();
    look_for_pictures(value, 0, dir, &mut found);
    found
}

fn look_for_pictures(value: &Value, depth: usize, dir: &std::path::Path, found: &mut Vec<Attachment>) {
    if depth > 8 || found.len() >= PICTURES_PER_CALL {
        return;
    }
    match value {
        Value::Array(items) => {
            for item in items {
                look_for_pictures(item, depth + 1, dir, found);
            }
        }
        Value::Object(map) => {
            if let Some(picture) = picture_part(value, dir) {
                found.push(picture);
                return;
            }
            for item in map.values().filter(|v| v.is_array() || v.is_object()) {
                look_for_pictures(item, depth + 1, dir, found);
            }
        }
        _ => {}
    }
}

/// One part that is a picture, as a picture kept where it can be opened.
fn picture_part(part: &Value, dir: &std::path::Path) -> Option<Attachment> {
    let kind = part["type"].as_str()?;
    let mime_of = |key: &str| part[key].as_str().unwrap_or_default().to_string();
    let (mime, data, url) = match kind {
        "image" if part["source"]["type"] == "base64" => (
            part["source"]["media_type"].as_str().unwrap_or("image/png").to_string(),
            part["source"]["data"].as_str(),
            None,
        ),
        "image" if part["data"].is_string() => {
            let mime = [mime_of("mimeType"), mime_of("mime_type"), mime_of("mime")]
                .into_iter()
                .find(|m| !m.is_empty())
                .unwrap_or_else(|| "image/png".into());
            (mime, part["data"].as_str(), None)
        }
        "image" | "file" | "image_url" | "input_image" | "inputImage" => {
            let mime = mime_of("mime");
            if kind == "file" && !mime.starts_with("image/") {
                return None;
            }
            let url = part["url"]
                .as_str()
                .or(part["image_url"]["url"].as_str())
                .or(part["image_url"].as_str())
                .or(part["imageUrl"].as_str())
                .or(part["source"]["uri"].as_str())?;
            (mime, None, Some(url))
        }
        _ => return None,
    };
    if let Some(url) = url {
        if let Some(path) = url.strip_prefix("file://") {
            return Some(picture_file(&super::opencode::percent_decoded(path)));
        }
        let (head, data) = url.strip_prefix("data:")?.split_once(";base64,")?;
        let mime = if head.starts_with("image/") { head.to_string() } else { mime };
        return kept_picture(dir, &mime, data).map(|path| picture_file(&path));
    }
    kept_picture(dir, &mime, data?).map(|path| picture_file(&path))
}

/// A picture that came only as data, kept as a file in `dir` (the chats'
/// own pictures, where the Pi extension keeps those of its sessions, and
/// lets go of those not looked at in a month): named by what it holds,
/// written once.
fn kept_picture(dir: &std::path::Path, mime: &str, data: &str) -> Option<String> {
    if data.len() > PICTURE_DATA_LARGEST {
        return None;
    }
    let bytes = base64::engine::general_purpose::STANDARD.decode(data.trim()).ok()?;
    let format = image::guess_format(&bytes).ok()?;
    let ext = format.extensions_str().first().copied().unwrap_or_else(|| mime.rsplit('/').next().unwrap_or("png"));
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    bytes.hash(&mut hasher);
    let file = dir.join(format!("{:016x}.{ext}", hasher.finish()));
    if file.exists() {
        // Looked at again: kept a while longer.
        let _ = std::fs::File::options()
            .write(true)
            .open(&file)
            .and_then(|f| f.set_modified(std::time::SystemTime::now()));
    } else {
        create_private_dir(dir).ok()?;
        write_private(&file, &bytes).ok()?;
        let_go_of_old(dir);
    }
    Some(file.to_string_lossy().into_owned())
}

/// How long a kept picture stays not looked at: as the Pi extension keeps
/// those of its sessions, in the same folder.
const PICTURES_KEPT: std::time::Duration = std::time::Duration::from_secs(30 * 24 * 3600);

/// The pictures in `dir` not looked at for [`PICTURES_KEPT`], let go: once
/// a run, where a picture is first kept (a host where Pi never runs has no
/// one else to do it).
fn let_go_of_old(dir: &std::path::Path) {
    static DONE: OnceLock<()> = OnceLock::new();
    if DONE.set(()).is_err() {
        return;
    }
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let now = std::time::SystemTime::now();
    for entry in entries.flatten() {
        let old = entry
            .metadata()
            .ok()
            .filter(|meta| meta.is_file())
            .and_then(|meta| meta.modified().ok())
            .and_then(|at| now.duration_since(at).ok())
            .is_some_and(|age| age > PICTURES_KEPT);
        if old {
            let _ = std::fs::remove_file(entry.path());
        }
    }
}

#[cfg(unix)]
fn create_private_dir(dir: &std::path::Path) -> std::io::Result<()> {
    use std::os::unix::fs::DirBuilderExt;
    std::fs::DirBuilder::new().recursive(true).mode(0o700).create(dir)
}

#[cfg(unix)]
fn write_private(file: &std::path::Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    // Written aside and moved in whole: a reader never finds half of it.
    let part = file.with_extension(format!("part-{}", std::process::id()));
    let mut out = std::fs::OpenOptions::new().write(true).create(true).truncate(true).mode(0o600).open(&part)?;
    out.write_all(bytes)?;
    drop(out);
    std::fs::rename(&part, file)
}

/// Small pictures for the pictures in what was said and done, where a line
/// names their files: a bridge's own items, and the Pi extension's, which
/// leaves the pictures it was sent as files. Any other line goes as it
/// came.
pub(crate) fn fill_line(line: &str) -> String {
    fill_within(line, ROOM_PER_LINE)
}

/// `fill_line`, the newest pictures first, until `room` bytes of small
/// pictures are in.
fn fill_within(line: &str, mut room: usize) -> String {
    if !line.contains("\"attachments\"") && !line.contains("\"pictures\"") {
        return line.to_string();
    }
    let Ok(mut ev) = serde_json::from_str::<Ev>(line) else {
        return line.to_string();
    };
    let items: &mut [Item] = match &mut ev {
        Ev::History { items, .. } => items,
        Ev::Item { item, .. } => std::slice::from_mut(item),
        _ => return line.to_string(),
    };
    'items: for item in items.iter_mut().rev() {
        let pictures = match item {
            Item::User { attachments, .. } => attachments,
            Item::Command { pictures, .. } | Item::Tool { pictures, .. } => pictures,
            _ => continue,
        };
        for picture in pictures.iter_mut().rev().filter(|a| a.image && a.thumb.is_none()) {
            let Some(small) = picture.path.as_deref().and_then(of_file) else {
                continue;
            };
            if small.len() > room {
                break 'items;
            }
            room -= small.len();
            picture.thumb = Some(small);
        }
    }
    serde_json::to_string(&ev).unwrap_or_else(|_| line.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_picture_gets_a_small_one_and_anything_else_none() {
        let dir = tempfile::tempdir().unwrap();
        let picture = dir.path().join("shot.png");
        image::RgbImage::from_pixel(1200, 600, image::Rgb([200, 30, 30])).save(&picture).unwrap();
        let text = dir.path().join("notes.txt");
        std::fs::write(&text, "not a picture").unwrap();

        let small = of_file(picture.to_str().unwrap()).expect("a small picture");
        let bytes = base64::engine::general_purpose::STANDARD.decode(small).unwrap();
        let back = image::load_from_memory(&bytes).unwrap();
        assert_eq!((back.width(), back.height()), (SIDE, SIDE / 2));
        assert!(of_file(text.to_str().unwrap()).is_none());
        assert!(of_file("/no/such/file.png").is_none());

        let line = format!(
            r#"{{"ev":"item","item":{{"kind":"user","id":"u","text":"look","attachments":[{{"name":"shot.png","image":true,"path":"{}"}}]}}}}"#,
            picture.display()
        );
        let filled: Ev = serde_json::from_str(&fill_line(&line)).unwrap();
        let Ev::Item {
            item: Item::User { attachments, .. },
            ..
        } = filled
        else {
            panic!("not a user item");
        };
        assert!(attachments[0].thumb.is_some());
        assert_eq!(fill_line(r#"{"ev":"state","state":"idle"}"#), r#"{"ev":"state","state":"idle"}"#);

        // Past the room a line has, the older pictures go without.
        let one = of_file(picture.to_str().unwrap()).unwrap().len();
        let user = |id: &str| {
            format!(
                r#"{{"kind":"user","id":"{id}","text":"","attachments":[{{"name":"a.png","image":true,"path":"{}"}}]}}"#,
                picture.display()
            )
        };
        let history = format!(r#"{{"ev":"history","items":[{},{}],"more":false,"reset":true}}"#, user("old"), user("new"));
        let Ev::History { items, .. } = serde_json::from_str(&fill_within(&history, one)).unwrap() else {
            panic!("not history");
        };
        let thumbs: Vec<bool> = items
            .iter()
            .map(|item| matches!(item, Item::User { attachments, .. } if attachments[0].thumb.is_some()))
            .collect();
        assert_eq!(thumbs, [false, true], "the newest has its picture");
    }

    #[test]
    fn pictures_a_call_gave_back_are_found_in_every_shape() {
        let mut png = std::io::Cursor::new(Vec::new());
        image::RgbImage::from_pixel(40, 20, image::Rgb([1, 2, 3])).write_to(&mut png, image::ImageFormat::Png).unwrap();
        let data = base64::engine::general_purpose::STANDARD.encode(png.into_inner());
        let result = serde_json::json!([
            {"type": "text", "text": "a screenshot"},
            {"type": "image", "source": {"type": "base64", "media_type": "image/png", "data": data}},
            {"type": "tool_result", "content": [{"type": "image", "data": data, "mimeType": "image/png"}]},
            {"type": "file", "mime": "image/png", "url": format!("data:image/png;base64,{data}")},
            {"type": "file", "mime": "text/plain", "url": "file:///w/notes.txt"},
            {"type": "image_url", "image_url": {"url": "file:///w/shot.png"}},
        ]);
        let dir = tempfile::tempdir().unwrap();
        let found = pictures_kept_in(&result, dir.path());
        let paths: Vec<&str> = found.iter().map(|p| p.path.as_deref().unwrap()).collect();
        assert_eq!(paths.len(), 4, "{paths:?}");
        // The same picture three times over is one file.
        assert_eq!(paths[0], paths[1]);
        assert_eq!(paths[1], paths[2]);
        assert!(paths[0].ends_with(".png") && paths[0].starts_with(dir.path().to_str().unwrap()), "{}", paths[0]);
        assert_eq!(paths[3], "/w/shot.png");
        assert!(found.iter().all(|p| p.image && p.thumb.is_none()));
        let kept = std::fs::read(paths[0]).unwrap();
        assert!(image::load_from_memory(&kept).is_ok());

        // A call's pictures get small ones as they go, as what is said does.
        let item = Item::Tool {
            id: "t".into(),
            name: "screenshot".into(),
            detail: String::new(),
            status: crate::Status::Done,
            pictures: found,
        };
        let line = serde_json::to_string(&Ev::item(item)).unwrap();
        let Ev::Item { item: Item::Tool { pictures, .. }, .. } = serde_json::from_str(&fill_line(&line)).unwrap() else {
            panic!("not a tool");
        };
        assert!(pictures[0].thumb.is_some() && pictures[3].thumb.is_none());
    }
}
