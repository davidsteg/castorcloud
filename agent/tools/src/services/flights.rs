//! Flights (AirTrail). Uses AirTrail's REST API v1 with a per-member key that
//! only has read scopes.

use std::time::Duration;

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use serde::Deserialize;
use serde::de::DeserializeOwned;
use serde_json::Value;

use super::{Ctx, lenient_str};
use crate::db::{Db, KEY_NAME, new_api_key, sha256, sql_error};
use crate::limits::{Out, ToolError, clamp_limit, js_round, js_string, js_value, to_number};
use crate::members::Member;
use crate::schema::{Args, Tool, limit, string};
use crate::upstream::{Request, TtlCache, get_json};

const LABEL: &str = "Flights";

pub const AIRTRAIL_SCOPES: [&str; 4] = [
    "profile.read",
    "flight.read.own",
    "stats.read",
    "reference_data.read",
];

/// The scopes go in as an untyped array literal (see photos.rs).
fn insert_key() -> String {
    format!(
        r#"INSERT INTO api_key (name, user_id, key, scopes)
           VALUES ($1, (SELECT id FROM "user" WHERE id::text = $2), $3, '{{{}}}')"#,
        AIRTRAIL_SCOPES.join(",")
    )
}

#[derive(Deserialize, Debug, Clone)]
pub struct Airport {
    #[serde(default, deserialize_with = "lenient_str")]
    pub iata: Option<String>,
    pub icao: String,
    pub name: String,
    #[serde(default, deserialize_with = "lenient_str")]
    pub municipality: Option<String>,
    pub country: String,
}

#[derive(Deserialize, Debug, Clone)]
pub struct Named {
    pub name: String,
}

#[derive(Deserialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
pub struct Passenger {
    #[serde(default, deserialize_with = "lenient_str")]
    pub seat_class: Option<String>,
    #[serde(default, deserialize_with = "lenient_str")]
    pub seat: Option<String>,
}

