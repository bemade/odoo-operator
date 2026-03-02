"""Liveness probe for Odoo cron pods.

Detects a stuck cron system by querying PostgreSQL directly.  The probe
fails when ALL of the following are true:

  1. Active cron jobs are overdue (nextcall >10 min in the past).
  2. No cron job has completed recently (lastcall within 10 min).
  3. The Odoo DB user has no active queries (rules out long-running jobs).

This catches the common failure mode where a transient DB disconnect
kills the cron thread but the main Odoo process stays alive.
"""
import sys

try:
    import psycopg2

    conf = {}
    for line in open("/etc/odoo/odoo.conf"):
        line = line.strip()
        if "=" in line and line[0] not in "[#;":
            k, v = line.split("=", 1)
            conf[k.strip()] = v.strip()

    cn = psycopg2.connect(
        host=conf["db_host"],
        port=int(conf["db_port"]),
        dbname=conf["db_name"],
        user=conf["db_user"],
        password=conf["db_password"],
        connect_timeout=3,
    )
    cr = cn.cursor()
    cr.execute(
        "SELECT"
        " (SELECT count(*) FROM ir_cron"
        "  WHERE active"
        "  AND nextcall < (now() AT TIME ZONE 'UTC') - interval '10 min'),"
        " (SELECT count(*) FROM ir_cron"
        "  WHERE active"
        "  AND lastcall > (now() AT TIME ZONE 'UTC') - interval '10 min'),"
        " (SELECT count(*) FROM pg_stat_activity"
        "  WHERE datname = current_database()"
        "  AND usename = current_user"
        "  AND state = 'active'"
        "  AND pid != pg_backend_pid())"
    )
    overdue, recent, active = cr.fetchone()
    cn.close()

    if overdue > 0 and recent == 0 and active == 0:
        sys.exit(1)
except Exception:
    sys.exit(1)
