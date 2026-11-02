-- Deterministic neutralize-failure injection.  Run ONLY through
-- ../inject-neutralize-failure.sh, which connects to the demo's SOURCE
-- database (demo/prod) and nothing else.
--
-- Simulates a custom module whose mail server survives `odoo neutralize`: a
-- BEFORE UPDATE trigger keeps the seeded smtp.example.com row active.
-- pg_dump carries the function and trigger into the refresh's temporary DB,
-- so the refresh's "no active outgoing mail servers" check after neutralize
-- fails every time.  The source instance itself is unaffected unless someone
-- archives that row.  Undo with neutralize-failure-undo.sql.

-- Belt and braces: abort unless connected to the database the wrapper
-- resolved (passed as -v target_db=...).
SET smoke.expected_db TO :'target_db';
DO $$
BEGIN
  IF current_database() <> current_setting('smoke.expected_db') THEN
    RAISE EXCEPTION 'connected to %, expected %', current_database(),
      current_setting('smoke.expected_db');
  END IF;
  IF to_regclass('public.ir_mail_server') IS NULL THEN
    RAISE EXCEPTION '% is not an Odoo database (no ir_mail_server)', current_database();
  END IF;
END $$;

CREATE OR REPLACE FUNCTION smoke_keep_mail_active() RETURNS trigger
LANGUAGE plpgsql AS $$
BEGIN
  IF NEW.smtp_host = 'smtp.example.com' THEN NEW.active := true; END IF;
  RETURN NEW;
END $$;
DROP TRIGGER IF EXISTS smoke_keep_mail_active ON ir_mail_server;
CREATE TRIGGER smoke_keep_mail_active BEFORE UPDATE ON ir_mail_server
  FOR EACH ROW EXECUTE FUNCTION smoke_keep_mail_active();
