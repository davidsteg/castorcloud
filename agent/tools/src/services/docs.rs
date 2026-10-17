//! Documents (Paperless-ngx). Paperless has no per-user API keys for SSO
//! accounts, so the tools use the stack's service account and apply Paperless'
//! own visibility rules for the asking member (owner, shared with the member or
//! one of their groups, or unowned; superusers see everything).

use std::collections::HashMap;
use std::time::Duration;

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use serde::Deserialize;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};

use super::{Ctx, lenient_str};
use crate::limits::{
    DOCUMENT_CHARS, EXCERPT_CHARS, Out, ToolError, clamp_limit, excerpt, js_len, js_slice, js_trim,
    spread,
};
use crate::members::Member;
use crate::schema::{Args, Field, Kind, Tool, limit, string};
use crate::upstream::{Request, TtlCache, get_json};

const LABEL: &str = "Documents";
const FIELDS: &str = "id,title,created,created_date,correspondent,document_type,tags,owner,permissions,__search_hit__";

#[derive(Deserialize, Default, Debug)]
pub struct Ids {
    #[serde(default)]
    users: Option<Vec<i64>>,
    #[serde(default)]
    groups: Option<Vec<i64>>,
}

#[derive(Deserialize, Default, Debug)]
pub struct Perms {
    #[serde(default)]
    view: Option<Ids>,
    #[serde(default)]
    change: Option<Ids>,
}

#[derive(Deserialize, Debug)]
struct SearchHit {
    #[serde(default, deserialize_with = "lenient_str")]
    highlights: Option<String>,
}

#[derive(Deserialize, Debug)]
pub struct PaperlessDoc {
    pub id: i64,
    #[serde(default, deserialize_with = "lenient_str")]
    title: Option<String>,
    #[serde(default, deserialize_with = "lenient_str")]
    created: Option<String>,
    #[serde(default, deserialize_with = "lenient_str")]
    created_date: Option<String>,
    #[serde(default)]
    correspondent: Option<i64>,
    #[serde(default)]
    document_type: Option<i64>,
    #[serde(default)]
    tags: Option<Vec<i64>>,
    #[serde(default)]
    pub owner: Option<i64>,
    #[serde(default)]
    pub permissions: Option<Perms>,
    #[serde(default, deserialize_with = "lenient_str")]
    content: Option<String>,
    #[serde(default)]
    __search_hit__: Option<SearchHit>,
}

#[derive(Deserialize, Clone, Debug)]
pub struct PaperlessUser {
    pub id: i64,
    #[serde(default, deserialize_with = "lenient_str")]
    email: Option<String>,
    #[serde(default)]
    pub is_superuser: Option<bool>,
    #[serde(default)]
    pub groups: Option<Vec<i64>>,
}

#[derive(Deserialize)]
struct Page<T> {
    results: Vec<T>,
}

#[derive(Deserialize)]
struct Named {
    id: i64,
    name: String,
}

/// Paperless' visibility rule for one document and one user.
pub fn can_view(doc: &PaperlessDoc, user: &PaperlessUser) -> bool {
    if user.is_superuser == Some(true) {
        return true;
    }
    if doc.owner.is_none() || doc.owner == Some(user.id) {
        return true;
    }
    let p = doc.permissions.as_ref();
    let ids = |f: fn(&Ids) -> &Option<Vec<i64>>| -> Vec<i64> {
        p.into_iter()
            .flat_map(|p| [&p.view, &p.change])
            .flatten()
            .flat_map(|i| f(i).iter().flatten().copied())
            .collect()
    };
    let in_users = ids(|i| &i.users).contains(&user.id);
    let mine = user.groups.as_deref().unwrap_or_default();
    in_users || ids(|i| &i.groups).iter().any(|g| mine.contains(g))
}

pub struct Docs {
    url: String,
    auth: String,
    users: TtlCache<Option<PaperlessUser>>,
    names: TtlCache<HashMap<i64, String>>,
}

impl Docs {
    pub fn new(url: String, user: String, password: String) -> Self {
        Self {
            url,
            auth: format!("Basic {}", STANDARD.encode(format!("{user}:{password}"))),
            users: TtlCache::new(Duration::from_secs(5 * 60)),
            names: TtlCache::new(Duration::from_secs(5 * 60)),
        }
    }

