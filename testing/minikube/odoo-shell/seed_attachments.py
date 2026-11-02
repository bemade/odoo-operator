# Run inside `odoo shell` on the SOURCE instance (demo/prod): create a few
# attachments, so the filestore holds files that exist only on the source
# (unlike regenerable web assets) and a refresh must really copy them.
import os

A = env['ir.attachment']
made = []
for i in range(5):
    data = (f"smoke attachment {i}\n" * 200).encode()
    made.append(A.create({'name': f'smoke-{i}.txt', 'raw': data}))
made.append(A.create({'name': 'smoke-random.bin', 'raw': os.urandom(1024 * 1024)}))
env.cr.commit()
for a in made:
    print("INFO seeded attachment", a.id, a.name, a.store_fname, flush=True)
print("SEED_DONE attachments", len(made), flush=True)
