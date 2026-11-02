# Run inside `odoo shell` on the SOURCE instance (demo/prod): a "real"
# outgoing mail server that must NOT stay active on a refreshed staging
# (neutralize deactivates it; the operator injects the Mailpit one).
# smtp.example.com is a reserved name: nothing is ever sent there.
S = env['ir.mail_server'].sudo().with_context(active_test=False)
if not S.search([('name', '=', 'prod-smtp')]):
    S.create({'name': 'prod-smtp', 'smtp_host': 'smtp.example.com', 'smtp_port': 587,
              'smtp_encryption': 'starttls', 'smtp_user': 'prod@example.com',
              'smtp_pass': 'not-a-real-password'})
env.cr.commit()
for s in S.search([]):
    print("INFO mail server", s.id, s.name, s.smtp_host, s.smtp_port, s.smtp_encryption,
          "active=%s" % s.active, flush=True)
print("SEED_DONE mailserver", flush=True)
