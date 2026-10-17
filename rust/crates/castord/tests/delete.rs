// DELETE against the real schema, with Docker, SSH, Authentik and the LLM
// gateway faked: the commands and calls it makes, the rows afterwards, and a retry after a
// failure halfway finishes the job. Runs when TEST_DATABASE_URL is set.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod common;

use castord::{Handler, JobCx};
use common::*;

/// Authentik with everything the tenant had: groups, OIDC and proxy apps,
/// the embedded outpost serving the stacks' providers 7 and 9.
fn full(method: &str, path: &str) -> (u16, String) {
    let body = match (method, path) {
        ("DELETE", _) => return (204, String::new()),
        ("PATCH", _) => "{}",
        (_, p) if p.starts_with("/api/v3/core/groups/?") => r#"{"results":[{"pk":"g-1"}]}"#,
        (_, p) if p.starts_with("/api/v3/core/applications/?") => r#"{"results":[{"slug":"x"}]}"#,
        (_, p) if p.starts_with("/api/v3/providers/oauth2/?") => r#"{"results":[{"pk":11}]}"#,
        (_, p) if p.starts_with("/api/v3/providers/proxy/?") => r#"{"results":[{"pk":7}]}"#,
        (_, p) if p.starts_with("/api/v3/outposts/instances/?") => r#"{"results":[{"pk":"op-1","providers":[7,9]}]}"#,
        _ => return (404, "{}".into()),
    };
    (200, body.into())
}

fn broken(_: &str, _: &str) -> (u16, String) {
    (503, "down".into())
}

