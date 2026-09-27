// Envelope encryption for everything sensitive the control plane stores.
//
//   SECRETS_ENCRYPTION_KEY  encrypts tenant secrets (DB passwords, OIDC secrets)
//   VOLUME_KEK              wraps each tenant's volume data-encryption key (DEK)
//
// Both are 32-byte hex keys from the environment. In production the KEK
// belongs in an HSM / Vault transit engine; only wrap/unwrap would change.
import { createCipheriv, createDecipheriv, randomBytes } from "node:crypto";

const VERSION = "v1";

function keyFromEnv(name: string): Buffer {
  const hex = process.env[name];
  if (!hex || !/^[0-9a-f]{64}$/i.test(hex)) {
    throw new Error(`${name} must be 32 bytes as 64 hex characters`);
  }
  return Buffer.from(hex, "hex");
}

/** AES-256-GCM. Output: "v1.<iv>.<tag>.<ciphertext>" (base64url parts). */
export function encrypt(plaintext: Buffer | string, key: Buffer, aad = ""): string {
  const iv = randomBytes(12);
  const cipher = createCipheriv("aes-256-gcm", key, iv);
  cipher.setAAD(Buffer.from(aad));
  const ct = Buffer.concat([cipher.update(plaintext), cipher.final()]);
  return [VERSION, iv, cipher.getAuthTag(), ct].map((p) => (typeof p === "string" ? p : p.toString("base64url"))).join(".");
}

export function decrypt(sealed: string, key: Buffer, aad = ""): Buffer {
  const [version, iv, tag, ct] = sealed.split(".");
  if (version !== VERSION || !iv || !tag || ct === undefined) throw new Error("unsupported ciphertext");
  const decipher = createDecipheriv("aes-256-gcm", key, Buffer.from(iv, "base64url"));
  decipher.setAAD(Buffer.from(aad));
  decipher.setAuthTag(Buffer.from(tag, "base64url"));
  return Buffer.concat([decipher.update(Buffer.from(ct, "base64url")), decipher.final()]);
}

// The tenant id is bound as additional data, so a ciphertext copied to
// another tenant's row fails to decrypt.

export function sealSecret(tenantId: string, name: string, value: string): string {
  return encrypt(value, keyFromEnv("SECRETS_ENCRYPTION_KEY"), `${tenantId}:${name}`);
}

export function openSecret(tenantId: string, name: string, sealed: string): string {
  return decrypt(sealed, keyFromEnv("SECRETS_ENCRYPTION_KEY"), `${tenantId}:${name}`).toString("utf8");
}

/** New random volume key, returned wrapped for storage. */
export function newWrappedVolumeKey(tenantId: string): string {
  return encrypt(randomBytes(64), keyFromEnv("VOLUME_KEK"), `volume:${tenantId}`);
}

/** Only ever passed to cryptsetup via stdin; never written to disk. */
export function unwrapVolumeKey(tenantId: string, wrapped: string): Buffer {
  return decrypt(wrapped, keyFromEnv("VOLUME_KEK"), `volume:${tenantId}`);
}
