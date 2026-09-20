#!/bin/bash
# Kratos observability instance startup. Runs on every boot and, through the timer it installs,
# every few minutes afterwards. Every step is idempotent, and it exits 0 when the bundle or the
# secret is not yet present so that the timer simply tries again: the documented order applies
# Terraform first and uploads the bundle afterwards, so the first boot legitimately finds neither.
set -euo pipefail

DATA_DEVICE=/dev/disk/by-id/google-observability-data
DATA_ROOT=/mnt/disks/data
BUNDLE_DIR="$DATA_ROOT/bundle"
COMPOSE="$DATA_ROOT/docker-compose"
# Pinned: this helper runs on the host network and can mint the instance token and read the
# Grafana secret, so it must not float on a mutable tag.
CLOUD_CLI="gcr.io/google.com/cloudsdktool/google-cloud-cli@sha256:409a43ae520cd0acff8185636a4df9a656a44df2c7d58b99300eeed81142c5a2"
METADATA_IP=169.254.169.254
TRUSTED_STORAGE_CLIENTS=(172.30.1.12 172.30.2.10 172.30.2.11 172.30.2.12)

metadata() {
  curl -sf -H Metadata-Flavor:Google \
    "http://metadata.google.internal/computeMetadata/v1/instance/attributes/$1"
}

gcloud_helper() {
  docker run --rm --network host "$@" "$CLOUD_CLI" gcloud "${@:$#}" 2>/dev/null
}

# --- Block containers from reaching the instance metadata server.
# The instance identity can read the config bucket, write telemetry objects and log in to Cloud
# SQL. A workload or a compromised service in a bridged container must not be able to mint that
# token. Loki, Tempo, MLflow and the Cloud SQL Auth Proxy use fixed, dedicated bridge addresses
# because their Google Cloud clients need short-lived instance credentials. No other bridged
# container may reach metadata.
configure_metadata_firewall() {
  if ! iptables -C DOCKER-USER -d "$METADATA_IP" -j REJECT 2>/dev/null; then
    iptables -I DOCKER-USER -d "$METADATA_IP" -j REJECT
  fi
  for client_ip in "${TRUSTED_STORAGE_CLIENTS[@]}"; do
    if ! iptables -C DOCKER-USER -s "$client_ip" -d "$METADATA_IP" -j ACCEPT 2>/dev/null; then
      iptables -I DOCKER-USER -s "$client_ip" -d "$METADATA_IP" -j ACCEPT
    fi
  done
}

configure_metadata_firewall

# --- Prepare the data disk without ever reformatting one that already holds data.
# `fsck` exits 1 when it corrected errors, which is a normal, successful outcome. Keying a
# reformat off a non-zero exit would therefore destroy a healthy filesystem.
mkdir -p "$DATA_ROOT"
if ! mountpoint -q "$DATA_ROOT"; then
  if ! blkid "$DATA_DEVICE" >/dev/null 2>&1; then
    echo "no filesystem signature on $DATA_DEVICE; creating one"
    mkfs.ext4 -F "$DATA_DEVICE"
  fi
  fsck.ext4 -p "$DATA_DEVICE" || [ $? -le 2 ]
  mount -o discard,defaults "$DATA_DEVICE" "$DATA_ROOT"
fi
mkdir -p "$DATA_ROOT"/{prometheus,loki,tempo,grafana,mlflow} "$BUNDLE_DIR" /var/lib/kratos

# --- Each service runs as a non-root user from its own image, so the directory it writes into
# must belong to that user. A root-owned bind mount leaves the data path unwritable and the
# service crash-looping. These are the users the pinned images declare.
chown -R 65534:65534 "$DATA_ROOT/prometheus"   # prom/prometheus runs as nobody
chown -R 10001:10001 "$DATA_ROOT/loki" "$DATA_ROOT/tempo"
chown -R 472:0 "$DATA_ROOT/grafana"            # grafana runs as uid 472
chmod 0750 "$DATA_ROOT"/{prometheus,loki,tempo,grafana}

# --- Install the reconciliation timer on first boot, so a replaced bundle is picked up without
# anyone rebooting the instance. The unit runs this same script.
if [ ! -f /etc/systemd/system/kratos-observability.timer ]; then
  cat > /etc/systemd/system/kratos-observability.service <<'UNIT'
[Unit]
Description=Reconcile the Kratos observability stack
Wants=network-online.target
After=network-online.target

[Service]
Type=oneshot
ExecStart=/bin/bash /var/lib/kratos/startup.sh
UNIT
  cat > /etc/systemd/system/kratos-observability.timer <<'UNIT'
