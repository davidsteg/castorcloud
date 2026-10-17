// Compatibility with packages/core/src/crypto.ts, in both directions:
//   fixtures/node-sealed.json  sealed by TypeScript, opened here
//   fixtures/rust-sealed.json  sealed here, opened by packages/core/src/crypto.test.ts
// Regenerate the second with: CASTOR_WRITE_FIXTURES=1 cargo test -p castor-crypto
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use castor_crypto::{Dataset, Key, Keys, dataset_key, decrypt, encrypt};
use serde::{Deserialize, Serialize};

#[derive(Deserialize, Serialize)]
struct Fixture {
    comment: String,
    secrets_key: String,
    volume_kek: String,
    secrets: Vec<SecretCase>,
    volumes: Vec<VolumeCase>,
}

#[derive(Deserialize, Serialize)]
struct SecretCase {
    tenant: String,
    name: String,
    value: String,
    sealed: String,
}

#[derive(Deserialize, Serialize)]
struct VolumeCase {
    tenant: String,
    wrapped: String,
    dek_hex: String,
}

fn keys(f: &Fixture) -> Keys {
    Keys {
        secrets: Key::from_hex("SECRETS_ENCRYPTION_KEY", &f.secrets_key).unwrap(),
        volume_kek: Key::from_hex("VOLUME_KEK", &f.volume_kek).unwrap(),
    }
}

fn node_fixture() -> Fixture {
    serde_json::from_str(include_str!("fixtures/node-sealed.json")).unwrap()
}

#[test]
fn opens_what_typescript_sealed() {
    let f = node_fixture();
    let keys = keys(&f);
    for c in &f.secrets {
        assert_eq!(*keys.open_secret(&c.tenant, &c.name, &c.sealed).unwrap(), c.value);
        assert!(keys.open_secret("other", &c.name, &c.sealed).is_err());
        assert!(keys.open_secret(&c.tenant, "OTHER", &c.sealed).is_err());
    }
    for c in &f.volumes {
        assert_eq!(
            hex::encode(&*keys.unwrap_volume_key(&c.tenant, &c.wrapped).unwrap()),
            c.dek_hex
        );
        assert!(keys.unwrap_volume_key("other", &c.wrapped).is_err());
    }
}

#[test]
fn rust_fixture_matches_its_keys() {
    let f: Fixture = serde_json::from_str(include_str!("fixtures/rust-sealed.json")).unwrap();
    let keys = keys(&f);
    for c in &f.secrets {
        assert_eq!(*keys.open_secret(&c.tenant, &c.name, &c.sealed).unwrap(), c.value);
    }
    for c in &f.volumes {
        assert_eq!(
            hex::encode(&*keys.unwrap_volume_key(&c.tenant, &c.wrapped).unwrap()),
            c.dek_hex
        );
    }
}

#[test]
fn write_rust_fixture() {
    if std::env::var_os("CASTOR_WRITE_FIXTURES").is_none() {
        return;
    }
    let mut f = node_fixture();
    f.comment = "Sealed by rust/crates/crypto with fixed test-only keys; opened by packages/core/src/crypto.test.ts. \
                 Regenerate: CASTOR_WRITE_FIXTURES=1 cargo test -p castor-crypto"
        .into();
    let keys = keys(&f);
    for c in &mut f.secrets {
        c.sealed = keys.seal_secret(&c.tenant, &c.name, &c.value).unwrap();
    }
    for c in &mut f.volumes {
        c.wrapped = keys.new_wrapped_volume_key(&c.tenant).unwrap();
        c.dek_hex = hex::encode(&*keys.unwrap_volume_key(&c.tenant, &c.wrapped).unwrap());
    }
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/rust-sealed.json");
    std::fs::write(path, serde_json::to_string_pretty(&f).unwrap() + "\n").unwrap();
}

#[test]
fn round_trip_uses_fresh_ivs() {
    let key = Key::from_hex("K", &"ab".repeat(32)).unwrap();
    let a = encrypt(b"value", &key, b"aad").unwrap();
    let b = encrypt(b"value", &key, b"aad").unwrap();
    assert_ne!(a, b);
    assert_eq!(&*decrypt(&a, &key, b"aad").unwrap(), b"value");
    assert!(decrypt(&a, &key, b"other").is_err());
}

#[test]
fn rejects_tampering_and_malformed_input() {
    let key = Key::from_hex("K", &"ab".repeat(32)).unwrap();
    let sealed = encrypt(b"value", &key, b"").unwrap();
    let parts: Vec<&str> = sealed.split('.').collect();

    let other_ct = format!("{}.{}.{}.b3RoZXI", parts[0], parts[1], parts[2]);
    let short_iv = format!("{}.AAAA.{}.{}", parts[0], parts[2], parts[3]);
    let short_tag = format!("{}.{}.AAAA.{}", parts[0], parts[1], parts[3]);
    let extra = format!("{sealed}.x");
    let v2 = sealed.replacen("v1.", "v2.", 1);
    for bad in [other_ct, short_iv, short_tag, extra, v2, "v1".into(), String::new()] {
        assert!(decrypt(&bad, &key, b"").is_err(), "{bad}");
    }
    assert!(decrypt(&sealed, &Key::from_hex("K", &"cd".repeat(32)).unwrap(), b"").is_err());
}

#[test]
fn rejects_bad_keys() {
    for bad in ["", "ab", &"zz".repeat(32), &"ab".repeat(33)] {
        assert!(Key::from_hex("K", bad).is_err());
    }
    assert_eq!(
        format!("{:?}", Key::from_hex("K", &"ab".repeat(32)).unwrap()),
        "Key(<redacted>)"
    );
}

/// hkdfSync("sha256", bytes 0..63, empty salt, label, 32) in Node.
#[test]
fn dataset_keys_match_zfs_ts() {
    let volume_key: Vec<u8> = (0u8..64).collect();
    let hex = |d| String::from_utf8(dataset_key(&volume_key, d).unwrap().to_vec()).unwrap();
    assert_eq!(
        hex(Dataset::Tenant),
        "6767812656dff0e2c711c92212e062504e2bbc7cf41be35c93b8732f99e4e35d\n"
    );
    assert_eq!(
        hex(Dataset::Stacks),
        "17d3c0cf56e461fcf93697bfbc31b84c0f36c4edfeda4d773eedefbab5c33e01\n"
    );
}
