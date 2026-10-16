//! REVOKE_ACCESS: ends a person's access in the tenant's apps now, as
//! `Lifecycle.revokeAccess` and `access.ts` in apps/provisioner/src. None of
//! the apps has an admin API for this, so each command works on the app's
//! own database or runtime inside the tenant stack. Values reach SQL only as
//! psql variables, and code only as environment variables.
//!
//! Scopes (RevocationScope):
//!   WEB      browser sessions; phone apps stay connected (portal sign-out)
//!   DEVICES  every session and API key the person made ("sign out everywhere")
//!   REMOVED  DEVICES plus the assistant's key (the member left the tenant)
//!
//! When the apps don't run, the requests stay queued for the next start.

use std::sync::Arc;

use serde_json::json;
use zeroize::Zeroizing;

use crate::db::audit;
use crate::node::{Node, NodeRow};
use crate::suspend::compose_args;
use crate::{Control, Handler, Job, JobCx, valid};

/// Name of the keys the assistant's tools issue (agent/tools/src/db.ts KEY_NAME).
pub const ASSISTANT_KEY_NAME: &str = "Castor assistant (read-only)";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scope {
    Web,
    Devices,
    Removed,
}

impl Scope {
    fn parse(s: &str) -> Result<Self, String> {
        match s {
            "WEB" => Ok(Self::Web),
            "DEVICES" => Ok(Self::Devices),
            "REMOVED" => Ok(Self::Removed),
            _ => Err(format!("unknown revocation scope {s}")),
        }
    }

    fn name(self) -> &'static str {
        match self {
            Self::Web => "WEB",
            Self::Devices => "DEVICES",
            Self::Removed => "REMOVED",
        }
    }

    fn keep_key(self) -> &'static str {
        if self == Self::Removed { "" } else { ASSISTANT_KEY_NAME }
    }
}

/// Strongest scope first: one run per person covers the weaker requests.
pub fn strongest(scopes: &[Scope]) -> Scope {
    if scopes.contains(&Scope::Removed) {
        Scope::Removed
    } else if scopes.contains(&Scope::Devices) {
        Scope::Devices
    } else {
        Scope::Web
    }
}

pub struct Who {
    pub email: String,
    pub sub: String,
    /// ISO 8601 UTC with milliseconds, as JavaScript's toISOString.
    pub before: String,
}

