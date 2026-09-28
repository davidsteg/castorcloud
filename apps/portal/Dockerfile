# Customer portal (Next.js). Build from the repo root:
#   docker build -f apps/portal/Dockerfile --build-arg BASE_DOMAIN=castorcloud.ch -t pc-portal .
# Configuration comes from the environment at run time (see .env.example);
# only BASE_DOMAIN is baked into the client bundle.
FROM node:24-alpine@sha256:ebfe2f90462722a7a4de65e91990e97fe0d401c70e0e762c5b53302f905ec1c1 AS build
WORKDIR /app
COPY package.json package-lock.json turbo.json tsconfig.base.json ./
COPY apps/portal/package.json apps/portal/
COPY packages/brand/package.json packages/brand/
COPY packages/core/package.json packages/core/
COPY packages/db/package.json packages/db/
COPY packages/pricing/package.json packages/pricing/
COPY packages/db/prisma packages/db/prisma
# Install scripts stay off; only the Prisma client is generated explicitly.
RUN npm ci --ignore-scripts -w @pc/portal -w @pc/db --include-workspace-root \
 && npx -w @pc/db prisma generate
COPY packages packages
COPY apps/portal apps/portal
ARG BASE_DOMAIN=castorcloud.ch
ENV BASE_DOMAIN=$BASE_DOMAIN NEXT_TELEMETRY_DISABLED=1 PC_STANDALONE=1
RUN npm run build -w @pc/portal

FROM node:24-alpine@sha256:ebfe2f90462722a7a4de65e91990e97fe0d401c70e0e762c5b53302f905ec1c1
# The runtime never installs packages: drop npm and corepack (and their CVEs).
RUN rm -rf /usr/local/lib/node_modules/npm /usr/local/lib/node_modules/corepack \
    /usr/local/bin/npm /usr/local/bin/npx /usr/local/bin/corepack
ENV NODE_ENV=production NEXT_TELEMETRY_DISABLED=1 HOSTNAME=0.0.0.0 PORT=3000
WORKDIR /app
COPY --from=build --chown=node:node /app/apps/portal/.next/standalone ./
COPY --from=build --chown=node:node /app/apps/portal/.next/static apps/portal/.next/static
COPY --from=build --chown=node:node /app/apps/portal/public apps/portal/public
USER node
EXPOSE 3000
HEALTHCHECK --interval=30s --timeout=3s CMD ["wget", "-qO", "/dev/null", "http://127.0.0.1:3000/icon.svg"]
CMD ["node", "apps/portal/server.js"]
