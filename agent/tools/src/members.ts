// Who is asking. Every household member has an own Hermes profile whose MCP
// config carries that member's bearer token; the token decides whose photos,
// documents, places and flights the tools may read. The file is rendered by
// the provisioner and holds only token hashes.
import { createHash, timingSafeEqual } from "node:crypto";
import { readFileSync } from "node:fs";

export type Member = {
  /** Member id in the control plane (also the Hermes profile name). */
  id: string;
  email: string;
  /** Authentik "sub": how OIDC-only services (AirTrail) know the user. */
  sub: string;
  name?: string;
  tokenSha256: string;
};

export const sha256Hex = (value: string) => createHash("sha256").update(value).digest("hex");

export function loadMembers(file: string): Member[] {
  const parsed = JSON.parse(readFileSync(file, "utf8")) as { members?: Member[] };
  return parsed.members ?? [];
}

export function memberForToken(members: Member[], authorization: string | undefined): Member | undefined {
  const token = authorization?.match(/^Bearer\s+(\S+)$/i)?.[1];
  if (!token) return undefined;
  const hash = Buffer.from(sha256Hex(token), "hex");
  return members.find((m) => {
    const expected = Buffer.from(m.tokenSha256, "hex");
    return expected.length === hash.length && timingSafeEqual(expected, hash);
  });
}
