//! Minimal disclosure: tools answer with a few short, structured facts and a
//! link back into the portal, never whole files or long lists. Everything a
//! tool returns is sent to the LLM provider.
//!
//! The helpers keep the JavaScript semantics of the first (TypeScript)
//! version where answers depend on them: lengths count UTF-16 code units,
//! numbers round like Math.round and print like JSON.stringify.

use jiff::Timestamp;
use jiff::civil::DateTime;
use jiff::tz::TimeZone;
use serde_json::{Map, Value};

pub const MAX_ITEMS: i64 = 25;
pub const EXCERPT_CHARS: usize = 280;
pub const DOCUMENT_CHARS: usize = 6000;

/// Largest integer a JSON number from the model may carry (Number.MAX_SAFE_INTEGER).
pub const MAX_SAFE_INTEGER: f64 = 9_007_199_254_740_991.0;

/// A tool failure. `User` is shown to the model (and can be told to the
/// user); `Internal` is logged and answered with a generic message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ToolError {
    User(String),
    Internal(String),
}

impl ToolError {
    pub fn user(msg: impl Into<String>) -> Self {
        Self::User(msg.into())
    }
    pub fn internal(msg: impl Into<String>) -> Self {
        Self::Internal(msg.into())
    }
}

pub fn clamp_limit(limit: Option<i64>, fallback: i64) -> usize {
    // In 1..=MAX_ITEMS, so the cast is lossless.
    limit.unwrap_or(fallback).clamp(1, MAX_ITEMS) as usize
}

/// White space as JavaScript's `\s` and `String.prototype.trim` see it.
pub fn js_space(c: char) -> bool {
    matches!(
        c,
        '\u{9}'..='\u{d}'
            | ' '
            | '\u{a0}'
            | '\u{1680}'
            | '\u{2000}'..='\u{200a}'
            | '\u{2028}'
            | '\u{2029}'
            | '\u{202f}'
            | '\u{205f}'
            | '\u{3000}'
            | '\u{feff}'
    )
}

pub fn js_trim(s: &str) -> &str {
    s.trim_matches(js_space)
}

/// Length in UTF-16 code units (JavaScript's `.length`).
pub fn js_len(s: &str) -> usize {
    s.encode_utf16().count()
}

/// `s.slice(start, end)` in UTF-16 code units. A cut through a surrogate pair
/// drops the half character instead of producing a lone surrogate.
pub fn js_slice(s: &str, start: usize, end: usize) -> String {
    let mut out = String::new();
    let mut pos = 0usize;
    for c in s.chars() {
        let w = c.len_utf16();
        if pos >= end {
            break;
        }
        if pos >= start && pos + w <= end {
            out.push(c);
        }
        pos += w;
    }
    out
}

/// Removes `<...>` tags like `text.replace(/<[^>]*>/g, "")`.
fn strip_tags(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(lt) = rest.find('<') {
        match rest[lt..].find('>') {
            Some(gt) => {
                out.push_str(&rest[..lt]);
                rest = &rest[lt + gt + 1..];
            }
            None => break,
        }
    }
    out.push_str(rest);
    out
}

/// Every run of white space becomes one space.
fn collapse_space(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut in_space = false;
    for c in text.chars() {
        if js_space(c) {
            if !in_space {
                out.push(' ');
            }
            in_space = true;
        } else {
            out.push(c);
            in_space = false;
        }
    }
    out
}

/// Plain text, whitespace collapsed, cut at a word boundary.
pub fn excerpt(text: Option<&str>, max: usize) -> Option<String> {
    let text = text.filter(|t| !t.is_empty())?;
    let collapsed = collapse_space(&strip_tags(text));
    let plain = js_trim(&collapsed);
    if js_len(plain) <= max {
        return (!plain.is_empty()).then(|| plain.to_owned());
    }
    let cut = js_slice(plain, 0, max);
    // lastIndexOf(" ") in code units; -1 when there is none.
    let space = cut.rfind(' ').map(|b| js_len(&cut[..b]));
    let end = match space {
        Some(s) if (s as f64) > max as f64 * 0.6 => s,
        _ => max,
    };
    Some(format!("{}…", js_slice(&cut, 0, end)))
}

/// Math.round: halves round towards +infinity.
pub fn js_round(x: f64) -> f64 {
    if !x.is_finite() {
        return x;
    }
    let f = x.floor();
    if x - f >= 0.5 { f + 1.0 } else { f }
}

