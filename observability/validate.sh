#!/usr/bin/env sh
# Validate the observability bundle with each component's own checker, without starting it.
# Every image is the digest pinned in compose.yaml. Runs in CI and locally; needs only Docker.
set -eu

# Git Bash on Windows rewrites container-absolute paths unless this is set.
export MSYS_NO_PATHCONV=1

cd "$(dirname "$0")"

PROMETHEUS="prom/prometheus@sha256:5ce7540c3c00ef4ab0c9d2c995c6a5b9c421f44b4a115d97a2c7af3b1c21cbb0"
LOKI="grafana/loki@sha256:1107dd5274e0ada47e42472b7a7e71f3b2a2fe878878108f3e2f9e51528f0193"
TEMPO="grafana/tempo@sha256:0296560ac66f8a3600d7fb3014a52c189d4d9c3549ad6ff441bf2409855d68d5"
COLLECTOR="otel/opentelemetry-collector-contrib@sha256:fd328de2552466ad78385e1b1289c3f2402b1c45f265b252aab1955b42845ac1"

config="$(pwd)/config"

echo "compose"
# A password is required at runtime; any value satisfies the interpolation check.
KRATOS_GRAFANA_ADMIN_PASSWORD=validate-only docker compose config --quiet

echo "prometheus"
docker run --rm --entrypoint promtool -v "$config/prometheus/prometheus.yml:/p.yml:ro" \
  "$PROMETHEUS" check config /p.yml >/dev/null

echo "collector"
docker run --rm -v "$config/otel-gateway/config.yaml:/c.yaml:ro" \
  "$COLLECTOR" validate --config=/c.yaml

echo "loki"
docker run --rm -v "$config/loki/loki.yaml:/l.yaml:ro" \
  "$LOKI" -config.file=/l.yaml -verify-config

echo "tempo"
docker run --rm -v "$config/tempo/tempo.yaml:/t.yaml:ro" \
  "$TEMPO" -config.file=/t.yaml \
  -backend-scheduler.provider.work.compaction.block-retention=72h -config.verify=true

echo "images pinned by digest"
# A tag without a digest would silently change what CI validated and what a VM would run.
if grep -nE "^\s+image: .*" compose.yaml | grep -v "@sha256:"; then
  echo "the images above are not pinned by digest" >&2
  exit 1
fi

echo "all observability configuration is valid"
