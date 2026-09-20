from __future__ import annotations

import json
from pathlib import Path

import pytest

from workload import load_run_identity, write_json_atomically


def test_run_identity_requires_valid_worker_identifiers() -> None:
    identity = load_run_identity(
        {
            "KRATOS_JOB_ID": "22222222-2222-4222-8222-222222222222",
            "KRATOS_ATTEMPT_ID": "11111111-1111-4111-8111-111111111111",
        }
    )
    assert identity.as_dict() == {
        "job_id": "22222222-2222-4222-8222-222222222222",
        "attempt_id": "11111111-1111-4111-8111-111111111111",
    }

    with pytest.raises(RuntimeError, match="must be valid UUIDs"):
        load_run_identity({"KRATOS_JOB_ID": "not-a-uuid", "KRATOS_ATTEMPT_ID": "also-invalid"})


def test_atomic_output_replaces_the_file_and_leaves_no_partial_file(tmp_path: Path) -> None:
    output = tmp_path / "result.json"
    output.write_text("old", encoding="utf-8")

    write_json_atomically({"status": "complete", "metric": 0.95}, output)

    assert json.loads(output.read_text(encoding="utf-8")) == {
        "metric": 0.95,
        "status": "complete",
    }
    assert list(tmp_path.glob(".*.tmp")) == []