/// A computed number as JSON.stringify writes it: integers without ".0",
/// NaN and infinities as null.
pub fn js_number(x: f64) -> Value {
    if !x.is_finite() {
        return Value::Null;
    }
    if x.fract() == 0.0 && x.abs() < 9.0e18 {
        // Integral and in range, so the cast is exact (and -0 becomes 0).
        return Value::from(x as i64);
    }
    serde_json::Number::from_f64(x).map_or(Value::Null, Value::Number)
}

/// An upstream value passed through to an answer, with floats that are
/// integers written as integers (1234.0 -> 1234), as JavaScript would.
pub fn js_value(v: &Value) -> Value {
    match v {
        Value::Number(n) if n.is_f64() => n.as_f64().map_or(Value::Null, js_number),
        Value::Array(a) => Value::Array(a.iter().map(js_value).collect()),
        Value::Object(o) => {
            Value::Object(o.iter().map(|(k, v)| (k.clone(), js_value(v))).collect())
        }
        other => other.clone(),
    }
}

/// JavaScript's StringToNumber.
fn string_to_number(s: &str) -> f64 {
    let t = js_trim(s);
    if t.is_empty() {
        return 0.0;
    }
    let radix = |p: &str, r: u32| -> f64 {
        if p.is_empty() {
            return f64::NAN;
        }
        p.chars()
            .try_fold(0f64, |acc, c| {
                c.to_digit(r).map(|d| acc * f64::from(r) + f64::from(d))
            })
            .unwrap_or(f64::NAN)
    };
    let lower = t.to_ascii_lowercase();
    if let Some(p) = lower.strip_prefix("0x") {
        return radix(p, 16);
    }
    if let Some(p) = lower.strip_prefix("0o") {
        return radix(p, 8);
    }
    if let Some(p) = lower.strip_prefix("0b") {
        return radix(p, 2);
    }
    match t {
        "Infinity" | "+Infinity" => return f64::INFINITY,
        "-Infinity" => return f64::NEG_INFINITY,
        _ => {}
    }
    // Rust also reads "inf" and "nan", JavaScript does not.
    if !t
        .chars()
        .all(|c| c.is_ascii_digit() || matches!(c, '.' | 'e' | 'E' | '+' | '-'))
    {
        return f64::NAN;
    }
    t.parse::<f64>().unwrap_or(f64::NAN)
}

/// JavaScript's Number(value) for JSON values.
pub fn to_number(v: Option<&Value>) -> f64 {
    match v {
        None => f64::NAN,
        Some(Value::Null) => 0.0,
        Some(Value::Bool(b)) => f64::from(u8::from(*b)),
        Some(Value::Number(n)) => n.as_f64().unwrap_or(f64::NAN),
        Some(Value::String(s)) => string_to_number(s),
        Some(Value::Array(a)) => match a.as_slice() {
            [] | [Value::Null] => 0.0,
            [one @ (Value::String(_) | Value::Number(_) | Value::Array(_))] => to_number(Some(one)),
            _ => f64::NAN,
        },
        Some(Value::Object(_)) => f64::NAN,
    }
}

/// `${value}` in a template literal.
pub fn js_string(v: Option<&Value>) -> String {
    match v {
        None => "undefined".into(),
        Some(Value::Null) => "null".into(),
        Some(Value::String(s)) => s.clone(),
        Some(Value::Number(n)) => match n.as_f64() {
            Some(f) => match js_number(f) {
                Value::Number(n) => n.to_string(),
                _ => "NaN".into(),
            },
            None => n.to_string(),
        },
        Some(Value::Bool(b)) => b.to_string(),
        Some(Value::Array(a)) => a
            .iter()
            .map(|v| {
                if v.is_null() {
                    String::new()
                } else {
                    js_string(Some(v))
                }
            })
            .collect::<Vec<_>>()
            .join(","),
        Some(Value::Object(_)) => "[object Object]".into(),
    }
}

/// ~11 m precision: enough to name a place, not to pinpoint a doorstep.
pub fn round_coord(value: Option<&Value>) -> Option<f64> {
    match value {
        None | Some(Value::Null) => return None,
        Some(Value::String(s)) if s.is_empty() => return None,
        _ => {}
    }
    let n = to_number(value);
    n.is_finite().then(|| js_round(n * 1e4) / 1e4)
}

