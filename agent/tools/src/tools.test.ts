import assert from "node:assert/strict";
import { test } from "node:test";
import { ToolError, clampLimit, excerpt, isoDate, localTime } from "./limits.ts";
import { memberForToken, sha256Hex, type Member } from "./members.ts";
import { canView, type PaperlessDoc, type PaperlessUser } from "./services/docs.ts";
import { filterFlights, summarizeFlight, type Flight } from "./services/flights.ts";
import { summarizeAsset } from "./services/photos.ts";
import { stretches } from "./services/places.ts";

const member = (id: string, token: string): Member => ({ id, email: `${id}@x.ch`, sub: `sub-${id}`, tokenSha256: sha256Hex(token) });

test("a token only identifies its own member", () => {
  const members = [member("anna", "token-a"), member("ben", "token-b")];
  assert.equal(memberForToken(members, "Bearer token-a")?.id, "anna");
  assert.equal(memberForToken(members, "Bearer token-b")?.id, "ben");
  assert.equal(memberForToken(members, "Bearer token-c"), undefined);
  assert.equal(memberForToken(members, "token-a"), undefined);
  assert.equal(memberForToken(members, undefined), undefined);
});

test("excerpts are short plain text", () => {
  assert.equal(excerpt("<span class='match'>Hotel</span>   Lisboa\n invoice"), "Hotel Lisboa invoice");
  const long = excerpt("word ".repeat(200), 50)!;
  assert.ok(long.length <= 51 && long.endsWith("…"));
  assert.equal(excerpt(""), undefined);
});

test("limits and dates are validated", () => {
  assert.equal(clampLimit(500), 25);
  assert.equal(clampLimit(0), 1);
  assert.equal(clampLimit(undefined), 10);
  // Days are local to the tenant (Zurich: UTC+2 in summer, UTC+1 in winter).
  assert.equal(isoDate("2026-05-01", false, "Europe/Zurich"), "2026-04-30T22:00:00.000Z");
  assert.equal(isoDate("2026-05-01", true, "Europe/Zurich"), "2026-05-01T21:59:59.999Z");
  assert.equal(isoDate("2026-01-15", false, "Europe/Zurich"), "2026-01-14T23:00:00.000Z");
  assert.equal(localTime("2026-05-01T08:00:00Z", "Europe/Zurich"), "2026-05-01T10:00");
  assert.throws(() => isoDate("last tuesday"), ToolError);
});

test("documents follow Paperless visibility", () => {
  const anna: PaperlessUser = { id: 2, username: "anna", groups: [7] };
  const doc = (d: Partial<PaperlessDoc>): PaperlessDoc => ({ id: 1, title: "t", ...d });
  assert.ok(canView(doc({ owner: 2 }), anna));
  assert.ok(canView(doc({ owner: null }), anna));
  assert.ok(!canView(doc({ owner: 3 }), anna));
  assert.ok(canView(doc({ owner: 3, permissions: { view: { users: [2] } } }), anna));
  assert.ok(canView(doc({ owner: 3, permissions: { change: { groups: [7] } } }), anna));
  assert.ok(canView(doc({ owner: 3 }), { ...anna, is_superuser: true }));
});

test("location points become city stretches", () => {
  const t = (iso: string) => Date.parse(iso) / 1000;
  const out = stretches([
    { timestamp: t("2026-05-01T09:00Z"), city: "Zürich", country_name: "Switzerland" },
    { timestamp: t("2026-05-01T08:00Z"), city: "Zürich", country_name: "Switzerland" },
    { timestamp: t("2026-05-01T12:00Z"), city: null, country_name: null },
    { timestamp: t("2026-05-01T15:00Z"), city: "Lisbon", country_name: "Portugal" },
  ]);
  assert.deepEqual(out, [
    { place: "Zürich, Switzerland", from: "2026-05-01T10:00", to: "2026-05-01T11:00" },
    { place: "Lisbon, Portugal", from: "2026-05-01T17:00", to: "2026-05-01T17:00" },
  ]);
});

test("flights are filtered by date and airport and summarized", () => {
  const lis = { iata: "LIS", icao: "LPPT", name: "Humberto Delgado", municipality: "Lisbon", country: "PT" };
  const zrh = { iata: "ZRH", icao: "LSZH", name: "Zurich Airport", municipality: "Zurich", country: "CH" };
  const flights: Flight[] = [
    { id: 1, date: "2026-05-01", from: zrh, to: lis, flightNumber: "LX2084", duration: 9900, airline: { name: "Swiss" }, departure: "2026-05-01T11:40:00.000Z" },
    { id: 2, date: "2025-01-10", from: zrh, to: { ...lis, iata: "OPO", municipality: "Porto" } },
  ];
  assert.deepEqual(filterFlights(flights, { airport: "lisbon" }).map((f) => f.id), [1]);
  assert.deepEqual(filterFlights(flights, { from: "2026-01-01" }).map((f) => f.id), [1]);
  assert.deepEqual(summarizeFlight(flights[0]), {
    date: "2026-05-01", from: "Zurich (ZRH, CH)", to: "Lisbon (LIS, PT)", flight: "LX2084", airline: "Swiss", minutes: 165,
    departure: "2026-05-01T13:40", // Zurich time
  });
});

test("photos are described, never sent", () => {
  const s = summarizeAsset(
    { id: "abc", type: "IMAGE", localDateTime: "2026-05-02T18:30:00.000Z", exifInfo: { city: "Lisbon", country: "Portugal" }, people: [{ name: "Ben" }] },
    "https://muster.castorcloud.ch",
  );
  assert.deepEqual(s, {
    taken: "2026-05-02T18:30", place: "Lisbon, Portugal", people: ["Ben"], link: "https://muster.castorcloud.ch/photos/photos/abc",
  });
});
