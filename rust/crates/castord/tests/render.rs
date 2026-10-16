// The render module against what apps/provisioner/src/tenant.ts renders for
// the same inputs (fixtures/render.json, made by fixtures/gen-render.mts).
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use castord::render::*;
use serde_json::Value;

fn fixture() -> Value {
    serde_json::from_str(include_str!("fixtures/render.json")).unwrap()
}

fn vars(v: &Value) -> Vars {
    v.as_object()
        .unwrap()
        .iter()
        .map(|(k, v)| (k.clone(), v.as_str().unwrap().to_owned()))
        .collect()
}

fn platform(f: &Value) -> Platform {
    let p = &f["platform"];
    let s = |k: &str| p[k].as_str().unwrap().to_owned();
    Platform {
        base_domain: s("baseDomain"),
        scheme: s("scheme"),
        brand_name: s("brandName"),
        time_zone: s("timeZone"),
        auth_host_ip: s("authHostIp"),
    }
}

#[test]
fn renders_what_typescript_renders() {
    let f = fixture();
    let p = platform(&f);
    let secrets = vars(&f["secrets"]);
    let versions = vars(&f["versions"]);
    assert_eq!(
        parse_env_file(include_str!("../../../../packages/stack-templates/versions.env")),
        versions
    );
    assert_eq!(
        serde_json::to_string_pretty(&immich_config("anna", &p, &secrets)).unwrap(),
        f["immich"].as_str().unwrap()
    );
    for (key, assistant) in [
        ("env", &[][..]),
        ("env_assistant", &[Service::Photos, Service::Flights][..]),
    ] {
        let env = tenant_env(&EnvInput {
            slug: "anna",
            p: &p,
            secrets: &secrets,
            versions: &versions,
            config_dir: "/n/tenants/anna",
            brand_dir: "/n/files/brand",
            ca_dir: "/n/files/ca",
            assistant_services: assistant,
        })
        .unwrap();
        assert_eq!(env, f[key].as_str().unwrap(), "{key}");
    }
    assert_eq!(
        edge_route("anna", &p, "http://10.0.0.5:8480"),
        f["edge"].as_str().unwrap()
    );
    assert_eq!(
        volumes_override("/mnt/tank/castor/t/anna/data"),
        f["volumes"].as_str().unwrap()
    );
    for s in STACK_SERVICES {
        assert_eq!(
            Value::Array(redirect_uris("anna", &p, s)),
            f["redirects"][s.key()],
            "{}",
            s.key()
        );
    }
}
