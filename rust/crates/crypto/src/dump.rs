//! Sealed dumps of the control-plane database (apps/provisioner/src/db-dump.ts).
//! The database holds every tenant's wrapped volume key, so without it no
//! tenant backup can be restored. Dumps are sealed to an X25519 public key
//! whose secret half is kept offline: the control plane can write dumps but
//! not read them back, and neither can the backup box.
//!
//! Format: "CASTORDB1" | ephemeral public key (32) | iv (12) | tag (16) | ciphertext.
//! AES-256-GCM; key = HKDF-SHA256(X25519(ephemeral, recipient), salt = both
//! public keys, info "castor-db-dump-v1"). The header is authenticated.

use aes_gcm::aead::{AeadInOut, KeyInit};
use aes_gcm::{Aes256Gcm, Nonce, Tag};
use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD as B64;
use x25519_dalek::{PublicKey, StaticSecret};
use zeroize::Zeroizing;

use crate::{Error, IV_LEN, TAG_LEN, random};

const MAGIC: &[u8] = b"CASTORDB1";
const INFO: &[u8] = b"castor-db-dump-v1";
const HEADER: usize = MAGIC.len() + 32;

fn seal_key(ephemeral: &[u8; 32], recipient: &[u8; 32], shared: &[u8; 32]) -> Result<Zeroizing<[u8; 32]>, Error> {
    let mut salt = [0u8; 64];
    salt[..32].copy_from_slice(ephemeral);
    salt[32..].copy_from_slice(recipient);
    let mut key = Zeroizing::new([0u8; 32]);
    hkdf::Hkdf::<sha2::Sha256>::new(Some(&salt), shared)
        .expand(INFO, key.as_mut())
        .map_err(|_| Error::Decrypt)?;
    Ok(key)
}

/// BACKUP_DB_PUBLIC_KEY: 32 bytes, base64url.
pub fn public_key(b64: &str) -> Result<[u8; 32], Error> {
    B64.decode(b64.trim())
        .ok()
        .and_then(|k| <[u8; 32]>::try_from(k).ok())
        .ok_or_else(|| Error::BadKey("BACKUP_DB_PUBLIC_KEY (base64url)".into()))
}

/// A new key pair: BACKUP_DB_PUBLIC_KEY (base64url) and the offline secret
/// key ("castor-db-secret-<d>.<x>", the JWK fields db-dump.ts writes).
pub fn key_pair() -> Result<(String, Zeroizing<String>), Error> {
    let secret = StaticSecret::from(*random::<32>()?);
    let public = PublicKey::from(&secret);
    let d = Zeroizing::new(secret.to_bytes());
    Ok((
        B64.encode(public.as_bytes()),
        Zeroizing::new(format!(
            "castor-db-secret-{}.{}",
            B64.encode(d.as_ref()),
            B64.encode(public.as_bytes())
        )),
    ))
}

/// Seals `plain` to the recipient's public key with a fresh ephemeral key.
pub fn seal(plain: &[u8], recipient: &[u8; 32]) -> Result<Vec<u8>, Error> {
    let eph = StaticSecret::from(*random::<32>()?);
    let eph_pub = PublicKey::from(&eph);
    let shared = eph.diffie_hellman(&PublicKey::from(*recipient));
    // A low-order recipient key would make the shared secret predictable.
    if !shared.was_contributory() {
        return Err(Error::BadKey("BACKUP_DB_PUBLIC_KEY".into()));
    }
    let key = seal_key(eph_pub.as_bytes(), recipient, shared.as_bytes())?;
    let iv = random::<IV_LEN>()?;
    let mut out = Vec::with_capacity(HEADER + IV_LEN + TAG_LEN + plain.len());
    out.extend_from_slice(MAGIC);
    out.extend_from_slice(eph_pub.as_bytes());
    let mut ct = plain.to_vec();
    let tag = Aes256Gcm::new(&(*key).into())
        .encrypt_inout_detached(&Nonce::from(*iv), &out, ct.as_mut_slice().into())
        .map_err(|_| Error::Decrypt)?;
    out.extend_from_slice(iv.as_ref());
    out.extend_from_slice(&tag);
    out.extend_from_slice(&ct);
    Ok(out)
}

