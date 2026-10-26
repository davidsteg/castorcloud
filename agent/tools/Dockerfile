# Read-only MCP tools for the tenant assistant (docs/assistant.md). Build from
# the repo root:
#   docker build -f agent/tools/Dockerfile -t pc-assistant-tools:2 .
# A static binary on an empty image: no shell, no runtime, nothing else to use.
FROM rust:1.99-alpine@sha256:b1d5dd5a22a09404d3e798c0cf051b689077360374eb505fa5bb93c479693d87 AS build
WORKDIR /src
COPY agent/tools/Cargo.toml agent/tools/Cargo.lock ./
COPY agent/tools/src src
RUN cargo build --release --locked && cp target/release/assistant-tools /assistant-tools

FROM scratch
COPY --from=build /assistant-tools /assistant-tools
USER 65534:65534
EXPOSE 8080
HEALTHCHECK --interval=30s --timeout=3s CMD ["/assistant-tools", "--health"]
ENTRYPOINT ["/assistant-tools"]
