//! Quotes and charts from Yahoo Finance's chart API, which answers without
//! a key. It is not an official API: it turns away a client that asks too
//! often, and may change what it answers, so what is here reads it loosely
//! and takes what it finds.

use serde_json::Value;
use std::time::Duration;

const HOST: &str = "https://query1.finance.yahoo.com";
/// Yahoo turns away a client that does not say it is a browser of sorts.
const USER_AGENT: &str = "Mozilla/5.0 (compatible; ThinkTerm stocks plugin)";
const TIMEOUT: Duration = Duration::from_secs(10);

/// Why nothing came.
#[derive(Debug, Clone, PartialEq)]
pub enum Failure {
    /// Asked too often: wait longer before the next.
    Limited,
    Failed(String),
}

/// A symbol's day so far: its price, where it closed the day before, and
/// its prices through the day.
#[derive(Debug, Clone, PartialEq)]
pub struct Day {
    pub symbol: String,
    pub price: f64,
    pub previous: f64,
    pub closes: Vec<f64>,
}

/// A chart of a symbol over a range, with what is known of it besides.
#[derive(Debug, Clone, PartialEq)]
pub struct Chart {
    pub name: String,
    pub currency: String,
    /// The exchange's time zone, as it names it: "EDT".
    pub zone: String,
    pub price: f64,
    pub previous: f64,
    /// The exchange's offset from UTC, in seconds: times show as it has
    /// them.
    pub offset: i64,
    /// When the price was last set, in seconds since the epoch.
    pub time: Option<i64>,
    pub times: Vec<i64>,
    pub closes: Vec<f64>,
}

/// The ranges a chart can show, and the step Yahoo gives each.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Range {
    Day,
    Week,
    Month,
    HalfYear,
    Year,
    FiveYears,
}

impl Range {
    pub const ALL: [Self; 6] = [
        Self::Day,
        Self::Week,
        Self::Month,
        Self::HalfYear,
        Self::Year,
        Self::FiveYears,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Self::Day => "1D",
            Self::Week => "5D",
            Self::Month => "1M",
            Self::HalfYear => "6M",
            Self::Year => "1Y",
            Self::FiveYears => "5Y",
        }
    }

    fn query(self) -> (&'static str, &'static str) {
        match self {
            Self::Day => ("1d", "5m"),
            Self::Week => ("5d", "30m"),
            Self::Month => ("1mo", "1d"),
            Self::HalfYear => ("6mo", "1d"),
            Self::Year => ("1y", "1d"),
            Self::FiveYears => ("5y", "1wk"),
        }
    }
}

/// The day so far of each of `symbols`, in one request.
pub fn days(symbols: &[String]) -> Result<Vec<Day>, Failure> {
    let list: Vec<String> = symbols.iter().map(|symbol| encode(symbol)).collect();
    let url = format!(
        "{HOST}/v8/finance/spark?symbols={}&range=1d&interval=5m",
        list.join(",")
    );
    let answer = get(&url)?;
    Ok(symbols
        .iter()
        .filter_map(|symbol| day(symbol, answer.get(symbol)?))
        .collect())
}

fn day(symbol: &str, found: &Value) -> Option<Day> {
    let closes = numbers(&found["close"]);
    let price = closes
        .last()
        .copied()
        .or_else(|| found["fulldayPrice"].as_f64())?;
    let previous = found["chartPreviousClose"]
        .as_f64()
        .or_else(|| found["previousClose"].as_f64())
        .unwrap_or(price);
    Some(Day {
        symbol: symbol.to_string(),
        price,
        previous,
        closes,
    })
}

