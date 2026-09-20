#!/bin/bash
# Kratos observability instance startup. Runs on every boot; every step is idempotent.
set -euo pipefail

DATA_DEVICE=/dev/disk/by-id/google-observability-data
DATA_ROOT=/mnt/disks/data
BUNDLE_DIR="$DATA_ROOT/bundle"
COMPOSE=/var/lib/kratos/docker-compose

metadata() {
  curl -sf -H Metadata-Flavor:Google \
    "http://metadata.google.internal/computeMetadata/v1/instance/attributes/$1"
}

# --- Block containers from reaching the instance metadata server.
# The instance identity can read the config bucket, write telemetry objects and log in to Cloud
# SQL. A workload or a compromised service in any container must not be able to mint that token,
# so the bridge networks cannot route to the metadata address at all. The Cloud SQL Auth Proxy
# needs it, and reaches it over the host network namespace instead.
if ! iptables -C DOCKER-USER -d 169.254.169.254 -j REJECT 2>/dev/null; then
  iptables -I DOCKER-USER -d 169.254.169.254 -j REJECT
fi

# --- Prepare the data disk without ever reformatting one that already holds data.
# `fsck` exits 1 when it corrected errors, which is a normal, successful outcome. Keying a
# reformat off a non-zero exit would therefore destroy a healthy filesystem.
if ! blkid "$DATA_DEVICE" >/dev/null 2>&1; then
  echo "no filesystem signature on $DATA_DEVICE; creating one"
  mkfs.ext4 -F "$DATA_DEVICE"
fi
fsck.ext4 -p "$DATA_DEVICE" || [ $? -le 2 ]
mkdir -p "$DATA_ROOT"
mountpoint -q "$DATA_ROOT" || mount -o discard,defaults "$DATA_DEVICE" "$DATA_ROOT"
mkdir -p "$DATA_ROOT"/{prometheus,loki,tempo,grafana} "$BUNDLE_DIR" /var/lib/kratos

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
CURRENT_GENERATION="$(docker run --rm --network host \
  gcr.io/google.com/cloudsdktool/google-cloud-cli:stable \
  gcloud storage ls --format='value(generation)' "$BUNDLE_OBJECT" 2>/dev/null || true)"
RECORDED_GENERATION="$(cat "$DATA_ROOT/bundle.generation" 2>/dev/null || true)"
if [ -n "$CURRENT_GENERATION" ] && [ "$CURRENT_GENERATION" != "$RECORDED_GENERATION" ]; then
  docker run --rm --network host -v "$DATA_ROOT:/data" \
    gcr.io/google.com/cloudsdktool/google-cloud-cli:stable \
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
docker run --rm --network host \
  gcr.io/google.com/cloudsdktool/google-cloud-cli:stable \
  gcloud secrets versions access latest --secret="$GRAFANA_SECRET" \
  > /var/lib/kratos/grafana-password

{
  echo "KRATOS_GRAFANA_ADMIN_USER=$(metadata grafana-user)"
  echo "KRATOS_GRAFANA_ADMIN_PASSWORD=$(cat /var/lib/kratos/grafana-password)"
  echo "KRATOS_DATA_ROOT=$DATA_ROOT"
  echo "KRATOS_TELEMETRY_BUCKET=$(metadata telemetry-bucket)"
  echo "KRATOS_GRAFANA_HOST=$(metadata grafana-host)"
  echo "KRATOS_MLFLOW_HOST=$(metadata mlflow-host)"
  echo "KRATOS_MLFLOW_DATABASE_URI=$(metadata mlflow-database-uri)"
  echo "KRATOS_MLFLOW_INSTANCE=$(metadata mlflow-instance)"
} > "$BUNDLE_DIR/.env"

cd "$BUNDLE_DIR"
"$COMPOSE" -f compose.yaml -f compose.cloud.yaml up -d --remove-orphans
