import assert from "node:assert/strict";
import { test } from "node:test";
import { auditDetails, auditFilter, auditQuery } from "./audit-view.ts";

test("filters come from the URL, checked", () => {
  assert.deepEqual(auditFilter({}), {});
  assert.deepEqual(auditFilter({ action: "staff.grant", slug: "Muster", actor: "ops-cli:david" }), {
    action: { value: "staff.grant", prefix: false },
    slug: "muster",
    actor: "ops-cli:david",
  });
  assert.deepEqual(auditFilter({ action: "backup." }).action, { value: "backup.", prefix: true });
  assert.deepEqual(auditFilter({ action: ["tenant.retry", "x"] }).action, { value: "tenant.retry", prefix: false });
  assert.equal(
    auditFilter({ before: "2026-11-02T21:00:00.000Z" }).before?.toISOString(),
    "2026-11-02T21:00:00.000Z",
  );
});

test("malformed filters are dropped", () => {
  for (const bad of ["", "staff grant", "staff..grant", "'; drop", ".x", "a".repeat(65), "x\ny"]) {
    assert.equal(auditFilter({ action: bad }).action, undefined, bad);
  }
  for (const bad of ["-x", "x-", "a b", "a/b", "a".repeat(64)]) {
    assert.equal(auditFilter({ slug: bad }).slug, undefined, bad);
  }
  for (const bad of ["a b", "x\ny", "<script>", "a".repeat(81)]) {
    assert.equal(auditFilter({ actor: bad }).actor, undefined, bad);
  }
  for (const bad of ["yesterday", "2026-13-45T99:00:00Z", "2026-11-02", "2026-11-02T21:00:00+01:00"]) {
    assert.equal(auditFilter({ before: bad }).before, undefined, bad);
  }
});

test("the filter goes back into the next page's link", () => {
  const f = auditFilter({ action: "staff.", slug: "muster" });
  assert.equal(auditQuery(f), "?action=staff.&slug=muster");
  assert.equal(
    auditQuery({}, new Date("2026-11-02T21:00:00.000Z")),
    "?before=2026-11-02T21%3A00%3A00.000Z",
  );
});

test("details are one line, cut", () => {
  assert.equal(auditDetails(null), "");
  assert.equal(auditDetails({ slug: "muster" }), '{"slug":"muster"}');
  const long = auditDetails({ x: "y".repeat(1000) }, 50);
  assert.equal(long.length, 50);
  assert.ok(long.endsWith("…"));
});