[Unit]
Description=Re-read the observability bundle and reconcile the stack

[Timer]
OnBootSec=2min
OnUnitActiveSec=5min
AccuracySec=30s

[Install]
WantedBy=timers.target
UNIT
  install -m 0755 "$0" /var/lib/kratos/startup.sh
  systemctl daemon-reload
  systemctl enable --now kratos-observability.timer
fi

# --- Container-Optimized OS ships no Compose plugin, so fetch the pinned binary once and verify
# it against the checksum Terraform supplies. An unverified binary is never executed.
COMPOSE_URL="$(metadata compose-url)"
COMPOSE_SHA256="$(metadata compose-sha256)"
if ! echo "$COMPOSE_SHA256  $COMPOSE" | sha256sum --check --status 2>/dev/null; then
  curl -sfL "$COMPOSE_URL" -o "$COMPOSE.download"
  echo "$COMPOSE_SHA256  $COMPOSE.download" | sha256sum --check --status
  chmod 0755 "$COMPOSE.download"
  mv "$COMPOSE.download" "$COMPOSE"
fi

# --- Fetch the observability bundle, and re-fetch when its generation changes.
BUNDLE_OBJECT="$(metadata config-bundle)"
CURRENT_GENERATION="$(docker run --rm --network host "$CLOUD_CLI" \
  gcloud storage objects describe --format='value(generation)' "$BUNDLE_OBJECT" 2>/dev/null || true)"
if [ -z "$CURRENT_GENERATION" ]; then
  echo "no bundle at $BUNDLE_OBJECT yet; the timer will retry"
  exit 0
fi
RECORDED_GENERATION="$(cat "$DATA_ROOT/bundle.generation" 2>/dev/null || true)"
if [ "$CURRENT_GENERATION" != "$RECORDED_GENERATION" ]; then
  docker run --rm --network host -v "$DATA_ROOT:/data" "$CLOUD_CLI" \
    gcloud storage cp "$BUNDLE_OBJECT" /data/bundle.tar.gz
  rm -rf "$BUNDLE_DIR"
  mkdir -p "$BUNDLE_DIR"
  tar -xzf "$DATA_ROOT/bundle.tar.gz" -C "$BUNDLE_DIR" --strip-components=1
  echo "$CURRENT_GENERATION" > "$DATA_ROOT/bundle.generation"
fi

# --- Read the Grafana administrator password from Secret Manager into a private environment file.
# It is never written into an image, the bundle, or Terraform state.
GRAFANA_SECRET="$(metadata grafana-secret)"
umask 077
if ! docker run --rm --network host "$CLOUD_CLI" \
  gcloud secrets versions access latest --secret="$GRAFANA_SECRET" \
  > /var/lib/kratos/grafana-password 2>/dev/null; then
  echo "no version of secret $GRAFANA_SECRET yet; the timer will retry"
  exit 0
fi

MLFLOW_INSTANCE="$(metadata mlflow-instance)"
{
  echo "KRATOS_GRAFANA_ADMIN_USER=$(metadata grafana-user)"
  echo "KRATOS_GRAFANA_ADMIN_PASSWORD=$(cat /var/lib/kratos/grafana-password)"
  echo "KRATOS_DATA_ROOT=$DATA_ROOT"
  echo "KRATOS_TELEMETRY_BUCKET=$(metadata telemetry-bucket)"
  echo "KRATOS_GRAFANA_HOST=$(metadata grafana-host)"
  echo "KRATOS_MLFLOW_HOST=$(metadata mlflow-host)"
  echo "KRATOS_MLFLOW_DATABASE_URI=$(metadata mlflow-database-uri)"
  echo "KRATOS_MLFLOW_INSTANCE=${MLFLOW_INSTANCE}"
} > "$BUNDLE_DIR/.env"

# The Cloud SQL overlay is applied only when a database was actually provisioned. Without it the
# stack must not reference the proxy at all, or Compose fails to interpolate before anything runs.
OVERLAYS=(-f compose.yaml -f compose.cloud.yaml)
if [ -n "$MLFLOW_INSTANCE" ]; then
  OVERLAYS+=(-f compose.cloudsql.yaml)
fi

cd "$BUNDLE_DIR"
"$COMPOSE" "${OVERLAYS[@]}" up -d --remove-orphans
# Docker can rewrite DOCKER-USER while creating networks. Reconcile the allowlist after Compose so
# the explicit storage-client exceptions remain ahead of the metadata deny rule.
configure_metadata_firewall
