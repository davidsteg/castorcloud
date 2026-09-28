# Read-only MCP tools for the tenant assistant. Build from the repo root:
#   docker build -f agent/tools/Dockerfile -t pc-assistant-tools:1 .
FROM node:24-alpine@sha256:ebfe2f90462722a7a4de65e91990e97fe0d401c70e0e762c5b53302f905ec1c1
WORKDIR /app
COPY package.json package-lock.json ./
COPY agent/tools/package.json agent/tools/
RUN npm ci --omit=dev --ignore-scripts -w @pc/assistant-tools && npm cache clean --force \
 # The runtime never installs packages: drop npm and corepack (and their CVEs).
 && rm -rf /usr/local/lib/node_modules/npm /usr/local/lib/node_modules/corepack \
    /usr/local/bin/npm /usr/local/bin/npx /usr/local/bin/corepack
COPY agent/tools/src agent/tools/src
USER node
WORKDIR /app/agent/tools
EXPOSE 8080
HEALTHCHECK --interval=30s --timeout=3s CMD ["wget", "-qO", "/dev/null", "http://127.0.0.1:8080/healthz"]
CMD ["node", "--experimental-strip-types", "--disable-warning=ExperimentalWarning", "src/server.ts"]
