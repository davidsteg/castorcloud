# Read-only MCP tools for the tenant assistant. Build from the repo root:
#   docker build -f agent/tools/Dockerfile -t pc-assistant-tools:1 .
FROM node:24-alpine
WORKDIR /app
COPY package.json package-lock.json ./
COPY agent/tools/package.json agent/tools/
RUN npm ci --omit=dev --ignore-scripts -w @pc/assistant-tools && npm cache clean --force
COPY agent/tools/src agent/tools/src
USER node
WORKDIR /app/agent/tools
EXPOSE 8080
HEALTHCHECK --interval=30s --timeout=3s CMD wget -qO- http://127.0.0.1:8080/healthz || exit 1
CMD ["node", "--experimental-strip-types", "--disable-warning=ExperimentalWarning", "src/server.ts"]
