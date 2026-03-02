"""Startup probe for Odoo cron pods.

Verifies the pod can reach PostgreSQL and that the ir_cron table exists
(meaning Odoo modules have finished loading).  Runs as an exec probe
via ``python3 -c "$(cat this_file)"``.
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
    cr.execute("SELECT 1 FROM ir_cron LIMIT 1")
    cn.close()
except Exception:
    sys.exit(1)
