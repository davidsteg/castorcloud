import assert from "node:assert/strict";
import { test } from "node:test";
import { imageRepo, imageSource, installImage } from "./images.ts";

test("without CASTOR_IMAGE_TAG images are built locally", () => {
  assert.deepEqual(imageSource({}), { kind: "build" });
  const calls: string[][] = [];
  installImage({ docker: (a) => (calls.push(a), "") }, "cryptd", "pc-cryptd:1", ["ctx"], { kind: "build" });
  assert.deepEqual(calls, [["build", "-q", "-t", "pc-cryptd:1", "ctx"]]);
});

test("a registry tag needs a cosign key and sane names", () => {
  assert.throws(() => imageSource({ CASTOR_IMAGE_TAG: "abc" }), /CASTOR_COSIGN_PUBKEY/);
  assert.throws(() => imageSource({ CASTOR_IMAGE_TAG: "a b", CASTOR_COSIGN_PUBKEY: "k" }), /tag/);
  assert.throws(() => imageSource({ CASTOR_IMAGE_TAG: "abc", CASTOR_COSIGN_PUBKEY: "k", CASTOR_IMAGE_REPO: "ghcr.io/x;rm" }), /repository/);
  assert.deepEqual(imageSource({ CASTOR_IMAGE_TAG: "abc", CASTOR_COSIGN_PUBKEY: "/k.pub", GHCR_PULL_TOKEN: "t" }), {
    kind: "registry", repo: "ghcr.io/davidsteg/castorcloud", tag: "abc", pubkey: "/k.pub", token: "t", user: "castor",
  });
  assert.equal(imageRepo("ghcr.io/davidsteg/castorcloud", "assistant-tools"), "ghcr.io/davidsteg/castorcloud-assistant-tools");
});
