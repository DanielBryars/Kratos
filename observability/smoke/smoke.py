"""Prove that the local observability stack carries Kratos identifiers end to end.

One metric, one log record and one span are sent to the gateway by OTLP/HTTP, and one MLflow run is
logged, all carrying the same job and attempt identifiers. The script then reads each signal back
from the store that should hold it. It uses only the standard library and runs on the stack's
private network, because Prometheus, Loki and Tempo are deliberately not published.
"""

import base64
import json
import os
import sys
import time
import urllib.error
import urllib.parse
import urllib.request
import uuid
from collections.abc import Callable
from typing import Any

GATEWAY = "http://otel-gateway:4318"
PROMETHEUS = "http://prometheus:9090"
LOKI = "http://loki:3100"
TEMPO = "http://tempo:3200"
GRAFANA = "http://grafana:3000"
MLFLOW = "http://mlflow:5000"
SERVICE_NAME = "kratos-smoke"
DEADLINE_SECONDS = 180


def call(
    url: str, payload: dict[str, Any] | None = None, headers: dict[str, str] | None = None
) -> Any:
    body = None if payload is None else json.dumps(payload).encode()
    request = urllib.request.Request(url, data=body, headers=headers or {})
    if body is not None:
        request.add_header("Content-Type", "application/json")
    with urllib.request.urlopen(request, timeout=10) as response:
        raw = response.read()
    try:
        return json.loads(raw) if raw else None
    except json.JSONDecodeError:
        return raw.decode(errors="replace")


def eventually(description: str, probe: Callable[[], Any]) -> Any:
    """Retry ``probe`` until it returns something truthy; stores index what they receive late."""
    deadline = time.monotonic() + DEADLINE_SECONDS
    last_error = "no result"
    while time.monotonic() < deadline:
        try:
            if found := probe():
                return found
        except (urllib.error.URLError, OSError, KeyError, IndexError, ValueError) as error:
            last_error = repr(error)
        time.sleep(3)
    raise SystemExit(f"FAILED: {description}: {last_error}")


def ready(url: str) -> bool:
    call(url)
    return True


def attributes(values: dict[str, str]) -> list[dict[str, Any]]:
    return [{"key": key, "value": {"stringValue": value}} for key, value in values.items()]


