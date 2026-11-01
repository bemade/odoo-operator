# Run inside `odoo shell` on the TARGET instance (demo/staging): send one mail
# through the active ir.mail_server, which should be the operator-injected
# Mailpit server.  smoke.sh prepends SUBJECT = '...'.
M = env['ir.mail_server'].sudo()
msg = M.build_email('smoke@odoo-op.local', ['dev@odoo-op.local'], SUBJECT, 'staging -> mailpit')
mid = M.send_email(msg)
print("SENT", mid, flush=True)