/// A command run with `docker compose exec -T` in one of the tenant's services.
#[derive(Debug, PartialEq)]
pub struct AccessCommand {
    pub service: &'static str,
    pub command: Vec<String>,
    pub env: Vec<(&'static str, String)>,
    pub input: String,
}

fn psql(user: &str, db: &str, vars: &[(&str, &str)]) -> Vec<String> {
    let mut v: Vec<String> = ["psql", "-X", "-q", "-v", "ON_ERROR_STOP=1", "-U", user, "-d", db]
        .iter()
        .map(|s| (*s).to_owned())
        .collect();
    for (k, val) in vars {
        v.push("-v".into());
        v.push(format!("{k}={val}"));
    }
    v
}

/// Immich: sessions carry appVersion only when the phone app created them,
/// so WEB keeps the app signed in.
fn photos(who: &Who, scope: Scope) -> AccessCommand {
    let web = (scope == Scope::Web).to_string();
    AccessCommand {
        service: "immich-db",
        command: psql(
            "immich",
            "immich",
            &[
                ("email", &who.email),
                ("before", &who.before),
                ("web", &web),
                ("keep", scope.keep_key()),
            ],
        ),
        env: Vec::new(),
        input: r#"
DELETE FROM session s USING "user" u
 WHERE s."userId" = u.id AND lower(u.email) = lower(:'email') AND s."createdAt" < :'before'::timestamptz
   AND (NOT :'web'::boolean OR s."appVersion" IS NULL);
DELETE FROM api_key k USING "user" u
 WHERE k."userId" = u.id AND lower(u.email) = lower(:'email')
   AND NOT :'web'::boolean AND k.name <> :'keep';
"#
        .into(),
    }
}

/// Paperless: a fresh unusable password ends every session (Django checks
/// the auth hash). Tokens (phone apps) survive WEB.
fn docs(who: &Who, scope: Scope) -> AccessCommand {
    AccessCommand {
        service: "paperless",
        command: ["python3", "manage.py", "shell", "--no-imports"]
            .iter()
            .map(|s| (*s).to_owned())
            .collect(),
        env: vec![("PC_EMAIL", who.email.clone()), ("PC_SCOPE", scope.name().into())],
        input: r#"
import os
from django.contrib.auth.models import User
from rest_framework.authtoken.models import Token
for user in User.objects.filter(email__iexact=os.environ["PC_EMAIL"]).exclude(email=""):
    user.set_unusable_password()
    user.save(update_fields=["password"])
    if os.environ["PC_SCOPE"] != "WEB":
        Token.objects.filter(user=user).delete()
"#
        .into(),
    }
}

/// Dawarich: a new random password hash ends sessions and remember-me
/// cookies; DEVICES also replaces the API key the phone app uses.
fn places(who: &Who, scope: Scope) -> AccessCommand {
    AccessCommand {
        service: "dawarich",
        command: vec![
            "bin/rails".into(),
            "runner".into(),
            r#"
user = User.where("lower(email) = ?", ENV.fetch("PC_EMAIL").downcase).first
if user
  user.update_columns(encrypted_password: Devise::Encryptor.digest(User, SecureRandom.hex(32)))
  user.update_columns(api_key: SecureRandom.hex(32)) unless ENV.fetch("PC_SCOPE") == "WEB"
end
"#
            .into(),
        ],
        env: vec![("PC_EMAIL", who.email.clone()), ("PC_SCOPE", scope.name().into())],
        input: String::new(),
    }
}

/// AirTrail: web only, linked by the OIDC subject. DEVICES also drops API
/// keys and grants to other apps.
fn flights(who: &Who, scope: Scope) -> AccessCommand {
    let web = (scope == Scope::Web).to_string();
    AccessCommand {
        service: "db",
        command: psql(
            "postgres",
            "airtrail",
            &[("sub", &who.sub), ("web", &web), ("keep", scope.keep_key())],
        ),
        env: Vec::new(),
        input: r#"
DELETE FROM session s USING "user" u WHERE s.user_id = u.id AND u.oauth_id = :'sub';
DELETE FROM api_key k USING "user" u
 WHERE k.user_id = u.id AND u.oauth_id = :'sub' AND NOT :'web'::boolean AND k.name <> :'keep';
DELETE FROM oauth_grant g USING "user" u
 WHERE g.user_id = u.id AND u.oauth_id = :'sub' AND NOT :'web'::boolean;
"#
        .into(),
    }
}

/// Commands that end `who`'s access in the given apps.
pub fn access_commands(apps: &[&str], who: &Who, scope: Scope) -> Vec<AccessCommand> {
    apps.iter()
        .filter_map(|a| match *a {
            "photos" => Some(photos(who, scope)),
            "docs" => Some(docs(who, scope)),
            "places" => Some(places(who, scope)),
            "flights" => Some(flights(who, scope)),
            _ => None,
        })
        .collect()
}

/// `docker compose` arguments that run the command in its service.
pub fn exec_args(cmd: &AccessCommand) -> Vec<String> {
    let mut v = vec!["exec".to_owned(), "-T".to_owned()];
    for (k, val) in &cmd.env {
        v.push("-e".into());
        v.push(format!("{k}={val}"));
    }
    v.push(cmd.service.into());
    v.extend(cmd.command.iter().cloned());
    v
}

pub struct RevokeAccess(pub Arc<Control>);

#[derive(sqlx::FromRow)]
struct Tenant {
    id: String,
    slug: String,
    status: String,
    #[sqlx(rename = "nodeId")]
    node_id: Option<String>,
}

#[derive(sqlx::FromRow)]
struct Request {
    id: String,
    email: String,
    sub: String,
    scope: String,
    created: String,
}

fn sql(e: sqlx::Error) -> String {
    e.to_string()
}

#[async_trait::async_trait]
impl Handler for RevokeAccess {
    fn kind(&self) -> &'static str {
        "REVOKE_ACCESS"
    }

    async fn run(&self, cx: &JobCx, job: &Job) -> Result<(), String> {
        let c = &self.0;
        let db = &cx.db;
        let t = sqlx::query_as::<_, Tenant>(
            r#"SELECT id, slug, status::text AS status, "nodeId" FROM "Tenant" WHERE id = $1"#,
        )
        .bind(&job.tenant_id)
        .fetch_optional(db)
        .await
        .map_err(sql)?
        .ok_or_else(|| format!("tenant {} not found", job.tenant_id))?;
        let Some(node_id) = t.node_id.as_deref().filter(|_| t.status == "ACTIVE") else {
            let msg = format!("apps not running ({}); revocations wait for the next start", t.status);
            return cx.report(job, "identity", &msg).await;
        };
        if !valid::slug(&t.slug) {
            return Err(format!("invalid tenant slug {:?}", t.slug));
        }
        let node = Node::new(&NodeRow::load(db, node_id).await?, &c.root, c.exec.clone())?;
        // The booked services, in STACK_SERVICES order.
        let booked: Vec<String> = sqlx::query_scalar(
            r#"SELECT lower(kind::text) FROM "ServiceInstance"
               WHERE "tenantId" = $1 AND enabled AND kind::text IN ('PHOTOS', 'DOCS', 'PLACES', 'FLIGHTS')"#,
        )
        .bind(&t.id)
        .fetch_all(db)
        .await
        .map_err(sql)?;
        let enabled: Vec<&str> = crate::delete::STACK_SERVICES
            .into_iter()
            .filter(|s| booked.iter().any(|b| b == s))
            .collect();
        process(c, cx, &node, &t.id, &t.slug, &enabled).await?;
        cx.report(job, "ready", "access revoked").await
    }
}

