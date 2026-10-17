//! Photos (Immich). Search runs inside Immich: text queries use its own CLIP
//! model on the node, so no image ever leaves the stack. Answers are metadata
//! only (date, place, people, caption) plus a link to open the photo.

use std::time::Duration;

use serde::Deserialize;
use serde_json::{Value, json};

use super::{Ctx, lenient_str};
use crate::db::{Db, KEY_NAME, new_api_key, sha256, sql_error};
use crate::limits::{Out, ToolError, clamp_limit, excerpt, js_slice};
use crate::members::Member;
use crate::schema::{Args, Tool, limit, string};
use crate::upstream::{Request, TtlCache, get_json};

const LABEL: &str = "Photos";

/// Enough to search and name people; nothing that changes or downloads.
pub const IMMICH_PERMISSIONS: [&str; 3] = ["asset.read", "person.read", "album.read"];

/// The permissions go in as an untyped array literal (constants, never
/// input), so Postgres reads it as whatever array type the column has.
fn insert_key() -> String {
    format!(
        r#"INSERT INTO api_key (name, key, "userId", permissions)
           VALUES ($1, $2, (SELECT id FROM "user" WHERE id::text = $3), '{{{}}}')"#,
        IMMICH_PERMISSIONS.join(",")
    )
}

#[derive(Deserialize, Default)]
#[serde(rename_all = "camelCase")]
struct Exif {
    #[serde(default, deserialize_with = "lenient_str")]
    city: Option<String>,
    #[serde(default, deserialize_with = "lenient_str")]
    country: Option<String>,
    #[serde(default, deserialize_with = "lenient_str")]
    description: Option<String>,
}

