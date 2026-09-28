// Ending a person's access to a tenant's apps: browser sessions, phone app
// sessions and API keys. None of the apps has an admin API for this, so each
// command works on the app's own database or runtime inside the tenant stack
// (verified against the versions in packages/stack-templates/versions.env).
//
// Scopes (RevocationScope in the control-plane DB):
//   WEB      browser sessions; phone apps stay connected (portal sign-out)
//   DEVICES  every session and API key the person made ("sign out everywhere")
//   REMOVED  DEVICES plus the assistant's key (the member left the tenant)
import type { ServiceKey } from "./tenant.ts";

export type Scope = "WEB" | "DEVICES" | "REMOVED";

/** A command run with `docker compose exec -T` in one of the tenant's services. */
export type AccessCommand = {
  app: ServiceKey;
  service: string;
  command: string[];
  env?: Record<string, string>;
  input?: string;
};

/** Name of the keys the assistant's tools issue (agent/tools/src/db.ts KEY_NAME). */
export const ASSISTANT_KEY_NAME = "Castor assistant (read-only)";

type Who = { email: string; sub: string; before: Date };

// Values reach SQL only as psql variables (:'name' quotes them).
const psql = (user: string, db: string, vars: Record<string, string>) => [
  "psql", "-X", "-q", "-v", "ON_ERROR_STOP=1", "-U", user, "-d", db,
  ...Object.entries(vars).flatMap(([k, v]) => ["-v", `${k}=${v}`]),
];

const keepKey = (scope: Scope) => (scope === "REMOVED" ? "" : ASSISTANT_KEY_NAME);

/**
 * Immich: sessions carry appVersion only when the phone app created them
 * (getUserAgentDetails), so WEB keeps the app signed in.
 */
function photos(who: Who, scope: Scope): AccessCommand {
  return {
    app: "photos",
    service: "immich-db",
    command: psql("immich", "immich", {
      email: who.email, before: who.before.toISOString(), web: String(scope === "WEB"), keep: keepKey(scope),
    }),
    input: `
DELETE FROM session s USING "user" u
 WHERE s."userId" = u.id AND lower(u.email) = lower(:'email') AND s."createdAt" < :'before'::timestamptz
   AND (NOT :'web'::boolean OR s."appVersion" IS NULL);
DELETE FROM api_key k USING "user" u
 WHERE k."userId" = u.id AND lower(u.email) = lower(:'email')
   AND NOT :'web'::boolean AND k.name <> :'keep';
`,
  };
}

/**
 * Paperless: Django rejects every session whose auth hash no longer matches
 * the password, and SSO users have no usable one, so a fresh unusable
 * password ends all their sessions. Tokens (phone apps) survive WEB.
 */
function docs(who: Who, scope: Scope): AccessCommand {
  return {
    app: "docs",
    service: "paperless",
    command: ["python3", "manage.py", "shell", "--no-imports"],
    env: { PC_EMAIL: who.email, PC_SCOPE: scope },
    input: `
import os
from django.contrib.auth.models import User
from rest_framework.authtoken.models import Token
for user in User.objects.filter(email__iexact=os.environ["PC_EMAIL"]).exclude(email=""):
    user.set_unusable_password()
    user.save(update_fields=["password"])
    if os.environ["PC_SCOPE"] != "WEB":
        Token.objects.filter(user=user).delete()
`,
  };
}

/**
 * Dawarich: Devise checks each session and remember-me cookie against the
 * password hash, so a new random one ends them. The phone app uses the API
 * key, which DEVICES replaces (the assistant reads the current key itself).
 */
function places(who: Who, scope: Scope): AccessCommand {
  return {
    app: "places",
    service: "dawarich",
    command: ["bin/rails", "runner", `
user = User.where("lower(email) = ?", ENV.fetch("PC_EMAIL").downcase).first
if user
  user.update_columns(encrypted_password: Devise::Encryptor.digest(User, SecureRandom.hex(32)))
  user.update_columns(api_key: SecureRandom.hex(32)) unless ENV.fetch("PC_SCOPE") == "WEB"
end
`],
    env: { PC_EMAIL: who.email, PC_SCOPE: scope },
  };
}

/**
 * AirTrail: web only (no phone app), linked by the OIDC subject. DEVICES also
 * drops API keys and grants to other apps (their tokens cascade).
 */
function flights(who: Who, scope: Scope): AccessCommand {
  return {
    app: "flights",
    service: "db",
    command: psql("postgres", "airtrail", { sub: who.sub, web: String(scope === "WEB"), keep: keepKey(scope) }),
    input: `
DELETE FROM session s USING "user" u WHERE s.user_id = u.id AND u.oauth_id = :'sub';
DELETE FROM api_key k USING "user" u
 WHERE k.user_id = u.id AND u.oauth_id = :'sub' AND NOT :'web'::boolean AND k.name <> :'keep';
DELETE FROM oauth_grant g USING "user" u
 WHERE g.user_id = u.id AND u.oauth_id = :'sub' AND NOT :'web'::boolean;
`,
  };
}

const BUILDERS: Record<ServiceKey, (who: Who, scope: Scope) => AccessCommand> = { photos, docs, places, flights };

/** Commands that end `who`'s access in the given apps. */
export function accessCommands(apps: ServiceKey[], who: Who, scope: Scope): AccessCommand[] {
  return apps.map((app) => BUILDERS[app](who, scope));
}

/** Strongest scope first: one run per person covers the weaker requests. */
export function strongest(scopes: Scope[]): Scope {
  return scopes.includes("REMOVED") ? "REMOVED" : scopes.includes("DEVICES") ? "DEVICES" : "WEB";
}

/** `docker compose` arguments that run the command in its service. */
export function execArgs(cmd: AccessCommand): string[] {
  return [
    "exec", "-T",
    ...Object.entries(cmd.env ?? {}).flatMap(([k, v]) => ["-e", `${k}=${v}`]),
    cmd.service, ...cmd.command,
  ];
}
