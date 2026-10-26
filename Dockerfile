# One image for the control plane's own services. Each runs as its own
# container from the same image, with only the secrets and mounts that
# service needs:
#   portal     customer portal (Next.js, port 3000), the default
#   castord    the worker: tenant jobs and recurring tasks (rust/README.md)
#   monitor    platform checks and alerts (castor-monitor)
#   telegram   Castor Telegram bot (castor-telegram)
#   admin      operator commands (castor-admin)
#   migrate    apply database migrations (prisma migrate deploy)
#   node       the data plane on a TrueNAS (castor-node-role): the one holder of
#              the gatekeeper token, no database and no tenant keys
#              (docs/gatekeeper.md)
# Build from the repo root:
#   docker build --build-arg BASE_DOMAIN=castorcloud.ch -t castor .
# Only BASE_DOMAIN is baked in (into the portal's client bundle); everything
# else comes from the environment at run time (see .env.example).

FROM rust:1.99-alpine@sha256:b1d5dd5a22a09404d3e798c0cf051b689077360374eb505fa5bb93c479693d87 AS rust
WORKDIR /src
COPY rust/Cargo.toml rust/Cargo.lock ./
COPY rust/crates crates
RUN cargo build --release --locked -p castord --bins \
 && mkdir /out \
 && cp target/release/castord target/release/castor-admin target/release/castor-monitor target/release/castor-telegram \
    target/release/castor-node-role /out/

FROM node:24-alpine@sha256:06d7adf3e0ef61a13f1d28682726366f97a15aa1888c934d3ecec86c33918b53 AS portal
WORKDIR /app
COPY package.json package-lock.json turbo.json tsconfig.base.json ./
COPY apps/portal/package.json apps/portal/
COPY packages/brand/package.json packages/brand/
COPY packages/core/package.json packages/core/
COPY packages/db/package.json packages/db/
COPY packages/pricing/package.json packages/pricing/
COPY packages/stacks/package.json packages/stacks/
COPY packages/db/prisma packages/db/prisma
# Install scripts stay off; only the Prisma client is generated explicitly.
RUN npm ci --ignore-scripts -w @pc/portal -w @pc/db \
 && npx -w @pc/db prisma generate \
 && npm cache clean --force
COPY packages packages
COPY apps/portal apps/portal
ARG BASE_DOMAIN=castorcloud.ch
ENV BASE_DOMAIN=$BASE_DOMAIN NEXT_TELEMETRY_DISABLED=1
# Alpine uses musl: drop the glibc builds of native modules.
RUN npm run build -w @pc/portal && rm -rf apps/portal/.next/cache \
    node_modules/@next/swc-*-gnu node_modules/@img/sharp-linux-* node_modules/@img/sharp-libvips-linux-*

FROM node:24-alpine@sha256:06d7adf3e0ef61a13f1d28682726366f97a15aa1888c934d3ecec86c33918b53
# castord drives nodes with the docker CLI and OpenSSH, and dumps the
# control plane with pg_dumpall (the server's major version). The runtime never
# installs packages, so npm, corepack and yarn go (and their CVEs with them).
RUN apk add --no-cache docker-cli docker-cli-compose openssh-client postgresql17-client tar \
 && rm -rf /usr/local/lib/node_modules/npm /usr/local/lib/node_modules/corepack \
    /usr/local/bin/npm /usr/local/bin/npx /usr/local/bin/corepack \
    /opt/yarn-* /usr/local/bin/yarn /usr/local/bin/yarnpkg
ENV NODE_ENV=production NEXT_TELEMETRY_DISABLED=1 CASTOR_ROOT=/app
WORKDIR /app
# Workspaces keep their own node_modules where versions differ, so take all of /app.
COPY --from=portal /app /app
COPY --from=rust /out/ /usr/local/bin/
# What castord sends to nodes: stack templates, brand assets, node files.
# With CASTOR_IMAGE_TAG unset nodes build their images from infra/node and
# agent/tools; in production they pull them signed (docs/ci.md).
COPY agent/tools agent/tools
COPY infra/node infra/node
COPY infra/control-plane/traefik infra/control-plane/traefik
COPY docker-entrypoint.sh /usr/local/bin/castor
# castord writes tenant routes and rendered tenant files here; mount them
# to keep them and to share the routes with Traefik.
RUN mkdir .tenants && chown node:node .tenants infra/control-plane/traefik/dynamic
USER node
EXPOSE 3000
HEALTHCHECK --interval=30s --timeout=3s CMD ["castor", "healthcheck"]
ENTRYPOINT ["castor"]
CMD ["portal"]