#[derive(Deserialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
pub struct Flight {
    pub date: String,
    #[serde(default, deserialize_with = "lenient_str")]
    pub departure: Option<String>,
    #[serde(default)]
    pub duration: Option<f64>,
    #[serde(default, deserialize_with = "lenient_str")]
    pub flight_number: Option<String>,
    #[serde(default)]
    pub from: Option<Airport>,
    #[serde(default)]
    pub to: Option<Airport>,
    #[serde(default)]
    pub airline: Option<Named>,
    #[serde(default)]
    pub aircraft: Option<Named>,
    #[serde(default)]
    pub passengers: Option<Vec<Passenger>>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct PageInfo {
    #[serde(default, deserialize_with = "lenient_str")]
    next_cursor: Option<String>,
}

#[derive(Deserialize)]
struct FlightPage {
    data: Vec<Flight>,
    page: PageInfo,
}

#[derive(Deserialize)]
struct Ref {
    #[serde(default, deserialize_with = "lenient_str")]
    name: Option<String>,
    #[serde(default, deserialize_with = "lenient_str")]
    iata: Option<String>,
    #[serde(default, deserialize_with = "lenient_str")]
    icao: Option<String>,
}

impl Ref {
    fn code(&self) -> String {
        self.iata
            .clone()
            .or_else(|| self.icao.clone())
            .or_else(|| self.name.clone())
            .unwrap_or_else(|| "undefined".into())
    }
}

#[derive(Deserialize)]
struct Route {
    from: Ref,
    to: Ref,
    #[serde(default)]
    count: Option<Value>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Stats {
    #[serde(default)]
    flights: Option<Value>,
    #[serde(default)]
    distance_km: Option<Value>,
    #[serde(default)]
    duration_seconds: Option<Value>,
    #[serde(default)]
    airports: Option<Value>,
    #[serde(default)]
    top_airline: Option<Ref>,
    #[serde(default)]
    top_airport: Option<Ref>,
    #[serde(default)]
    top_route: Option<Route>,
}

#[derive(Deserialize)]
struct StatsAnswer {
    #[serde(default)]
    data: Option<Stats>,
}

fn airport(a: Option<&Airport>) -> Option<String> {
    a.map(|a| {
        format!(
            "{} ({}, {})",
            a.municipality.as_ref().unwrap_or(&a.name),
            a.iata.as_ref().unwrap_or(&a.icao),
            a.country
        )
    })
}

pub fn summarize_flight(f: &Flight, ctx: &Ctx) -> Result<Value, ToolError> {
    let first = f.passengers.as_ref().and_then(|p| p.first());
    let departure = match f.departure.as_deref() {
        Some(d) if !d.is_empty() => Some(ctx.zone.local_time(d)?),
        _ => None,
    };
    let minutes = f
        .duration
        .filter(|d| *d != 0.0 && !d.is_nan())
        .map(|d| js_round(d / 60.0));
    let mut out = Out::new()
        .set("date", f.date.clone())
        .opt("from", airport(f.from.as_ref()))
        .opt("to", airport(f.to.as_ref()))
        .opt("flight", f.flight_number.clone())
        .opt("airline", f.airline.as_ref().map(|a| a.name.clone()))
        .opt("aircraft", f.aircraft.as_ref().map(|a| a.name.clone()))
        .opt("departure", departure);
    if let Some(m) = minutes {
        out = out.num("minutes", m);
    }
    Ok(out
        .opt("seat", first.and_then(|p| p.seat.clone()))
        .opt("class", first.and_then(|p| p.seat_class.clone()))
        .compact())
}

fn matches_airport(a: Option<&Airport>, q: &str) -> bool {
    a.is_some_and(|a| {
        [
            a.iata.as_deref(),
            Some(a.icao.as_str()),
            Some(a.name.as_str()),
            a.municipality.as_deref(),
            Some(a.country.as_str()),
        ]
        .into_iter()
        .flatten()
        .any(|v| v.to_lowercase().contains(q))
    })
}

/// Flights on or after `from`, on or before `to` (calendar days) and from or
/// to an airport matching `airport`; empty filters match everything.
pub fn filter_flights<'a>(
    flights: &'a [Flight],
    from: Option<&str>,
    to: Option<&str>,
    airport: Option<&str>,
) -> Vec<&'a Flight> {
    let q = airport.map(str::to_lowercase).filter(|q| !q.is_empty());
    let from = from.filter(|f| !f.is_empty());
    let to = to.filter(|t| !t.is_empty());
    flights
        .iter()
        .filter(|f| {
            from.is_none_or(|from| f.date.as_str() >= from)
                && to.is_none_or(|to| f.date.as_str() <= to)
                && q.as_deref().is_none_or(|q| {
                    matches_airport(f.from.as_ref(), q) || matches_airport(f.to.as_ref(), q)
                })
        })
        .collect()
}

pub struct Flights {
    url: String,
    db: Db,
    keys: TtlCache<String>,
}

impl Flights {
    pub fn new(url: String, db_url: String) -> Self {
        Self {
            url,
            db: Db::new(db_url),
            keys: TtlCache::new(Duration::from_secs(12 * 3600)),
        }
    }

    pub fn tools() -> Vec<Tool> {
        vec![
            Tool {
                name: "list_flights",
                title: "List flights",
                description: "The user's flights, newest first, optionally filtered by date range and by an airport, city or country \
                    (matches departure or arrival)."
                    .into(),
                fields: vec![
                    string("from", None, Some("On or after (YYYY-MM-DD)")),
                    string("to", None, Some("On or before (YYYY-MM-DD)")),
                    string("airport", Some(80), Some("IATA/ICAO code, airport, city or country")),
                    limit(),
                ],
            },
            Tool {
                name: "flight_stats",
                title: "Flight statistics",
                description: "Totals over all of the user's flights: count, distance, time in the air, airports, top route/airline.".into(),
                fields: Vec::new(),
            },
        ]
    }

