#!/bin/sh
set -eu

: "${KRATOS_DATABASE_CONNECTION_NAME:?database connection name is required}"

/usr/local/bin/cloud-sql-proxy \
  --address=127.0.0.1 \
  --port=5432 \
  --auto-iam-authn \
  --structured-logs \
  "${KRATOS_DATABASE_CONNECTION_NAME}" &
proxy_pid=$!

cleanup() {
  kill "${proxy_pid}" 2>/dev/null || true
  wait "${proxy_pid}" 2>/dev/null || true
}
trap cleanup EXIT HUP INT TERM

/usr/local/bin/kratos-migrate
