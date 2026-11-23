//! How full a Codex thread's context is, read from the file Codex keeps of
//! the thread (its rollout: one JSON record a line, appended as the thread
//! goes on). What the model is sent is counted by what it is -- Codex's
//! instructions, its tools, what was said, its tool calls -- and scaled to
//! the total Codex last reported, which the file holds too.

use crate::{ContextInfo, ContextPart, PartKind, Usage};
use serde_json::Value;
use std::collections::HashMap;
use std::io::{BufRead, BufReader, Seek, SeekFrom};
use std::path::PathBuf;

/// What a picture is reckoned to take of the context, in tokens.
const PICTURE_TOKENS: f64 = 1200.0;

pub(crate) struct Rollout {
    /// None until Codex says where the file is (a thread whose first turn
    /// has not run has none yet).
    path: Option<PathBuf>,
    /// How far the file has been read: lines whole only.
    offset: u64,
    /// What each kind takes, counted, since the start or the last
    /// compaction.
    parts: HashMap<PartKind, f64>,
    /// What Codex said of where it runs, as last said in full: each new
    /// one stands for the last.
    world: f64,
    used: Option<u64>,
    window: Option<u64>,
    totals: Usage,
}

impl Rollout {
    pub(crate) fn new(path: Option<PathBuf>) -> Self {
        Self {
            path,
            offset: 0,
            parts: HashMap::new(),
            world: 0.0,
            used: None,
            window: None,
            totals: Usage::default(),
        }
    }

    /// Where the file is, once Codex says (on joining a thread that had
    /// none when the chat opened).
    pub(crate) fn found(&mut self, path: PathBuf) {
        if self.path.is_none() {
            self.path = Some(path);
        }
    }

    /// Reads what was added to the file since the last read.
    pub(crate) fn read_on(&mut self) {
        let Some(path) = &self.path else {
            return;
        };
        let Ok(mut file) = std::fs::File::open(path) else {
            return;
        };
        if file.seek(SeekFrom::Start(self.offset)).is_err() {
            return;
        }
        let mut reader = BufReader::new(file);
        let mut line = Vec::new();
        loop {
            line.clear();
            match reader.read_until(b'\n', &mut line) {
                // A line still being written waits for the next read.
                Ok(n) if n > 0 && line.last() == Some(&b'\n') => {
                    self.offset += n as u64;
                    if let Ok(record) = serde_json::from_slice::<Value>(&line) {
                        self.take(&record);
                    }
                }
                _ => break,
            }
        }
    }

    /// The total Codex reported as a turn went on (`thread/tokenUsage/updated`).
    pub(crate) fn reported(&mut self, usage: &Value) {
        if let Some(used) = usage["last"]["totalTokens"].as_u64() {
            self.used = Some(used);
        }
        if let Some(window) = usage["modelContextWindow"].as_u64() {
            self.window = Some(window);
        }
        let total = &usage["total"];
        self.totals = Usage {
            input: total["inputTokens"].as_u64(),
            output: total["outputTokens"].as_u64(),
            cache_read: total["cachedInputTokens"].as_u64(),
            cache_write: total["cacheWriteInputTokens"].as_u64().filter(|&n| n > 0),
            reasoning: total["reasoningOutputTokens"].as_u64(),
            cost: None,
        };
    }

    pub(crate) fn info(&self) -> ContextInfo {
        let mut counted: Vec<(PartKind, f64)> = self.parts.iter().map(|(kind, tokens)| (*kind, *tokens)).collect();
        if self.world > 0.0 {
            counted.push((PartKind::Instructions, self.world));
        }
        let sum: f64 = counted.iter().map(|(_, tokens)| tokens).sum();
        let scale = match self.used {
            Some(used) if sum > 0.0 => used as f64 / sum,
            _ => 1.0,
        };
        let mut parts: HashMap<PartKind, u64> = HashMap::new();
        for (kind, tokens) in counted {
            *parts.entry(kind).or_default() += (tokens * scale).round() as u64;
        }
        let mut parts: Vec<ContextPart> = parts
            .into_iter()
            .filter(|(_, tokens)| *tokens > 0)
            .map(|(kind, tokens)| ContextPart { kind, tokens })
            .collect();
        parts.sort_by_key(|part| std::cmp::Reverse(part.tokens));
        ContextInfo {
            used: self.used,
            window: self.window,
            parts,
            compacts_at: None,
            totals: self.totals.clone(),
        }
    }

