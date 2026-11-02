-- AuditLog is append-only (docs/security.md, "Access by staff"): rows are
-- added, never changed or removed, by the portal, castord or anything else
-- that connects with the application's role.
--
-- A superuser can still drop these triggers or skip them with
-- session_replication_role = replica (the tests clean up that way). Those
-- are DDL or superuser steps, not something an application bug or a SQL
-- injection through Prisma reaches; the nightly sealed dump keeps an
-- off-host copy (docs/backups.md).
CREATE FUNCTION "audit_log_append_only"() RETURNS trigger
LANGUAGE plpgsql AS $$
BEGIN
  RAISE EXCEPTION 'AuditLog is append-only: % refused', TG_OP
    USING ERRCODE = 'insufficient_privilege';
END;
$$;

CREATE TRIGGER "AuditLog_append_only"
  BEFORE UPDATE OR DELETE ON "AuditLog"
  FOR EACH ROW EXECUTE FUNCTION "audit_log_append_only"();

CREATE TRIGGER "AuditLog_no_truncate"
  BEFORE TRUNCATE ON "AuditLog"
  FOR EACH STATEMENT EXECUTE FUNCTION "audit_log_append_only"();
