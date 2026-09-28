from __future__ import annotations

import json
from pathlib import Path
from uuid import UUID

import pyarrow as pa
import pyarrow.parquet as pq
import pytest
import torch

import train


def write_selection(
    root: Path,
    *,
    episodes: list[int] | None = None,
    selects_every_episode: bool = False,
) -> Path:
    selected = [7, 2] if episodes is None else episodes
    directory = root / ".kratos"
    directory.mkdir(parents=True)
    path = directory / "training.json"
    path.write_text(
        json.dumps(
            {
                "schema_version": "1.0",
                "alias": "training",
                "dataset_id": "11111111-1111-4111-8111-111111111111",
                "dataset_name": "Robot reaches",
                "dataset_version_id": "22222222-2222-4222-8222-222222222222",
                "version_number": 3,
                "manifest_sha256": "a" * 64,
                "dataset_view_id": (
                    None if selects_every_episode else "33333333-3333-4333-8333-333333333333"
                ),
                "dataset_view_name": None if selects_every_episode else "Good reaches",
                "dataset_view_manifest_sha256": None if selects_every_episode else "b" * 64,
                "included_episodes": [] if selects_every_episode else selected,
                "selects_every_episode": selects_every_episode,
            }
        ),
        encoding="utf-8",
    )
    return path


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


def test_curated_selection_preserves_exact_identity_and_episode_order(tmp_path: Path) -> None:
    path = write_selection(tmp_path, episodes=[7, 2, 11])

    selection = train.load_dataset_selection(input_directory=tmp_path)

    assert selection.dataset_id == UUID("11111111-1111-4111-8111-111111111111")
    assert selection.dataset_version_id == UUID("22222222-2222-4222-8222-222222222222")
    assert selection.dataset_view_id == UUID("33333333-3333-4333-8333-333333333333")
    assert selection.included_episodes == (7, 2, 11)
    assert selection.selection_sha256 == train.sha256_file(path)


def test_selection_refuses_a_complete_version_with_episode_subset(tmp_path: Path) -> None:
    path = write_selection(tmp_path, selects_every_episode=True)
    document = json.loads(path.read_text(encoding="utf-8"))
    document["included_episodes"] = [1]
    path.write_text(json.dumps(document), encoding="utf-8")

    with pytest.raises(RuntimeError, match="contradictory view fields"):
        train.load_dataset_selection(input_directory=tmp_path)


def test_lerobot_rows_follow_curated_episode_order_and_ignore_other_episodes(
    tmp_path: Path,
) -> None:
    write_selection(tmp_path, episodes=[7, 2])
    data = tmp_path / "training" / "data" / "chunk-000"
    data.mkdir(parents=True)
    pq.write_table(
        pa.table(
            {
                "episode_index": [2, 9, 7, 2, 7, 9, 2, 7],
                "observation.state": [
                    [2.0, 20.0],
                    [9.0, 90.0],
                    [7.0, 70.0],
                    [2.1, 21.0],
                    [7.1, 71.0],
                    [9.1, 91.0],
                    [2.2, 22.0],
                    [7.2, 72.0],
                ],
                "action": [[value] for value in [2.0, 9.0, 7.0, 2.1, 7.1, 9.1, 2.2, 7.2]],
            }
        ),
        data / "file-000.parquet",
    )
    selection = train.load_dataset_selection(input_directory=tmp_path)

    features, actions, order = train.load_lerobot_rows(selection, tmp_path)

    assert order == (7, 2)
    assert features[:, 0].tolist() == pytest.approx([7.0, 7.1, 7.2, 2.0, 2.1, 2.2])
    assert actions[:, 0].tolist() == pytest.approx([7.0, 7.1, 7.2, 2.0, 2.1, 2.2])
