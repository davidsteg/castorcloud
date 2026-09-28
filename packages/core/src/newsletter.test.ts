import assert from "node:assert/strict";
import { test } from "node:test";
import { RateLimiter, hashToken, newConfirmToken, normalizeEmail, unsubscribeToken, verifyUnsubscribeToken } from "./newsletter.ts";

test("addresses are trimmed and lower-cased", () => {
  assert.equal(normalizeEmail("  Anna.Muster+news@Example.CH "), "anna.muster+news@example.ch");
  assert.equal(normalizeEmail("x@xn--bcher-kva.ch"), "x@xn--bcher-kva.ch");
});

test("things that aren't a plain address are rejected", () => {
  for (const bad of [
    undefined, 42, "", "anna", "anna@", "@example.ch", "anna@example", "anna@@example.ch",
    "anna muster@example.ch", "\"anna\"@example.ch", "anna@example.ch\nBcc: x@y.ch", "anna@exa_mple.ch",
    ".anna@example.ch", "anna.@example.ch", "an..na@example.ch", "anna@-example.ch", "<anna@example.ch>",
    `${"a".repeat(65)}@example.ch`, `a@${"b".repeat(250)}.ch`,
  ]) {
    assert.equal(normalizeEmail(bad), null, String(bad));
  }
});

test("confirmation tokens are random and only their hash is kept", () => {
  const a = newConfirmToken();
  const b = newConfirmToken();
  assert.notEqual(a.token, b.token);
  assert.equal(a.hash, hashToken(a.token));
  assert.notEqual(a.hash, a.token);
  assert.ok(a.token.length >= 43);
});

test("unsubscribe tokens verify only for their subscriber and secret", () => {
  const t = unsubscribeToken("secret", "sub1");
  assert.ok(verifyUnsubscribeToken("secret", "sub1", t));
  assert.ok(!verifyUnsubscribeToken("secret", "sub2", t));
  assert.ok(!verifyUnsubscribeToken("other", "sub1", t));
  assert.ok(!verifyUnsubscribeToken("secret", "sub1", t.slice(1)));
  assert.ok(!verifyUnsubscribeToken("secret", "sub1", undefined));
  assert.throws(() => unsubscribeToken("", "sub1"));
});

test("the rate limiter allows the limit per window", () => {
  const rl = new RateLimiter(2, 1000);
  assert.ok(rl.take("ip", 0));
  assert.ok(rl.take("ip", 10));
  assert.ok(!rl.take("ip", 20));
  assert.ok(rl.take("other", 20));
  assert.ok(rl.take("ip", 1000));
});
