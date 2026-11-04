#![cfg_attr(not(windows), forbid(unsafe_code))]
#![cfg_attr(windows, deny(unsafe_code))]

use serde::Deserialize;
use std::io::{BufRead, Read};

// Not every platform's window reads every field.
#[allow(dead_code)]
#[derive(Deserialize)]
#[serde(rename_all = "PascalCase", deny_unknown_fields)]
struct Intent {
    id: String,
    source: String,
    targets: String,
    input: bool,
    seconds: u64,
    text: Text,
}

/// What the window says, already in the user's language.
#[allow(dead_code)]
#[derive(Deserialize)]
#[serde(rename_all = "PascalCase", deny_unknown_fields)]
struct Text {
    title: String,
    heading: String,
    connection: String,
    terminals: String,
    detail: String,
    approve: String,
    cancel: String,
    reason: String,
    password: String,
}

impl Intent {
    #[allow(dead_code)]
    fn read(reader: &mut impl BufRead) -> Option<Self> {
        let mut line = Vec::new();
        reader.take(8192).read_until(b'\n', &mut line).ok()?;
        if line.len() >= 8192 || line.last() != Some(&b'\n') {
            return None;
        }
        let value: Self = serde_json::from_slice(&line).ok()?;
        let text_valid = |text: &str, limit, multiline| {
            !text.is_empty()
                && text.len() <= limit
                && !text
                    .chars()
                    .any(|c| c.is_control() && !(multiline && c == '\n'))
        };
        if value.id.len() != 32
            || !value.id.bytes().all(|byte| byte.is_ascii_hexdigit())
            || value.seconds == 0
            || value.seconds > 7200
            || !text_valid(&value.source, 256, false)
            || !text_valid(&value.targets, 4096, true)
            || ![
                &value.text.title,
                &value.text.heading,
                &value.text.connection,
                &value.text.terminals,
                &value.text.approve,
                &value.text.cancel,
                &value.text.reason,
                &value.text.password,
            ]
            .iter()
            .all(|text| text_valid(text, 512, false))
            || !text_valid(&value.text.detail, 1024, true)
        {
            return None;
        }
        Some(value)
    }
}

#[cfg(any(target_os = "linux", windows))]
fn emit(kind: &str, id: Option<&str>) -> std::io::Result<()> {
    use std::io::Write;
    let mut out = std::io::stdout().lock();
    serde_json::to_writer(&mut out, &serde_json::json!({ "kind": kind, "id": id }))?;
    out.write_all(b"\n")?;
    out.flush()
}

#[cfg(target_os = "linux")]
mod linux;
#[cfg(windows)]
#[allow(unsafe_code)]
mod win;

fn main() {
    #[cfg(target_os = "linux")]
    let result = linux::run();
    #[cfg(windows)]
    let result = win::run();
    #[cfg(any(target_os = "linux", windows))]
    if result.is_ok() {
        return;
    }
    // macOS packages its native platform implementation.
    std::process::exit(2);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn native_intent_rejects_controls_extra_fields_and_large_input() {
        let request = serde_json::json!({
            "Id": "0123456789abcdef0123456789abcdef", "Source": "server-a",
            "Targets": "21 [example]\n22 [example]", "Input": true, "Seconds": 1800,
            "Text": {
                "Title": "title", "Heading": "heading", "Connection": "Connection: server-a",
                "Terminals": "Terminals:", "Detail": "Access: example\nDuration: 30 minutes",
                "Approve": "Allow", "Cancel": "Cancel", "Reason": "reason", "Password": "password",
            },
        });
        let read = |value: &serde_json::Value| {
            let line = format!("{value}\n");
            Intent::read(&mut line.as_bytes())
        };
        let parsed = read(&request).unwrap();
        assert!(parsed.input);
        assert_eq!(parsed.seconds, 1800);
        let mut changed = request.clone();
        changed["Source"] = "server-a\nRead only".into();
        assert!(read(&changed).is_none());
        changed = request.clone();
        changed["Success"] = true.into();
        assert!(read(&changed).is_none());
        changed = request.clone();
        changed["Seconds"] = 0.into();
        assert!(read(&changed).is_none());
        changed = request;
        changed["Text"]["Heading"] = "heading\nRead only".into();
        assert!(read(&changed).is_none());
        assert!(Intent::read(&mut vec![b' '; 8192].as_slice()).is_none());
    }
}