def main() -> int:
    identity = {
        "service.name": SERVICE_NAME,
        "kratos.worker.id": str(uuid.uuid4()),
        "kratos.job.id": str(uuid.uuid4()),
        "kratos.attempt.id": str(uuid.uuid4()),
    }
    job_id, attempt_id = identity["kratos.job.id"], identity["kratos.attempt.id"]
    resource = {"attributes": attributes(identity)}
    scope = {"name": "kratos.smoke"}
    trace_id, span_id = uuid.uuid4().hex, uuid.uuid4().hex[:16]

    for name, url in {
        "gateway": "http://otel-gateway:13133/",
        "prometheus": f"{PROMETHEUS}/-/ready",
        "loki": f"{LOKI}/ready",
        "tempo": f"{TEMPO}/ready",
        "grafana": f"{GRAFANA}/api/health",
        "mlflow": f"{MLFLOW}/health",
    }.items():
        eventually(f"{name} did not become ready", lambda url=url: ready(url))

    now = time.time_ns()
    call(
        f"{GATEWAY}/v1/metrics",
        {
            "resourceMetrics": [
                {
                    "resource": resource,
                    "scopeMetrics": [
                        {
                            "scope": scope,
                            "metrics": [
                                {
                                    "name": "kratos.smoke.loss",
                                    "gauge": {
                                        "dataPoints": [{"timeUnixNano": str(now), "asDouble": 0.25}]
                                    },
                                }
                            ],
                        }
                    ],
                }
            ]
        },
    )
    call(
        f"{GATEWAY}/v1/logs",
        {
            "resourceLogs": [
                {
                    "resource": resource,
                    "scopeLogs": [
                        {
                            "scope": scope,
                            "logRecords": [
                                {
                                    "timeUnixNano": str(now),
                                    "severityText": "INFO",
                                    "body": {"stringValue": "kratos smoke log line"},
                                    "traceId": trace_id,
                                    "spanId": span_id,
                                }
                            ],
                        }
                    ],
                }
            ]
        },
    )
    call(
        f"{GATEWAY}/v1/traces",
        {
            "resourceSpans": [
                {
                    "resource": resource,
                    "scopeSpans": [
                        {
                            "scope": scope,
                            "spans": [
                                {
                                    "traceId": trace_id,
                                    "spanId": span_id,
                                    "name": "kratos.smoke.step",
                                    "kind": 1,
                                    "startTimeUnixNano": str(now - 1_000_000),
                                    "endTimeUnixNano": str(now),
                                    "attributes": attributes({"kratos.job.id": job_id}),
                                }
                            ],
                        }
                    ],
                }
            ]
        },
    )

    api = f"{MLFLOW}/api/2.0/mlflow"
    try:
        experiment_id = call(f"{api}/experiments/create", {"name": SERVICE_NAME})["experiment_id"]
    except urllib.error.HTTPError:
        found = call(f"{api}/experiments/get-by-name?experiment_name={SERVICE_NAME}")
        experiment_id = found["experiment"]["experiment_id"]
    run_id = call(
        f"{api}/runs/create",
        {
            "experiment_id": experiment_id,
            "start_time": now // 1_000_000,
            "tags": [{"key": key, "value": value} for key, value in identity.items()],
        },
    )["run"]["info"]["run_id"]
    call(
        f"{api}/runs/log-batch",
        {
            "run_id": run_id,
            "params": [{"key": "epochs", "value": "1"}],
            "metrics": [
                {"key": "loss", "value": 0.25, "timestamp": now // 1_000_000, "step": 1}
            ],
        },
    )
    call(f"{api}/runs/update", {"run_id": run_id, "status": "FINISHED"})

    def metric() -> Any:
        """The series itself must stay narrow; a per-run label would be a series per attempt."""
        query = urllib.parse.quote("kratos_smoke_loss")
        series = call(f"{PROMETHEUS}/api/v1/query?query={query}")["data"]["result"]
        if not series:
            return None
        labels = series[0]["metric"]
        forbidden = {"kratos_job_id", "kratos_attempt_id"} & labels.keys()
        if forbidden:
            raise SystemExit(f"FAILED: per-run labels on a metric series: {sorted(forbidden)}")
        return labels

    def run_association() -> Any:
        """Job and attempt reach the run through target_info, not through the series labels."""
        query = urllib.parse.quote(f'target_info{{kratos_attempt_id="{attempt_id}"}}')
        series = call(f"{PROMETHEUS}/api/v1/query?query={query}")["data"]["result"]
        return series[0]["metric"]["kratos_job_id"] if series else None

    def log_line() -> Any:
        query = urllib.parse.quote(
            f'{{service_name="{SERVICE_NAME}"}} | kratos_attempt_id="{attempt_id}"'
        )
        start = now - 3_600_000_000_000
        streams = call(f"{LOKI}/loki/api/v1/query_range?query={query}&start={start}")
        return streams["data"]["result"][0]["values"][0][1]

    def span() -> Any:
        found = json.dumps(call(f"{TEMPO}/api/traces/{trace_id}"))
        return "kratos.smoke.step" if job_id in found and "kratos.smoke.step" in found else None

    def mlflow_run() -> Any:
        runs = call(
            f"{api}/runs/search",
            {
                "experiment_ids": [experiment_id],
                "filter": f"tags.`kratos.attempt.id` = '{attempt_id}'",
            },
        )["runs"]
        metrics = {item["key"]: item["value"] for item in runs[0]["data"]["metrics"]}
        return {"run_id": runs[0]["info"]["run_id"], "metrics": metrics}

    def datasources() -> Any:
        user = os.environ["KRATOS_GRAFANA_ADMIN_USER"]
        password = os.environ["KRATOS_GRAFANA_ADMIN_PASSWORD"]
        token = base64.b64encode(f"{user}:{password}".encode()).decode()
        headers = {"Authorization": f"Basic {token}"}
        health = {}
        for uid in ("kratos-prometheus", "kratos-loki", "kratos-tempo"):
            health[uid] = call(f"{GRAFANA}/api/datasources/uid/{uid}/health", headers=headers)[
                "status"
            ]
        return health if set(health.values()) == {"OK"} else None

    def anonymous_grafana_is_refused() -> Any:
        try:
            call(f"{GRAFANA}/api/datasources")
        except urllib.error.HTTPError as error:
            return error.code if error.code in (401, 403) else None
        return None

    evidence = {
        "identity": identity,
        "trace_id": trace_id,
        "prometheus_series": eventually("metric did not reach Prometheus", metric),
        "prometheus_run_association": eventually(
            "target_info did not associate the run", run_association
        ),
        "loki_line": eventually("log record did not reach Loki", log_line),
        "tempo_span": eventually("span did not reach Tempo", span),
        "mlflow_run": eventually("run was not found in MLflow by attempt tag", mlflow_run),
        "grafana_datasources": eventually("a Grafana datasource is unhealthy", datasources),
        "grafana_anonymous_status": eventually(
            "Grafana answered an unauthenticated request", anonymous_grafana_is_refused
        ),
    }
    print(json.dumps({"status": "passed", **evidence}, indent=2))
    return 0


if __name__ == "__main__":
    sys.exit(main())
