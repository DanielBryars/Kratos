"""Run Kratos' reproducible, self-contained CUDA training example."""

from __future__ import annotations

import csv
import hashlib
import json
import os
import platform
import random
import sys
from datetime import UTC, datetime
from pathlib import Path
from time import perf_counter
from typing import Any, cast

import torch
from torch import Tensor, nn

SCHEMA_VERSION = "1.0"
WORKLOAD_VERSION = "kratos-training-example-v1"
DATASET_VERSION = "kratos-shapes-v1"
DATASET_SHA256 = "c338e2ffabc1a0470ad2d4c0b9efa3ab53a82135aaca54845b3a3ebafd746451"
DATASET_PATH = Path(__file__).parent / "data" / "kratos_shapes_v1.csv"
CHECKPOINT_PATH = Path("/tmp/kratos-training-example-v1.pt")
SEED = 20260920
EPOCHS = 180
LEARNING_RATE = 0.025
OUTPUT_LIMIT_BYTES = 8 * 1024


class Classifier(nn.Module):
    """A deliberately small multilayer classifier for the bundled dataset."""

    def __init__(self) -> None:
        super().__init__()
        self.layers = nn.Sequential(nn.Linear(4, 16), nn.Tanh(), nn.Linear(16, 3))

    def forward(self, features: Tensor) -> Tensor:
        return cast(Tensor, self.layers(features))


