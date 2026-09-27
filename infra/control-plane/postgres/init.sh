#!/bin/sh
# Creates the databases used by the control plane on first start.
set -e
for db in authentik portal; do
  psql -v ON_ERROR_STOP=1 --username "$POSTGRES_USER" -d postgres \
    -tc "SELECT 1 FROM pg_database WHERE datname = '$db'" | grep -q 1 \
    || psql -v ON_ERROR_STOP=1 --username "$POSTGRES_USER" -d postgres -c "CREATE DATABASE $db"
done
