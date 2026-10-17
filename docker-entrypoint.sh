#!/bin/sh
# Entry point of the castor image (Dockerfile): picks the service to run.
set -eu
cmd=${1:-portal}
[ $# -gt 0 ] && shift
case "$cmd" in
  portal)   cd /app/apps/portal && exec node /app/node_modules/next/dist/bin/next start --port "${PORT:-3000}" "$@" ;;
  castord)  exec castord "$@" ;;
  monitor)  exec castor-monitor "$@" ;;
  telegram) exec castor-telegram "$@" ;;
  admin)    exec castor-admin "$@" ;;
  migrate)  cd /app/packages/db && exec node /app/node_modules/prisma/build/index.js migrate deploy "$@" ;;
  # Only the portal listens; the other services count as healthy while running.
  healthcheck)
    case "$(tr '\0' ' ' < /proc/1/cmdline)" in
      *next*) exec wget -qO /dev/null "http://127.0.0.1:${PORT:-3000}/icon.svg" ;;
      *) exit 0 ;;
    esac ;;
  *) echo "usage: castor {portal|castord|monitor|telegram|admin|migrate} [args]" >&2; exit 64 ;;
esac
