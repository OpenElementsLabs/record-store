#!/bin/sh
# Runs after files are removed.
#
# Stored objects, the generated credentials and the service account are left
# behind on purpose. Removing a package must never be the thing that destroys
# the data it was storing; the notice below says how to finish the job by hand.
set -eu

if command -v systemctl > /dev/null 2>&1; then
    systemctl daemon-reload > /dev/null 2>&1 || true
fi

case "${1:-}" in
    upgrade | 1 | 2) exit 0 ;;
esac

cat <<'NOTICE'

Record Store has been removed. Left in place deliberately:

  /var/lib/record-store             stored objects and metadata
  /etc/record-store/record-store.env  generated credentials
  the record-store service account

To remove them as well, once you are certain:

  sudo rm -rf /var/lib/record-store /etc/record-store
  sudo userdel record-store

NOTICE

exit 0
