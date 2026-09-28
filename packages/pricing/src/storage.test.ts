import assert from "node:assert/strict";
import { test } from "node:test";
import { fitsStorage, formatStorage, minStorageUnits, storageLevel, storageQuotaMb } from "./storage.ts";

const GB = 1024;

test("one storage unit is 100 GB", () => {
  assert.equal(storageQuotaMb(1), 100 * GB);
  assert.equal(storageQuotaMb(5), 500 * GB);
});

test("warning levels", () => {
  const q = storageQuotaMb(1);
  assert.equal(storageLevel(0, q), "ok");
  assert.equal(storageLevel(79 * GB, q), "ok");
  assert.equal(storageLevel(80 * GB, q), "warn");
  assert.equal(storageLevel(95 * GB, q), "full");
  assert.equal(storageLevel(120 * GB, q), "full");
});

test("downgrades must leave 10 % headroom", () => {
  assert.equal(fitsStorage(90 * GB, 1), true);
  assert.equal(fitsStorage(91 * GB, 1), false);
  assert.equal(minStorageUnits(0), 1);
  assert.equal(minStorageUnits(91 * GB), 2);
  assert.equal(minStorageUnits(250 * GB), 3);
  assert.equal(minStorageUnits(10_000 * GB), 50);
});

test("formatting", () => {
  assert.equal(formatStorage(500), "500 MB");
  assert.equal(formatStorage(1.5 * GB), "1.5 GB");
  assert.equal(formatStorage(120 * GB), "120 GB");
  assert.equal(formatStorage(2500 * GB), "2.5 TB");
});
