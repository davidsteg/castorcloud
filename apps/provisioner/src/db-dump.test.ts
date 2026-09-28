import assert from "node:assert/strict";
import { test } from "node:test";
import { dumpKeyPair, openDump, sealDump } from "./db-dump.ts";

test("a dump opens only with the matching secret key", () => {
  const kp = dumpKeyPair();
  const plain = Buffer.from("-- PostgreSQL database cluster dump\nCREATE ROLE pc;\n");
  const sealed = sealDump(plain, kp.publicKey);
  assert.ok(!sealed.includes(plain), "no plaintext in the output");
  assert.deepEqual(openDump(sealed, kp.secretKey), plain);
  assert.throws(() => openDump(sealed, dumpKeyPair().secretKey));
});

test("any change to a dump is detected", () => {
  const kp = dumpKeyPair();
  const sealed = sealDump(Buffer.alloc(1000, 1), kp.publicKey);
  for (const i of [3, 12, 50, 60, sealed.length - 1]) {
    const bad = Buffer.from(sealed);
    bad[i]! ^= 1;
    assert.throws(() => openDump(bad, kp.secretKey), `byte ${i}`);
  }
  assert.throws(() => openDump(sealed.subarray(0, sealed.length - 10), kp.secretKey), "truncated");
});

test("rejects a malformed public key", () => {
  assert.throws(() => sealDump(Buffer.from("x"), "short"));
});
