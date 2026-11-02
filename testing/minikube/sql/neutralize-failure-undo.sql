-- Remove what neutralize-failure-inject.sql created.  Run ONLY through
-- ../inject-neutralize-failure.sh undo.
SET smoke.expected_db TO :'target_db';
DO $$
BEGIN
  IF current_database() <> current_setting('smoke.expected_db') THEN
    RAISE EXCEPTION 'connected to %, expected %', current_database(),
      current_setting('smoke.expected_db');
  END IF;
END $$;
DROP TRIGGER IF EXISTS smoke_keep_mail_active ON ir_mail_server;
DROP FUNCTION IF EXISTS smoke_keep_mail_active();
