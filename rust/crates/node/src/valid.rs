//! Checks for every value that reaches a command line or a path. Each one
//! matches what the shell scripts accepted, or is stricter.

use crate::{Fail, Result};

fn only(s: &str, ok: impl Fn(u8) -> bool) -> bool {
    s.bytes().all(ok)
}

fn lower_digit_dash(b: u8) -> bool {
    b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-'
}

/// Tenant slug: `[a-z0-9-]+`, not starting with `-` (no option injection).
pub fn slug(s: &str) -> Result<&str> {
    if s.is_empty() || s.starts_with('-') || !only(s, lower_digit_dash) {
        return Err(Fail::invalid("invalid slug"));
    }
    Ok(s)
}

/// Tag of a dataset set aside during a restore: `[a-z0-9-]+`.
pub fn tag(s: &str) -> Result<&str> {
    if s.is_empty() || !only(s, lower_digit_dash) {
        return Err(Fail::invalid("invalid tag"));
    }
    Ok(s)
}

/// Backup snapshot or bookmark name: `cb-[A-Za-z0-9-]+`.
pub fn snap_name(s: &str) -> Result<&str> {
    match s.strip_prefix("cb-") {
        Some(rest) if !rest.is_empty() && only(rest, |b| b.is_ascii_alphanumeric() || b == b'-') => Ok(s),
        _ => Err(Fail::invalid(format!("invalid name: {s}"))),
    }
}

/// Size in MB: decimal digits only, and it has to fit.
pub fn mb(s: &str) -> Result<u64> {
    if s.is_empty() || !only(s, |b| b.is_ascii_digit()) {
        return Err(Fail::invalid("invalid size"));
    }
    s.parse().map_err(|_| Fail::invalid("invalid size"))
}

/// A data directory inside a tenant dataset: `[a-z0-9-]+`, no dot files.
pub fn data_dir(s: &str) -> Result<&str> {
    if s.is_empty() || !only(s, lower_digit_dash) {
        return Err(Fail::invalid(format!("invalid dir: {s}")));
    }
    Ok(s)
}

/// A word of the gate's command line: `[A-Za-z0-9._/=-]+`.
pub fn gate_word(s: &str) -> Result<&str> {
    if s.is_empty() || !only(s, |b| b.is_ascii_alphanumeric() || b"._/=-".contains(&b)) {
        return Err(Fail::invalid("castor-ssh: bad argument"));
    }
    Ok(s)
}

/// A path relative to the gate's ROOT: plain components only (no empty,
/// `.` or `..` components, not absolute, no trailing slash).
pub fn rel_path(s: &str) -> Result<&str> {
    let ok = !s.is_empty()
        && s.split('/')
            .all(|c| !c.is_empty() && c != "." && c != ".." && !c.contains(".."));
    if !ok {
        return Err(Fail::invalid("castor-ssh: bad path"));
    }
    Ok(s)
}

/// A 64-character hex dataset key as read from stdin (one trailing newline allowed).
pub fn hex_key(raw: &[u8]) -> Result<&[u8]> {
    let key = raw.strip_suffix(b"\n").unwrap_or(raw);
    if key.len() != 64 || !key.iter().all(u8::is_ascii_hexdigit) {
        return Err(Fail::invalid("the key must be 64 hex characters on stdin"));
    }
    Ok(key)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    #[test]
    fn slugs() {
        for ok in ["a", "anna-2", "0"] {
            assert!(slug(ok).is_ok(), "{ok}");
        }
        for bad in ["", "-rf", "A", "a b", "a/b", "a.b", "../x", "a;b", "ä"] {
            assert!(slug(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn snapshot_names() {
        assert!(snap_name("cb-20261016T181500Z").is_ok());
        for bad in ["cb-", "cb", "xb-1", "cb-a_b", "cb-a@b", "cb-a#b", "cb-a b", ""] {
            assert!(snap_name(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn sizes() {
        assert_eq!(mb("10240").unwrap(), 10240);
        for bad in ["", "-1", "1.5", "1M", "99999999999999999999999"] {
            assert!(mb(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn paths() {
        for ok in ["tenants/anna/.env", "tenants/anna/stack.yml", "a"] {
            assert!(rel_path(ok).is_ok(), "{ok}");
        }
        for bad in ["", "/etc/passwd", "a//b", "a/../b", "..", "a/..b", "a/", "./a", "a/./b"] {
            assert!(rel_path(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn gate_words() {
        for bad in ["a;b", "$(id)", "`id`", "a|b", "a b", "", "*", "~root", "a\nb"] {
            assert!(gate_word(bad).is_err(), "{bad:?}");
        }
        assert!(gate_word("tenants/anna/.env").is_ok());
    }

    #[test]
    fn keys() {
        let key = "ab".repeat(32);
        assert!(hex_key(key.as_bytes()).is_ok());
        assert!(hex_key(format!("{key}\n").as_bytes()).is_ok());
        assert!(hex_key(format!("{key}\n\n").as_bytes()).is_err());
        assert!(hex_key(&key.as_bytes()[..63]).is_err());
        assert!(hex_key("zz".repeat(32).as_bytes()).is_err());
        assert!(hex_key(b"").is_err());
    }
}
