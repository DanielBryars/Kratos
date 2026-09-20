"""A small, framework-neutral workload that obeys the Kratos container contract."""

from __future__ import annotations

import json
import os
import subprocess
import sys
from collections.abc import Mapping
from contextlib import suppress
from dataclasses import dataclass
from datetime import UTC, datetime
from pathlib import Path
from typing import Any
from uuid import UUID

SCHEMA_VERSION = "1.0"
WORKLOAD_VERSION = "replace-me-v1"
OUTPUT_DIRECTORY = Path("/kratos/outputs")
RESULT_PATH = OUTPUT_DIRECTORY / "result.json"
STDOUT_LIMIT_BYTES = 8 * 1024


@dataclass(frozen=True)
class RunIdentity:
    job_id: UUID
    attempt_id: UUID

    def as_dict(self) -> dict[str, str]:
        return {"job_id": str(self.job_id), "attempt_id": str(self.attempt_id)}


def load_run_identity(environment: Mapping[str, str] = os.environ) -> RunIdentity:
    """Read the correlation identifiers supplied by the Kratos worker."""
    try:
        return RunIdentity(
            job_id=UUID(environment["KRATOS_JOB_ID"]),
            attempt_id=UUID(environment["KRATOS_ATTEMPT_ID"]),
        )
    except KeyError as error:
        raise RuntimeError(f"required run identity is missing: {error.args[0]}") from error
    except ValueError as error:
        raise RuntimeError("Kratos job and attempt identifiers must be valid UUIDs") from error


def detect_gpu() -> dict[str, str | int]:
    """Fail early unless the NVIDIA runtime exposed exactly one usable GPU."""
    command = [
        "nvidia-smi",
        "--query-gpu=name,memory.total",
        "--format=csv,noheader,nounits",
    ]
    try:
        result = subprocess.run(
            command,
            check=True,
            capture_output=True,
            text=True,
            timeout=15,
        )
    except (FileNotFoundError, subprocess.SubprocessError) as error:
        raise RuntimeError("the assigned NVIDIA GPU is unavailable") from error
    rows = [row.strip() for row in result.stdout.splitlines() if row.strip()]
    if len(rows) != 1:
        raise RuntimeError(f"expected one assigned GPU, found {len(rows)}")
    try:
        name, memory_mib = (part.strip() for part in rows[0].rsplit(",", maxsplit=1))
        return {"name": name, "memory_mib": int(memory_mib)}
    except (TypeError, ValueError) as error:
        raise RuntimeError("nvidia-smi returned an unexpected GPU description") from error


def run_user_workload(gpu: Mapping[str, str | int]) -> dict[str, Any]:
    """Replace this body with training or inference code and return compact metadata."""
    return {
        "message": "Replace run_user_workload with your code",
        "gpu": dict(gpu),
    }


def write_json_atomically(payload: Mapping[str, Any], path: Path = RESULT_PATH) -> None:
    """Publish a complete output file without exposing a partially written result."""
    if not path.parent.is_dir():
        raise RuntimeError(f"durable output directory is unavailable: {path.parent}")
    temporary_path = path.with_name(f".{path.name}.{os.getpid()}.tmp")
    try:
        with temporary_path.open("x", encoding="utf-8") as stream:
            json.dump(payload, stream, separators=(",", ":"), sort_keys=True)
            stream.write("\n")
            stream.flush()
            os.fsync(stream.fileno())
        os.replace(temporary_path, path)
    finally:
        temporary_path.unlink(missing_ok=True)


def encode_stdout(payload: Mapping[str, Any]) -> str:
    encoded = json.dumps(payload, separators=(",", ":"), sort_keys=True)
    if len(encoded.encode("utf-8")) > STDOUT_LIMIT_BYTES:
        raise RuntimeError(f"structured result exceeded {STDOUT_LIMIT_BYTES} bytes")
    return encoded


def success_result(identity: RunIdentity, result: Mapping[str, Any]) -> dict[str, Any]:
    return {
        "schema_version": SCHEMA_VERSION,
        "status": "succeeded",
        "completed_at": datetime.now(UTC).isoformat(),
        "workload_version": WORKLOAD_VERSION,
        "run": identity.as_dict(),
        "result": dict(result),
        "outputs": [{"path": "result.json", "role": "result"}],
    }


def failure_result(error: Exception, environment: Mapping[str, str] = os.environ) -> dict[str, Any]:
    payload: dict[str, Any] = {
        "schema_version": SCHEMA_VERSION,
        "status": "failed",
        "workload_version": WORKLOAD_VERSION,
        "error_type": type(error).__name__,
        "detail": str(error)[:512],
    }
    with suppress(RuntimeError):
        payload["run"] = load_run_identity(environment).as_dict()
    return payload


def main() -> int:
    try:
        identity = load_run_identity()
        gpu = detect_gpu()
        result = run_user_workload(gpu)
        payload = success_result(identity, result)
        write_json_atomically(payload)
    except Exception as error:  # noqa: BLE001 - process boundary returns a structured failure
        print(encode_stdout(failure_result(error)))
        return 1
    print(encode_stdout(payload))
    return 0


if __name__ == "__main__":
    sys.exit(main())
