#!/bin/sh
# Runs after the deb or rpm unpacks. Must be idempotent: dpkg and rpm both call
# it on upgrades as well as first installs.
set -eu

CONFIG_DIR=/etc/record-store
ENV_FILE="${CONFIG_DIR}/record-store.env"

# Random hex without depending on openssl, which is not installed everywhere.
random_hex() {
    bytes="$1"
    if command -v openssl > /dev/null 2>&1; then
        openssl rand -hex "$bytes"
    else
        od -An -tx1 -N "$bytes" /dev/urandom | tr -d ' \n'
    fi
}

# useradd exists on both families; the flags below are the portable subset.
if ! getent group record-store > /dev/null 2>&1; then
    groupadd --system record-store
fi
if ! getent passwd record-store > /dev/null 2>&1; then
    useradd --system --gid record-store \
        --home-dir /var/lib/record-store --no-create-home \
        --shell /sbin/nologin \
        --comment "Record Store object storage" \
        record-store
fi

install -d -m 0750 -o root -g record-store "$CONFIG_DIR"
if [ -f "${CONFIG_DIR}/record-store.toml" ]; then
    chown root:record-store "${CONFIG_DIR}/record-store.toml"
    chmod 0640 "${CONFIG_DIR}/record-store.toml"
fi
install -d -m 0700 -o record-store -g record-store /var/lib/record-store

# Record Store ships no default credentials, and a package that installs the
# same secret on every machine would be exactly that. Instead each install gets
# its own, generated once and never regenerated on upgrade.
if [ ! -f "$ENV_FILE" ]; then
    umask 077
    access_key="rs-$(random_hex 8)"
    secret_key="$(random_hex 24)"
    master_key="$(random_hex 32)"
    system_token="$(random_hex 32)"
    cat > "$ENV_FILE" <<ENVEOF
# Credentials for the Record Store server, generated at install time and unique
# to this machine. Read by systemd through EnvironmentFile= in
# /usr/lib/systemd/system/record-store.service.
#
# Keep this file at mode 0600. Upgrades never rewrite it.
#
# The root credential is the S3 account with full access. Treat it as a
# break-glass key: create service accounts for applications and then set
# root_s3_enabled = false in record-store.toml.
RECORD_STORE_ROOT_ACCESS_KEY=${access_key}
RECORD_STORE_ROOT_SECRET_KEY=${secret_key}

# Encrypts stored service-account credentials. Losing it makes every stored
# credential unreadable, so back it up somewhere other than this host.
RECORD_STORE_CREDENTIAL_MASTER_KEY=${master_key}

# Bearer token for the management API and the web console.
RECORD_STORE_MANAGEMENT_SYSTEM_TOKEN=${system_token}

# Optional, lower-privilege management roles. Each must differ from the others.
#RECORD_STORE_MANAGEMENT_STORAGE_TOKEN=
#RECORD_STORE_MANAGEMENT_AUDITOR_TOKEN=

# Enables authenticated Prometheus scraping. Metrics stay closed while unset.
#RECORD_STORE_METRICS_SCRAPE_TOKEN=
ENVEOF
    chown root:root "$ENV_FILE"
    chmod 0600 "$ENV_FILE"
    generated=yes
else
    generated=no
fi

if command -v systemctl > /dev/null 2>&1; then
    systemctl daemon-reload > /dev/null 2>&1 || true
fi

# Deliberately not enabled or started: an object store should begin running
# because an operator decided to, having first read where its data goes.
if [ "$generated" = yes ]; then
    cat <<'NOTICE'

Record Store is installed but not running.

  Credentials were generated for this machine in
  /etc/record-store/record-store.env  (root-only, unique per install)

  Review /etc/record-store/record-store.toml, then:

    sudo sh -c 'set -a; . /etc/record-store/record-store.env; exec record-store server --config /etc/record-store/record-store.toml check-config'
    sudo systemctl enable --now record-store

  The S3 API listens on 0.0.0.0:7600 and the management API on 127.0.0.1:7601.
  Back up RECORD_STORE_CREDENTIAL_MASTER_KEY off this host before storing
  anything you cannot lose.

NOTICE
fi

exit 0
