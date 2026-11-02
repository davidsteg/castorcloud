-- Least-privilege database roles (docs/security.md, "Database roles").
--
--   castor_app     the portal, castord, castor-monitor, castor-telegram and
--                  castor-admin. Reads and writes rows; owns nothing, so it
--                  can't change the schema or drop AuditLog's triggers, and
--                  it may only add to AuditLog.
--   castor_backup  pg_dumpall for the nightly sealed dump: reads everything
--                  (pg_read_all_data), writes nothing.
--
-- Both are created without a login. The operator gives each one a password
-- once (docs/deploy-roles.md); until then everything keeps working with the
-- owner role it uses today. Migrations keep running as the owner.
DO $$
BEGIN
  IF NOT EXISTS (SELECT 1 FROM pg_roles WHERE rolname = 'castor_app') THEN
    CREATE ROLE castor_app NOLOGIN NOSUPERUSER NOCREATEDB NOCREATEROLE NOREPLICATION NOBYPASSRLS;
  END IF;
  IF NOT EXISTS (SELECT 1 FROM pg_roles WHERE rolname = 'castor_backup') THEN
    CREATE ROLE castor_backup NOLOGIN NOSUPERUSER NOCREATEDB NOCREATEROLE NOREPLICATION NOBYPASSRLS;
  END IF;
  EXECUTE format('GRANT CONNECT ON DATABASE %I TO castor_app, castor_backup', current_database());
  EXECUTE format('GRANT USAGE ON SCHEMA %I TO castor_app', current_schema());
  EXECUTE format('GRANT SELECT, INSERT, UPDATE, DELETE ON ALL TABLES IN SCHEMA %I TO castor_app', current_schema());
  EXECUTE format('GRANT USAGE, SELECT ON ALL SEQUENCES IN SCHEMA %I TO castor_app', current_schema());
  -- Tables and sequences later migrations create (as this same owner).
  EXECUTE format('ALTER DEFAULT PRIVILEGES IN SCHEMA %I GRANT SELECT, INSERT, UPDATE, DELETE ON TABLES TO castor_app', current_schema());
  EXECUTE format('ALTER DEFAULT PRIVILEGES IN SCHEMA %I GRANT USAGE, SELECT ON SEQUENCES TO castor_app', current_schema());
END
$$;

GRANT pg_read_all_data TO castor_backup;

-- The migration history is the owner's (absent where the SQL is applied
-- without Prisma, as in CI).
DO $$
BEGIN
  IF to_regclass('_prisma_migrations') IS NOT NULL THEN
    REVOKE ALL ON TABLE "_prisma_migrations" FROM castor_app;
  END IF;
END
$$;

-- AuditLog: read and add, nothing else (its triggers refuse the rest anyway).
REVOKE UPDATE, DELETE, TRUNCATE ON TABLE "AuditLog" FROM castor_app;
