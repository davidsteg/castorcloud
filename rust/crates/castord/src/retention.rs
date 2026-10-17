//! Which backups to keep: pure functions, so the policy is
//! tested apart from ZFS. The backup box enforces its own floor on top
//! (castor-backup: nothing younger than MIN_DAYS, never a tenant's newest).

use std::collections::{HashMap, HashSet};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Snapshot {
    pub gen_: String,
    pub snap: String,
    /// Seconds since the epoch.
    pub created: i64,
    pub used_bytes: u64,
}

#[derive(Debug, Clone, Copy)]
pub struct Policy {
    /// Keep every backup this recent.
    pub all_hours: i64,
    /// Then the newest backup of each of the last N days / ISO weeks / months that have one.
    pub daily: usize,
    pub weekly: usize,
    pub monthly: usize,
}

pub const DEFAULT: Policy = Policy {
    all_hours: 48,
    daily: 14,
    weekly: 8,
    monthly: 12,
};

/// (year, month, day) of a day count since 1970-01-01 (proleptic Gregorian).
pub fn civil(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    // In range by construction.
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    (y, m as u32, d as u32)
}

/// Day count since 1970-01-01 of a date.
pub fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y.rem_euclid(400);
    let m = i64::from(m);
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + i64::from(d) - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// ISO year and week of a day (the week's Thursday decides the year).
fn iso_week(days: i64) -> (i64, i64) {
    let dow = (days + 4).rem_euclid(7); // 0 = Sunday
    let thursday = days + 4 - if dow == 0 { 7 } else { dow };
    let (y, _, _) = civil(thursday);
    (y, (thursday - days_from_civil(y, 1, 1)) / 7 + 1)
}

/// Backup snapshot and bookmark name for a point in time: cb-20260928T0600Z.
pub fn snapshot_name(secs: i64) -> String {
    let (y, m, d) = civil(secs.div_euclid(86_400));
    let s = secs.rem_euclid(86_400);
    format!("cb-{y:04}{m:02}{d:02}T{:02}{:02}Z", s / 3600, s % 3600 / 60)
}

#[derive(Clone, Copy)]
enum Bucket {
    Day,
    Week,
    Month,
}

impl Bucket {
    /// The UTC day, ISO week or month of a time.
    fn of(self, secs: i64) -> (i64, i64, i64) {
        let day = secs.div_euclid(86_400);
        let (y, m, d) = civil(day);
        match self {
            Bucket::Day => (y, i64::from(m), i64::from(d)),
            Bucket::Week => {
                let (y, w) = iso_week(day);
                (y, w, 0)
            }
            Bucket::Month => (y, i64::from(m), 0),
        }
    }
}

/// The snapshots to delete, oldest first. The newest is always kept.
pub fn to_prune(snaps: &[Snapshot], now: i64, policy: Policy) -> Vec<Snapshot> {
    let mut newest_first: Vec<&Snapshot> = snaps.iter().collect();
    // Stable, as Array.prototype.sort: ties keep the box's order.
    newest_first.sort_by_key(|s| std::cmp::Reverse(s.created));
    let mut keep: HashSet<usize> = HashSet::new();
    if !newest_first.is_empty() {
        keep.insert(0);
    }
    for (i, s) in newest_first.iter().enumerate() {
        if now - s.created <= policy.all_hours * 3600 {
            keep.insert(i);
        }
    }
    for (n, bucket) in [
        (policy.daily, Bucket::Day),
        (policy.weekly, Bucket::Week),
        (policy.monthly, Bucket::Month),
    ] {
        let mut seen = HashSet::new();
        for (i, s) in newest_first.iter().enumerate() {
            if seen.len() >= n {
                break;
            }
            // The newest of its bucket.
            if seen.insert(bucket.of(s.created)) {
                keep.insert(i);
            }
        }
    }
    let mut out: Vec<Snapshot> = newest_first
        .iter()
        .enumerate()
        .filter(|(i, _)| !keep.contains(i))
        .map(|(_, s)| (*s).clone())
        .collect();
    out.reverse();
    out
}

fn generation(s: &str) -> bool {
    s.strip_prefix('g')
        .is_some_and(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()))
}

/// `castor-backup list` lines: "<slug> <gen> <snap> <created-epoch> <used-bytes>",
/// per tenant in the box's order (oldest first); slugs in first-seen order.
pub fn parse_list(text: &str) -> Vec<(String, Vec<Snapshot>)> {
    let mut out: Vec<(String, Vec<Snapshot>)> = Vec::new();
    let mut index: HashMap<String, usize> = HashMap::new();
    for line in text.lines() {
        let w: Vec<&str> = line.split_whitespace().collect();
        let (Some(slug), Some(g), Some(snap), Some(created)) = (w.first(), w.get(1), w.get(2), w.get(3)) else {
            continue;
        };
        if !generation(g) || !snap.starts_with("cb-") || !created.bytes().all(|b| b.is_ascii_digit()) {
            continue;
        }
        let Ok(created) = created.parse::<i64>() else {
            continue;
        };
        let s = Snapshot {
            gen_: (*g).to_owned(),
            snap: (*snap).to_owned(),
            created,
            used_bytes: w.get(4).and_then(|u| u.parse().ok()).unwrap_or(0),
        };
        let i = *index.entry((*slug).to_owned()).or_insert_with(|| {
            out.push(((*slug).to_owned(), Vec::new()));
            out.len() - 1
        });
        if let Some((_, list)) = out.get_mut(i) {
            list.push(s);
        }
    }
    out
}

