//! Timeline (Dawarich). Answers "where was I" with visits and city-level
//! stretches of time, never with raw GPS tracks.

use std::time::Duration;

use serde::Deserialize;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};

use super::{Ctx, lenient_str};
use crate::db::{Db, sql_error};
use crate::limits::{
    Out, ToolError, Zone, clamp_limit, js_round, js_value, round_coord, to_number,
};
use crate::members::Member;
use crate::schema::{Args, Field, Tool, limit, string};
use crate::upstream::{Request, TtlCache, get_json};

const LABEL: &str = "Timeline";

#[derive(Deserialize, Default)]
struct Place {
    #[serde(default)]
    latitude: Option<Value>,
    #[serde(default)]
    longitude: Option<Value>,
}

#[derive(Deserialize)]
struct Visit {
    started_at: String,
    ended_at: String,
    #[serde(default)]
    duration: Option<Value>,
    #[serde(default, deserialize_with = "lenient_str")]
    name: Option<String>,
    #[serde(default, deserialize_with = "lenient_str")]
    status: Option<String>,
    #[serde(default)]
    place: Option<Place>,
}

#[derive(Deserialize, Debug, Clone)]
pub struct Point {
    pub timestamp: f64,
    #[serde(default, deserialize_with = "lenient_str")]
    pub city: Option<String>,
    #[serde(default, deserialize_with = "lenient_str")]
    pub country_name: Option<String>,
    #[serde(default, deserialize_with = "lenient_str")]
    pub country: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct YearStats {
    #[serde(default)]
    year: Option<Value>,
    #[serde(default)]
    total_distance_km: Option<Value>,
    #[serde(default)]
    total_countries_visited: Option<Value>,
    #[serde(default)]
    total_cities_visited: Option<Value>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Stats {
    #[serde(default)]
    total_distance_km: Option<Value>,
    #[serde(default)]
    total_countries_visited: Option<Value>,
    #[serde(default)]
    total_cities_visited: Option<Value>,
    yearly_stats: Vec<YearStats>,
}

/// Consecutive points in the same city become one stretch of time.
pub fn stretches(points: &[Point], max: usize, zone: &Zone) -> Result<Vec<Value>, ToolError> {
    let mut sorted: Vec<&Point> = points.iter().collect();
    sorted.sort_by(|a, b| {
        a.timestamp
            .partial_cmp(&b.timestamp)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    let mut out: Vec<(String, String, String)> = Vec::new();
    for p in sorted {
        let country = p.country_name.as_ref().or(p.country.as_ref());
        let parts: Vec<&str> = [p.city.as_ref(), country]
            .into_iter()
            .flatten()
            .map(String::as_str)
            .filter(|s| !s.is_empty())
            .collect();
        if parts.is_empty() {
            continue;
        }
        let place = parts.join(", ");
        let at = zone.local_time_secs(p.timestamp)?;
        match out.last_mut() {
            Some(last) if last.0 == place => last.2 = at,
            _ => out.push((place, at.clone(), at)),
        }
    }
    if out.len() > max {
        let last = out.pop();
        out.truncate(max.saturating_sub(1));
        out.extend(last);
    }
    Ok(out
        .into_iter()
        .map(|(place, from, to)| json!({ "place": place, "from": from, "to": to }))
        .collect())
}

pub struct Places {
    url: String,
    db: Db,
    keys: TtlCache<String>,
}

struct Range {
    from: (String, i64),
    to: (String, i64),
}

impl Places {
    pub fn new(url: String, db_url: String) -> Self {
        Self {
            url,
            db: Db::new(db_url),
            keys: TtlCache::new(Duration::from_secs(3600)),
        }
    }

    pub fn tools() -> Vec<Tool> {
        let date_args = || -> Vec<Field> {
            vec![
                string("date", None, Some("A single day (YYYY-MM-DD)")),
                string("from", None, Some("Start (YYYY-MM-DD)")),
                string("to", None, Some("End (YYYY-MM-DD)")),
            ]
        };
        let mut visits = date_args();
        visits.push(limit());
        vec![
            Tool {
                name: "where_was_i",
                title: "Where was I",
                description: "Where the user was during a day or period, from their location timeline: the cities/countries \
                    with time ranges, plus confirmed visits to named places."
                    .into(),
                fields: date_args(),
            },
            Tool {
                name: "list_visits",
                title: "List visits",
                description: "Visits to places (restaurants, hotels, friends' homes, ...) detected in the timeline for a period.".into(),
                fields: visits,
            },
            Tool {
                name: "travel_stats",
                title: "Travel statistics",
                description: "Distance travelled and number of countries/cities visited, in total and per year.".into(),
                fields: Vec::new(),
            },
        ]
    }

    /// Dawarich keeps one API key per user in plain text (users.api_key).
    async fn api_key(&self, member: &Member) -> Result<String, ToolError> {
        self.keys
            .get(&member.id, || async {
                let key: Option<Option<String>> = sqlx::query_scalar("SELECT api_key::text FROM users WHERE lower(email) = lower($1)")
                    .bind(&member.email)
                    .fetch_optional(self.db.pool().await?)
                    .await
                    .map_err(sql_error)?;
                key.flatten().filter(|k| !k.is_empty()).ok_or_else(|| {
                    ToolError::user("Timeline has not been opened by this person yet, so there is nothing to search")
                })
            })
            .await
    }

    async fn dawarich<T: DeserializeOwned>(
        &self,
        ctx: &Ctx<'_>,
        r: Request<'_>,
    ) -> Result<T, ToolError> {
        let key = self.api_key(ctx.member).await?;
        let res = get_json(ctx.http, r.header("authorization", format!("Bearer {key}"))).await;
        if res.is_err() {
            self.keys.delete(&ctx.member.id);
        }
        res
    }

    fn request<'a>(&'a self, path: &'a str) -> Request<'a> {
        Request::get(&self.url, path, LABEL)
    }

    fn range(zone: &Zone, args: &Args) -> Result<Range, ToolError> {
        // `date ?? from`: an empty date still wins.
        let date = args.str("date");
        let from = zone.iso_date(date.or(args.str("from")), false)?;
        let to = zone.iso_date(date.or(args.str("to")), true)?;
        let (Some(from), Some(to)) = (from, to) else {
            return Err(ToolError::user(
                "give a date, or both from and to (YYYY-MM-DD)",
            ));
        };
        if to.1 - from.1 > 400 * 86_400_000 {
            return Err(ToolError::user("ask about at most about a year at a time"));
        }
        Ok(Range { from, to })
    }

    fn visits_request<'a>(&'a self, r: &Range) -> Request<'a> {
        self.request("/api/v1/visits")
            .query("start_at", r.from.0.clone())
            .query("end_at", r.to.0.clone())
    }

    pub async fn call(&self, ctx: &Ctx<'_>, name: &str, args: &Args) -> Result<Value, ToolError> {
        let zone = ctx.zone;
        match name {
            "where_was_i" => {
                let r = Self::range(zone, args)?;
                let start = r.from.1.div_euclid(1000);
                let end = r.to.1.div_euclid(1000);
                let points_req = self
                    .request("/api/v1/points")
                    .query("start_at", start.to_string())
                    .query("end_at", end.to_string())
                    .query("per_page", "5000".to_owned())
                    .query("order", "asc".to_owned());
                let (points, visits): (Vec<Point>, Vec<Visit>) = tokio::try_join!(
                    self.dawarich(ctx, points_req),
                    self.dawarich(ctx, self.visits_request(&r))
                )?;
                let visits = visits
                    .iter()
                    .filter(|v| v.status.as_deref() != Some("declined"))
                    .take(25)
                    .map(|v| {
                        Ok(Out::new()
                            .opt("name", v.name.clone())
                            .set("from", zone.local_time(&v.started_at)?)
                            .set("to", zone.local_time(&v.ended_at)?)
                            .compact())
                    })
                    .collect::<Result<Vec<_>, ToolError>>()?;
                let link = format!(
                    "{}/places/map?start_at={}&end_at={}",
                    ctx.portal_origin,
                    zone.local_day(&r.from.0)?,
                    zone.local_day(&r.to.0)?
                );
                Ok(Out::new()
                    .set("places", stretches(&points, 25, zone)?)
                    .set("visits", visits)
                    .opt(
                        "note",
                        points
                            .is_empty()
                            .then_some("no location data recorded in this period"),
                    )
                    .set("link", link)
                    .build())
            }
            "list_visits" => {
                let r = Self::range(zone, args)?;
                let visits: Vec<Visit> = self.dawarich(ctx, self.visits_request(&r)).await?;
                let items = visits
                    .iter()
                    .filter(|v| v.status.as_deref() != Some("declined"))
                    .take(clamp_limit(args.int("limit"), 10))
                    .map(|v| {
                        let place = v.place.as_ref();
                        Ok(Out::new()
                            .opt("name", v.name.clone())
                            .set("from", zone.local_time(&v.started_at)?)
                            .set("to", zone.local_time(&v.ended_at)?)
                            .opt("minutes", v.duration.as_ref().map(js_value))
                            .opt("lat", round_coord(place.and_then(|p| p.latitude.as_ref())))
                            .opt("lon", round_coord(place.and_then(|p| p.longitude.as_ref())))
                            .compact())
                    })
                    .collect::<Result<Vec<_>, ToolError>>()?;
                Ok(json!({ "count": items.len(), "items": items }))
            }
            _ => {
                let s: Stats = self.dawarich(ctx, self.request("/api/v1/stats")).await?;
                let round = |v: &Option<Value>| js_round(to_number(v.as_ref()));
                let years: Vec<Value> = s
                    .yearly_stats
                    .iter()
                    .take(10)
                    .map(|y| {
                        Out::new()
                            .opt("year", y.year.as_ref().map(js_value))
                            .num("km", round(&y.total_distance_km))
                            .opt(
                                "countries",
                                y.total_countries_visited.as_ref().map(js_value),
                            )
                            .opt("cities", y.total_cities_visited.as_ref().map(js_value))
                            .build()
                    })
                    .collect();
                Ok(Out::new()
                    .num("totalKm", round(&s.total_distance_km))
                    .opt(
                        "countries",
                        s.total_countries_visited.as_ref().map(js_value),
                    )
                    .opt("cities", s.total_cities_visited.as_ref().map(js_value))
                    .set("years", years)
                    .build())
            }
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    fn t(iso: &str) -> f64 {
        Zone::new("UTC").parse(iso).unwrap() as f64 / 1000.0
    }

    fn point(ts: &str, city: Option<&str>, country: Option<&str>) -> Point {
        Point {
            timestamp: t(ts),
            city: city.map(Into::into),
            country_name: country.map(Into::into),
            country: None,
        }
    }

    #[test]
    fn location_points_become_city_stretches() {
        let zone = Zone::new("Europe/Zurich");
        let out = stretches(
            &[
                point("2026-05-01T09:00Z", Some("Zürich"), Some("Switzerland")),
                point("2026-05-01T08:00Z", Some("Zürich"), Some("Switzerland")),
                point("2026-05-01T12:00Z", None, None),
                point("2026-05-01T15:00Z", Some("Lisbon"), Some("Portugal")),
            ],
            25,
            &zone,
        )
        .unwrap();
        assert_eq!(
            Value::Array(out),
            json!([
                { "place": "Zürich, Switzerland", "from": "2026-05-01T10:00", "to": "2026-05-01T11:00" },
                { "place": "Lisbon, Portugal", "from": "2026-05-01T17:00", "to": "2026-05-01T17:00" },
            ])
        );
    }

    #[test]
    fn long_timelines_keep_the_first_and_the_last_stretch() {
        let zone = Zone::new("UTC");
        let mut points: Vec<Point> = (0..30)
            .map(|i| Point {
                timestamp: f64::from(i) * 3600.0,
                city: Some(format!("T{i}")),
                country_name: None,
                country: None,
            })
            .collect();
        points.push(Point {
            timestamp: 200_000.0,
            city: None,
            country_name: None,
            country: Some("Switzerland".into()),
        });
        let out = stretches(&points, 25, &zone).unwrap();
        assert_eq!(out.len(), 25);
        assert_eq!(out[23]["place"], "T23");
        assert_eq!(out[24]["place"], "Switzerland");
    }
}
