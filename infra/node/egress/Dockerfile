# Egress proxy of a custom stack (docs/stacks.md). Built on each STACKS node
# by castord (rust/crates/castord/src/stacks/edge.rs) as castor.local/egress:2.
# A static binary on an empty image: no shell, no runtime, nothing else to use.
FROM rust:1.99-alpine@sha256:b1d5dd5a22a09404d3e798c0cf051b689077360374eb505fa5bb93c479693d87 AS build
WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY src src
RUN cargo build --release --locked && cp target/release/castor-egress /castor-egress

FROM scratch
COPY --from=build /castor-egress /castor-egress
USER 65534:65534
ENTRYPOINT ["/castor-egress"]
