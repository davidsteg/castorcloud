//! Envelope encryption for everything sensitive the control plane stores.
//!
//! Byte-compatible with `packages/core/src/crypto.ts`: values sealed by the
//! TypeScript worker open here and the other way round, so both can run side
//! by side while the core moves to Rust.
//!
//!   SECRETS_ENCRYPTION_KEY  encrypts tenant secrets (DB passwords, OIDC secrets)
//!   VOLUME_KEK              wraps each tenant's volume data-encryption key (DEK)
//!
//! Both are 32-byte keys given as 64 hex characters. Keys and plaintexts are
//! wiped from memory when dropped.
//!
//! Format: `v1.<iv>.<tag>.<ciphertext>`, AES-256-GCM, each part base64url
//! without padding, 12-byte IV, 16-byte tag. This side is stricter than
//! Node when opening: other IV or tag lengths, padding and extra parts are
//! rejected (Node never writes them).

use aes_gcm::aead::{AeadInOut, KeyInit};
use aes_gcm::{Aes256Gcm, Nonce, Tag};
use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD as B64;
use zeroize::{Zeroize, ZeroizeOnDrop, Zeroizing};

const VERSION: &str = "v1";
const IV_LEN: usize = 12;
const TAG_LEN: usize = 16;
/// Volume DEKs are 64 bytes: LUKS2 AES-XTS-512 needs two 256-bit keys.
pub const VOLUME_KEY_LEN: usize = 64;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("{0} must be 32 bytes as 64 hex characters")]
    BadKey(String),
    #[error("unsupported ciphertext")]
    Unsupported,
    /// Wrong key, wrong tenant/name binding, or tampered data. Deliberately
    /// carries no detail.
    #[error("decryption failed")]
    Decrypt,
    #[error("no randomness available")]
    Random,
    #[error("plaintext is not UTF-8")]
    NotUtf8,
}

/// A 256-bit key. Not `Clone`, not `Debug`-printable, wiped on drop.
#[derive(Zeroize, ZeroizeOnDrop)]
pub struct Key([u8; 32]);

impl Key {
    pub fn from_hex(name: &str, hex_key: &str) -> Result<Self, Error> {
        let mut bytes = [0u8; 32];
        if hex_key.len() != 64 || hex::decode_to_slice(hex_key, &mut bytes).is_err() {
            bytes.zeroize();
            return Err(Error::BadKey(name.to_owned()));
        }
        Ok(Self(bytes))
    }

    /// Reads the key from the environment variable `name`.
    pub fn from_env(name: &str) -> Result<Self, Error> {
        let value = Zeroizing::new(std::env::var(name).map_err(|_| Error::BadKey(name.to_owned()))?);
        Self::from_hex(name, &value)
    }

    fn cipher(&self) -> Aes256Gcm {
        Aes256Gcm::new(&self.0.into())
    }
}

impl std::fmt::Debug for Key {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Key(<redacted>)")
    }
}

fn random<const N: usize>() -> Result<Zeroizing<[u8; N]>, Error> {
    let mut out = Zeroizing::new([0u8; N]);
    getrandom::fill(out.as_mut()).map_err(|_| Error::Random)?;
    Ok(out)
}

/// AES-256-GCM with a fresh random IV. Output: `v1.<iv>.<tag>.<ciphertext>`.
pub fn encrypt(plaintext: &[u8], key: &Key, aad: &[u8]) -> Result<String, Error> {
    let iv = random::<IV_LEN>()?;
    let mut buf = plaintext.to_vec();
    let tag = key
        .cipher()
        .encrypt_inout_detached(&Nonce::from(*iv), aad, buf.as_mut_slice().into())
        .map_err(|_| Error::Decrypt)?;
    Ok(format!(
        "{VERSION}.{}.{}.{}",
        B64.encode(*iv),
        B64.encode(tag),
        B64.encode(&buf)
    ))
}