    /// Issues a read-only AirTrail key. AirTrail links SSO accounts by the OIDC
    /// subject and stores base64(SHA-256(key)) (see src/lib/server/utils/hash.ts;
    /// it NFKC-normalizes first, a no-op on our base64url keys).
    async fn api_key(&self, member: &Member) -> Result<String, ToolError> {
        self.keys
            .get(&member.id, || async {
                let db = self.db.pool().await?;
                let user: Option<String> = sqlx::query_scalar(r#"SELECT id::text FROM "user" WHERE oauth_id = $1"#)
                    .bind(&member.sub)
                    .fetch_optional(db)
                    .await
                    .map_err(sql_error)?;
                let Some(user_id) = user else {
                    return Err(ToolError::user("Flights has not been opened by this person yet, so there is nothing to search"));
                };
                let key = new_api_key()?;
                let mut tx = db.begin().await.map_err(sql_error)?;
                sqlx::query(r#"DELETE FROM api_key WHERE user_id = (SELECT id FROM "user" WHERE id::text = $1) AND name = $2"#)
                    .bind(&user_id)
                    .bind(KEY_NAME)
                    .execute(&mut *tx)
                    .await
                    .map_err(sql_error)?;
                sqlx::query(&insert_key())
                    .bind(KEY_NAME)
                    .bind(&user_id)
                    .bind(STANDARD.encode(sha256(&key)))
                    .execute(&mut *tx)
                    .await
                    .map_err(sql_error)?;
                tx.commit().await.map_err(sql_error)?;
                Ok(key)
            })
            .await
    }

    async fn airtrail<T: DeserializeOwned>(
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

    pub async fn call(&self, ctx: &Ctx<'_>, name: &str, args: &Args) -> Result<Value, ToolError> {
        if name == "list_flights" {
            let mut all: Vec<Flight> = Vec::new();
            let mut cursor: Option<String> = None;
            // A frequent flyer has a few hundred flights; stop at 1000.
            for _ in 0..10 {
                let r = Request::get(&self.url, "/api/v1/flights", LABEL)
                    .query("scope", "mine".to_owned())
                    .query("limit", "100".to_owned())
                    .query("cursor", cursor.clone());
                let page: FlightPage = self.airtrail(ctx, r).await?;
                all.extend(page.data);
                match page.page.next_cursor.filter(|c| !c.is_empty()) {
                    Some(c) => cursor = Some(c),
                    None => break,
                }
            }
            // Flight dates are calendar days.
            let day = |v: Option<&str>, end: bool| -> Result<Option<String>, ToolError> {
                match v {
                    Some("") => Ok(None),
                    v => ctx
                        .zone
                        .iso_date(v, end)?
                        .map(|(iso, _)| ctx.zone.local_day(&iso))
                        .transpose(),
                }
            };
            let from = day(args.str("from"), false)?;
            let to = day(args.str("to"), true)?;
            let mut matched =
                filter_flights(&all, from.as_deref(), to.as_deref(), args.str("airport"));
            matched.sort_by(|a, b| b.date.cmp(&a.date));
            let items = matched
                .iter()
                .take(clamp_limit(args.int("limit"), 10))
                .map(|f| summarize_flight(f, ctx))
                .collect::<Result<Vec<_>, _>>()?;
            return Ok(Out::new()
                .set("total", matched.len())
                .set("items", items)
                .set("link", format!("{}/flights/", ctx.portal_origin))
                .build());
        }
        let r = Request::get(&self.url, "/api/v1/stats", LABEL).query("scope", "mine".to_owned());
        let answer: StatsAnswer = self.airtrail(ctx, r).await?;
        let Some(data) = answer.data else {
            return Err(ToolError::user("no statistics yet"));
        };
        let hours = js_round(to_number(data.duration_seconds.as_ref()) / 360.0) / 10.0;
        Ok(Out::new()
            .opt("flights", data.flights.as_ref().map(js_value))
            .opt("distanceKm", data.distance_km.as_ref().map(js_value))
            .num("hoursInAir", hours)
            .opt("airports", data.airports.as_ref().map(js_value))
            .opt("topAirline", data.top_airline.and_then(|a| a.name))
            .opt(
                "topAirport",
                data.top_airport.map(|a| {
                    format!(
                        "{} ({})",
                        a.name.as_deref().unwrap_or("undefined"),
                        a.code()
                    )
                }),
            )
            .opt(
                "topRoute",
                data.top_route.map(|r| {
                    format!(
                        "{} – {} ({}×)",
                        r.from.code(),
                        r.to.code(),
                        js_string(r.count.as_ref())
                    )
                }),
            )
            .compact())
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use crate::services::tests::with_ctx;
    use serde_json::json;

    fn flights() -> Vec<Flight> {
        let lis = json!({ "iata": "LIS", "icao": "LPPT", "name": "Humberto Delgado", "municipality": "Lisbon", "country": "PT" });
        let zrh = json!({ "iata": "ZRH", "icao": "LSZH", "name": "Zurich Airport", "municipality": "Zurich", "country": "CH" });
        let mut opo = lis.clone();
        opo["iata"] = "OPO".into();
        opo["municipality"] = "Porto".into();
        serde_json::from_value(json!([
            { "id": 1, "date": "2026-05-01", "from": zrh, "to": lis, "flightNumber": "LX2084", "duration": 9900,
              "airline": { "name": "Swiss" }, "departure": "2026-05-01T11:40:00.000Z" },
            { "id": 2, "date": "2025-01-10", "from": zrh, "to": opo },
        ]))
        .unwrap()
    }

    #[test]
    fn flights_are_filtered_by_date_and_airport_and_summarized() {
        let f = flights();
        let dates = |v: Vec<&Flight>| v.iter().map(|f| f.date.clone()).collect::<Vec<_>>();
        assert_eq!(
            dates(filter_flights(&f, None, None, Some("lisbon"))),
            ["2026-05-01"]
        );
        assert_eq!(
            dates(filter_flights(&f, Some("2026-01-01"), None, None)),
            ["2026-05-01"]
        );
        assert_eq!(
            dates(filter_flights(&f, None, Some("2025-12-31"), None)),
            ["2025-01-10"]
        );
        assert_eq!(
            dates(filter_flights(&f, None, None, Some("CH"))),
            ["2026-05-01", "2025-01-10"]
        );
        assert_eq!(
            dates(filter_flights(&f, Some(""), None, Some(""))),
            ["2026-05-01", "2025-01-10"]
        );
        with_ctx("https://muster.castorcloud.ch", |ctx| {
            assert_eq!(
                summarize_flight(&f[0], ctx).unwrap(),
                json!({
                    "date": "2026-05-01", "from": "Zurich (ZRH, CH)", "to": "Lisbon (LIS, PT)", "flight": "LX2084",
                    "airline": "Swiss", "departure": "2026-05-01T13:40", "minutes": 165, // Zurich time
                })
            );
            // Key order as in the TypeScript version.
            assert_eq!(
                summarize_flight(&f[0], ctx).unwrap().to_string(),
                r#"{"date":"2026-05-01","from":"Zurich (ZRH, CH)","to":"Lisbon (LIS, PT)","flight":"LX2084","airline":"Swiss","departure":"2026-05-01T13:40","minutes":165}"#
            );
        });
    }

    #[test]
    fn the_key_insert_grants_exactly_the_read_scopes() {
        assert!(
            insert_key()
                .ends_with("'{profile.read,flight.read.own,stats.read,reference_data.read}')")
        );
        assert!(AIRTRAIL_SCOPES.iter().all(|p| {
            p.bytes()
                .all(|b| b.is_ascii_lowercase() || b == b'.' || b == b'_')
        }));
    }
}
