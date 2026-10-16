// Apps we've reviewed and offer ready to install. Each is a stack definition
// in the same compose subset customers write (spec.ts), with images pinned by
// digest. Bump deliberately: read the release notes, re-pin the digest
// (the index digest of the multi-arch image), and check it still runs under
// the stack hardening (render.ts).
export type CatalogApp = {
  key: string;
  label: string;
  /** One line, English; the portal translates the common ones. */
  description: string;
  homepage: string;
  /** Default stack name when installed. */
  stack: string;
  source: string;
};

export const CATALOG: CatalogApp[] = [
  {
    key: "it-tools",
    label: "IT Tools",
    description: "Handy tools for developers: encoders, converters, generators.",
    homepage: "https://github.com/CorentinTh/it-tools",
    stack: "tools",
    source: `
services:
  app:
    image: corentinth/it-tools:2024.10.22-7ca5933@sha256:8b8128748339583ca951af03dfe02a9a4d7363f61a216226fc28030731a5a61f
    mem_limit: 128m
    cpus: 0.25
x-castor:
  web: app:80
`,
  },
  {
    key: "linkding",
    label: "linkding",
    description: "Bookmark manager. Signs you in with your Castor Cloud login.",
    homepage: "https://linkding.link",
    stack: "links",
    source: `
services:
  app:
    image: sissbruecker/linkding:1.47.0@sha256:e35cb50e0581178f245125ffaa909c565416c94c9f22ace305a7d234a4345522
    environment:
      # The sign-in happens in front of the app (Authentik); linkding trusts its header.
      LD_ENABLE_AUTH_PROXY: "True"
      LD_AUTH_PROXY_USERNAME_HEADER: HTTP_X_AUTHENTIK_USERNAME
      LD_CSRF_TRUSTED_ORIGINS: \${castor:origin}
    volumes:
      - data:/etc/linkding/data
    mem_limit: 256m
    cpus: 0.5
volumes:
  data:
x-castor:
  web: app:9090
  storage_gb: 10
`,
  },
  {
    key: "actual",
    label: "Actual Budget",
    description: "Personal budgeting with envelope budgets.",
    homepage: "https://actualbudget.org",
    stack: "budget",
    source: `
services:
  app:
    image: actualbudget/actual-server:26.10.0-alpine@sha256:67b1b5e9584e3e143b4f5c41e52b4506ed9cdd1072392d5ab5369f2d4fd0bffd
    volumes:
      - data:/data
    mem_limit: 256m
    cpus: 0.5
volumes:
  data:
x-castor:
  web: app:5006
  storage_gb: 10
`,
  },
];

export function catalogApp(key: string): CatalogApp | undefined {
  return CATALOG.find((a) => a.key === key);
}
