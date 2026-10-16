import assert from "node:assert/strict";
import { test } from "node:test";
import { AI_CREDIT_TIERS_CHF } from "./catalog.ts";
import { aiBudgetChf, aiBudgetUsd } from "./ai-budget.ts";
import { quote } from "./quote.ts";

const parts = (q: ReturnType<typeof quote>) => Object.values(q.breakdown).reduce((a, b) => a + b, 0);

test("breakdown parts add up to the effective monthly price", () => {
  for (const interval of ["month", "year"] as const) {
    const q = quote({ services: ["photos", "docs", "assistant"], storageUnits: 5, extraMembers: 2, interval, aiCreditChf: 20 });
    assert.equal(parts(q), q.effectiveMonthlyRp);
  }
});

test("donations follow the booked services", () => {
  const q = quote({ services: ["photos"], storageUnits: 1 });
  assert.ok(q.donations.immich! > 0);
  assert.equal(q.donations.paperless, undefined);
  // Base fee and service both go to the booked service's project.
  assert.deepEqual(Object.keys(q.donations), ["immich"]);
  assert.equal(q.breakdown.openSourceRp, q.donations.immich!);
});

test("more storage changes only servers/storage, operations, VAT and payment fees", () => {
  const small = quote({ services: ["photos", "docs"], storageUnits: 1 });
  const big = quote({ services: ["photos", "docs"], storageUnits: 20 });
  assert.equal(big.breakdown.openSourceRp, small.breakdown.openSourceRp);
  assert.deepEqual(big.donations, small.donations);
  assert.equal(big.breakdown.aiCreditRp, small.breakdown.aiCreditRp);
  assert.ok(big.breakdown.infrastructureRp > small.breakdown.infrastructureRp);
  assert.ok(big.breakdown.operationsRp > small.breakdown.operationsRp);
});

test("the donation is 10 % of the net base price", () => {
  // base 3.00 + photos 3.00 = 6.00 gross -> 5.55 net -> 0.56 donation
  const q = quote({ services: ["photos"], storageUnits: 7 });
  assert.equal(q.breakdown.openSourceRp, 56);
});

test("smallest photo plan: base + photos + 100 GB = CHF 8.00", () => {
  const q = quote({ services: ["photos"], storageUnits: 1 });
  assert.equal(q.monthlyRp, 800);
  assert.equal(q.chargeRp, 800);
  assert.ok(q.breakdown.operationsRp > 0, "must not sell at a loss");
});

test("yearly billing charges 10 months for 12", () => {
  const q = quote({ services: ["photos"], storageUnits: 1, interval: "year" });
  assert.equal(q.chargeRp, 8000);
  assert.equal(q.effectiveMonthlyRp, 667);
});

test("AI credit: required with the assistant, never discounted, no donation", () => {
  assert.equal(quote({ services: ["photos"] }).config.aiCreditChf, 0);
  assert.equal(quote({ services: ["assistant"] }).config.aiCreditChf, 10);
  assert.equal(quote({ services: ["assistant"], aiCreditChf: 7 }).config.aiCreditChf, 10);

  const m = quote({ services: ["assistant"], aiCreditChf: 20 });
  const y = quote({ services: ["assistant"], aiCreditChf: 20, interval: "year" });
  assert.equal(y.chargeRp, (300 + 300 + 200) * 10 + 2000 * 12);
  assert.equal(y.breakdown.aiCreditRp, m.breakdown.aiCreditRp);
  const noCredit = quote({ services: ["assistant"], aiCreditChf: 5 });
  assert.equal(m.breakdown.openSourceRp, noCredit.breakdown.openSourceRp);
  assert.ok(m.breakdown.operationsRp > 0);
});

test("input is clamped and de-duplicated", () => {
  const q = quote({ services: ["photos", "photos", "nope" as never], storageUnits: 999, extraMembers: -3 });
  assert.deepEqual(q.config.services, ["photos"]);
  assert.equal(q.config.storageUnits, 50);
  assert.equal(q.config.extraMembers, 0);
});

test("the AI budget is the credit net of VAT and payment fees", () => {
  // 10 / 1.081 - 10 * 3.6 % = 8.8906… -> 8.89
  assert.equal(aiBudgetChf(10), 8.89);
  assert.equal(aiBudgetChf(0), 0);
  assert.equal(aiBudgetUsd(10, 1.1), 9.77);
  for (const chf of AI_CREDIT_TIERS_CHF) {
    const q = quote({ services: ["assistant"], storageUnits: 1, aiCreditChf: chf });
    // Never more than what the price breakdown attributes to the credit.
    assert.ok(aiBudgetChf(chf) * 100 <= q.breakdown.aiCreditRp);
  }
});

test("stack resources are priced per unit, without a donation share", () => {
  const plain = quote({ services: ["photos"], storageUnits: 1 });
  const q = quote({ services: ["photos"], storageUnits: 1, stacks: { memoryUnits: 4, cpuUnits: 2, storageUnits: 1 } });
  // 4 x 0.50 + 2 x 1.00 + 1 x 0.40
  assert.equal(q.monthlyRp - plain.monthlyRp, 440);
  assert.equal(q.breakdown.openSourceRp, plain.breakdown.openSourceRp);
  assert.ok(q.breakdown.operationsRp > plain.breakdown.operationsRp, "must not sell stack resources at a loss");
  assert.deepEqual(q.lines.filter((l) => l.key.startsWith("stack")).map((l) => [l.key, l.quantity]), [
    ["stackram", 4], ["stackcpu", 2], ["stackdisk", 1],
  ]);
  assert.equal(plain.lines.some((l) => l.key.startsWith("stack")), false);
});

test("stack units are clamped", () => {
  const q = quote({ services: ["photos"], stacks: { memoryUnits: 1e6, cpuUnits: -2, storageUnits: 3.4 } });
  assert.deepEqual(q.config.stacks, { memoryUnits: 256, cpuUnits: 0, storageUnits: 3 });
});
