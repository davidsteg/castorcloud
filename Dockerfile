# One image for Castor's own Node services. Each runs as its own container
# from the same image, with only the secrets and mounts that service needs:
#   portal     customer portal (Next.js, port 3000), the default
#   worker     provisioning worker
#   monitor    platform checks and alerts
#   telegram   Castor Telegram bot
#   node       node admin CLI        backup   backup admin CLI
#   migrate    apply database migrations (prisma migrate deploy)
# Build from the repo root:
#   docker build --build-arg BASE_DOMAIN=castorcloud.ch -t castor .
# Only BASE_DOMAIN is baked in (into the portal's client bundle); everything
# else comes from the environment at run time (see .env.example).
FROM node:24-alpine@sha256:ebfe2f90462722a7a4de65e91990e97fe0d401c70e0e762c5b53302f905ec1c1 AS build
WORKDIR /app
COPY package.json package-lock.json turbo.json tsconfig.base.json ./
COPY apps/portal/package.json apps/portal/
COPY apps/provisioner/package.json apps/provisioner/
COPY packages/brand/package.json packages/brand/
COPY packages/core/package.json packages/core/
COPY packages/db/package.json packages/db/
COPY packages/pricing/package.json packages/pricing/
COPY packages/db/prisma packages/db/prisma
# Install scripts stay off; only the Prisma client is generated explicitly.
RUN npm ci --ignore-scripts -w @pc/portal -w @pc/provisioner -w @pc/db \
 && npx -w @pc/db prisma generate \
 && npm cache clean --force
COPY packages packages
COPY apps/portal apps/portal
ARG BASE_DOMAIN=castorcloud.ch
ENV BASE_DOMAIN=$BASE_DOMAIN NEXT_TELEMETRY_DISABLED=1
# Alpine uses musl: drop the glibc builds of native modules.
RUN npm run build -w @pc/portal && rm -rf apps/portal/.next/cache \
    node_modules/@next/swc-*-gnu node_modules/@img/sharp-linux-* node_modules/@img/sharp-libvips-linux-*
COPY apps/provisioner apps/provisioner
# The worker builds the assistant tools image on nodes from this source.
COPY agent agent
COPY infra/control-plane/traefik infra/control-plane/traefik

FROM node:24-alpine@sha256:ebfe2f90462722a7a4de65e91990e97fe0d401c70e0e762c5b53302f905ec1c1
# The worker drives nodes with the docker CLI over SSH. The runtime never
# installs packages, so npm and corepack go (and their CVEs with them).
RUN apk add --no-cache docker-cli docker-cli-compose openssh-client tar \
 && rm -rf /usr/local/lib/node_modules/npm /usr/local/lib/node_modules/corepack \
    /usr/local/bin/npm /usr/local/bin/npx /usr/local/bin/corepack
ENV NODE_ENV=production NEXT_TELEMETRY_DISABLED=1
WORKDIR /app
# Workspaces keep their own node_modules where versions differ, so take all of /app.
COPY --from=build /app /app
COPY docker-entrypoint.sh /usr/local/bin/castor
# The worker writes tenant routes here; mount it to share it with Traefik.
RUN chown node:node infra/control-plane/traefik/dynamic
USER node
EXPOSE 3000
HEALTHCHECK --interval=30s --timeout=3s CMD ["castor", "healthcheck"]
ENTRYPOINT ["castor"]
CMD ["portal"]
