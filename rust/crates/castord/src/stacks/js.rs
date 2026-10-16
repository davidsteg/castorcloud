//! The few JavaScript conversions spec.ts relies on (String(v), Number(v),
//! trim, key order), so a stack definition means the same here as in the
//! portal. Where the two could ever disagree on what a value is, the
//! definition would be checked as one thing and run as another.

use serde_json::{Map, Value};

/// WhiteSpace and LineTerminator of ECMAScript (what trim() and \s strip).
pub fn is_space(c: char) -> bool {
    matches!(
        c,
        '\t' | '\n' | '\u{b}' | '\u{c}' | '\r' | ' ' | '\u{a0}' | '\u{1680}' | '\u{2000}'
            ..='\u{200a}' | '\u{2028}' | '\u{2029}' | '\u{202f}' | '\u{205f}' | '\u{3000}' | '\u{feff}'
    )
}

pub fn trim(s: &str) -> &str {
    s.trim_matches(is_space)
}

/// The first `n` UTF-16 units, as String.prototype.slice(0, n) (never
/// splitting a character).
pub fn slice(s: &str, n: usize) -> &str {
    let mut units = 0;
    for (i, c) in s.char_indices() {
        units += c.len_utf16();
        if units > n {
            return &s[..i];
        }
    }
    s
}

/// An array index key ("0", "17"): JavaScript lists these first, ascending.
fn index_key(k: &str) -> Option<u32> {
    if k == "0" {
        return Some(0);
    }
    if k.starts_with('0') || k.is_empty() || !k.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    k.parse::<u32>().ok().filter(|n| *n < u32::MAX)
}

/// Object.keys order.
pub fn keys(o: &Map<String, Value>) -> Vec<&str> {
    let mut index: Vec<(u32, &str)> = o.keys().filter_map(|k| index_key(k).map(|n| (n, k.as_str()))).collect();
    index.sort_unstable();
    let mut out: Vec<&str> = index.into_iter().map(|(_, k)| k).collect();
    out.extend(o.keys().filter(|k| index_key(k).is_none()).map(String::as_str));
    out
}

/// Number.prototype.toString().
pub fn number_string(x: f64) -> String {
    if x.is_nan() {
        return "NaN".into();
    }
    if x == 0.0 {
        return "0".into();
    }
    if x.is_infinite() {
        return if x > 0.0 { "Infinity" } else { "-Infinity" }.into();
    }
    if x < 0.0 {
        return format!("-{}", number_string(-x));
    }
    // Shortest round-trip digits, as d.ddde<exp>.
    let sci = format!("{x:e}");
    let (mantissa, exp) = sci.split_once('e').unwrap_or((&sci, "0"));
    let digits: String = mantissa.chars().filter(char::is_ascii_digit).collect();
    let k = i64::try_from(digits.len()).unwrap_or(0);
    let n = exp.parse::<i64>().unwrap_or(0) + 1;
    let zeros = |c: i64| "0".repeat(usize::try_from(c).unwrap_or(0));
    if k <= n && n <= 21 {
        format!("{digits}{}", zeros(n - k))
    } else if 0 < n && n <= 21 {
        let (a, b) = digits.split_at(usize::try_from(n).unwrap_or(0));
        format!("{a}.{b}")
    } else if -6 < n && n <= 0 {
        format!("0.{}{digits}", zeros(-n))
    } else {
        let e = n - 1;
        let sign = if e < 0 { '-' } else { '+' };
        let (first, rest) = digits.split_at(1);
        if rest.is_empty() {
            format!("{first}e{sign}{}", e.abs())
        } else {
            format!("{first}.{rest}e{sign}{}", e.abs())
        }
    }
}

/// String(v).
pub fn string(v: &Value) -> String {
    match v {
        Value::Null => "null".into(),
        Value::Bool(b) => b.to_string(),
        Value::Number(n) => number_string(n.as_f64().unwrap_or(f64::NAN)),
        Value::String(s) => s.clone(),
        Value::Array(a) => a
            .iter()
            .map(|x| if x.is_null() { String::new() } else { string(x) })
            .collect::<Vec<_>>()
            .join(","),
        Value::Object(_) => "[object Object]".into(),
    }
}

/// Number(v).
pub fn number(v: &Value) -> f64 {
    match v {
        Value::Null => 0.0,
        Value::Bool(b) => f64::from(u8::from(*b)),
        Value::Number(n) => n.as_f64().unwrap_or(f64::NAN),
        Value::String(s) => string_to_number(s),
        Value::Array(_) => string_to_number(&string(v)),
        Value::Object(_) => f64::NAN,
    }
}

