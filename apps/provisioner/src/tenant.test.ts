import assert from "node:assert/strict";
import { test } from "node:test";
import { edgeRoute, type Platform } from "./tenant.ts";

const p: Platform = { baseDomain: "castorcloud.ch", scheme: "https", brandName: "Castor Cloud", timeZone: "Europe/Zurich", authHostIp: "10.0.0.2" };

test("edge route sends the service and assistant hosts of the tenant to its node", () => {
  const yml = edgeRoute("demo", p, "http://10.0.0.31");
  assert.match(yml, /rule: 'Host\(`photos-demo\.castorcloud\.ch`\) \|\| Host\(`docs-demo\.castorcloud\.ch`\) \|\| Host\(`places-demo\.castorcloud\.ch`\) \|\| Host\(`flights-demo\.castorcloud\.ch`\) \|\| Host\(`assistant-demo\.castorcloud\.ch`\)'/);
  assert.match(yml, /entryPoints: \[websecure\]/);
  assert.match(yml, /- url: "http:\/\/10\.0\.0\.31"/);
  assert.match(yml, /^ {4}pc-t-demo:$/m);
});
