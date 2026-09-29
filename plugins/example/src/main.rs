//! An example ThinkTerm plugin: two calls, in one small program. To make
//! one of your own, copy this directory, change the id in plugin.toml and
//! the name in Cargo.toml, and change what the calls do.
//! docs/thinkterm/plugins.md says how to install it.
//!
//! - `{"op": "decode", "text": "…"}` answers with what the text decodes
//!   to, or `null`:
//!   `thinkterm plugin call text-tools '{"op":"decode","text":"aGk="}'`.
//! - `{"op": "uuid"}` answers with a new UUID.

use anyhow::{bail, Context as _};
use base64::alphabet;
use base64::engine::general_purpose::{GeneralPurpose, GeneralPurposeConfig};
use base64::engine::DecodePaddingMode;
use base64::Engine as _;
use serde_json::{json, Value};
use thinkterm_plugin_sdk::{Cx, Plugin};

struct TextTools;

impl Plugin for TextTools {
    fn call(&mut self, body: Value, _cx: &mut Cx) -> anyhow::Result<Value> {
        match body["op"].as_str() {
            Some("decode") => {
                let text = body["text"]
                    .as_str()
                    .context("\"decode\" needs a \"text\"")?;
                Ok(match decode(text.trim()) {
                    Some(decoded) => json!({"kind": decoded.kind, "text": decoded.text}),
                    None => Value::Null,
                })
            }
            Some("uuid") => Ok(json!(uuid::Uuid::new_v4().to_string())),
            _ => bail!("unknown op; this plugin knows \"decode\" and \"uuid\""),
        }
    }
}

fn main() -> std::io::Result<()> {
    thinkterm_plugin_sdk::run(TextTools)
}

#[derive(Debug, PartialEq)]
struct Decoded {
    kind: &'static str,
    text: String,
}

/// What `text` decodes to, trying a JWT, a Unix time, URL encoding and
/// Base64 in that order.
fn decode(text: &str) -> Option<Decoded> {
    if text.is_empty() {
        return None;
    }
    jwt(text)
        .or_else(|| unix_time(text))
        .or_else(|| percent(text))
        .or_else(|| base64_text(text))
}

/// Base64 in either alphabet, padded or not.
fn base64_bytes(text: &str) -> Option<Vec<u8>> {
    let config =
        GeneralPurposeConfig::new().with_decode_padding_mode(DecodePaddingMode::Indifferent);
    GeneralPurpose::new(&alphabet::STANDARD, config)
        .decode(text)
        .or_else(|_| GeneralPurpose::new(&alphabet::URL_SAFE, config).decode(text))
        .ok()
}

/// A JSON Web Token: its header and payload, the signature left out.
fn jwt(text: &str) -> Option<Decoded> {
    let mut parts = text.split('.');
    let (header, payload, _signature) = (parts.next()?, parts.next()?, parts.next()?);
    if parts.next().is_some() {
        return None;
    }
    let object = |part: &str| {
        serde_json::from_slice::<Value>(&base64_bytes(part)?)
            .ok()
            .filter(Value::is_object)
    };
    let pretty = |value: &Value| serde_json::to_string_pretty(value).unwrap_or_default();
    Some(Decoded {
        kind: "JWT",
        text: format!(
            "{}\n{}",
            pretty(&object(header)?),
            pretty(&object(payload)?)
        ),
    })
}

/// Seconds (ten digits) or milliseconds (thirteen) since 1970, as a date.
fn unix_time(text: &str) -> Option<Decoded> {
    if !text.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    let seconds: i64 = match text.len() {
        10 => text.parse().ok()?,
        13 => text.parse::<i64>().ok()? / 1000,
        _ => return None,
    };
    let (days, time) = (seconds.div_euclid(86_400), seconds.rem_euclid(86_400));
    let (year, month, day) = civil_from_days(days);
    Some(Decoded {
        kind: "Unix time",
        text: format!(
            "{year:04}-{month:02}-{day:02} {:02}:{:02}:{:02} UTC",
            time / 3600,
            time / 60 % 60,
            time % 60
        ),
    })
}

