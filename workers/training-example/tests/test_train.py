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


def test_structured_result_is_compact_and_stable() -> None:
    encoded = train.encode_result({"status": "succeeded", "schema_version": "1.0"})
    assert len(encoded.encode()) <= train.OUTPUT_LIMIT_BYTES
    assert json.loads(encoded) == {"schema_version": "1.0", "status": "succeeded"}


def test_oversized_result_is_rejected() -> None:
    with pytest.raises(RuntimeError, match="structured result exceeded"):
        train.encode_result({"detail": "x" * train.OUTPUT_LIMIT_BYTES})
