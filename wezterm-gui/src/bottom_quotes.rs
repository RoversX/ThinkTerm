use crate::native_settings::NativeBottomQuoteMode;
use anyhow::Context;
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::PathBuf;
use std::sync::OnceLock;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

const MIN_QUOTE_INTERVAL_MINUTES: u32 = 1;
const MAX_QUOTE_INTERVAL_MINUTES: u32 = 24 * 60;

#[derive(Debug, Clone, Deserialize, Serialize)]
pub(crate) struct BottomQuote {
    pub(crate) text: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) author: Option<String>,
}

impl BottomQuote {
    pub(crate) fn display_text(&self) -> String {
        let text = self.text.trim();
        match self
            .author
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            Some(author) if !text.is_empty() => format!("{text} - {author}"),
            _ => text.to_string(),
        }
    }
}

#[derive(Debug, Clone)]
struct QuoteCache {
    modified: Option<SystemTime>,
    quotes: Vec<BottomQuote>,
}

static QUOTE_CACHE: OnceLock<Mutex<Option<QuoteCache>>> = OnceLock::new();

pub(crate) fn quotes_path() -> PathBuf {
    crate::native_settings::settings_path().with_file_name("bottom_quotes.json")
}

pub(crate) fn ensure_quotes_file() -> anyhow::Result<PathBuf> {
    let path = quotes_path();
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    if !path.exists() {
        write_default_quotes(&path)?;
    }
    Ok(path)
}

pub(crate) fn reset_quotes_file() -> anyhow::Result<PathBuf> {
    let path = quotes_path();
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    write_default_quotes(&path)?;
    *QUOTE_CACHE.get_or_init(|| Mutex::new(None)).lock() = None;
    Ok(path)
}

pub(crate) fn selected_quote(
    mode: NativeBottomQuoteMode,
    interval_minutes: u32,
) -> Option<BottomQuote> {
    let quotes = load_quotes();
    if quotes.is_empty() {
        return None;
    }
    let index = selected_index(mode, interval_minutes, quotes.len());
    quotes.get(index).cloned()
}

pub(crate) fn next_rotation_delay(interval_minutes: u32) -> Duration {
    let seconds = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or(0);
    let interval_secs = interval_seconds(interval_minutes);
    let remaining = interval_secs - (seconds % interval_secs);
    Duration::from_secs(remaining.max(1))
}

fn load_quotes() -> Vec<BottomQuote> {
    let path = quotes_path();
    let modified = fs::metadata(&path).and_then(|meta| meta.modified()).ok();
    let cache = QUOTE_CACHE.get_or_init(|| Mutex::new(None));
    {
        let guard = cache.lock();
        if let Some(existing) = guard.as_ref() {
            if existing.modified == modified {
                return existing.quotes.clone();
            }
        }
    }

    let quotes = match fs::read_to_string(&path) {
        Ok(data) => parse_quotes(&data).unwrap_or_else(|err| {
            log::warn!("Unable to parse bottom quotes {}: {err:#}", path.display());
            default_quotes()
        }),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            if let Err(err) = ensure_quotes_file() {
                log::warn!(
                    "Unable to create default bottom quotes {}: {err:#}",
                    path.display()
                );
            }
            default_quotes()
        }
        Err(err) => {
            log::warn!("Unable to read bottom quotes {}: {err:#}", path.display());
            default_quotes()
        }
    };

    *cache.lock() = Some(QuoteCache {
        modified,
        quotes: quotes.clone(),
    });
    quotes
}

fn write_default_quotes(path: &PathBuf) -> anyhow::Result<()> {
    let data = serde_json::to_vec_pretty(&default_quotes())?;
    let tmp = path.with_extension("json.tmp");
    fs::write(&tmp, data).with_context(|| format!("write {}", tmp.display()))?;
    fs::rename(&tmp, path).with_context(|| format!("replace {}", path.display()))?;
    Ok(())
}

fn parse_quotes(data: &str) -> anyhow::Result<Vec<BottomQuote>> {
    let quotes: Vec<BottomQuote> = serde_json::from_str(data)?;
    Ok(quotes
        .into_iter()
        .filter(|quote| !quote.text.trim().is_empty())
        .collect())
}

fn selected_index(mode: NativeBottomQuoteMode, interval_minutes: u32, len: usize) -> usize {
    if len <= 1 {
        return 0;
    }
    let interval_secs = interval_seconds(interval_minutes);
    let bucket = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs() / interval_secs)
        .unwrap_or(0);
    match mode {
        NativeBottomQuoteMode::Timed => (bucket as usize) % len,
        NativeBottomQuoteMode::PseudoRandom => (stable_hash(bucket) as usize) % len,
    }
}

fn interval_seconds(interval_minutes: u32) -> u64 {
    let minutes = interval_minutes.clamp(MIN_QUOTE_INTERVAL_MINUTES, MAX_QUOTE_INTERVAL_MINUTES);
    u64::from(minutes) * 60
}

fn stable_hash(mut value: u64) -> u64 {
    value ^= value >> 33;
    value = value.wrapping_mul(0xff51afd7ed558ccd);
    value ^= value >> 33;
    value = value.wrapping_mul(0xc4ceb9fe1a85ec53);
    value ^ (value >> 33)
}

fn default_quotes() -> Vec<BottomQuote> {
    vec![
        BottomQuote {
            text: "Until death, all defeat is psychological. Be undefeatable.".to_string(),
            author: None,
        },
        BottomQuote {
            text: "If things are not failing, you are not innovating enough.".to_string(),
            author: Some("Elon Musk".to_string()),
        },
        BottomQuote {
            text: "I don't ever give up.".to_string(),
            author: Some("Elon Musk".to_string()),
        },
        BottomQuote {
            text: "Starting a company is like eating glass and staring into the abyss.".to_string(),
            author: Some("Elon Musk".to_string()),
        },
        BottomQuote {
            text: "Stay hungry. Stay foolish.".to_string(),
            author: Some("Steve Jobs".to_string()),
        },
        BottomQuote {
            text: "The best way to predict the future is to invent it.".to_string(),
            author: Some("Alan Kay".to_string()),
        },
        BottomQuote {
            text: "What I cannot create, I do not understand.".to_string(),
            author: Some("Richard Feynman".to_string()),
        },
        BottomQuote {
            text: "Programs must be written for people to read.".to_string(),
            author: Some("Harold Abelson".to_string()),
        },
        BottomQuote {
            text: "Simplicity is the soul of efficiency.".to_string(),
            author: Some("Austin Freeman".to_string()),
        },
        BottomQuote {
            text: "Make it work, make it right, make it fast.".to_string(),
            author: Some("Kent Beck".to_string()),
        },
    ]
}
