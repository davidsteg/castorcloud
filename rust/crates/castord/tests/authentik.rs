// Setting up a tenant's Authentik objects: the
// requests against an Authentik that has nothing yet, and only updates
// against one that has everything.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod common;

use castord::authentik::{Authentik, OidcApp, ProxyApp};
use common::*;
use zeroize::Zeroizing;

fn client(url: &str) -> Authentik {
    Authentik::new(url, Zeroizing::new("ak".into()), castord::http::client(None).unwrap())
}

/// Lookups shared by both worlds: flows, scopes, the signing key, the outpost.
fn shared(path: &str) -> Option<&'static str> {
    Some(match path {
        p if p.starts_with("/api/v3/flows/instances/?slug=default-provider-authorization") => {
            r#"{"results":[{"pk":"flow-authz"}]}"#
        }
        p if p.starts_with("/api/v3/flows/instances/?slug=default-provider-invalidation") => {
            r#"{"results":[{"pk":"flow-inval"}]}"#
        }
        p if p.contains("scope-openid") => r#"{"results":[{"pk":"m-openid"}]}"#,
        p if p.contains("scope-email") => r#"{"results":[{"pk":"m-email"}]}"#,
        p if p.contains("scope-profile") => r#"{"results":[{"pk":"m-profile"}]}"#,
        p if p.starts_with("/api/v3/crypto/certificatekeypairs/") => r#"{"results":[{"pk":"key-1"}]}"#,
        p if p.starts_with("/api/v3/outposts/instances/?") => r#"{"results":[{"pk":"op-1","providers":[3]}]}"#,
        _ => return None,
    })
}

