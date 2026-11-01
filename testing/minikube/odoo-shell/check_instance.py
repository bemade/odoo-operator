# Run inside `odoo shell`: verify filestore vs ir_attachment consistency,
# neutralization and the outgoing mail server configuration.
#
# smoke.sh prepends:
#   ROLE = 'source' | 'target'   # demo/prod or the refreshed demo/staging
#   STAGING_SMTP_HOST = '...'    # host the operator injects on staging
#
# Output, one per line, parsed by smoke.sh:
#   CHECK PASS|FAIL <name> <detail>
#   INFO <key> <value...>
#   CHECK_DONE                   # absent if the script died part-way
import hashlib
import os


def check(name, ok, detail=""):
    print("CHECK", "PASS" if ok else "FAIL", name, detail, flush=True)


def info(*args):
    print("INFO", *args, flush=True)


A = env['ir.attachment'].sudo().with_context(active_test=False)
info("db", env.cr.dbname)

# Every stored attachment row, including res_field (binary-field) ones that
# ir.attachment.search() hides, must have its file.
env.cr.execute("SELECT store_fname FROM ir_attachment WHERE store_fname IS NOT NULL")
rows = [r[0] for r in env.cr.fetchall()]
missing_all = [f for f in rows if not os.path.exists(A._full_path(f))]
check("filestore_complete", bool(rows) and not missing_all,
      "rows=%d missing=%d sample=%s" % (len(rows), len(missing_all), missing_all[:5]))

# Files of ORM-visible attachments must match their checksum.
bad = []
for a in A.search([('store_fname', '!=', False)]):
    p = a._full_path(a.store_fname)
    if os.path.exists(p):
        with open(p, 'rb') as f:
            if hashlib.sha1(f.read()).hexdigest() != a.checksum:
                bad.append(a.store_fname)
check("filestore_checksums", not bad, "bad=%d sample=%s" % (len(bad), bad[:5]))

fs_dir = A._filestore()
n_files = sum(len(fs) for _, _, fs in os.walk(fs_dir)) if os.path.isdir(fs_dir) else -1
info("filestore_dir", fs_dir, "files=%d" % n_files)

# The attachments seed_attachments.py created on the source.
smoke = A.search([('name', '=like', 'smoke-%')])
smoke_missing = [a.name for a in smoke if not a.store_fname or not os.path.exists(a._full_path(a.store_fname))]
info("smoke_attachments", len(smoke))
check("smoke_attachments_present", bool(smoke) and not smoke_missing,
      "count=%d missing=%s%s" % (len(smoke), smoke_missing[:5],
                                 "" if smoke else " (run: smoke.sh seed)"))

ICP = env['ir.config_parameter'].sudo()
neutralized = str(ICP.get_param('database.is_neutralized') or '').lower() in ('true', '1')
info("web_base_url", ICP.get_param('web.base.url'))

servers = env['ir.mail_server'].sudo().with_context(active_test=False).search([])
for s in servers:
    info("mail_server", s.id, s.name, s.smtp_host, s.smtp_port, s.smtp_encryption, "active=%s" % s.active)
active = servers.filtered('active')
prod_smtp_active = bool(active.filtered(lambda s: s.smtp_host == 'smtp.example.com'))

if ROLE == 'source':
    check("source_not_neutralized", not neutralized, "is_neutralized=%s" % neutralized)
    check("source_mail_server_active", prod_smtp_active,
          "smtp.example.com active=%s%s" % (prod_smtp_active,
                                            "" if prod_smtp_active else " (run: smoke.sh seed)"))
else:
    check("target_neutralized", neutralized, "is_neutralized=%s" % neutralized)
    check("target_source_mail_server_inactive", not prod_smtp_active,
          "smtp.example.com active=%s" % prod_smtp_active)
    hosts = sorted(active.mapped('smtp_host'))
    check("target_only_staging_smtp_active", hosts == [STAGING_SMTP_HOST],
          "active hosts=%s expected=[%s]" % (hosts, STAGING_SMTP_HOST))

print("CHECK_DONE", flush=True)
