#!/bin/sh
# One PostGIS instance per tenant hosts the Paperless, Dawarich and AirTrail
# databases, each with its own role. (Immich needs its own VectorChord image.)
set -e
create() {
  psql -v ON_ERROR_STOP=1 -U "$POSTGRES_USER" -d postgres <<SQL
CREATE ROLE $1 LOGIN PASSWORD '$2';
CREATE DATABASE $1 OWNER $1;
SQL
}
create paperless "$PAPERLESS_DB_PASSWORD"
create dawarich "$DAWARICH_DB_PASSWORD"
create airtrail "$AIRTRAIL_DB_PASSWORD"
psql -v ON_ERROR_STOP=1 -U "$POSTGRES_USER" -d dawarich -c "CREATE EXTENSION IF NOT EXISTS postgis"