fn fresh(method: &str, path: &str) -> (u16, String) {
    if method == "GET" {
        return (200, shared(path).unwrap_or(r#"{"results":[]}"#).into());
    }
    let body = match path {
        "/api/v3/core/groups/" => r#"{"pk":"g-new"}"#,
        "/api/v3/providers/oauth2/" | "/api/v3/providers/proxy/" => r#"{"pk":7}"#,
        "/api/v3/core/applications/" => r#"{"pk":"app-new","slug":"x"}"#,
        _ => "{}",
    };
    (200, body.into())
}

fn existing(method: &str, path: &str) -> (u16, String) {
    if method != "GET" {
        return (200, r#"{"pk":9}"#.into());
    }
    if let Some(b) = shared(path) {
        return (200, b.into());
    }
    let body = match path {
        p if p.starts_with("/api/v3/core/groups/?") => r#"{"results":[{"pk":"g-old"}]}"#,
        p if p.starts_with("/api/v3/providers/") => r#"{"results":[{"pk":9}]}"#,
        p if p.starts_with("/api/v3/core/applications/?") => r#"{"results":[{"pk":"app-old"}]}"#,
        p if p.starts_with("/api/v3/policies/bindings/?") => r#"{"results":[{"pk":"b-1"}]}"#,
        "/api/v3/core/groups/g-old/?include_users=true" => {
            r#"{"users_obj":[{"pk":4,"email":"anna@example.ch","uid":"sub-a"}]}"#
        }
        _ => r#"{"results":[]}"#,
    };
    (200, body.into())
}

fn writes(seen: &std::sync::Mutex<Vec<String>>) -> Vec<String> {
    seen.lock()
        .unwrap()
        .iter()
        .filter(|l| !l.starts_with("GET "))
        .cloned()
        .collect()
}

#[tokio::test]
async fn creates_what_is_missing() {
    let (url, seen) = http("ak", fresh).await;
    let ak = client(&url);
    let d = ak.provider_defaults().await.unwrap();
    assert_eq!(ak.ensure_group("pc-anna-members").await.unwrap(), "g-new");
    ak.add_user_to_group("g-new", 4).await.unwrap();
    let uris = [serde_json::json!({"matching_mode": "strict", "url": "https://photos.anna.castor.test/auth/login"})];
    ak.ensure_oidc_app(
        &OidcApp {
            slug: "anna-photos",
            name: "Photos (anna)",
            client_id: "anna-photos",
            client_secret: "s3cret",
            redirect_uris: &uris,
            launch_url: "https://castor.test/photos",
            allowed_group_pk: "g-new",
        },
        &d,
    )
    .await
    .unwrap();
    ak.ensure_proxy_app(
        &ProxyApp {
            slug: "anna-linkding",
            name: "linkding (anna)",
            external_host: "https://linkding.anna.stacks.test",
            allowed_group_pk: "g-new",
        },
        &d,
    )
    .await
    .unwrap();
    let oidc = r#"{"name":"anna-photos","client_type":"confidential","client_id":"anna-photos","client_secret":"s3cret","redirect_uris":[{"matching_mode":"strict","url":"https://photos.anna.castor.test/auth/login"}],"sub_mode":"hashed_user_id","include_claims_in_id_token":true,"authorization_flow":"flow-authz","invalidation_flow":"flow-inval","property_mappings":["m-openid","m-email","m-profile"],"signing_key":"key-1"}"#;
    let proxy = r#"{"name":"anna-linkding","mode":"forward_single","external_host":"https://linkding.anna.stacks.test","authorization_flow":"flow-authz","invalidation_flow":"flow-inval","intercept_header_auth":false,"basic_auth_enabled":false,"access_token_validity":"hours=12"}"#;
    let app = |name: &str, slug: &str, launch: &str| {
        format!(r#"{{"name":"{name}","slug":"{slug}","provider":7,"meta_launch_url":"{launch}"}}"#)
    };
    let photos = app("Photos (anna)", "anna-photos", "https://castor.test/photos");
    let linkding = app("linkding (anna)", "anna-linkding", "https://linkding.anna.stacks.test");
    let bind = r#"{"target":"app-new","group":"g-new","order":0,"enabled":true}"#;
    assert_eq!(
        writes(&seen),
        vec![
            r#"POST /api/v3/core/groups/ auth=true {"name":"pc-anna-members","is_superuser":false}"#.to_owned(),
            r#"POST /api/v3/core/groups/g-new/add_user/ auth=true {"pk":4}"#.to_owned(),
            format!("POST /api/v3/providers/oauth2/ auth=true {oidc}"),
            format!("POST /api/v3/core/applications/ auth=true {photos}"),
            format!("PATCH /api/v3/core/applications/anna-photos/ auth=true {photos}"),
            format!("POST /api/v3/policies/bindings/ auth=true {bind}"),
            format!("POST /api/v3/providers/proxy/ auth=true {proxy}"),
            format!("POST /api/v3/core/applications/ auth=true {linkding}"),
            format!("PATCH /api/v3/core/applications/anna-linkding/ auth=true {linkding}"),
            format!("POST /api/v3/policies/bindings/ auth=true {bind}"),
            r#"PATCH /api/v3/outposts/instances/op-1/ auth=true {"providers":[3,7]}"#.to_owned(),
        ]
    );
}

#[tokio::test]
async fn updates_what_exists() {
    let (url, seen) = http("ak", existing).await;
    let ak = client(&url);
    let d = ak.provider_defaults().await.unwrap();
    assert_eq!(ak.ensure_group("pc-anna-members").await.unwrap(), "g-old");
    let users = ak.group_users("g-old").await.unwrap();
    assert_eq!(
        (users[0].pk, users[0].email.as_str(), users[0].uid.as_str()),
        (4, "anna@example.ch", "sub-a")
    );
    ak.remove_user_from_group("g-old", 4).await.unwrap();
    ak.ensure_oidc_app(
        &OidcApp {
            slug: "anna-docs",
            name: "Documents (anna)",
            client_id: "anna-docs",
            client_secret: "s",
            redirect_uris: &[],
            launch_url: "https://castor.test/docs",
            allowed_group_pk: "g-old",
        },
        &d,
    )
    .await
    .unwrap();
    let w = writes(&seen);
    assert_eq!(w.len(), 3, "{w:?}");
    assert_eq!(
        w[0],
        r#"POST /api/v3/core/groups/g-old/remove_user/ auth=true {"pk":4}"#
    );
    assert!(
        w[1].starts_with(r#"PATCH /api/v3/providers/oauth2/9/ auth=true {"name":"anna-docs""#),
        "{w:?}"
    );
    assert_eq!(
        w[2],
        r#"PATCH /api/v3/core/applications/anna-docs/ auth=true {"name":"Documents (anna)","slug":"anna-docs","provider":9,"meta_launch_url":"https://castor.test/docs"}"#
    );
    // Ids from Authentik never reach a URL unchecked.
    assert!(ak.add_user_to_group("../admin", 1).await.is_err());
}

#[tokio::test]
async fn a_missing_default_flow_is_an_error() {
    let (url, _) = empty_authentik().await;
    let Err(e) = client(&url).provider_defaults().await else {
        panic!("expected an error")
    };
    assert_eq!(e, "Authentik: authorization flow not found");
}