fn digits(s: &str) -> bool {
    !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit())
}

/// StringToNumber: decimal (with exponent), Infinity, 0x / 0o / 0b.
pub fn string_to_number(s: &str) -> f64 {
    let t = trim(s);
    if t.is_empty() {
        return 0.0;
    }
    for (prefix, radix) in [("0x", 16), ("0X", 16), ("0o", 8), ("0O", 8), ("0b", 2), ("0B", 2)] {
        if let Some(rest) = t.strip_prefix(prefix) {
            if rest.is_empty() || !rest.chars().all(|c| c.is_digit(radix)) {
                return f64::NAN;
            }
            return rest.chars().fold(0.0, |acc, c| {
                acc * f64::from(radix) + f64::from(c.to_digit(radix).unwrap_or(0))
            });
        }
    }
    let unsigned = t.strip_prefix(['+', '-']).unwrap_or(t);
    if unsigned == "Infinity" {
        return if t.starts_with('-') {
            f64::NEG_INFINITY
        } else {
            f64::INFINITY
        };
    }
    let (mantissa, exp) = match unsigned.find(['e', 'E']) {
        Some(i) => (&unsigned[..i], Some(&unsigned[i + 1..])),
        None => (unsigned, None),
    };
    let mantissa_ok = match mantissa.split_once('.') {
        Some((a, b)) => (digits(a) && (b.is_empty() || digits(b))) || (a.is_empty() && digits(b)),
        None => digits(mantissa),
    };
    let exp_ok = exp.is_none_or(|e| digits(e.strip_prefix(['+', '-']).unwrap_or(e)));
    if !mantissa_ok || !exp_ok {
        return f64::NAN;
    }
    t.parse::<f64>().unwrap_or(f64::NAN)
}

pub fn is_integer(x: f64) -> bool {
    x.is_finite() && x.trunc() == x
}

/// Math.round for the values here (ties go up).
pub fn round(x: f64) -> f64 {
    let r = x.round();
    if r - x == 0.5 && x < 0.0 { r + 1.0 } else { r }
}

/// A JSON number the way JSON.stringify writes it (integers without ".0").
pub fn num(x: f64) -> Value {
    if is_integer(x) && x.abs() < 9_007_199_254_740_992.0 {
        // In range, so the conversion is exact.
        #[allow(clippy::cast_possible_truncation)]
        Value::from(x as i64)
    } else {
        Value::from(x)
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;
    use serde_json::json;

    #[test]
    fn like_javascript() {
        for (x, s) in [
            (1.0, "1"),
            (0.5, "0.5"),
            (1e21, "1e+21"),
            (1.5e21, "1.5e+21"),
            (123_456_789_012_345_680_000.0, "123456789012345680000"),
            (0.000_001, "0.000001"),
            (1e-7, "1e-7"),
            (-2.5, "-2.5"),
            (0.1 + 0.2, "0.30000000000000004"),
        ] {
            assert_eq!(number_string(x), s);
        }
        assert_eq!(string(&json!([1, null, "a", [2, 3]])), "1,,a,2,3");
        assert_eq!(string(&json!({})), "[object Object]");
        for (s, x) in [
            (" 0.5 ", 0.5),
            ("", 0.0),
            ("0x1A", 26.0),
            ("1e3", 1000.0),
            (".5", 0.5),
            ("5.", 5.0),
            ("-Infinity", f64::NEG_INFINITY),
        ] {
            assert_eq!(string_to_number(s), x, "{s}");
        }
        for s in ["abc", "1,5", "0x", "-0x5", "inf", "nan", "1e", "1_000", "."] {
            assert!(string_to_number(s).is_nan(), "{s}");
        }
        assert_eq!(number(&json!(["7"])), 7.0);
        assert_eq!(number(&json!(true)), 1.0);
        assert!(number(&json!([1, 2])).is_nan());
        let o: Map<String, Value> = serde_json::from_str(r#"{"b":1,"2":1,"a":1,"1":1,"01":1}"#).unwrap();
        assert_eq!(keys(&o), ["1", "2", "b", "a", "01"]);
        assert_eq!(slice("aé😀b", 3), "aé");
        assert_eq!(trim("\u{feff} x\u{3000}"), "x");
    }
}