/// Opens a dump with the offline secret key ("castor-db-secret-<d>.<x>").
pub fn open(sealed: &[u8], secret_key: &str) -> Result<Zeroizing<Vec<u8>>, Error> {
    let bad = || Error::BadKey("the secret key (castor-db-secret-…)".into());
    let rest = secret_key.trim().strip_prefix("castor-db-secret-").ok_or_else(bad)?;
    let (d, x) = rest.split_once('.').ok_or_else(bad)?;
    let d: Zeroizing<Vec<u8>> = Zeroizing::new(B64.decode(d).map_err(|_| bad())?);
    let d: [u8; 32] = d.as_slice().try_into().map_err(|_| bad())?;
    let x: [u8; 32] = B64.decode(x).ok().and_then(|x| x.try_into().ok()).ok_or_else(bad)?;
    let secret = StaticSecret::from(d);
    if PublicKey::from(&secret).as_bytes() != &x {
        return Err(bad());
    }
    if sealed.len() < HEADER + IV_LEN + TAG_LEN || &sealed[..MAGIC.len()] != MAGIC {
        return Err(Error::Unsupported);
    }
    let eph: [u8; 32] = sealed[MAGIC.len()..HEADER].try_into().map_err(|_| Error::Unsupported)?;
    let iv: [u8; IV_LEN] = sealed[HEADER..HEADER + IV_LEN]
        .try_into()
        .map_err(|_| Error::Unsupported)?;
    let tag: [u8; TAG_LEN] = sealed[HEADER + IV_LEN..HEADER + IV_LEN + TAG_LEN]
        .try_into()
        .map_err(|_| Error::Unsupported)?;
    let shared = secret.diffie_hellman(&PublicKey::from(eph));
    let key = seal_key(&eph, &x, shared.as_bytes())?;
    let mut buf = Zeroizing::new(sealed[HEADER + IV_LEN + TAG_LEN..].to_vec());
    Aes256Gcm::new(&(*key).into())
        .decrypt_inout_detached(
            &Nonce::from(iv),
            &sealed[..HEADER],
            buf.as_mut_slice().into(),
            &Tag::from(tag),
        )
        .map_err(|_| Error::Decrypt)?;
    Ok(buf)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    #[test]
    fn new_key_pairs_seal_and_open() {
        let (public, secret) = key_pair().unwrap();
        let sealed = seal(b"dump", &public_key(&public).unwrap()).unwrap();
        assert_eq!(open(&sealed, &secret).unwrap().as_slice(), b"dump");
        assert_ne!(key_pair().unwrap().0, public);
    }

    /// A throwaway key pair and a dump sealed by db-dump.ts with it.
    const FIXTURE: &str = include_str!("../tests/fixtures/db-dump.json");

    fn fixture() -> (String, String, Vec<u8>) {
        let v: serde_json::Value = serde_json::from_str(FIXTURE).unwrap();
        let sealed = base64::engine::general_purpose::STANDARD
            .decode(v["sealed"].as_str().unwrap())
            .unwrap();
        (
            v["publicKey"].as_str().unwrap().into(),
            v["secretKey"].as_str().unwrap().into(),
            sealed,
        )
    }

    #[test]
    fn opens_what_the_typescript_worker_sealed() {
        let (_, secret, sealed) = fixture();
        assert_eq!(open(&sealed, &secret).unwrap().as_slice(), b"-- castor dump fixture\n");
    }

    #[test]
    fn seals_for_the_same_key_and_rejects_tampering() {
        let (public, secret, _) = fixture();
        let sealed = seal(b"pg_dumpall output", &public_key(&public).unwrap()).unwrap();
        assert_eq!(&sealed[..9], b"CASTORDB1");
        assert_eq!(open(&sealed, &secret).unwrap().as_slice(), b"pg_dumpall output");
        for i in [0, 12, 45, 60, sealed.len() - 1] {
            let mut bad = sealed.clone();
            bad[i] ^= 1;
            assert!(open(&bad, &secret).is_err(), "byte {i}");
        }
        assert!(open(&sealed[..40], &secret).is_err());
        assert!(public_key("short").is_err());
        // The all-zero point is refused.
        assert!(seal(b"x", &[0u8; 32]).is_err());
        let other = format!("castor-db-secret-{}", &secret["castor-db-secret-".len() + 1..]);
        assert!(open(&sealed, &other).is_err());
    }
}
