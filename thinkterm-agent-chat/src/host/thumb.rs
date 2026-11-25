//! Small pictures of the pictures sent in a chat, for a phone to show
//! beside what was said: made here, beside the files, so that only they
//! cross the connection.

use crate::{Ev, Item};
use base64::Engine;
use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};

/// The longest side of a small picture, in pixels.
const SIDE: u32 = 480;
/// Files larger than this are not opened to make one.
const LARGEST: u64 = 40 << 20;
/// The most small pictures added to one line: with the history the Pi
/// extension fits in 4 MiB, the line stays well under the 8 MiB a client
/// takes in. Older pictures past it go without one.
const ROOM_PER_LINE: usize = 2 << 20;

/// A JPEG of the picture in the file at `path`, base64; none for what is
/// no picture, or cannot be read. Each is made once.
pub(crate) fn of_file(path: &str) -> Option<String> {
    static MADE: OnceLock<Mutex<HashMap<String, Option<String>>>> = OnceLock::new();
    if path.is_empty() {
        return None;
    }
    let made = MADE.get_or_init(Default::default);
    if let Some(known) = made.lock().ok()?.get(path) {
        return known.clone();
    }
    let small = std::fs::metadata(path)
        .ok()
        .filter(|meta| meta.is_file() && meta.len() <= LARGEST)
        .and_then(|_| std::fs::read(path).ok())
        .and_then(|bytes| of_bytes(&bytes));
    made.lock().ok()?.insert(path.to_string(), small.clone());
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

/// Small pictures for the pictures in what was said, where a line names
/// their files: the Pi extension's, which leaves the pictures it was
/// sent as files. Any other line goes as it came.
pub(crate) fn fill_line(line: &str) -> String {
    fill_within(line, ROOM_PER_LINE)
}

/// `fill_line`, the newest pictures first, until `room` bytes of small
/// pictures are in.
fn fill_within(line: &str, mut room: usize) -> String {
    if !line.contains("\"attachments\"") {
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
        if let Item::User { attachments, .. } = item {
            for picture in attachments.iter_mut().rev().filter(|a| a.image && a.thumb.is_none()) {
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
}
