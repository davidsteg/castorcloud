import assert from "node:assert/strict";
import { test } from "node:test";
import { coveredStacks, parseIds, resolveUser, uncoveredVolumes, untarFile } from "./stacks.ts";
import { stacksDatasetKey, datasetKey } from "./zfs.ts";
import { tarFile } from "./stacks-node.ts";

const passwd = "root:x:0:0:root:/root:/bin/sh\nnode:x:1000:1001::/home/node:/bin/sh\nwww-data:x:33:33::/var/www:/usr/sbin/nologin\n";
const group = "root:x:0:\nnode:x:1001:\nwww-data:x:33:\nstaff:x:50:\n";

test("container users resolve to numbers, also by name from the image", () => {
  assert.deepEqual(resolveUser(""), { uid: 0, gid: 0 });
  assert.deepEqual(resolveUser("1000"), { uid: 1000, gid: 1000 });
  assert.deepEqual(resolveUser("1000:50"), { uid: 1000, gid: 50 });
  assert.deepEqual(resolveUser("node", { passwd, group }), { uid: 1000, gid: 1001 });
  assert.deepEqual(resolveUser("www-data:staff", { passwd, group }), { uid: 33, gid: 50 });
  assert.throws(() => resolveUser("nobody", { passwd, group }), /not in the image/);
  assert.throws(() => resolveUser("70000"), /outside the allowed range/);
  assert.equal(parseIds(passwd).get("node"), 1000);
});

test("tar round trip (docker cp)", () => {
  assert.equal(untarFile(tarFile("passwd", passwd)), passwd);
  assert.throws(() => untarFile(Buffer.alloc(10)));
});

test("the booking covers stacks in the order they were added", () => {
  const s = (id: string, memoryUnits: number) => ({ id, units: { memoryUnits, cpuUnits: 1, storageUnits: 0 } });
  const { covered, over } = coveredStacks([s("a", 2), s("b", 4), s("c", 1)], { memoryUnits: 3, cpuUnits: 3, storageUnits: 0 });
  assert.deepEqual(covered.map((x) => x.id), ["a", "c"]);
  assert.deepEqual(over.map((x) => x.id), ["b"]);
});

test("the stacks dataset key differs from the tenant dataset key", () => {
  const volumeKey = Buffer.alloc(64, 7);
  const a = stacksDatasetKey(volumeKey).toString();
  assert.match(a, /^[0-9a-f]{64}\n$/);
  assert.notEqual(a, datasetKey(volumeKey).toString());
});

test("an image VOLUME needs a mount or tmpfs over it", () => {
  assert.deepEqual(uncoveredVolumes(["/data", "/var/lib/x/"], ["/data", "/tmp"]), ["/var/lib/x"]);
  assert.deepEqual(uncoveredVolumes(["/srv/app/data"], ["/srv/app"]), []);
  assert.deepEqual(uncoveredVolumes(["/database"], ["/data"]), ["/database"]);
});
