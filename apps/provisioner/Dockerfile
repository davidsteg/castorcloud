# Provisioning worker and its companions (monitor, Telegram bot, node admin).
# Build from the repo root:
#   docker build -f apps/provisioner/Dockerfile -t pc-provisioner .
# Default command is the worker; run the others with e.g. `node --import tsx src/monitor.ts`.
# The worker drives nodes with the docker CLI over SSH, so mount an SSH key
# and known_hosts at /home/node/.ssh (read-only), and mount the Traefik
# dynamic directory at /app/infra/control-plane/traefik/dynamic.
FROM node:24-alpine@sha256:ebfe2f90462722a7a4de65e91990e97fe0d401c70e0e762c5b53302f905ec1c1 AS deps
WORKDIR /app
COPY package.json package-lock.json ./
COPY apps/provisioner/package.json apps/provisioner/
COPY packages/core/package.json packages/core/
COPY packages/db/package.json packages/db/
COPY packages/pricing/package.json packages/pricing/
COPY packages/db/prisma packages/db/prisma
# Install scripts stay off; only the Prisma client is generated explicitly.
RUN npm ci --ignore-scripts -w @pc/provisioner -w @pc/db \
 && npx -w @pc/db prisma generate \
 && npm cache clean --force

FROM node:24-alpine@sha256:ebfe2f90462722a7a4de65e91990e97fe0d401c70e0e762c5b53302f905ec1c1
# The runtime never installs packages: drop npm and corepack (and their CVEs).
RUN apk add --no-cache docker-cli docker-cli-compose openssh-client tar \
 && rm -rf /usr/local/lib/node_modules/npm /usr/local/lib/node_modules/corepack \
    /usr/local/bin/npm /usr/local/bin/npx /usr/local/bin/corepack
ENV NODE_ENV=production
WORKDIR /app
COPY --from=deps /app/node_modules node_modules
COPY package.json package-lock.json ./
COPY packages packages
COPY apps/provisioner apps/provisioner
# The worker builds the assistant tools image on nodes from this source.
COPY agent agent
COPY infra/control-plane/traefik infra/control-plane/traefik
RUN chown node:node infra/control-plane/traefik/dynamic
USER node
WORKDIR /app/apps/provisioner
CMD ["node", "--import", "tsx", "src/worker.ts"]