/// An answer object. `compact` drops undefined/null/empty values so answers
/// stay small; a computed number (`num`) that is NaN survives it and is
/// written as null, as in the TypeScript version.
#[derive(Default)]
pub struct Out(Vec<(&'static str, Value, bool)>);

impl Out {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn set(mut self, key: &'static str, value: impl Into<Value>) -> Self {
        self.0.push((key, value.into(), false));
        self
    }
    /// Left out when undefined (None), like JSON.stringify does.
    pub fn opt(mut self, key: &'static str, value: Option<impl Into<Value>>) -> Self {
        if let Some(v) = value {
            self.0.push((key, v.into(), false));
        }
        self
    }
    pub fn num(mut self, key: &'static str, value: f64) -> Self {
        let v = js_number(value);
        let nan = v.is_null();
        self.0.push((key, v, nan));
        self
    }
    pub fn compact(self) -> Value {
        let empty = |v: &Value| match v {
            Value::Null => true,
            Value::String(s) => s.is_empty(),
            Value::Array(a) => a.is_empty(),
            _ => false,
        };
        Value::Object(
            self.0
                .into_iter()
                .filter(|(_, v, nan)| *nan || !empty(v))
                .map(|(k, v, _)| (k.to_owned(), v))
                .collect(),
        )
    }
    pub fn build(self) -> Value {
        Value::Object(
            self.0
                .into_iter()
                .map(|(k, v, _)| (k.to_owned(), v))
                .collect(),
        )
    }
}

/// Appends the entries of `extra` to `base` (object spread).
pub fn spread(base: Value, extra: Value) -> Value {
    let mut out: Map<String, Value> = match base {
        Value::Object(m) => m,
        _ => Map::new(),
    };
    if let Value::Object(e) = extra {
        out.extend(e);
    }
    Value::Object(out)
}

// ---- Time --------------------------------------------------------------

const MS_PER_DAY: i64 = 86_400_000;
/// JavaScript's time range: ±100,000,000 days around 1970.
const MAX_TIME: i64 = 8_640_000_000_000_000;

/// Days since 1970-01-01 of a proleptic Gregorian date (month 1..=12; days
/// past the end of the month roll over, as JavaScript's Date does).
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468 + (d - 1)
}

fn civil_from_days(z: i64) -> (i64, i64, i64) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (yoe + era * 400 + i64::from(m <= 2), m, d)
}

/// Wall-clock fields of a time value (ms since 1970, UTC).
fn fields(ms: i64) -> (i64, i64, i64, i64, i64, i64, i64) {
    let days = ms.div_euclid(MS_PER_DAY);
    let rem = ms.rem_euclid(MS_PER_DAY);
    let (y, mo, d) = civil_from_days(days);
    (
        y,
        mo,
        d,
        rem / 3_600_000,
        rem / 60_000 % 60,
        rem / 1000 % 60,
        rem % 1000,
    )
}

/// Date.prototype.toISOString.
pub fn to_iso_string(ms: i64) -> String {
    let (y, mo, d, h, mi, s, milli) = fields(ms);
    let year = if (0..=9999).contains(&y) {
        format!("{y:04}")
    } else if y < 0 {
        format!("-{:06}", -y)
    } else {
        format!("+{y:06}")
    };
    format!("{year}-{mo:02}-{d:02}T{h:02}:{mi:02}:{s:02}.{milli:03}Z")
}

struct Scan<'a> {
    b: &'a [u8],
    i: usize,
}

impl Scan<'_> {
    fn digits(&mut self, n: usize) -> Option<i64> {
        let s = self.b.get(self.i..self.i + n)?;
        if !s.iter().all(u8::is_ascii_digit) {
            return None;
        }
        self.i += n;
        Some(s.iter().fold(0i64, |a, d| a * 10 + i64::from(d - b'0')))
    }
    fn eat(&mut self, c: u8) -> bool {
        if self.b.get(self.i) == Some(&c) {
            self.i += 1;
            true
        } else {
            false
        }
    }
    fn peek(&self) -> Option<u8> {
        self.b.get(self.i).copied()
    }
}

/// The tenant's time zone: dates the user types and times we answer are local.
pub struct Zone {
    pub name: String,
    tz: Option<TimeZone>,
}

