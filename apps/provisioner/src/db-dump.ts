// Encrypted dumps of the control-plane database. The database holds every
// tenant's wrapped volume key: without it the tenant backups can't be
// restored, so it's backed up too. Dumps are sealed to an X25519 public key
// whose private half David keeps offline. The control plane can write dumps
// but can't read them back, and neither can the backup box.
//
// Format: "CASTORDB1" | ephemeral public key (32) | iv (12) | tag (16) | ciphertext
// AES-256-GCM, key = HKDF-SHA256(X25519(ephemeral, recipient), salt = both
// public keys, info "castor-db-dump-v1"). The header is authenticated.
import { createCipheriv, createDecipheriv, createPrivateKey, createPublicKey, diffieHellman, generateKeyPairSync, hkdfSync, randomBytes, type KeyObject } from "node:crypto";

const MAGIC = Buffer.from("CASTORDB1");
const INFO = "castor-db-dump-v1";

const raw = (k: KeyObject) => Buffer.from(k.export({ format: "jwk" }).x!, "base64url");
const publicKey = (x: Buffer) => createPublicKey({ key: { kty: "OKP", crv: "X25519", x: x.toString("base64url") }, format: "jwk" });

function sealKey(ephemeralPub: Buffer, recipientPub: Buffer, shared: Buffer) {
  return Buffer.from(hkdfSync("sha256", shared, Buffer.concat([ephemeralPub, recipientPub]), INFO, 32));
}

/** A new key pair: the public key goes in BACKUP_DB_PUBLIC_KEY, the secret one offline. */
export function dumpKeyPair() {
  const { publicKey: pub, privateKey } = generateKeyPairSync("x25519");
  const jwk = privateKey.export({ format: "jwk" });
  return { publicKey: raw(pub).toString("base64url"), secretKey: `castor-db-secret-${jwk.d}.${jwk.x}` };
}

export function sealDump(plain: Buffer, recipientPublicKey: string): Buffer {
  const recipient = Buffer.from(recipientPublicKey, "base64url");
  if (recipient.length !== 32) throw new Error("BACKUP_DB_PUBLIC_KEY must be a 32-byte X25519 key (base64url)");
  const eph = generateKeyPairSync("x25519");
  const ephPub = raw(eph.publicKey);
  const key = sealKey(ephPub, recipient, diffieHellman({ privateKey: eph.privateKey, publicKey: publicKey(recipient) }));
  const iv = randomBytes(12);
  const cipher = createCipheriv("aes-256-gcm", key, iv);
  const header = Buffer.concat([MAGIC, ephPub]);
  cipher.setAAD(header);
  const ct = Buffer.concat([cipher.update(plain), cipher.final()]);
  key.fill(0);
  return Buffer.concat([header, iv, cipher.getAuthTag(), ct]);
}

export function openDump(sealed: Buffer, secretKey: string): Buffer {
  const m = /^castor-db-secret-([A-Za-z0-9_-]{43})\.([A-Za-z0-9_-]{43})$/.exec(secretKey.trim());
  if (!m) throw new Error("not a castor-db-secret key");
  const priv = createPrivateKey({ key: { kty: "OKP", crv: "X25519", d: m[1]!, x: m[2]! }, format: "jwk" });
  if (!sealed.subarray(0, MAGIC.length).equals(MAGIC)) throw new Error("not a Castor database dump");
  let o = MAGIC.length;
  const ephPub = sealed.subarray(o, (o += 32));
  const iv = sealed.subarray(o, (o += 12));
  const tag = sealed.subarray(o, (o += 16));
  const key = sealKey(ephPub, Buffer.from(m[2]!, "base64url"), diffieHellman({ privateKey: priv, publicKey: publicKey(ephPub) }));
  const decipher = createDecipheriv("aes-256-gcm", key, iv);
  decipher.setAAD(sealed.subarray(0, MAGIC.length + 32));
  decipher.setAuthTag(tag);
  try {
    return Buffer.concat([decipher.update(sealed.subarray(o)), decipher.final()]);
  } finally {
    key.fill(0);
  }
}