#[tokio::test]
async fn deletes_like_the_typescript_worker() {
    let Some(db) = db().await else { return };
    let w = world(&db, "TRUENAS").await;
    let s = &w.slug;
    std::fs::write(w.traefik.join(format!("tenant-{s}.yml")), "x").unwrap();
    let fake = Fake::new(vec![]);
    let (llm, llm_seen) = gateway().await;
    let (ak, ak_seen) = http("ak", full).await;
    let j = job(&db, &w.tenant, "DELETE").await;
    castord::delete::Delete(control(fake.clone(), &w.traefik, Some(&llm), &ak))
        .run(&JobCx { db: db.clone() }, &j)
        .await
        .unwrap();

    let ssh = "DOCKER_HOST=ssh://deploy@10.0.0.5 DOCKER_CONTEXT= docker";
    let gate = "ssh -o BatchMode=yes -T deploy@10.0.0.5";
    let edge = "pc-stacks-edge-traefik-1";
    let p = format!("pc-t-{s}");
    let mut want = Vec::new();
    for st in ["linkding", "ittools"] {
        let sp = format!("pc-s-{s}--{st}");
        want.push(format!("{ssh} exec {edge} rm -f /routes/{sp}.yml"));
        want.push(format!("{ssh} network disconnect -f {sp}-edge {edge}"));
        want.push(format!("{ssh} compose -p {sp} down --volumes --remove-orphans"));
        if st == "linkding" {
            want.push(format!("{ssh} image rm postgres:17"));
            want.push(format!("{ssh} image rm ghcr.io/x/linkding@sha256:ab"));
        }
    }
    want.push(format!("{gate} castor-zfs stacks-destroy {s}"));
    for c in ["pc-node-immich-ml", "pc-node-tika", "pc-node-gotenberg"] {
        want.push(format!("{ssh} network disconnect -f {p}-shared {c}"));
    }
    want.push(format!("{ssh} network disconnect -f {p}-llm pc-node-llm-gateway"));
    want.push(format!(
        "{ssh} compose -p {p} -f /repo/packages/stack-templates/tenant/compose.yml \
         --project-directory /mnt/tank/castor/node/files/templates/tenant \
         -f /repo/.tenants/{s}/compose.volumes.yml --env-file /repo/.tenants/{s}/tenant.env \
         --profile photos --profile docs --profile places --profile flights --profile assistant \
         down --volumes --remove-orphans"
    ));
    want.push(format!("{ssh} network rm {p}-shared"));
    want.push(format!("{ssh} network rm {p}-llm"));
    want.push(format!("{gate} castor-zfs destroy {s}"));
    want.push(format!("{gate} castor-rm tenants/{s}"));
    let calls = fake.calls();
    assert_eq!(calls, want);

    assert!(!w.traefik.join(format!("tenant-{s}.yml")).exists());
    assert!(!w.traefik.join(format!("stacks-{s}.yml")).exists());
    assert_eq!(
        llm_seen.lock().unwrap().last().unwrap(),
        r#"POST /key/delete auth=true {"keys":["sk-tenant key"]}"#
    );
    let ak = ak_seen.lock().unwrap().clone();
    let writes: Vec<&str> = ak
        .iter()
        .filter(|l| !l.starts_with("GET "))
        .map(String::as_str)
        .collect();
    let mut want = Vec::new();
    for st in ["linkding", "ittools"] {
        want.push(r#"PATCH /api/v3/outposts/instances/op-1/ auth=true {"providers":[9]}"#.to_owned());
        want.push(format!("DELETE /api/v3/core/applications/stack-{s}--{st}/ auth=true "));
        want.push("DELETE /api/v3/providers/proxy/7/ auth=true ".to_owned());
    }
    for svc in ["photos", "docs", "places", "flights"] {
        want.push(format!("DELETE /api/v3/core/applications/{s}-{svc}/ auth=true "));
        want.push("DELETE /api/v3/providers/oauth2/11/ auth=true ".to_owned());
    }
    want.push("DELETE /api/v3/core/groups/g-1/ auth=true ".to_owned());
    want.push("DELETE /api/v3/core/groups/g-1/ auth=true ".to_owned());
    assert_eq!(writes, want);
    assert!(
        ak.iter()
            .any(|l| l.starts_with(&format!("GET /api/v3/core/groups/?name=cust-{s}-owners ")))
    );
    assert!(ak.iter().any(|l| l.contains("superuser_full_list=true")));

    let t = &w.tenant;
    assert_eq!(
        rows(&db, r#"SELECT status::text FROM "Tenant" WHERE id = $1"#, t).await,
        vec!["DELETED"]
    );
    for table in ["VolumeKey", "Secret", "Stack"] {
        let q = format!(r#"SELECT id FROM "{table}" WHERE "tenantId" = $1"#);
        assert!(rows(&db, &q, t).await.is_empty(), "{table}");
    }
    assert_eq!(
        rows(
            &db,
            r#"SELECT action || ' ' || details::text FROM "AuditLog" WHERE "tenantId" = $1"#,
            t
        )
        .await,
        vec![format!(r#"tenant.deleted {{"slug": "{s}", "cryptoShredded": true}}"#)]
    );
    cleanup(&db, &w).await;
}

#[tokio::test]
async fn a_retry_after_a_failure_finishes_the_deletion() {
    let Some(db) = db().await else { return };
    let w = world(&db, "LOCAL").await;
    let t = &w.tenant;
    let (ak, _) = http("ak", broken).await;
    let j = job(&db, t, "DELETE").await;
    let e = castord::delete::Delete(control(Fake::new(vec![]), &w.traefik, None, &ak))
        .run(&JobCx { db: db.clone() }, &j)
        .await
        .unwrap_err();
    assert!(e.starts_with("Authentik GET /providers/proxy/ -> 503"), "{e}");
    assert_eq!(
        rows(&db, r#"SELECT status::text FROM "Tenant" WHERE id = $1"#, t).await,
        vec!["DELETING"]
    );

    let fake = Fake::new(vec![]);
    let (ak, _) = empty_authentik().await;
    castord::delete::Delete(control(fake.clone(), &w.traefik, None, &ak))
        .run(&JobCx { db: db.clone() }, &j)
        .await
        .unwrap();
    assert_eq!(
        rows(&db, r#"SELECT status::text FROM "Tenant" WHERE id = $1"#, t).await,
        vec!["DELETED"]
    );
    // The local node: LUKS through cryptd, no castor-rm.
    let calls = fake.calls();
    assert!(calls.contains(&format!(
        "docker run --rm -i --privileged -v /srv/pc:/srv/pc:rshared pc-cryptd:1 destroy {}",
        w.slug
    )));
    assert!(!calls.iter().any(|c| c.contains("castor-rm")));
    cleanup(&db, &w).await;
}