impl Zone {
    /// `process.env.TZ || "Europe/Zurich"`.
    pub fn new(name: &str) -> Self {
        let name = if name.is_empty() {
            "Europe/Zurich"
        } else {
            name
        };
        Self {
            name: name.to_owned(),
            tz: TimeZone::get(name).ok(),
        }
    }

    pub fn is_valid(&self) -> bool {
        self.tz.is_some()
    }

    fn tz(&self) -> Result<&TimeZone, ToolError> {
        self.tz
            .as_ref()
            .ok_or_else(|| ToolError::internal(format!("invalid time zone {:?}", self.name)))
    }

    /// Seconds the zone is ahead of UTC at a time value.
    fn offset_seconds(&self, ms: i64) -> Result<i64, ToolError> {
        let ts = Timestamp::from_millisecond(ms)
            .map_err(|_| ToolError::internal("Invalid time value"))?;
        Ok(i64::from(self.tz()?.to_offset(ts).seconds()))
    }

    /// The local wall-clock time `fields` as a time value (an hour that a
    /// DST change skips counts with the offset before the change, a repeated
    /// one is its first occurrence, as JavaScript's Date does).
    fn local_to_ms(&self, days: i64, time_ms: i64) -> Option<i64> {
        let (y, mo, d) = civil_from_days(days);
        let dt = DateTime::new(
            i16::try_from(y).ok()?,
            i8::try_from(mo).ok()?,
            i8::try_from(d).ok()?,
            i8::try_from(time_ms / 3_600_000).ok()?,
            i8::try_from(time_ms / 60_000 % 60).ok()?,
            i8::try_from(time_ms / 1000 % 60).ok()?,
            i32::try_from(time_ms % 1000 * 1_000_000).ok()?,
        )
        .ok()?;
        let ts = self
            .tz
            .as_ref()?
            .to_ambiguous_timestamp(dt)
            .compatible()
            .ok()?;
        Some(ts.as_millisecond())
    }

    /// `new Date(value).getTime()` for the ISO 8601 forms JavaScript reads:
    /// YYYY, YYYY-MM, YYYY-MM-DD (UTC), optionally followed by a time
    /// HH:mm[:ss[.sss]] and Z or ±HH:mm (local time without one). Returns
    /// None for anything else (an Invalid Date).
    pub fn parse(&self, value: &str) -> Option<i64> {
        let mut s = Scan {
            b: value.as_bytes(),
            i: 0,
        };
        let year = match s.peek()? {
            c @ (b'+' | b'-') => {
                s.i += 1;
                let y = s.digits(6)?;
                if c == b'-' && y == 0 {
                    return None;
                }
                if c == b'-' { -y } else { y }
            }
            _ => s.digits(4)?,
        };
        let (mut month, mut day) = (1, 1);
        if s.eat(b'-') {
            month = s.digits(2)?;
            if s.eat(b'-') {
                day = s.digits(2)?;
            }
        }
        if !(1..=12).contains(&month) || !(1..=31).contains(&day) {
            return None;
        }
        let mut time = None;
        if matches!(s.peek(), Some(b'T' | b't' | b' ')) {
            s.i += 1;
            let h = s.digits(2)?;
            if !s.eat(b':') {
                return None;
            }
            let mi = s.digits(2)?;
            let (mut sec, mut milli) = (0, 0);
            if s.eat(b':') {
                sec = s.digits(2)?;
                if s.eat(b'.') {
                    let start = s.i;
                    while s.peek().is_some_and(|c| c.is_ascii_digit()) {
                        s.i += 1;
                    }
                    let frac = value.get(start..s.i)?;
                    if frac.is_empty() {
                        return None;
                    }
                    // Milliseconds; further digits are cut off.
                    milli = format!("{frac:0<3}").get(..3)?.parse().ok()?;
                }
            }
            let ok =
                (h < 24 || (h == 24 && mi == 0 && sec == 0 && milli == 0)) && mi < 60 && sec < 60;
            if !ok {
                return None;
            }
            time = Some(((h * 60 + mi) * 60 + sec) * 1000 + milli);
        }
        let offset = match s.peek() {
            None => None,
            Some(b'Z' | b'z') => {
                s.i += 1;
                Some(0)
            }
            Some(c @ (b'+' | b'-')) => {
                s.i += 1;
                let oh = s.digits(2)?;
                s.eat(b':');
                let om = s.digits(2)?;
                if oh > 23 || om > 59 {
                    return None;
                }
                let o = (oh * 60 + om) * 60_000;
                Some(if c == b'-' { -o } else { o })
            }
            Some(_) => return None,
        };
        if s.i != s.b.len() {
            return None;
        }
        let days = days_from_civil(year, month, day);
        let ms = match (time, offset) {
            (t, Some(o)) => days * MS_PER_DAY + t.unwrap_or(0) - o,
            (None, None) => days * MS_PER_DAY,
            (Some(t), None) => {
                let (days, t) = (days + t / MS_PER_DAY, t % MS_PER_DAY);
                self.local_to_ms(days, t)?
            }
        };
        (ms.abs() <= MAX_TIME).then_some(ms)
    }

