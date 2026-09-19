#!/bin/sh
# Runs before files are removed. dpkg passes "remove"/"upgrade"; rpm passes 0
# for a final removal and 1 for the remove half of an upgrade. Stopping the
# service during an upgrade would turn a package refresh into an outage, so
# both forms of "this is an upgrade" are filtered out here.
set -eu

case "${1:-}" in
    upgrade | 1) exit 0 ;;
esac

if command -v systemctl > /dev/null 2>&1; then
    systemctl --quiet is-enabled record-store 2>/dev/null && systemctl disable record-store > /dev/null 2>&1 || true
    systemctl --quiet is-active record-store 2>/dev/null && systemctl stop record-store > /dev/null 2>&1 || true
fi

exit 0
