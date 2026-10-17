// The tenant's LLM key, generated when the gateway
// doesn't know it, otherwise updated (keeping its monthly reset) and unblocked.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod common;

use castord::llm::Gateway;
use common::*;
use zeroize::Zeroizing;

fn gw(url: &str) -> Gateway {
    Gateway::new(
        url,
        Zeroizing::new("master".into()),
        castord::http::client(None).unwrap(),
    )
}

#[tokio::test]
async fn generates_an_unknown_key() {
    let (url, seen) = http("master", |_, p| {
        if p.starts_with("/key/info") {
            (404, "{}".into())
        } else {
            (200, "{}".into())
        }
    })
    .await;
    gw(&url).ensure_tenant_key("sk-1", "anna", "ct1", 9.77).await.unwrap();
    assert_eq!(
        seen.lock().unwrap()[1],
        r#"POST /key/generate auth=true {"key":"sk-1","key_alias":"tenant-anna-ct1","max_budget":9.77,"budget_duration":"1mo","models":["assistant"],"metadata":{"tenant_id":"ct1"}}"#
    );
}

#[tokio::test]
async fn updates_and_unblocks_a_known_key() {
    let (url, seen) = http("master", |_, _| (200, r#"{"info":{"spend":1,"blocked":true}}"#.into())).await;
    gw(&url).ensure_tenant_key("sk-1", "anna", "ct1", 24.44).await.unwrap();
    assert_eq!(
        seen.lock().unwrap()[1..],
        [
            r#"POST /key/update auth=true {"key":"sk-1","key_alias":"tenant-anna-ct1","max_budget":24.44,"models":["assistant"],"metadata":{"tenant_id":"ct1"}}"#.to_owned(),
            r#"POST /key/unblock auth=true {"key":"sk-1"}"#.to_owned(),
        ]
    );
}