pub fn decrypt(sealed: &str, key: &Key, aad: &[u8]) -> Result<Zeroizing<Vec<u8>>, Error> {
    let mut parts = sealed.split('.');
    let (Some(VERSION), Some(iv), Some(tag), Some(ct), None) =
        (parts.next(), parts.next(), parts.next(), parts.next(), parts.next())
    else {
        return Err(Error::Unsupported);
    };
    let iv: [u8; IV_LEN] = decode(iv)?.try_into().map_err(|_| Error::Unsupported)?;
    let tag: [u8; TAG_LEN] = decode(tag)?.try_into().map_err(|_| Error::Unsupported)?;
    let mut buf = Zeroizing::new(decode(ct)?);
    key.cipher()
        .decrypt_inout_detached(&Nonce::from(iv), aad, buf.as_mut_slice().into(), &Tag::from(tag))
        .map_err(|_| Error::Decrypt)?;
    Ok(buf)
}

fn decode(part: &str) -> Result<Vec<u8>, Error> {
    B64.decode(part).map_err(|_| Error::Unsupported)
}

/// The control plane's two keys, loaded once at startup.
#[derive(Debug)]
pub struct Keys {
    pub secrets: Key,
    pub volume_kek: Key,
}

impl Keys {
    pub fn from_env() -> Result<Self, Error> {
        Ok(Self {
            secrets: Key::from_env("SECRETS_ENCRYPTION_KEY")?,
            volume_kek: Key::from_env("VOLUME_KEK")?,
        })
    }

    // The tenant id (and secret name) is bound as additional data, so a
    // ciphertext copied to another tenant's row fails to decrypt.

    pub fn seal_secret(&self, tenant_id: &str, name: &str, value: &str) -> Result<String, Error> {
        encrypt(value.as_bytes(), &self.secrets, secret_aad(tenant_id, name).as_bytes())
    }

    pub fn open_secret(&self, tenant_id: &str, name: &str, sealed: &str) -> Result<Zeroizing<String>, Error> {
        let mut bytes = decrypt(sealed, &self.secrets, secret_aad(tenant_id, name).as_bytes())?;
        let text = String::from_utf8(std::mem::take(&mut *bytes)).map_err(|e| {
            e.into_bytes().zeroize();
            Error::NotUtf8
        })?;
        Ok(Zeroizing::new(text))
    }

    /// New random volume key, returned wrapped for storage.
    pub fn new_wrapped_volume_key(&self, tenant_id: &str) -> Result<String, Error> {
        let dek = random::<VOLUME_KEY_LEN>()?;
        encrypt(dek.as_ref(), &self.volume_kek, volume_aad(tenant_id).as_bytes())
    }

    /// Only ever handed to the node helper on stdin; never written to disk.
    pub fn unwrap_volume_key(&self, tenant_id: &str, wrapped: &str) -> Result<Zeroizing<Vec<u8>>, Error> {
        let dek = decrypt(wrapped, &self.volume_kek, volume_aad(tenant_id).as_bytes())?;
        if dek.len() != VOLUME_KEY_LEN {
            return Err(Error::Unsupported);
        }
        Ok(dek)
    }
}

/// What a ZFS dataset key is derived for (zfs.ts): the tenant's own data, or
/// its custom stacks on a STACKS node, which never holds the other key.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Dataset {
    Tenant,
    Stacks,
}

/// ZFS wants a 32-byte key; volume keys are 64 random bytes. HKDF-SHA256 with
/// an empty salt, as `datasetKey` and `stacksDatasetKey` in zfs.ts, so deleting
/// the one volume key still shreds both datasets. Hex with a newline is what
/// `keyformat=hex` reads from stdin.
pub fn dataset_key(volume_key: &[u8], dataset: Dataset) -> Result<Zeroizing<Vec<u8>>, Error> {
    let info: &[u8] = match dataset {
        Dataset::Tenant => b"castor-zfs-dataset-v1",
        Dataset::Stacks => b"castor-zfs-stacks-v1",
    };
    let mut key = Zeroizing::new([0u8; 32]);
    hkdf::Hkdf::<sha2::Sha256>::new(Some(&[]), volume_key)
        .expand(info, key.as_mut())
        .map_err(|_| Error::Unsupported)?;
    let mut out = Zeroizing::new(hex::encode(key.as_ref()).into_bytes());
    out.push(b'\n');
    Ok(out)
}

fn secret_aad(tenant_id: &str, name: &str) -> String {
    format!("{tenant_id}:{name}")
}

fn volume_aad(tenant_id: &str) -> String {
    format!("volume:{tenant_id}")
}
