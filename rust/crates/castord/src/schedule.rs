//! Recurring tasks, once per slot however many workers are up
//! (packages/core/src/schedule.ts): a worker runs a task only after moving its
//! nextRunAt forward in one UPDATE, and only one worker's UPDATE matches.
//!
//! Times of day are computed by Postgres, whose time zone data handles DST.

use sqlx::PgPool;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Every {
    /// Interval in milliseconds; a new task runs right away.
    Ms(i64),
    /// "HH:MM" in `tz`, on `weekday` only (0 = Sunday) when set; a new task
    /// runs first at its next slot.
    At {
        time: &'static str,
        tz: String,
        weekday: Option<i32>,
    },
}

/// The next run strictly after now, as a UTC timestamp without zone (the
/// column type): $2 interval ms, or $3 time of day, $4 zone, $5 weekday.
const NEXT: &str = r#"(CASE WHEN $2::bigint IS NOT NULL
        THEN now() AT TIME ZONE 'UTC' + make_interval(secs => $2::bigint / 1000.0)
        ELSE (SELECT min(t) AT TIME ZONE 'UTC' FROM (
                SELECT ((d + i) + $3::time) AT TIME ZONE $4::text AS t, extract(dow FROM d + i) AS dow
                FROM (SELECT (now() AT TIME ZONE $4::text)::date AS d) today, generate_series(0, 8) AS i
             ) c WHERE t > now() AND ($5::int IS NULL OR dow = $5::int))
     END)"#;

fn valid_time(t: &str) -> bool {
    let b = t.as_bytes();
    b.len() == 5
        && b[2] == b':'
        && b[..2].iter().chain(&b[3..]).all(u8::is_ascii_digit)
        && t[..2] < *"24"
        && t[3..] < *"60"
}

/// True when `name` is due and this worker won its slot (it then runs it).
pub async fn claim(db: &PgPool, name: &str, when: &Every) -> Result<bool, String> {
    let (ms, time, tz, weekday) = match when {
        Every::Ms(ms) if *ms > 0 => (Some(*ms), None, None, None),
        Every::Ms(ms) => return Err(format!("{name}: bad interval {ms}")),
        Every::At { time, tz, weekday } => {
            if !valid_time(time) || weekday.is_some_and(|w| !(0..=6).contains(&w)) {
                return Err(format!("{name}: bad time {time}"));
            }
            (None, Some(*time), Some(tz.as_str()), *weekday)
        }
    };
    let first = if ms.is_some() {
        "now() AT TIME ZONE 'UTC'".to_owned()
    } else {
        NEXT.to_owned()
    };
    sqlx::query(&format!(
        r#"INSERT INTO "Schedule" (name, "nextRunAt") VALUES ($1, {first}) ON CONFLICT (name) DO NOTHING"#
    ))
    .bind(name)
    .bind(ms)
    .bind(time)
    .bind(tz)
    .bind(weekday)
    .execute(db)
    .await
    .map_err(|e| format!("{name}: {e}"))?;
    let won = sqlx::query(&format!(
        r#"UPDATE "Schedule" SET "nextRunAt" = {NEXT}, "lastRunAt" = now() AT TIME ZONE 'UTC'
           WHERE name = $1 AND "nextRunAt" <= now() AT TIME ZONE 'UTC'"#
    ))
    .bind(name)
    .bind(ms)
    .bind(time)
    .bind(tz)
    .bind(weekday)
    .execute(db)
    .await
    .map_err(|e| format!("{name}: {e}"))?;
    Ok(won.rows_affected() == 1)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn times_of_day() {
        for t in ["00:00", "02:30", "23:59"] {
            assert!(valid_time(t), "{t}");
        }
        for t in ["24:00", "12:60", "2:30", "02:3", "aa:bb", "02-30"] {
            assert!(!valid_time(t), "{t}");
        }
    }
}