/// Generation number of "g3" (0 when there is none).
pub fn gen_number(g: Option<&str>) -> i64 {
    g.and_then(|g| g.strip_prefix('g'))
        .and_then(|n| n.parse().ok())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    /// 2026-09-28T12:00:00Z
    const NOW: i64 = 1_790_596_800;

    fn every6h(days: i64) -> Vec<Snapshot> {
        (0..days * 24)
            .step_by(6)
            .map(|h| {
                let created = NOW - h * 3600;
                Snapshot {
                    gen_: "g1".into(),
                    snap: snapshot_name(created),
                    created,
                    used_bytes: 0,
                }
            })
            .collect()
    }

    #[test]
    fn dates() {
        assert_eq!(civil(0), (1970, 1, 1));
        assert_eq!(civil(NOW / 86_400), (2026, 9, 28));
        assert_eq!(days_from_civil(2026, 9, 28), NOW / 86_400);
        assert_eq!(days_from_civil(2024, 2, 29) + 1, days_from_civil(2024, 3, 1));
        // 2026-01-01 is a Thursday: week 1 of 2026; 2027-01-01 (Friday) is in 2026's week 53.
        assert_eq!(iso_week(days_from_civil(2026, 1, 1)), (2026, 1));
        assert_eq!(iso_week(days_from_civil(2027, 1, 1)), (2026, 53));
        assert_eq!(iso_week(days_from_civil(2024, 12, 30)), (2025, 1));
        assert_eq!(iso_week(NOW / 86_400), (2026, 40));
        assert_eq!(snapshot_name(NOW - 6 * 3600 + 5 * 60 + 59), "cb-20260928T0605Z");
    }

    #[test]
    fn keeps_the_last_48_hours() {
        assert!(to_prune(&every6h(2), NOW, DEFAULT).is_empty());
    }

    #[test]
    fn thins_older_backups() {
        let snaps = every6h(400);
        let pruned = to_prune(&snaps, NOW, DEFAULT);
        let kept: Vec<&Snapshot> = snaps.iter().filter(|s| !pruned.contains(s)).collect();
        assert!((9 + 12..=9 + 14 + 8 + 12).contains(&kept.len()), "kept {}", kept.len());
        assert_eq!(kept[0], &snaps[0]);
        assert!(NOW - kept.last().unwrap().created > 300 * 86_400);
        assert!(pruned.windows(2).all(|w| w[0].created <= w[1].created));
    }

    #[test]
    fn keeps_the_newest_however_old() {
        let s = Snapshot {
            gen_: "g1".into(),
            snap: "cb-20240101T0000Z".into(),
            created: days_from_civil(2024, 1, 1) * 86_400,
            used_bytes: 0,
        };
        assert!(to_prune(&[s], NOW, DEFAULT).is_empty());
    }

    #[test]
    fn newest_per_day_across_generations() {
        let at = |h: i64| days_from_civil(2026, 9, 20) * 86_400 + h * 3600;
        let a = Snapshot {
            gen_: "g1".into(),
            snap: "cb-20260920T0000Z".into(),
            created: at(0),
            used_bytes: 0,
        };
        let b = Snapshot {
            gen_: "g2".into(),
            snap: "cb-20260920T1800Z".into(),
            created: at(18),
            used_bytes: 0,
        };
        let policy = Policy {
            all_hours: 0,
            daily: 5,
            weekly: 0,
            monthly: 0,
        };
        assert_eq!(to_prune(&[a.clone(), b], NOW, policy), vec![a]);
    }

    #[test]
    fn parses_the_box_list() {
        let m = parse_list(
            "alpha g1 cb-20260928T0600Z 1790000000 4096\nalpha g2 cb-20260928T1200Z 1790021600 0\nbad line\nbeta gx cb-1 1 1\n",
        );
        assert_eq!(m.len(), 1);
        assert_eq!(m[0].0, "alpha");
        assert_eq!(m[0].1.len(), 2);
        assert_eq!(m[0].1[1].gen_, "g2");
        assert_eq!(m[0].1[0].used_bytes, 4096);
        assert_eq!(gen_number(Some("g12")), 12);
        assert_eq!(gen_number(None), 0);
    }
}
