from __future__ import annotations

import json
from pathlib import Path

import pytest
import torch

import train


def test_bundled_dataset_has_expected_identity_and_shape() -> None:
    assert train.verify_dataset() == train.DATASET_SHA256
    features, labels = train.load_dataset()
    assert features.shape == (384, 4)
    assert labels.shape == (384,)
    assert set(labels.tolist()) == {0, 1, 2}


def test_dataset_tampering_is_rejected(tmp_path: Path) -> None:
    altered = tmp_path / "altered.csv"
    altered.write_bytes(train.DATASET_PATH.read_bytes() + b"\n")
    with pytest.raises(RuntimeError, match="dataset integrity failure"):
        train.verify_dataset(altered)


def test_cuda_is_required_without_cpu_fallback(monkeypatch: pytest.MonkeyPatch) -> None:
    monkeypatch.setattr(torch.cuda, "is_available", lambda: False)
    with pytest.raises(RuntimeError, match="CPU fallback is disabled"):
        train.require_cuda()


def test_run_identity_is_validated_and_exposes_correlation_attributes() -> None:
    identity = train.load_run_identity(
        {
            "KRATOS_JOB_ID": "22222222-2222-4222-8222-222222222222",
            "KRATOS_ATTEMPT_ID": "11111111-1111-4111-8111-111111111111",
        }
    )

    assert identity.result_fields() == {
        "job_id": "22222222-2222-4222-8222-222222222222",
        "attempt_id": "11111111-1111-4111-8111-111111111111",
    }
    assert identity.otel_resource_attributes() == {
        "kratos.job.id": "22222222-2222-4222-8222-222222222222",
        "kratos.attempt.id": "11111111-1111-4111-8111-111111111111",
    }


@pytest.mark.parametrize(
    "environment, message",
    [
        ({}, "required run identity is missing"),
        (
            {"KRATOS_JOB_ID": "not-a-uuid", "KRATOS_ATTEMPT_ID": "also-not-a-uuid"},
            "must be valid UUIDs",
        ),
    ],
)
def test_invalid_run_identity_fails_clearly(environment: dict[str, str], message: str) -> None:
    with pytest.raises(RuntimeError, match=message):
        train.load_run_identity(environment)


def test_failure_result_preserves_valid_run_identity() -> None:
    failure = train.failure_result(
        RuntimeError("training failed"),
        {
            "KRATOS_JOB_ID": "22222222-2222-4222-8222-222222222222",
            "KRATOS_ATTEMPT_ID": "11111111-1111-4111-8111-111111111111",
        },
    )

    assert failure["run"] == {
        "job_id": "22222222-2222-4222-8222-222222222222",
        "attempt_id": "11111111-1111-4111-8111-111111111111",
    }
    assert failure["telemetry"]["resource_attributes"]["kratos.attempt.id"] == (
        "11111111-1111-4111-8111-111111111111"
    )


def test_structured_result_is_compact_and_stable() -> None:
    encoded = train.encode_result({"status": "succeeded", "schema_version": "1.0"})
    assert len(encoded.encode()) <= train.OUTPUT_LIMIT_BYTES
    assert json.loads(encoded) == {"schema_version": "1.0", "status": "succeeded"}


def test_oversized_result_is_rejected() -> None:
    with pytest.raises(RuntimeError, match="structured result exceeded"):
        train.encode_result({"detail": "x" * train.OUTPUT_LIMIT_BYTES})


def test_checkpoint_is_written_to_the_declared_output(tmp_path: Path) -> None:
    checkpoint_path = tmp_path / "model.pt"
    checkpoint = {"seed": train.SEED, "weights": torch.tensor([1.0, 2.0])}

    digest = train.save_checkpoint(checkpoint, checkpoint_path)

    assert checkpoint_path.is_file()
    assert digest == train.sha256_file(checkpoint_path)
    assert torch.load(checkpoint_path, weights_only=False)["seed"] == train.SEED


def test_checkpoint_requires_a_worker_output_directory(tmp_path: Path) -> None:
    with pytest.raises(RuntimeError, match="durable output directory is unavailable"):
        train.save_checkpoint({"seed": train.SEED}, tmp_path / "missing" / "model.pt")