    pub fn tools() -> Vec<Tool> {
        vec![
            Tool {
                name: "search_documents",
                title: "Search documents",
                description: "Full-text search over the user's scanned documents (invoices, contracts, letters, receipts). \
                    Returns title, date, sender, type, tags and a short matching excerpt. Use read_document for more text."
                    .into(),
                fields: vec![
                    Field {
                        name: "query",
                        kind: Kind::Str { min: Some(1), max: Some(200) },
                        required: true,
                        description: Some("Words to look for, e.g. 'hotel lisbon invoice'"),
                    },
                    string("from", None, Some("Document date on or after (YYYY-MM-DD)")),
                    string("to", None, Some("Document date on or before (YYYY-MM-DD)")),
                    limit(),
                ],
            },
            Tool {
                name: "read_document",
                title: "Read a document",
                description: format!(
                    "Text (OCR) of one document found with search_documents, cut to at most {DOCUMENT_CHARS} characters. \
                     Use `offset` to continue reading a long document."
                ),
                fields: vec![
                    Field { name: "id", kind: Kind::Int { min: Some(0), exclusive: true, max: None }, required: true, description: None },
                    Field { name: "offset", kind: Kind::Int { min: Some(0), exclusive: false, max: None }, required: false, description: None },
                    Field {
                        name: "maxChars",
                        kind: Kind::Int { min: Some(200), exclusive: false, max: Some(DOCUMENT_CHARS as i64) },
                        required: false,
                        description: None,
                    },
                ],
            },
        ]
    }

    async fn paperless<T: DeserializeOwned>(
        &self,
        ctx: &Ctx<'_>,
        r: Request<'_>,
    ) -> Result<T, ToolError> {
        get_json(ctx.http, r.header("authorization", self.auth.clone())).await
    }

    fn request<'a>(&'a self, path: &'a str) -> Request<'a> {
        Request::get(&self.url, path, LABEL)
    }

    async fn paperless_user(
        &self,
        ctx: &Ctx<'_>,
        member: &Member,
    ) -> Result<PaperlessUser, ToolError> {
        let email = member.email.to_lowercase();
        let user = self
            .users
            .get(&email, || async {
                let page: Page<PaperlessUser> = self
                    .paperless(
                        ctx,
                        self.request("/api/users/")
                            .query("page_size", "1000".to_owned()),
                    )
                    .await?;
                Ok(page
                    .results
                    .into_iter()
                    .find(|u| u.email.as_ref().is_some_and(|e| e.to_lowercase() == email)))
            })
            .await?;
        user.ok_or_else(|| {
            self.users.delete(&email);
            ToolError::user(
                "Documents has not been opened by this person yet, so there is nothing to search",
            )
        })
    }

    async fn lookup(
        &self,
        ctx: &Ctx<'_>,
        kind: &'static str,
    ) -> Result<HashMap<i64, String>, ToolError> {
        self.names
            .get(kind, || async {
                let path = format!("/api/{kind}/");
                let page: Page<Named> = self
                    .paperless(
                        ctx,
                        self.request(&path).query("page_size", "1000".to_owned()),
                    )
                    .await?;
                Ok(page.results.into_iter().map(|r| (r.id, r.name)).collect())
            })
            .await
    }

    async fn describe(
        &self,
        ctx: &Ctx<'_>,
        docs: &[PaperlessDoc],
    ) -> Result<Vec<Value>, ToolError> {
        let (correspondents, types, tags) = tokio::try_join!(
            self.lookup(ctx, "correspondents"),
            self.lookup(ctx, "document_types"),
            self.lookup(ctx, "tags")
        )?;
        Ok(docs
            .iter()
            .map(|d| {
                let name = |map: &HashMap<i64, String>, id: Option<i64>| {
                    id.filter(|i| *i != 0).and_then(|i| map.get(&i).cloned())
                };
                let tag_names: Option<Vec<String>> = d.tags.as_ref().map(|t| {
                    t.iter()
                        .filter_map(|t| tags.get(t).filter(|n| !n.is_empty()).cloned())
                        .collect()
                });
                Out::new()
                    .set("id", d.id)
                    .opt("title", d.title.clone())
                    .opt(
                        "date",
                        d.created_date
                            .as_ref()
                            .or(d.created.as_ref())
                            .map(|c| js_slice(c, 0, 10)),
                    )
                    .opt("from", name(&correspondents, d.correspondent))
                    .opt("type", name(&types, d.document_type))
                    .opt("tags", tag_names)
                    .opt(
                        "excerpt",
                        excerpt(
                            d.__search_hit__
                                .as_ref()
                                .and_then(|h| h.highlights.as_deref()),
                            EXCERPT_CHARS,
                        ),
                    )
                    .set(
                        "link",
                        format!("{}/docs/documents/{}/details", ctx.portal_origin, d.id),
                    )
                    .compact()
            })
            .collect())
    }