/// The date `days` after 1970-01-01, by Howard Hinnant's `civil_from_days`.
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let day_of_era = z.rem_euclid(146_097);
    let year_of_era =
        (day_of_era - day_of_era / 1460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let shifted_month = (5 * day_of_year + 2) / 153;
    let day = (day_of_year - (153 * shifted_month + 2) / 5 + 1) as u32;
    let month = if shifted_month < 10 {
        shifted_month + 3
    } else {
        shifted_month - 9
    } as u32;
    (year_of_era + era * 400 + i64::from(month <= 2), month, day)
}

/// Text with `%XX` escapes in it, unescaped.
fn percent(text: &str) -> Option<Decoded> {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut escaped = false;
    let mut at = 0;
    while at < bytes.len() {
        let hex = bytes
            .get(at + 1..at + 3)
            .filter(|digits| bytes[at] == b'%' && digits.iter().all(u8::is_ascii_hexdigit));
        match hex {
            Some(digits) => {
                let digits = std::str::from_utf8(digits).ok()?;
                out.push(u8::from_str_radix(digits, 16).ok()?);
                escaped = true;
                at += 3;
            }
            None => {
                out.push(bytes[at]);
                at += 1;
            }
        }
    }
    if !escaped {
        return None;
    }
    Some(Decoded {
        kind: "URL encoding",
        text: String::from_utf8(out).ok()?,
    })
}

/// Base64 that decodes to readable text.
fn base64_text(text: &str) -> Option<Decoded> {
    let compact: String = text.split_ascii_whitespace().collect();
    if compact.len() < 4 {
        return None;
    }
    let decoded = String::from_utf8(base64_bytes(&compact)?).ok()?;
    if decoded
        .chars()
        .any(|c| c.is_control() && !matches!(c, '\n' | '\r' | '\t'))
    {
        return None;
    }
    Some(Decoded {
        kind: "Base64",
        text: decoded,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::engine::general_purpose::URL_SAFE_NO_PAD;

    fn decoded(text: &str) -> (&'static str, String) {
        let found = decode(text).unwrap_or_else(|| panic!("{text:?} did not decode"));
        (found.kind, found.text)
    }

    #[test]
    fn each_kind_decodes() {
        assert_eq!(
            decoded("aGVsbG8gd29ybGQ="),
            ("Base64", "hello world".into())
        );
        assert_eq!(decoded("aGVsbG8gd29ybGQ"), ("Base64", "hello world".into()));
        assert_eq!(decoded("a%20b%2Fc"), ("URL encoding", "a b/c".into()));
        assert_eq!(
            decoded("1700000000"),
            ("Unix time", "2023-11-14 22:13:20 UTC".into())
        );
        assert_eq!(
            decoded("1700000000123"),
            ("Unix time", "2023-11-14 22:13:20 UTC".into())
        );
        let token = format!(
            "{}.{}.signature",
            URL_SAFE_NO_PAD.encode(r#"{"alg":"HS256"}"#),
            URL_SAFE_NO_PAD.encode(r#"{"sub":"example"}"#)
        );
        let (kind, text) = decoded(&token);
        assert_eq!(kind, "JWT");
        assert!(text.contains("\"alg\": \"HS256\"") && text.contains("\"sub\": \"example\""));
    }

    #[test]
    fn what_is_not_encoded_is_not_decoded() {
        for text in ["", "test", "hello world", "12345", "100%", "%zz", "a.b.c"] {
            assert_eq!(decode(text), None, "{text:?}");
        }
    }

    #[test]
    fn the_calls() {
        let mut plugin = TextTools;
        let mut cx = Cx::new();
        assert_eq!(
            plugin
                .call(json!({"op": "decode", "text": " aGk= \n"}), &mut cx)
                .unwrap(),
            json!({"kind": "Base64", "text": "hi"})
        );
        assert_eq!(
            plugin
                .call(json!({"op": "decode", "text": ""}), &mut cx)
                .unwrap(),
            Value::Null
        );
        let made = plugin.call(json!({"op": "uuid"}), &mut cx).unwrap();
        let made = uuid::Uuid::parse_str(made.as_str().unwrap()).unwrap();
        assert_eq!(made.get_version_num(), 4);
        assert!(plugin.call(json!({"op": "nope"}), &mut cx).is_err());
    }
}