    /// Minutes the zone is ahead of UTC at `ms`, as the TypeScript version
    /// measured it (wall clock read to the second, then rounded).
    fn offset_minutes(&self, ms: i64) -> Result<i64, ToolError> {
        let off_ms = self.offset_seconds(ms)? * 1000;
        let diff = off_ms - ms.rem_euclid(1000);
        // Math.round(diff / 60000), exactly.
        Ok(js_round(diff as f64 / 60_000.0) as i64)
    }

    /// Accepts "2026-05-01" (a local day: start, or end with `end_of_day`)
    /// or a full ISO timestamp; returns the instant (ISO string and time
    /// value) or None for an empty value.
    pub fn iso_date(
        &self,
        value: Option<&str>,
        end_of_day: bool,
    ) -> Result<Option<(String, i64)>, ToolError> {
        let Some(value) = value.filter(|v| !v.is_empty()) else {
            return Ok(None);
        };
        let not_a_date = || ToolError::user(format!("\"{value}\" is not a date (use YYYY-MM-DD)"));
        let b = value.as_bytes();
        let day_only = b.len() == 10
            && b.iter().enumerate().all(|(i, c)| {
                if i == 4 || i == 7 {
                    *c == b'-'
                } else {
                    c.is_ascii_digit()
                }
            });
        let ms = if day_only {
            let time = if end_of_day {
                "23:59:59.999"
            } else {
                "00:00:00.000"
            };
            let wall = self
                .parse(&format!("{value}T{time}Z"))
                .ok_or_else(not_a_date)?;
            let ms = wall - self.offset_minutes(wall)? * 60_000;
            if ms.abs() > MAX_TIME {
                return Err(ToolError::internal("Invalid time value"));
            }
            ms
        } else {
            self.parse(value).ok_or_else(not_a_date)?
        };
        Ok(Some((to_iso_string(ms), ms)))
    }

    /// "2026-05-01T10:00" in the tenant's zone, for answers.
    pub fn local_time_ms(&self, ms: i64) -> Result<String, ToolError> {
        let local = ms + self.offset_seconds(ms)? * 1000;
        let (y, mo, d, h, mi, _, _) = fields(local);
        Ok(format!("{y:04}-{mo:02}-{d:02}T{h:02}:{mi:02}"))
    }

    /// localTime of an ISO string (an unreadable one is an internal error,
    /// like JavaScript's RangeError).
    pub fn local_time(&self, value: &str) -> Result<String, ToolError> {
        let ms = self
            .parse(value)
            .ok_or_else(|| ToolError::internal("Invalid time value"))?;
        self.local_time_ms(ms)
    }

    /// localTime of a Unix time in seconds.
    pub fn local_time_secs(&self, secs: f64) -> Result<String, ToolError> {
        let ms = (secs * 1000.0).trunc();
        if !ms.is_finite() || ms.abs() > MAX_TIME as f64 {
            return Err(ToolError::internal("Invalid time value"));
        }
        // Within ±8.64e15, so the cast is exact.
        self.local_time_ms(ms as i64)
    }

