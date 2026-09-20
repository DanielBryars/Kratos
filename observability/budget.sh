#!/usr/bin/env sh
# Report what the stack is holding, and fail when a volume is over its budget.
#
# Compose cannot impose a filesystem quota on a local volume, so this is an alert-and-act policy
# rather than a hard ceiling: it makes exhaustion visible before a disk fills, and it is the check
# an operator or a timer runs. Prometheus is the only service with an enforced byte ceiling of its
# own; everything else is bounded by age and ingestion rate, which bounds growth rate but not
# absolute bytes.
#
# Usage: sh budget.sh [budget-in-mib-per-volume]   (default 2048)
set -eu

export MSYS_NO_PATHCONV=1
cd "$(dirname "$0")"

BUDGET_MIB="${1:-2048}"
PROJECT=kratos-observability
over=0

for volume in prometheus-data loki-data tempo-data grafana-data mlflow-data; do
  name="${PROJECT}_${volume}"
  if ! docker volume inspect "$name" >/dev/null 2>&1; then
    echo "skip   $volume (not created)"
    continue
  fi
  used_mib=$(docker run --rm -v "$name:/measured:ro" \
    busybox:1.36-uclibc sh -c 'du -sm /measured 2>/dev/null | cut -f1')
  if [ "$used_mib" -gt "$BUDGET_MIB" ]; then
    echo "OVER   $volume ${used_mib} MiB (budget ${BUDGET_MIB} MiB)"
    over=$((over + 1))
  else
    echo "ok     $volume ${used_mib} MiB"
  fi
done

if [ "$over" -gt 0 ]; then
  cat >&2 <<'GUIDANCE'

A volume is over budget. The documented response, in order:
  1. Shorten retention in the service's configuration and let its compactor reclaim.
  2. For MLflow, which has no automatic cleanup, delete runs and their artefacts that are no
     longer referenced, then run `mlflow gc`.
  3. Only then raise the budget, and record why.
GUIDANCE
  exit 1
fi

echo "every volume is within ${BUDGET_MIB} MiB"