/// Carries out the tenant's pending AccessRevocations in the booked apps
/// (processRevocations in lifecycle.ts); also run by every apply.
pub(crate) async fn process(
    c: &Control,
    cx: &JobCx,
    node: &Node,
    tenant_id: &str,
    slug: &str,
    enabled: &[&str],
) -> Result<(), String> {
    let db = &cx.db;
    let pending = sqlx::query_as::<_, Request>(
        r#"SELECT id, email, sub, scope::text AS scope,
                  to_char("createdAt", 'YYYY-MM-DD"T"HH24:MI:SS.MS"Z"') AS created
           FROM "AccessRevocation" WHERE "tenantId" = $1 ORDER BY "createdAt", id"#,
    )
    .bind(tenant_id)
    .fetch_all(db)
    .await
    .map_err(sql)?;
    // Per person, in the order of their first request.
    let mut people: Vec<(String, Vec<Request>)> = Vec::new();
    for r in pending {
        match people.iter_mut().find(|(sub, _)| *sub == r.sub) {
            Some((_, list)) => list.push(r),
            None => people.push((r.sub.clone(), vec![r])),
        }
    }
    for (sub, requests) in people {
        let scopes = requests
            .iter()
            .map(|r| Scope::parse(&r.scope))
            .collect::<Result<Vec<_>, _>>()?;
        let scope = strongest(&scopes);
        let Some(last) = requests.last() else { continue };
        let who = Who {
            email: last.email.clone(),
            sub: sub.clone(),
            before: last.created.clone(),
        };
        for cmd in access_commands(enabled, &who, scope) {
            let args = compose_args(
                node,
                slug,
                enabled,
                &exec_args(&cmd).iter().map(String::as_str).collect::<Vec<_>>(),
            );
            node.docker_with(args, Some(Zeroizing::new(cmd.input.into_bytes())), false)
                .await?;
        }
        if scopes.contains(&Scope::Devices)
            && let Some(pk) = c.authentik.find_user_pk_by_email(&who.email).await?
        {
            c.authentik.end_user_sessions(pk).await?;
        }
        let ids: Vec<String> = requests.iter().map(|r| r.id.clone()).collect();
        sqlx::query(r#"DELETE FROM "AccessRevocation" WHERE id = ANY($1)"#)
            .bind(&ids)
            .execute(db)
            .await
            .map_err(sql)?;
        audit(
            db,
            "system",
            "access.revoked",
            Some(tenant_id),
            Some(json!({ "sub": sub, "scope": scope.name(), "apps": enabled })),
        )
        .await?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn who() -> Who {
        Who {
            email: "Anna@Example.ch".into(),
            sub: "abc123".into(),
            before: "2026-10-16T10:00:00.000Z".into(),
        }
    }

    #[test]
    fn scopes() {
        assert_eq!(strongest(&[Scope::Web, Scope::Devices]), Scope::Devices);
        assert_eq!(strongest(&[Scope::Web, Scope::Removed, Scope::Devices]), Scope::Removed);
        assert_eq!(strongest(&[Scope::Web]), Scope::Web);
    }

    #[test]
    fn commands_match_access_ts() {
        let c = access_commands(&["photos", "places"], &who(), Scope::Web);
        assert_eq!(
            exec_args(&c[0]),
            [
                "exec",
                "-T",
                "immich-db",
                "psql",
                "-X",
                "-q",
                "-v",
                "ON_ERROR_STOP=1",
                "-U",
                "immich",
                "-d",
                "immich",
                "-v",
                "email=Anna@Example.ch",
                "-v",
                "before=2026-10-16T10:00:00.000Z",
                "-v",
                "web=true",
                "-v",
                "keep=Castor assistant (read-only)",
            ]
        );
        assert_eq!(
            &exec_args(&c[1])[..7],
            [
                "exec",
                "-T",
                "-e",
                "PC_EMAIL=Anna@Example.ch",
                "-e",
                "PC_SCOPE=WEB",
                "dawarich"
            ]
        );
        let r = access_commands(&["flights", "docs"], &who(), Scope::Removed);
        assert_eq!(r[0].service, "db");
        assert!(exec_args(&r[0]).contains(&"keep=".to_owned()) && exec_args(&r[0]).contains(&"web=false".to_owned()));
        assert_eq!(
            r[1].env,
            vec![
                ("PC_EMAIL", "Anna@Example.ch".to_owned()),
                ("PC_SCOPE", "REMOVED".to_owned())
            ]
        );
    }
}