    /// The local calendar day of an instant, "2026-05-01".
    pub fn local_day(&self, value: &str) -> Result<String, ToolError> {
        Ok(js_slice(&self.local_time(value)?, 0, 10))
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn excerpts_are_short_plain_text() {
        assert_eq!(
            excerpt(
                Some("<span class='match'>Hotel</span>   Lisboa\n invoice"),
                EXCERPT_CHARS
            )
            .as_deref(),
            Some("Hotel Lisboa invoice")
        );
        let long = excerpt(Some(&"word ".repeat(200)), 50).unwrap();
        assert!(js_len(&long) <= 51 && long.ends_with('…'));
        assert_eq!(long, format!("{}…", "word ".repeat(10).trim_end()));
        assert_eq!(excerpt(Some(""), EXCERPT_CHARS), None);
        assert_eq!(excerpt(None, EXCERPT_CHARS), None);
        assert_eq!(excerpt(Some("<b></b>  "), EXCERPT_CHARS), None);
        // No space late enough: cut hard.
        assert_eq!(
            excerpt(Some(&"x".repeat(20)), 10).unwrap(),
            format!("{}…", "x".repeat(10))
        );
        // An unclosed tag stays.
        assert_eq!(excerpt(Some("a < b"), 10).as_deref(), Some("a < b"));
        // Lengths are UTF-16 code units, like the TypeScript version.
        assert_eq!(excerpt(Some("🎉🎉🎉"), 5).as_deref(), Some("🎉🎉…"));
        assert_eq!(
            excerpt(Some("\u{a0}a\u{3000}b\u{feff}"), 10).as_deref(),
            Some("a b")
        );
    }

    #[test]
    fn limits_are_clamped() {
        assert_eq!(clamp_limit(Some(500), 10), 25);
        assert_eq!(clamp_limit(Some(0), 10), 1);
        assert_eq!(clamp_limit(None, 10), 10);
        assert_eq!(clamp_limit(None, 25), 25);
    }

    #[test]
    fn numbers_follow_javascript() {
        assert_eq!(js_round(2.5), 3.0);
        assert_eq!(js_round(-2.5), -2.0);
        assert_eq!(js_round(0.499_999_999_999_999_94), 0.0);
        assert_eq!(js_number(47.0), json!(47));
        assert_eq!(js_number(-0.0), json!(0));
        assert_eq!(js_number(f64::NAN), Value::Null);
        assert_eq!(js_number(34.3), json!(34.3));
        assert_eq!(
            js_value(&json!({"a": [1.0, 2.5, "x"]})).to_string(),
            r#"{"a":[1,2.5,"x"]}"#
        );
        assert_eq!(round_coord(Some(&json!("47.366123456"))), Some(47.3661));
        assert_eq!(round_coord(Some(&json!(8.54559))), Some(8.5456));
        assert_eq!(round_coord(Some(&json!(-8.00005))), Some(-8.0));
        assert_eq!(round_coord(Some(&json!(""))), None);
        assert_eq!(round_coord(Some(&json!(null))), None);
        assert_eq!(round_coord(None), None);
        assert_eq!(round_coord(Some(&json!("abc"))), None);
        assert_eq!(round_coord(Some(&json!("inf"))), None);
        assert_eq!(round_coord(Some(&json!(" 1.5 "))), Some(1.5));
        assert_eq!(to_number(Some(&json!("0x10"))), 16.0);
        assert!(to_number(None).is_nan());
        assert_eq!(js_string(Some(&json!(5.0))), "5");
        assert_eq!(js_string(None), "undefined");
    }

    #[test]
    fn compact_drops_empty_values() {
        let v = Out::new()
            .opt("a", Some(json!(null)))
            .opt("b", Some(""))
            .opt("c", Some(json!([])))
            .opt("d", None::<Value>)
            .opt("e", Some(0))
            .num("f", f64::NAN)
            .set("g", "x")
            .compact();
        assert_eq!(v.to_string(), r#"{"e":0,"f":null,"g":"x"}"#);
    }

    #[test]
    fn days_are_local_to_the_tenant() {
        let z = Zone::new("Europe/Zurich");
        let iso = |v: &str, end: bool| z.iso_date(Some(v), end).unwrap().unwrap().0;
        // Zurich: UTC+2 in summer, UTC+1 in winter.
        assert_eq!(iso("2026-05-01", false), "2026-04-30T22:00:00.000Z");
        assert_eq!(iso("2026-05-01", true), "2026-05-01T21:59:59.999Z");
        assert_eq!(iso("2026-01-15", false), "2026-01-14T23:00:00.000Z");
        assert_eq!(
            z.local_time("2026-05-01T08:00:00Z").unwrap(),
            "2026-05-01T10:00"
        );
        assert!(matches!(
            z.iso_date(Some("last tuesday"), false),
            Err(ToolError::User(_))
        ));
        assert_eq!(z.iso_date(Some(""), false).unwrap(), None);
        assert_eq!(z.iso_date(None, true).unwrap(), None);
        // DST changes (last Sundays of March and October).
        assert_eq!(iso("2026-03-29", false), "2026-03-28T23:00:00.000Z");
        assert_eq!(iso("2026-03-29", true), "2026-03-29T21:59:59.999Z");
        assert_eq!(iso("2026-10-25", false), "2026-10-24T22:00:00.000Z");
        assert_eq!(iso("2026-10-25", true), "2026-10-25T22:59:59.999Z");
        // Days past the end of a month roll over, like JavaScript's Date.
        assert_eq!(iso("2026-02-30", false), "2026-03-01T23:00:00.000Z");
        assert!(z.iso_date(Some("2026-13-01"), false).is_err());
        assert!(z.iso_date(Some("2026-02-32"), false).is_err());
    }

    #[test]
    fn timestamps_parse_like_javascript() {
        let z = Zone::new("Europe/Zurich");
        let p = |v: &str| z.parse(v).map(to_iso_string);
        let cases = [
            (
                "2026-05-01T10:00:00.123456Z",
                Some("2026-05-01T10:00:00.123Z"),
            ),
            (
                "2026-05-01T10:00:00.1239Z",
                Some("2026-05-01T10:00:00.123Z"),
            ),
            ("2026-05-01 10:00", Some("2026-05-01T08:00:00.000Z")),
            ("2026-05-01T24:00", Some("2026-05-01T22:00:00.000Z")),
            ("2026-05-01T24:00:01", None),
            ("2026-05-01T10:00+0200", Some("2026-05-01T08:00:00.000Z")),
            ("2026-05-01T10:00:00+02", None),
            ("+002026-05-01", Some("2026-05-01T00:00:00.000Z")),
            ("2026-05-01Z", Some("2026-05-01T00:00:00.000Z")),
            ("2026-05-01t10:00z", Some("2026-05-01T10:00:00.000Z")),
            ("2026-05-01T10:00:60", None),
            ("2026", Some("2026-01-01T00:00:00.000Z")),
            ("2026-05", Some("2026-05-01T00:00:00.000Z")),
            ("2026-05-01T10:00:00", Some("2026-05-01T08:00:00.000Z")),
            ("2026-05-01T10:00:00.5", Some("2026-05-01T08:00:00.500Z")),
            (
                "-000001-01-01T00:00:00Z",
                Some("-000001-01-01T00:00:00.000Z"),
            ),
            (
                "2026-05-01T10:00:00.000+02:00",
                Some("2026-05-01T08:00:00.000Z"),
            ),
            (
                "2026-05-01T10:00:00.000+23:59",
                Some("2026-04-30T10:01:00.000Z"),
            ),
            ("2026-05-01T10:00:00.000+24:00", None),
            ("2026-05-01T1:00", None),
            ("2026-05-01T10", None),
            ("2026-13", None),
            ("2026-05-01T10:00:00 +02:00", None),
            ("2026-05-01T10:00:00.Z", None),
            ("2026-03-29T02:30", Some("2026-03-29T01:30:00.000Z")),
            ("2026-10-25T02:30", Some("2026-10-25T00:30:00.000Z")),
            ("0000-01-01T00:00:00.000Z", Some("0000-01-01T00:00:00.000Z")),
            ("20260501", None),
            ("", None),
        ];
        for (input, want) in cases {
            assert_eq!(p(input).as_deref(), want, "{input}");
        }
    }

    #[test]
    fn local_times_read_to_the_minute() {
        let z = Zone::new("");
        assert_eq!(z.name, "Europe/Zurich");
        assert_eq!(
            z.local_time("2026-01-15T07:05:59.999Z").unwrap(),
            "2026-01-15T08:05"
        );
        assert_eq!(
            z.local_time_secs(1_777_629_600.0).unwrap(),
            "2026-05-01T12:00"
        );
        assert_eq!(
            z.local_day("2026-04-30T22:00:00.000Z").unwrap(),
            "2026-05-01"
        );
        assert!(matches!(z.local_time("nope"), Err(ToolError::Internal(_))));
        let utc = Zone::new("UTC");
        assert_eq!(
            utc.local_time("2026-05-01T08:00:00Z").unwrap(),
            "2026-05-01T08:00"
        );
        let bad = Zone::new("Mars/Olympus");
        assert!(!bad.is_valid());
        assert!(matches!(
            bad.local_time("2026-05-01T08:00:00Z"),
            Err(ToolError::Internal(_))
        ));
    }
}