def sha256_file(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as stream:
        for block in iter(lambda: stream.read(64 * 1024), b""):
            digest.update(block)
    return digest.hexdigest()


def verify_dataset(path: Path = DATASET_PATH) -> str:
    actual = sha256_file(path)
    if actual != DATASET_SHA256:
        raise RuntimeError(
            f"dataset integrity failure: expected sha256:{DATASET_SHA256}, got sha256:{actual}"
        )
    return actual


def load_dataset(path: Path = DATASET_PATH) -> tuple[Tensor, Tensor]:
    rows: list[list[float]] = []
    labels: list[int] = []
    with path.open(newline="", encoding="utf-8") as stream:
        reader = csv.DictReader(stream)
        expected = ["feature_1", "feature_2", "feature_3", "feature_4", "label"]
        if reader.fieldnames != expected:
            raise RuntimeError("dataset schema does not match kratos-shapes-v1")
        for row in reader:
            rows.append([float(row[f"feature_{index}"]) for index in range(1, 5)])
            labels.append(int(row["label"]))
    if len(rows) != 384:
        raise RuntimeError(f"dataset row count is {len(rows)}; expected 384")
    return torch.tensor(rows, dtype=torch.float32), torch.tensor(labels, dtype=torch.long)


def configure_determinism() -> None:
    random.seed(SEED)
    torch.manual_seed(SEED)
    torch.cuda.manual_seed_all(SEED)
    torch.use_deterministic_algorithms(True)
    torch.backends.cudnn.benchmark = False
    torch.backends.cudnn.deterministic = True


def require_cuda() -> torch.device:
    if not torch.cuda.is_available():
        raise RuntimeError("CUDA GPU is required; CPU fallback is disabled")
    if torch.cuda.device_count() < 1:
        raise RuntimeError("CUDA reported no GPU devices; CPU fallback is disabled")
    device = torch.device("cuda:0")
    probe = torch.ones(1, device=device)
    if not probe.is_cuda:
        raise RuntimeError("CUDA tensor allocation failed; CPU fallback is disabled")
    return device


def model_state_sha256(model: nn.Module) -> str:
    """Hash tensor content and metadata independently of checkpoint serialization."""
    digest = hashlib.sha256()
    for name, tensor in sorted(model.state_dict().items()):
        contiguous = tensor.detach().cpu().contiguous()
        digest.update(name.encode())
        digest.update(str(contiguous.dtype).encode())
        digest.update(json.dumps(list(contiguous.shape)).encode())
        digest.update(contiguous.numpy().tobytes())
    return digest.hexdigest()


def accuracy(logits: Tensor, labels: Tensor) -> float:
    return float((logits.argmax(dim=1) == labels).float().mean().item())


def run_training() -> dict[str, Any]:
    dataset_hash = verify_dataset()
    configure_determinism()
    device = require_cuda()
    features, labels = load_dataset()

    validation_mask = torch.arange(labels.shape[0]) % 5 == 0
    train_features = features[~validation_mask].to(device)
    train_labels = labels[~validation_mask].to(device)
    validation_features = features[validation_mask].to(device)
    validation_labels = labels[validation_mask].to(device)
    if not all(
        value.is_cuda
        for value in (train_features, train_labels, validation_features, validation_labels)
    ):
        raise RuntimeError("training data was not placed on CUDA; CPU fallback is disabled")

    model = Classifier().to(device)
    if not all(parameter.is_cuda for parameter in model.parameters()):
        raise RuntimeError("model was not placed on CUDA; CPU fallback is disabled")
    optimizer = torch.optim.Adam(model.parameters(), lr=LEARNING_RATE)
    loss_function = nn.CrossEntropyLoss()

    started = perf_counter()
    initial_loss: float | None = None
    model.train()
    for _ in range(EPOCHS):
        optimizer.zero_grad(set_to_none=True)
        loss = loss_function(model(train_features), train_labels)
        if initial_loss is None:
            initial_loss = float(loss.item())
        loss.backward()
        optimizer.step()
    torch.cuda.synchronize(device)
    duration_ms = (perf_counter() - started) * 1000

    model.eval()
    with torch.no_grad():
        final_train_logits = model(train_features)
        validation_logits = model(validation_features)
        final_loss = float(loss_function(final_train_logits, train_labels).item())
        train_accuracy = accuracy(final_train_logits, train_labels)
        validation_accuracy = accuracy(validation_logits, validation_labels)
    if initial_loss is None or final_loss >= initial_loss:
        raise RuntimeError("training did not reduce loss")
    if validation_accuracy < 0.95:
        raise RuntimeError(f"validation accuracy {validation_accuracy:.4f} is below 0.95")

    state_hash = model_state_sha256(model)
    checkpoint: dict[str, Any] = {
        "workload_version": WORKLOAD_VERSION,
        "dataset_sha256": dataset_hash,
        "seed": SEED,
        "model_state_dict": model.state_dict(),
    }
    torch.save(checkpoint, CHECKPOINT_PATH)
    checkpoint_hash = sha256_file(CHECKPOINT_PATH)
    properties = torch.cuda.get_device_properties(device)

    return {
        "schema_version": SCHEMA_VERSION,
        "status": "succeeded",
        "completed_at": datetime.now(UTC).isoformat(),
        "workload": {
            "name": "Kratos CUDA classification example",
            "version": WORKLOAD_VERSION,
        },
        "dataset": {
            "name": "Kratos Shapes",
            "version": DATASET_VERSION,
            "sha256": dataset_hash,
            "rows": int(labels.shape[0]),
            "train_rows": int(train_labels.shape[0]),
            "validation_rows": int(validation_labels.shape[0]),
        },
        "configuration": {
            "seed": SEED,
            "epochs": EPOCHS,
            "learning_rate": LEARNING_RATE,
            "deterministic_algorithms": True,
            "cublas_workspace_config": os.environ.get("CUBLAS_WORKSPACE_CONFIG"),
            "cpu_fallback": False,
        },
        "metrics": {
            "initial_train_loss": round(initial_loss, 6),
            "final_train_loss": round(final_loss, 6),
            "train_accuracy": round(train_accuracy, 6),
            "validation_accuracy": round(validation_accuracy, 6),
            "training_duration_ms": round(duration_ms, 3),
        },
        "model": {
            "architecture": "Linear(4,16)-Tanh-Linear(16,3)",
            "state_sha256": state_hash,
            "checkpoint_sha256": checkpoint_hash,
            "checkpoint_location": str(CHECKPOINT_PATH),
            "checkpoint_durable": False,
        },
        "environment": {
            "python": platform.python_version(),
            "pytorch": torch.__version__,
            "cuda_runtime": torch.version.cuda,
            "gpu_name": properties.name,
            "gpu_compute_capability": f"{properties.major}.{properties.minor}",
        },
        "determinism_note": (
            "Recorded settings improve repeatability but do not guarantee identical floating-point "
            "results across GPU models, drivers, CUDA or PyTorch versions."
        ),
    }


def encode_result(payload: dict[str, Any]) -> str:
    encoded = json.dumps(payload, separators=(",", ":"), sort_keys=True)
    if len(encoded.encode("utf-8")) > OUTPUT_LIMIT_BYTES:
        raise RuntimeError(f"structured result exceeded {OUTPUT_LIMIT_BYTES} bytes")
    return encoded


def main() -> int:
    try:
        result = run_training()
    except Exception as error:  # noqa: BLE001 - process boundary returns a structured failure
        failure = {
            "schema_version": SCHEMA_VERSION,
            "status": "failed",
            "workload_version": WORKLOAD_VERSION,
            "error_type": type(error).__name__,
            "detail": str(error)[:512],
            "cpu_fallback": False,
        }
        print(encode_result(failure))
        return 1
    print(encode_result(result))
    return 0


if __name__ == "__main__":
    sys.exit(main())
