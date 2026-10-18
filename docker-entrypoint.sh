#!/bin/sh
# Entry point of the castor image (Dockerfile): picks the service to run.
set -eu
cmd=${1:-portal}
[ $# -gt 0 ] && shift
# Each role holds only its own secrets (docs/gatekeeper.md). The gatekeeper
# token belongs to the node role; castor-node itself refuses the control
# plane's secrets.
case "$cmd" in
  node|healthcheck) ;;
  *)
    if [ -n "${GATEKEEPER_TOKEN:-}${GATEKEEPER_TOKEN_FILE:-}${GATEKEEPER_URL:-}" ] || [ -e /run/secrets/gatekeeper_token ]; then
      echo "castor $cmd: the gatekeeper token belongs to the node role (castor node), not here" >&2
      exit 64
    fi ;;
esac
case "$cmd" in
  portal)   cd /app/apps/portal && exec node /app/node_modules/next/dist/bin/next start --port "${PORT:-3000}" "$@" ;;
  castord)  exec castord "$@" ;;
  monitor)  exec castor-monitor "$@" ;;
  telegram) exec castor-telegram "$@" ;;
  admin)    exec castor-admin "$@" ;;
  node)     exec castor-node "$@" ;;
  migrate)  cd /app/packages/db && exec node /app/node_modules/prisma/build/index.js migrate deploy "$@" ;;
  # Only the portal listens; the other services count as healthy while running.
  healthcheck)
    case "$(tr '\0' ' ' < /proc/1/cmdline)" in
      *next*) exec wget -qO /dev/null "http://127.0.0.1:${PORT:-3000}/icon.svg" ;;
      *) exit 0 ;;
    esac ;;
  *) echo "usage: castor {portal|castord|monitor|telegram|admin|migrate|node} [args]" >&2; exit 64 ;;
esac
