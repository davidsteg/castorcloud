# Egress proxy of a custom stack (docs/stacks.md). Built on each STACKS node
# by castord (rust/crates/castord/src/stacks/) as castor.local/egress:1. No dependencies.
FROM node:24-alpine@sha256:44fce696f38ba8c987c4db89954051000bbd05ffd8d8659ba15ed4a8602e937f
COPY proxy.mjs /proxy/proxy.mjs
USER 65534:65534
ENTRYPOINT ["node", "/proxy/proxy.mjs"]