#[derive(Deserialize)]
struct Named {
    #[serde(default, deserialize_with = "lenient_str")]
    name: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Asset {
    id: String,
    #[serde(default, deserialize_with = "lenient_str", rename = "type")]
    kind: Option<String>,
    #[serde(default, deserialize_with = "lenient_str")]
    local_date_time: Option<String>,
    #[serde(default, deserialize_with = "lenient_str")]
    file_created_at: Option<String>,
    #[serde(default)]
    is_favorite: Option<bool>,
    #[serde(default)]
    exif_info: Option<Exif>,
    #[serde(default)]
    people: Option<Vec<Named>>,
}

#[derive(Deserialize)]
struct Items {
    items: Vec<Asset>,
}

#[derive(Deserialize)]
struct SearchResult {
    assets: Items,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Person {
    id: String,
    #[serde(default, deserialize_with = "lenient_str")]
    name: Option<String>,
    #[serde(default)]
    is_hidden: Option<bool>,
}

#[derive(Deserialize)]
struct People {
    people: Vec<Person>,
}

pub struct Photos {
    url: String,
    db: Db,
    keys: TtlCache<String>,
}

pub fn summarize_asset(a: &Asset, ctx: &Ctx) -> Result<Value, ToolError> {
    let e = a.exif_info.as_ref();
    // localDateTime is the wall-clock time where the photo was taken.
    let taken = match (&a.local_date_time, &a.file_created_at) {
        (Some(l), _) => Some(js_slice(l, 0, 16)),
        (None, Some(f)) if !f.is_empty() => Some(ctx.zone.local_time(f)?),
        _ => None,
    };
    let place: Vec<&str> = e
        .into_iter()
        .flat_map(|e| [&e.city, &e.country])
        .filter_map(|v| v.as_deref().filter(|s| !s.is_empty()))
        .collect();
    let people: Option<Vec<&str>> = a.people.as_ref().map(|p| {
        p.iter()
            .filter_map(|p| p.name.as_deref().filter(|n| !n.is_empty()))
            .collect()
    });
    Ok(Out::new()
        .opt("taken", taken)
        .opt(
            "type",
            (a.kind.as_deref() == Some("VIDEO")).then_some("video"),
        )
        .opt("place", (!place.is_empty()).then(|| place.join(", ")))
        .opt(
            "caption",
            excerpt(e.and_then(|e| e.description.as_deref()), 160),
        )
        .opt("people", people)
        .opt("favorite", a.is_favorite.filter(|f| *f))
        .set(
            "link",
            format!("{}/photos/photos/{}", ctx.portal_origin, a.id),
        )
        .compact())
}

impl Photos {
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
                name: "search_photos",
                title: "Search photos",
                description: "Find the user's photos and videos. `query` is a free-text description of what is in the picture \
                    (e.g. 'dog on the beach', 'birthday cake'); combine with person, place and date filters. \
                    Returns date, place, people and a link per item, never the image itself."
                    .into(),
                fields: vec![
                    string("query", Some(200), Some("What the photo shows")),
                    string("person", Some(100), Some("Name of a person recognised in Photos")),
                    string("city", Some(100), None),
                    string("country", Some(100), None),
                    string("from", None, Some("Taken on or after (YYYY-MM-DD)")),
                    string("to", None, Some("Taken on or before (YYYY-MM-DD)")),
                    limit(),
                ],
            },
            Tool {
                name: "list_people",
                title: "List people in photos",
                description: "Names of the people the user has named in Photos (face recognition runs locally).".into(),
                fields: vec![limit()],
            },
        ]
    }

    /// Issues the member a read-only Immich API key (replacing the previous
    /// one). Immich stores the SHA-256 digest of the key (see its auth.service.ts).
    async fn api_key(&self, member: &Member) -> Result<String, ToolError> {
        self.keys
            .get(&member.id, || async {
                let db = self.db.pool().await?;
                let user: Option<String> =
                    sqlx::query_scalar(r#"SELECT id::text FROM "user" WHERE lower(email) = lower($1) AND "deletedAt" IS NULL"#)
                        .bind(&member.email)
                        .fetch_optional(db)
                        .await
                        .map_err(sql_error)?;
                let Some(user_id) = user else {
                    return Err(ToolError::user("Photos has not been opened by this person yet, so there is nothing to search"));
                };
                let key = new_api_key()?;
                let mut tx = db.begin().await.map_err(sql_error)?;
                sqlx::query(r#"DELETE FROM api_key WHERE "userId" = (SELECT id FROM "user" WHERE id::text = $1) AND name = $2"#)
                    .bind(&user_id)
                    .bind(KEY_NAME)
                    .execute(&mut *tx)
                    .await
                    .map_err(sql_error)?;
                sqlx::query(&insert_key())
                    .bind(KEY_NAME)
                    .bind(sha256(&key).to_vec())
                    .bind(&user_id)
                    .execute(&mut *tx)
                    .await
                    .map_err(sql_error)?;
                tx.commit().await.map_err(sql_error)?;
                Ok(key)
            })
            .await
    }

    async fn immich<T: serde::de::DeserializeOwned>(
        &self,
        ctx: &Ctx<'_>,
        r: Request<'_>,
    ) -> Result<T, ToolError> {
        let key = self.api_key(ctx.member).await?;
        let res = get_json(ctx.http, r.header("x-api-key", key)).await;
        if res.is_err() {
            self.keys.delete(&ctx.member.id); // a revoked key is re-issued on the next call
        }
        res
    }

    fn request<'a>(&'a self, path: &'a str) -> Request<'a> {
        Request::get(&self.url, path, LABEL)
    }

    pub async fn call(&self, ctx: &Ctx<'_>, name: &str, args: &Args) -> Result<Value, ToolError> {
        match name {
            "search_photos" => self.search(ctx, args).await,
            _ => {
                let res: People = self
                    .immich(
                        ctx,
                        self.request("/api/people")
                            .query("size", "100".to_owned())
                            .query("withHidden", "false".to_owned()),
                    )
                    .await?;
                let named: Vec<String> = res
                    .people
                    .into_iter()
                    .filter(|p| p.is_hidden != Some(true))
                    .filter_map(|p| p.name.filter(|n| !n.is_empty()))
                    .take(clamp_limit(args.int("limit"), 25))
                    .collect();
                Ok(json!({ "people": named }))
            }
        }
    }

    async fn search(&self, ctx: &Ctx<'_>, args: &Args) -> Result<Value, ToolError> {
        let size = clamp_limit(args.int("limit"), 10);
        let mut person_ids = None;
        if let Some(person) = args.str("person").filter(|p| !p.is_empty()) {
            let found: Vec<Person> = self
                .immich(
                    ctx,
                    self.request("/api/search/person")
                        .query("name", person.to_owned()),
                )
                .await?;
            if found.is_empty() {
                return Err(ToolError::user(format!(
                    "no person named \"{person}\" in Photos"
                )));
            }
            person_ids = Some(found.into_iter().take(3).map(|p| p.id).collect::<Vec<_>>());
        }
        let mut filters = Out::new()
            .opt("city", args.str("city"))
            .opt("country", args.str("country"))
            .opt(
                "takenAfter",
                ctx.zone.iso_date(args.str("from"), false)?.map(|d| d.0),
            )
            .opt(
                "takenBefore",
                ctx.zone.iso_date(args.str("to"), true)?.map(|d| d.0),
            )
            .opt("personIds", person_ids)
            .set("withExif", true)
            .set("withPeople", true)
            .set("size", size)
            .compact();
        let query = args.str("query").filter(|q| !q.is_empty());
        let path = if let (Some(q), Value::Object(f)) = (query, &mut filters) {
            f.insert("query".into(), q.into());
            "/api/search/smart"
        } else {
            if let Value::Object(f) = &mut filters {
                f.insert("order".into(), "desc".into());
            }
            "/api/search/metadata"
        };
        let result: SearchResult = self.immich(ctx, self.request(path).post(filters)).await?;
        let items = result
            .assets
            .items
            .iter()
            .take(size)
            .map(|a| summarize_asset(a, ctx))
            .collect::<Result<Vec<_>, _>>()?;
        Ok(json!({ "count": items.len(), "items": items }))
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use crate::services::tests::with_ctx;

    #[test]
    fn photos_are_described_never_sent() {
        with_ctx("https://muster.castorcloud.ch", |ctx| {
            let a: Asset = serde_json::from_value(json!({
                "id": "abc", "type": "IMAGE", "localDateTime": "2026-05-02T18:30:00.000Z",
                "exifInfo": { "city": "Lisbon", "country": "Portugal" }, "people": [{ "name": "Ben" }]
            }))
            .unwrap();
            assert_eq!(
                summarize_asset(&a, ctx).unwrap(),
                json!({ "taken": "2026-05-02T18:30", "place": "Lisbon, Portugal", "people": ["Ben"],
                        "link": "https://muster.castorcloud.ch/photos/photos/abc" })
            );
            let v: Asset = serde_json::from_value(json!({
                "id": "v", "type": "VIDEO", "fileCreatedAt": "2026-01-15T07:05:59.999Z", "isFavorite": true,
                "exifInfo": { "city": null, "country": "Switzerland", "description": "<b>Sunset</b>  at\n the river" },
                "people": [{ "name": "" }, {}]
            }))
            .unwrap();
            assert_eq!(
                summarize_asset(&v, ctx).unwrap().to_string(),
                r#"{"taken":"2026-01-15T08:05","type":"video","place":"Switzerland","caption":"Sunset at the river","favorite":true,"link":"https://muster.castorcloud.ch/photos/photos/v"}"#
            );
        });
    }

    #[test]
    fn the_key_insert_grants_exactly_the_read_permissions() {
        assert!(insert_key().ends_with("'{asset.read,person.read,album.read}')"));
        assert!(
            IMMICH_PERMISSIONS
                .iter()
                .all(|p| p.bytes().all(|b| b.is_ascii_lowercase() || b == b'.'))
        );
    }
}
