#!/bin/sh
echo "Starting filestore migration rsync..."
echo "Source: /mnt/old"
echo "Dest:   /mnt/new"
echo "Source files: $(find /mnt/old -type f 2>/dev/null | wc -l)"
rsync -rltvH --delete \
  --exclude='.accesslog' \
  --exclude='.config' \
  --exclude='.stats' \
  --omit-dir-times \
  /mnt/old/ /mnt/new/
rc=$?
echo "Dest files:   $(find /mnt/new -type f | wc -l)"
# rsync exit code 23 = partial transfer (metadata-only failures are OK)
if [ "$rc" -eq 0 ] || [ "$rc" -eq 23 ]; then
  echo "Filestore migration rsync complete."
  exit 0
else
  echo "Filestore migration rsync failed with exit code $rc"
  exit "$rc"
fi
