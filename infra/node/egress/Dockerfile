# Egress proxy of a custom stack (docs/stacks.md). Built on each STACKS node
# by castord (rust/crates/castord/src/stacks/edge.rs) as castor.local/egress:2.
# A static binary on an empty image: no shell, no runtime, nothing else to use.
FROM rust:1.91-alpine@sha256:45c1c35cd364b8055e9e86f8ecd3e8c874b2dcb658d8a4f94b5d111aa0d651a2 AS build
WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY src src
RUN cargo build --release --locked && cp target/release/castor-egress /castor-egress

FROM scratch
COPY --from=build /castor-egress /castor-egress
USER 65534:65534
ENTRYPOINT ["/castor-egress"]
