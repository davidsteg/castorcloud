// Writes node-sealed.json: values sealed by the TypeScript core that the Rust
// crate must open.  node gen-node-sealed.mts > node-sealed.json
import { encrypt, sealSecret, newWrappedVolumeKey, unwrapVolumeKey } from "../../../../../packages/core/src/crypto.ts";
const secretsKey = "000102030405060708090a0b0c0d0e0f101112131415161718191a1b1c1d1e1f";
const volumeKek = "f0e0d0c0b0a090807060504030201000ffeeddccbbaa99887766554433221100";
process.env.SECRETS_ENCRYPTION_KEY = secretsKey;
process.env.VOLUME_KEK = volumeKek;
const secrets = [["cm0tenant1", "DB_PASSWORD", "hunter2"], ["cm0tenant1", "EMPTY", ""], ["cm0tenant2", "OIDC_CLIENT_SECRET", "Grüezi 🏔 – ✓"]]
  .map(([tenant, name, value]) => ({ tenant, name, value, sealed: sealSecret(tenant, name, value) }));
const volumes = ["cm0tenant1", "cm0tenant2"].map((tenant) => {
  const wrapped = newWrappedVolumeKey(tenant);
  return { tenant, wrapped, dek_hex: unwrapVolumeKey(tenant, wrapped).toString("hex") };
});
console.log(JSON.stringify({ comment: "Sealed by packages/core/src/crypto.ts with fixed test-only keys. Regenerate with gen-node-sealed.mts", secrets_key: secretsKey, volume_kek: volumeKek, secrets, volumes }, null, 2));
