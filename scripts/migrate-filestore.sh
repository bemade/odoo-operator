#!/bin/sh
set -e
echo "Starting filestore migration rsync..."
echo "Source: /mnt/old"
echo "Dest:   /mnt/new"
echo "Source files: $(find /mnt/old -type f 2>/dev/null | wc -l)"
rsync -avH --delete \
  --exclude='.accesslog' \
  --exclude='.config' \
  --exclude='.stats' \
  --no-perms --no-owner --no-group \
  /mnt/old/ /mnt/new/
echo "Dest files:   $(find /mnt/new -type f | wc -l)"
echo "Filestore migration rsync complete."
