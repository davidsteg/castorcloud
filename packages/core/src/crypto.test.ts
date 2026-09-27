import assert from "node:assert/strict";
import { randomBytes } from "node:crypto";
import { test } from "node:test";
import { newWrappedVolumeKey, openSecret, sealSecret, unwrapVolumeKey } from "./crypto.ts";

process.env.SECRETS_ENCRYPTION_KEY = randomBytes(32).toString("hex");
process.env.VOLUME_KEK = randomBytes(32).toString("hex");

test("secrets round-trip", () => {
  const sealed = sealSecret("t1", "DB_PASSWORD", "hunter2");
  assert.notEqual(sealed, "hunter2");
  assert.equal(openSecret("t1", "DB_PASSWORD", sealed), "hunter2");
});

test("a secret copied to another tenant or name does not decrypt", () => {
  const sealed = sealSecret("t1", "DB_PASSWORD", "hunter2");
  assert.throws(() => openSecret("t2", "DB_PASSWORD", sealed));
  assert.throws(() => openSecret("t1", "OTHER", sealed));
});

test("volume keys are 64 random bytes bound to their tenant", () => {
  const wrapped = newWrappedVolumeKey("t1");
  assert.equal(unwrapVolumeKey("t1", wrapped).length, 64);
  assert.throws(() => unwrapVolumeKey("t2", wrapped));
});

test("tampering is detected", () => {
  const sealed = sealSecret("t1", "K", "value");
  const parts = sealed.split(".");
  parts[3] = Buffer.from("other").toString("base64url");
  assert.throws(() => openSecret("t1", "K", parts.join(".")));
});
