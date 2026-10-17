# Read-only MCP tools for the tenant assistant (docs/assistant.md). Build from
# the repo root:
#   docker build -f agent/tools/Dockerfile -t pc-assistant-tools:2 .
# A static binary on an empty image: no shell, no runtime, nothing else to use.
FROM rust:1.91-alpine@sha256:45c1c35cd364b8055e9e86f8ecd3e8c874b2dcb658d8a4f94b5d111aa0d651a2 AS build
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