    fn add(&mut self, kind: PartKind, tokens: f64) {
        if tokens > 0.0 {
            *self.parts.entry(kind).or_default() += tokens;
        }
    }

    fn take(&mut self, record: &Value) {
        let payload = &record["payload"];
        match record["type"].as_str() {
            Some("session_meta") => {
                let base = &payload["base_instructions"];
                let text = base.as_str().or_else(|| base["text"].as_str()).unwrap_or_default();
                self.add(PartKind::System, tokens_in(text));
                if let Some(window) = payload["context_window"].as_u64() {
                    self.window.get_or_insert(window);
                }
            }
            Some("response_item") => {
                if let Some((kind, tokens)) = item_tokens(payload) {
                    self.add(kind, tokens);
                }
            }
            Some("world_state") => self.world = tokens_in(&payload["state"].to_string()),
            // Earlier turns summed up: what the model is sent from now on
            // is what the compaction put in their place.
            Some("compacted") => {
                let kept: &[Value] = payload["replacement_history"].as_array().map_or(&[], Vec::as_slice);
                // The tools go on being sent; they are counted again only
                // where what replaces the history holds them.
                let tools_again = kept.iter().any(|item| item["type"].as_str() == Some("additional_tools"));
                self.parts
                    .retain(|kind, _| *kind == PartKind::System || (*kind == PartKind::Tools && !tools_again));
                for item in kept {
                    if let Some((kind, tokens)) = item_tokens(item) {
                        self.add(kind, tokens);
                    }
                }
                // The summary, where the history that replaces the old does
                // not hold it already.
                if kept.is_empty() {
                    self.add(PartKind::Summary, tokens_in(payload["message"].as_str().unwrap_or_default()));
                }
                // Not known again until the next answer.
                self.used = None;
            }
            Some("event_msg") if payload["type"].as_str() == Some("token_count") => {
                let info = &payload["info"];
                if let Some(used) = info["last_token_usage"]["total_tokens"].as_u64() {
                    self.used = Some(used);
                }
                if let Some(window) = info["model_context_window"].as_u64() {
                    self.window = Some(window);
                }
                let total = &info["total_token_usage"];
                if total.is_object() {
                    self.totals = Usage {
                        input: total["input_tokens"].as_u64(),
                        output: total["output_tokens"].as_u64(),
                        cache_read: total["cached_input_tokens"].as_u64(),
                        cache_write: total["cache_write_input_tokens"].as_u64().filter(|&n| n > 0),
                        reasoning: total["reasoning_output_tokens"].as_u64(),
                        cost: None,
                    };
                }
            }
            _ => {}
        }
    }
}

/// What one item the model is sent takes, and what it is.
fn item_tokens(item: &Value) -> Option<(PartKind, f64)> {
    let kind = match item["type"].as_str()? {
        "message" => match item["role"].as_str() {
            Some("developer") | Some("system") => PartKind::Instructions,
            Some("assistant") => PartKind::Assistant,
            _ if told(item) => PartKind::Instructions,
            _ => PartKind::User,
        },
        "agent_message" => PartKind::Assistant,
        // Past turns' thinking goes as Codex's sealed state, which the
        // model drops; only its summary is counted.
        "reasoning" => {
            let summary: String = item["summary"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|part| part["text"].as_str())
                .collect();
            return Some((PartKind::Thinking, tokens_in(&summary)));
        }
        "additional_tools" => return Some((PartKind::Tools, tokens_in(&item["tools"].to_string()))),
        "compaction" => return Some((PartKind::Summary, tokens_in(&item.to_string()))),
        // What the model sees of a call: its name and what it was given,
        // or what came of it.
        kind if kind.ends_with("_call") || kind.ends_with("_output") => {
            let tokens = ["name", "arguments", "input", "action", "output"]
                .iter()
                .map(|field| match &item[*field] {
                    Value::Null => 0.0,
                    Value::String(text) => tokens_in(text),
                    other => tokens_in(&other.to_string()),
                })
                .sum();
            return Some((PartKind::ToolUse, tokens));
        }
        _ => return None,
    };
    let mut tokens = 0.0;
    match &item["content"] {
        Value::String(text) => tokens += tokens_in(text),
        Value::Array(parts) => {
            for part in parts {
                if part["type"].as_str().is_some_and(|t| t.contains("image")) {
                    tokens += PICTURE_TOKENS;
                } else if let Some(text) = part["text"].as_str() {
                    tokens += tokens_in(text);
                }
            }
        }
        _ => {}
    }
    Some((kind, tokens))
}