    pub async fn call(&self, ctx: &Ctx<'_>, name: &str, args: &Args) -> Result<Value, ToolError> {
        let user = self.paperless_user(ctx, ctx.member).await?;
        if name == "search_documents" {
            let limit = clamp_limit(args.int("limit"), 10);
            // Paperless filters by calendar day; an empty date is sent empty.
            let day = |v: Option<&str>, end: bool| -> Result<Option<String>, ToolError> {
                match v {
                    Some("") => Ok(Some(String::new())),
                    v => ctx
                        .zone
                        .iso_date(v, end)?
                        .map(|(iso, _)| ctx.zone.local_day(&iso))
                        .transpose(),
                }
            };
            let r = self
                .request("/api/documents/")
                .query("query", args.str("query").map(str::to_owned))
                .query("full_perms", "true".to_owned())
                .query("fields", FIELDS.to_owned())
                .query("page_size", "50".to_owned()) // filtered for this member below
                .query("created__date__gte", day(args.str("from"), false)?)
                .query("created__date__lte", day(args.str("to"), true)?);
            let page: Page<PaperlessDoc> = self.paperless(ctx, r).await?;
            let visible: Vec<PaperlessDoc> = page
                .results
                .into_iter()
                .filter(|d| can_view(d, &user))
                .take(limit)
                .collect();
            let items = self.describe(ctx, &visible).await?;
            return Ok(json!({ "count": visible.len(), "items": items }));
        }
        let id = args.int("id").unwrap_or_default();
        let path = format!("/api/documents/{id}/");
        let doc: PaperlessDoc = self
            .paperless(
                ctx,
                self.request(&path).query("full_perms", "true".to_owned()),
            )
            .await?;
        if !can_view(&doc, &user) {
            return Err(ToolError::user("not found in Documents")); // same answer as a missing id
        }
        let text = normalize(doc.content.as_deref().unwrap_or_default());
        let offset = usize::try_from(args.int("offset").unwrap_or(0)).unwrap_or(usize::MAX);
        let max = usize::try_from(args.int("maxChars").unwrap_or(DOCUMENT_CHARS as i64))
            .unwrap_or(DOCUMENT_CHARS);
        let meta = self
            .describe(ctx, std::slice::from_ref(&doc))
            .await?
            .into_iter()
            .next()
            .unwrap_or_default();
        let total = js_len(&text);
        let end = offset.saturating_add(max);
        let mut more = Out::new().set("text", js_slice(&text, offset, end));
        if end < total {
            more = more.set("continuesAt", end).set("totalChars", total);
        }
        Ok(spread(meta, more.build()))
    }
}

/// OCR text with runs of blanks collapsed and at most one empty line in a row.
fn normalize(content: &str) -> String {
    let mut s = String::with_capacity(content.len());
    let mut blank = false;
    for c in content.chars() {
        if c == ' ' || c == '\t' {
            if !blank {
                s.push(' ');
            }
            blank = true;
        } else {
            s.push(c);
            blank = false;
        }
    }
    let mut out = String::with_capacity(s.len());
    let mut newlines = 0;
    for c in s.chars() {
        if c == '\n' {
            newlines += 1;
            if newlines <= 2 {
                out.push(c);
            }
        } else {
            newlines = 0;
            out.push(c);
        }
    }
    js_trim(&out).to_owned()
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    fn doc(v: Value) -> PaperlessDoc {
        let mut base = json!({ "id": 1, "title": "t" });
        if let (Value::Object(b), Value::Object(e)) = (&mut base, v) {
            b.extend(e);
        }
        serde_json::from_value(base).unwrap()
    }

    #[test]
    fn documents_follow_paperless_visibility() {
        let anna = PaperlessUser {
            id: 2,
            email: None,
            is_superuser: None,
            groups: Some(vec![7]),
        };
        assert!(can_view(&doc(json!({ "owner": 2 })), &anna));
        assert!(can_view(&doc(json!({ "owner": null })), &anna));
        assert!(can_view(&doc(json!({})), &anna));
        assert!(!can_view(&doc(json!({ "owner": 3 })), &anna));
        assert!(can_view(
            &doc(json!({ "owner": 3, "permissions": { "view": { "users": [2] } } })),
            &anna
        ));
        assert!(can_view(
            &doc(json!({ "owner": 3, "permissions": { "change": { "groups": [7] } } })),
            &anna
        ));
        assert!(!can_view(
            &doc(json!({ "owner": 3, "permissions": { "view": { "groups": [8] } } })),
            &anna
        ));
        assert!(can_view(
            &doc(json!({ "owner": 3 })),
            &PaperlessUser {
                is_superuser: Some(true),
                ..anna.clone()
            }
        ));
        let no_groups = PaperlessUser {
            groups: None,
            ..anna
        };
        assert!(!can_view(
            &doc(json!({ "owner": 3, "permissions": { "view": { "groups": [7] } } })),
            &no_groups
        ));
    }

    #[test]
    fn ocr_text_is_tidied() {
        assert_eq!(
            normalize("  Dear Anna,\n\n\n\n  this \t is\n\nit.\n\n\n"),
            "Dear Anna,\n\n this is\n\nit."
        );
        assert_eq!(normalize(""), "");
    }
}