/// `symbol` over `range`.
pub fn chart(symbol: &str, range: Range) -> Result<Chart, Failure> {
    let (range, interval) = range.query();
    let url = format!(
        "{HOST}/v8/finance/chart/{}?range={range}&interval={interval}",
        encode(symbol)
    );
    let answer = get(&url)?;
    let result = &answer["chart"]["result"][0];
    let meta = &result["meta"];
    let times: Vec<Option<i64>> = match result["timestamp"].as_array() {
        Some(times) => times.iter().map(Value::as_i64).collect(),
        None => Vec::new(),
    };
    let closes = result["indicators"]["quote"][0]["close"].as_array();
    // A step with no price -- a halt, a gap -- is left out, with its time.
    let (times, closes): (Vec<i64>, Vec<f64>) = times
        .iter()
        .zip(closes.into_iter().flatten())
        .filter_map(|(time, close)| Some(((*time)?, close.as_f64()?)))
        .unzip();
    let price = meta["regularMarketPrice"]
        .as_f64()
        .or_else(|| closes.last().copied())
        .ok_or_else(|| Failure::Failed(format!("no price for {symbol}")))?;
    let text = |key: &str| meta[key].as_str().unwrap_or_default().to_string();
    let name = [text("shortName"), text("longName")]
        .into_iter()
        .find(|name| !name.is_empty())
        .unwrap_or_else(|| symbol.to_string());
    Ok(Chart {
        name,
        currency: text("currency"),
        zone: text("timezone"),
        price,
        previous: meta["chartPreviousClose"]
            .as_f64()
            .or_else(|| meta["previousClose"].as_f64())
            .unwrap_or(price),
        offset: meta["gmtoffset"].as_i64().unwrap_or(0),
        time: meta["regularMarketTime"].as_i64(),
        times,
        closes,
    })
}

fn numbers(values: &Value) -> Vec<f64> {
    values
        .as_array()
        .map(|values| values.iter().filter_map(Value::as_f64).collect())
        .unwrap_or_default()
}

fn get(url: &str) -> Result<Value, Failure> {
    use http_req::request::Request;
    use http_req::uri::Uri;

    let uri = Uri::try_from(url).map_err(|err| Failure::Failed(format!("{err}")))?;
    let mut body = Vec::new();
    let response = Request::new(&uri)
        .header("User-Agent", USER_AGENT)
        .header("Accept", "application/json")
        .connect_timeout(Some(TIMEOUT))
        .read_timeout(Some(TIMEOUT))
        .timeout(TIMEOUT)
        .send(&mut body)
        .map_err(|err| Failure::Failed(format!("{err}")))?;
    let status = response.status_code();
    if u16::from(status) == 429 {
        return Err(Failure::Limited);
    }
    if !status.is_success() {
        return Err(Failure::Failed(format!("HTTP {status}")));
    }
    serde_json::from_slice(&body).map_err(|err| Failure::Failed(format!("{err}")))
}

/// `symbol` as it goes in a URL: "^GSPC" is "%5EGSPC".
fn encode(symbol: &str) -> String {
    let mut encoded = String::new();
    for byte in symbol.bytes() {
        if byte.is_ascii_alphanumeric() || b"-._".contains(&byte) {
            encoded.push(byte as char);
        } else {
            encoded.push_str(&format!("%{byte:02X}"));
        }
    }
    encoded
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn symbols_are_encoded_for_a_url() {
        assert_eq!(encode("^GSPC"), "%5EGSPC");
        assert_eq!(encode("EURUSD=X"), "EURUSD%3DX");
        assert_eq!(encode("BTC-USD"), "BTC-USD");
    }

    #[test]
    fn a_day_is_read_loosely() {
        let found = json!({"close": [1.0, null, 2.5], "chartPreviousClose": 2.0});
        let read = day("A", &found).unwrap();
        assert_eq!((read.price, read.previous), (2.5, 2.0));
        assert_eq!(read.closes, [1.0, 2.5], "the gap is left out");
        let bare = day("A", &json!({"fulldayPrice": 7.0})).unwrap();
        assert_eq!((bare.price, bare.previous), (7.0, 7.0));
        assert!(day("B", &json!({})).is_none());
    }
}