/// A user message Codex wrote itself: what it is told of where it runs
/// and the project's instructions, wrapped in its tags.
fn told(item: &Value) -> bool {
    let first = item["content"][0]["text"].as_str().unwrap_or_default().trim_start();
    ["<environment_context", "<user_instructions", "<permissions", "<skills_instructions", "# AGENTS.md"]
        .iter()
        .any(|tag| first.starts_with(tag))
}

/// Roughly what `text` takes of a model's context: a token for four
/// characters, one for each of a wide script's.
fn tokens_in(text: &str) -> f64 {
    let (wide, other) = text.chars().fold((0usize, 0usize), |(wide, other), c| {
        if c as u32 >= 0x2E80 {
            (wide + 1, other)
        } else {
            (wide, other + 1)
        }
    });
    wide as f64 + other as f64 / 4.0
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn line(value: Value) -> String {
        format!("{value}\n")
    }

    #[test]
    fn a_rollout_is_read_as_it_grows_and_scaled_to_the_reported_total() {
        let dir = std::env::temp_dir().join(format!("tt-rollout-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("rollout.jsonl");
        let mut file = std::fs::File::create(&path).unwrap();
        file.write_all(
            (line(serde_json::json!({ "type": "session_meta", "payload": { "base_instructions": { "text": "x".repeat(4000) } } }))
                + &line(serde_json::json!({ "type": "response_item", "payload": { "type": "message", "role": "user",
                    "content": [{ "type": "input_text", "text": "<environment_context>here</environment_context>" }] } }))
                + &line(serde_json::json!({ "type": "response_item", "payload": { "type": "message", "role": "user",
                    "content": [{ "type": "input_text", "text": "y".repeat(2000) }] } }))
                + &line(serde_json::json!({ "type": "response_item", "payload": { "type": "additional_tools", "role": "developer",
                    "tools": [{ "name": "t".repeat(400) }] } })))
            .as_bytes(),
        )
        .unwrap();
        let mut rollout = Rollout::new(Some(path.clone()));
        rollout.read_on();
        let info = rollout.info();
        assert_eq!(info.used, None);
        let tokens = |info: &ContextInfo, kind| info.parts.iter().find(|p| p.kind == kind).map_or(0, |p| p.tokens);
        assert_eq!(tokens(&info, PartKind::System), 1000);
        assert_eq!(tokens(&info, PartKind::User), 500);
        assert!(tokens(&info, PartKind::Instructions) > 0, "Codex's own tags are its instructions");

        // A half-written line waits; the total reported scales the parts.
        file.write_all(
            (line(serde_json::json!({ "type": "event_msg", "payload": { "type": "token_count", "info": {
                "last_token_usage": { "total_tokens": 3000 }, "model_context_window": 200000,
                "total_token_usage": { "input_tokens": 9000, "cached_input_tokens": 4000, "output_tokens": 300,
                    "reasoning_output_tokens": 0, "cache_write_input_tokens": 0 } } } }))
                + "{\"type\":\"response_item\"")
            .as_bytes(),
        )
        .unwrap();
        rollout.read_on();
        let info = rollout.info();
        assert_eq!((info.used, info.window), (Some(3000), Some(200000)));
        assert_eq!(info.parts.iter().map(|p| p.tokens).sum::<u64>().abs_diff(3000) <= 2, true);
        assert_eq!(info.totals.cache_read, Some(4000));
        assert_eq!(info.totals.cache_write, None);

        // A compaction keeps Codex's instructions and tools, and puts what
        // replaces the history (its summary within) in place of what was
        // said: the summary counted once.
        file.write_all(
            (",\"payload\":{}}\n".to_string()
                + &line(serde_json::json!({ "type": "compacted", "payload": { "message": "z".repeat(4000),
                    "replacement_history": [
                        { "type": "message", "role": "user", "content": [{ "type": "input_text", "text": "kept" }] },
                        { "type": "compaction", "encrypted_content": "s".repeat(400) }
                    ] } })))
            .as_bytes(),
        )
        .unwrap();
        rollout.read_on();
        let info = rollout.info();
        assert_eq!(info.used, None);
        let summary = tokens(&info, PartKind::Summary);
        assert!((100..1000).contains(&summary), "the summary once, from the history: {summary}");
        assert_eq!(tokens(&info, PartKind::User), 1);
        assert!(tokens(&info, PartKind::Tools) >= 100, "tools go on being sent");
        std::fs::remove_dir_all(&dir).ok();
    }
}
