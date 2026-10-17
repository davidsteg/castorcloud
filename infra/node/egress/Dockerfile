# Egress proxy of a custom stack (docs/stacks.md). Built on each STACKS node
# by castord (rust/crates/castord/src/stacks/edge.rs) as castor.local/egress:2.
# A static binary on an empty image: no shell, no runtime, nothing else to use.
FROM rust:1.98-alpine@sha256:7cc1c22d77d9432f7fe012a70e6d3e555af54c2a6832700ed7d553f1769ae89f AS build
WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY src src
RUN cargo build --release --locked && cp target/release/castor-egress /castor-egress

FROM scratch
COPY --from=build /castor-egress /castor-egress
USER 65534:65534
ENTRYPOINT ["/castor-egress"]
