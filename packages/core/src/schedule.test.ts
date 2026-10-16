import assert from "node:assert/strict";
import { test } from "node:test";
import { nextRun } from "./schedule.ts";

const at = (iso: string) => new Date(iso);

test("intervals run that long after the last run", () => {
  assert.deepEqual(nextRun({ everyMs: 3_600_000 }, at("2026-10-16T10:00:00Z")), at("2026-10-16T11:00:00Z"));
});

test("a daily time is local to its zone", () => {
  // Zurich is UTC+2 in summer time: 02:30 local = 00:30 UTC.
  const daily = { at: "02:30", tz: "Europe/Zurich" };
  assert.deepEqual(nextRun(daily, at("2026-10-16T00:00:00Z")), at("2026-10-16T00:30:00Z"));
  assert.deepEqual(nextRun(daily, at("2026-10-16T00:30:00Z")), at("2026-10-17T00:30:00Z"));
  // After the switch to winter time (25 Oct 2026) it is 01:30 UTC.
  assert.deepEqual(nextRun(daily, at("2026-10-25T00:00:00Z")), at("2026-10-25T01:30:00Z"));
  assert.deepEqual(nextRun(daily, at("2026-12-01T12:00:00Z")), at("2026-12-02T01:30:00Z"));
});

test("a weekly time runs on its weekday only", () => {
  const sunday4 = { at: "04:00", tz: "Europe/Zurich", weekday: 0 };
  // Friday 16 Oct 2026 -> Sunday 18 Oct, 04:00 CEST = 02:00 UTC.
  assert.deepEqual(nextRun(sunday4, at("2026-10-16T10:00:00Z")), at("2026-10-18T02:00:00Z"));
  assert.deepEqual(nextRun(sunday4, at("2026-10-18T02:00:00Z")), at("2026-10-25T03:00:00Z"));
});

test("bad times are refused", () => {
  assert.throws(() => nextRun({ at: "2:30", tz: "Europe/Zurich" }, new Date()));
});
