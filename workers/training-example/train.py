"""Run Kratos' reproducible, self-contained CUDA training example."""

from __future__ import annotations

import csv
import hashlib
import json
import math
import os
import platform
import random
import re
import sys
from collections.abc import Mapping
from dataclasses import dataclass
from datetime import UTC, datetime
from pathlib import Path
from time import perf_counter
from typing import Any, cast
from uuid import UUID

import pyarrow.parquet as parquet
import torch
from torch import Tensor, nn

SCHEMA_VERSION = "1.0"
WORKLOAD_VERSION = "kratos-training-example-v1"
DATASET_VERSION = "kratos-shapes-v1"
DATASET_SHA256 = "c338e2ffabc1a0470ad2d4c0b9efa3ab53a82135aaca54845b3a3ebafd746451"
DATASET_PATH = Path(__file__).parent / "data" / "kratos_shapes_v1.csv"
INPUT_DIRECTORY = Path("/kratos/inputs")
DATASET_INPUT_ALIAS = "training"
SELECTION_DIRECTORY = ".kratos"
LEROBOT_MAX_ROWS = 4_096
OUTPUT_DIRECTORY = Path("/kratos/outputs")
CHECKPOINT_PATH = OUTPUT_DIRECTORY / "model.pt"
SEED = 20260920
EPOCHS = 180
LEARNING_RATE = 0.025
OUTPUT_LIMIT_BYTES = 8 * 1024


@dataclass(frozen=True)
class RunIdentity:
    job_id: UUID
    attempt_id: UUID

    def result_fields(self) -> dict[str, str]:
        return {"job_id": str(self.job_id), "attempt_id": str(self.attempt_id)}

    def otel_resource_attributes(self) -> dict[str, str]:
        return {
            "kratos.job.id": str(self.job_id),
            "kratos.attempt.id": str(self.attempt_id),
        }


@dataclass(frozen=True)
class DatasetSelection:
    """Immutable dataset identity and ordered episode selection supplied by Kratos."""

    alias: str
    dataset_id: UUID
    dataset_name: str
    dataset_version_id: UUID
    version_number: int
    manifest_sha256: str
    dataset_view_id: UUID | None
    dataset_view_name: str | None
    dataset_view_manifest_sha256: str | None
    included_episodes: tuple[int, ...]
    selects_every_episode: bool
    selection_sha256: str

    def result_fields(self) -> dict[str, Any]:
        return {
            "alias": self.alias,
            "dataset_id": str(self.dataset_id),
            "name": self.dataset_name,
            "dataset_version_id": str(self.dataset_version_id),
            "version_number": self.version_number,
            "manifest_sha256": self.manifest_sha256,
            "dataset_view_id": str(self.dataset_view_id) if self.dataset_view_id else None,
            "dataset_view_name": self.dataset_view_name,
            "dataset_view_manifest_sha256": self.dataset_view_manifest_sha256,
            "included_episodes": list(self.included_episodes),
            "selects_every_episode": self.selects_every_episode,
            "selection_sha256": self.selection_sha256,
        }


def load_run_identity(environment: Mapping[str, str] = os.environ) -> RunIdentity:
    try:
        raw_job_id = environment["KRATOS_JOB_ID"]
        raw_attempt_id = environment["KRATOS_ATTEMPT_ID"]
    except KeyError as error:
        raise RuntimeError(f"required run identity is missing: {error.args[0]}") from error
    try:
        return RunIdentity(job_id=UUID(raw_job_id), attempt_id=UUID(raw_attempt_id))
    except ValueError as error:
        raise RuntimeError("Kratos job and attempt identifiers must be valid UUIDs") from error


class Classifier(nn.Module):
    """A deliberately small multilayer classifier for the bundled dataset."""

    def __init__(self) -> None:
        super().__init__()
        self.layers = nn.Sequential(nn.Linear(4, 16), nn.Tanh(), nn.Linear(16, 3))

    def forward(self, features: Tensor) -> Tensor:
        return cast(Tensor, self.layers(features))


class ActionRegressor(nn.Module):
    """A small bounded model for proving that mounted LeRobot rows reach CUDA training."""

    def __init__(self, feature_count: int, action_count: int) -> None:
        super().__init__()
        self.layers = nn.Sequential(
            nn.Linear(feature_count, 64), nn.Tanh(), nn.Linear(64, action_count)
        )

    def forward(self, features: Tensor) -> Tensor:
        return cast(Tensor, self.layers(features))


def sha256_file(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as stream:
        for block in iter(lambda: stream.read(64 * 1024), b""):
            digest.update(block)
    return digest.hexdigest()


def selection_path(
    alias: str = DATASET_INPUT_ALIAS, input_directory: Path = INPUT_DIRECTORY
) -> Path:
    if re.fullmatch(r"[a-z][a-z0-9_-]{0,31}", alias) is None:
        raise RuntimeError(f"dataset input alias is invalid: {alias!r}")
    return input_directory / SELECTION_DIRECTORY / f"{alias}.json"


def load_dataset_selection(
    alias: str = DATASET_INPUT_ALIAS, input_directory: Path = INPUT_DIRECTORY
) -> DatasetSelection:
    """Load the agent-authored selection and reject ambiguous view semantics."""
    path = selection_path(alias, input_directory)
    try:
        raw = json.loads(path.read_text(encoding="utf-8"))
    except (OSError, json.JSONDecodeError) as error:
        raise RuntimeError(f"dataset selection is unavailable or invalid: {path}") from error
    if not isinstance(raw, dict) or raw.get("schema_version") != "1.0":
        raise RuntimeError("dataset selection schema is not supported")
    if raw.get("alias") != alias:
        raise RuntimeError("dataset selection alias does not match the workload input")
    try:
        dataset_id = UUID(str(raw["dataset_id"]))
        dataset_version_id = UUID(str(raw["dataset_version_id"]))
        version_number = int(raw["version_number"])
        manifest_sha256 = str(raw["manifest_sha256"])
        dataset_name = str(raw["dataset_name"])
        view_id_raw = raw.get("dataset_view_id")
        dataset_view_id = UUID(str(view_id_raw)) if view_id_raw is not None else None
        view_name_raw = raw.get("dataset_view_name")
        dataset_view_name = str(view_name_raw) if view_name_raw is not None else None
        view_hash_raw = raw.get("dataset_view_manifest_sha256")
        dataset_view_manifest_sha256 = str(view_hash_raw) if view_hash_raw is not None else None
        episodes_raw = raw["included_episodes"]
        if not isinstance(episodes_raw, list) or any(
            not isinstance(episode, int) or isinstance(episode, bool) or episode < 0
            for episode in episodes_raw
        ):
            raise ValueError("included_episodes must be non-negative integers")
        included_episodes = tuple(episodes_raw)
        selects_every_episode = raw["selects_every_episode"]
        if not isinstance(selects_every_episode, bool):
            raise ValueError("selects_every_episode must be boolean")
    except (KeyError, TypeError, ValueError) as error:
        raise RuntimeError("dataset selection fields are invalid") from error
    if version_number < 1 or not dataset_name:
        raise RuntimeError("dataset selection identity is invalid")
    if re.fullmatch(r"[0-9a-f]{64}", manifest_sha256) is None:
        raise RuntimeError("dataset manifest digest is invalid")
    if len(set(included_episodes)) != len(included_episodes):
        raise RuntimeError("dataset selection repeats an episode")
    if selects_every_episode:
        if dataset_view_id is not None or included_episodes:
            raise RuntimeError("complete-version selection has contradictory view fields")
    elif (
        dataset_view_id is None
        or not included_episodes
        or dataset_view_manifest_sha256 is None
        or re.fullmatch(r"[0-9a-f]{64}", dataset_view_manifest_sha256) is None
    ):
        raise RuntimeError("curated dataset selection is incomplete")
    return DatasetSelection(
        alias=alias,
        dataset_id=dataset_id,
        dataset_name=dataset_name,
        dataset_version_id=dataset_version_id,
        version_number=version_number,
        manifest_sha256=manifest_sha256,
        dataset_view_id=dataset_view_id,
        dataset_view_name=dataset_view_name,
        dataset_view_manifest_sha256=dataset_view_manifest_sha256,
        included_episodes=included_episodes,
        selects_every_episode=selects_every_episode,
        selection_sha256=sha256_file(path),
    )


def resolve_dataset_selection(
    alias: str = DATASET_INPUT_ALIAS, input_directory: Path = INPUT_DIRECTORY
) -> DatasetSelection | None:
    """Resolve this workload's input without silently ignoring a differently named dataset."""
    expected = selection_path(alias, input_directory)
    if expected.is_file():
        return load_dataset_selection(alias, input_directory)
    available = sorted((input_directory / SELECTION_DIRECTORY).glob("*.json"))
    if available:
        aliases = ", ".join(path.stem for path in available)
        raise RuntimeError(
            f"dataset input alias {alias!r} is required; mounted selection aliases: {aliases}"
        )
    return None


def save_checkpoint(checkpoint: dict[str, Any], path: Path = CHECKPOINT_PATH) -> str:
    """Write the model only to the worker-provided durable-output mount."""
    if not path.parent.is_dir():
        raise RuntimeError(f"durable output directory is unavailable: {path.parent}")
    torch.save(checkpoint, path)
    return sha256_file(path)


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


def _numeric_vector(value: object, column: str) -> list[float]:
    if not isinstance(value, list) or not value:
        raise RuntimeError(f"LeRobot column {column!r} is not a non-empty numeric vector")
    try:
        vector = [float(item) for item in value]
    except (TypeError, ValueError) as error:
        raise RuntimeError(f"LeRobot column {column!r} contains non-numeric data") from error
    if any(not math.isfinite(item) for item in vector):
        raise RuntimeError(f"LeRobot column {column!r} contains a non-finite value")
    return vector


def load_lerobot_rows(
    selection: DatasetSelection,
    input_directory: Path = INPUT_DIRECTORY,
    max_rows: int = LEROBOT_MAX_ROWS,
) -> tuple[Tensor, Tensor, tuple[int, ...]]:
    """Read a bounded, ordered set of state/action rows from mounted LeRobot parquet files."""
    if max_rows < 5:
        raise ValueError("a LeRobot smoke run needs room for at least five rows")
    root = input_directory / selection.alias
    files = sorted((root / "data").glob("**/*.parquet"))
    if not files:
        raise RuntimeError(f"dataset input {selection.alias!r} has no data parquet files")
    selected = None if selection.selects_every_episode else set(selection.included_episodes)
    requested_order = list(selection.included_episodes)
    if selected is not None and len(requested_order) > max_rows:
        raise RuntimeError("curated selection has more episodes than the bounded smoke row limit")
    quotas: dict[int, int] | None = None
    if selected is not None:
        rows_per_episode, extra_rows = divmod(max_rows, len(requested_order))
        quotas = {
            episode: rows_per_episode + int(index < extra_rows)
            for index, episode in enumerate(requested_order)
        }
    by_episode: dict[int, list[tuple[list[float], list[float]]]] = {}
    encountered: list[int] = []
    encountered_set: set[int] = set()
    row_count = 0
    complete = False
    for path in files:
        batches = parquet.ParquetFile(path).iter_batches(
            batch_size=min(max_rows, 1_024),
            columns=["episode_index", "observation.state", "action"],
        )
        for batch in batches:
            columns = batch.to_pydict()
            for raw_episode, raw_state, raw_action in zip(
                columns["episode_index"],
                columns["observation.state"],
                columns["action"],
                strict=True,
            ):
                episode = int(raw_episode)
                if selected is not None and episode not in selected:
                    continue
                if episode not in encountered_set:
                    encountered_set.add(episode)
                    encountered.append(episode)
                episode_rows = by_episode.setdefault(episode, [])
                if quotas is not None and len(episode_rows) >= quotas[episode]:
                    continue
                episode_rows.append(
                    (
                        _numeric_vector(raw_state, "observation.state"),
                        _numeric_vector(raw_action, "action"),
                    )
                )
                row_count += 1
                if selected is None and row_count >= max_rows:
                    complete = True
                    break
            if selected is not None and quotas is not None:
                complete = all(
                    episode in encountered_set
                    and len(by_episode.get(episode, ())) >= quotas[episode]
                    for episode in requested_order
                )
            if complete:
                break
        if complete:
            break
    order = requested_order if selected is not None else encountered
    missing = [episode for episode in order if episode not in encountered_set]
    if missing:
        raise RuntimeError(f"selected LeRobot episodes are absent from parquet data: {missing}")
    rows = [row for episode in order for row in by_episode[episode]]
    if len(rows) < 5:
        raise RuntimeError("selected LeRobot episodes contain fewer than five usable rows")
    feature_count = len(rows[0][0])
    action_count = len(rows[0][1])
    if any(len(state) != feature_count or len(action) != action_count for state, action in rows):
        raise RuntimeError("LeRobot state/action vector widths are inconsistent")
    features = torch.tensor([state for state, _ in rows], dtype=torch.float32)
    actions_tensor = torch.tensor([action for _, action in rows], dtype=torch.float32)
    return features, actions_tensor, tuple(order)


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


def run_lerobot_training(
    run_identity: RunIdentity,
    selection: DatasetSelection,
    input_directory: Path = INPUT_DIRECTORY,
) -> dict[str, Any]:
    """Run a bounded CUDA regression using only the episodes named by the immutable view."""
    configure_determinism()
    device = require_cuda()
    features, actions, episode_order = load_lerobot_rows(selection, input_directory)
    validation_mask = torch.arange(actions.shape[0]) % 5 == 0
    train_features = features[~validation_mask]
    train_actions = actions[~validation_mask]
    validation_features = features[validation_mask]
    validation_actions = actions[validation_mask]
    feature_mean = train_features.mean(dim=0)
    feature_scale = train_features.std(dim=0).clamp_min(1e-6)
    action_mean = train_actions.mean(dim=0)
    action_scale = train_actions.std(dim=0).clamp_min(1e-6)
    train_features = ((train_features - feature_mean) / feature_scale).to(device)
    validation_features = ((validation_features - feature_mean) / feature_scale).to(device)
    train_actions = ((train_actions - action_mean) / action_scale).to(device)
    validation_actions = ((validation_actions - action_mean) / action_scale).to(device)
    model = ActionRegressor(features.shape[1], actions.shape[1]).to(device)
    optimizer = torch.optim.Adam(model.parameters(), lr=0.01)
    loss_function = nn.MSELoss()
    started = perf_counter()
    initial_loss: float | None = None
    model.train()
    for _ in range(120):
        optimizer.zero_grad(set_to_none=True)
        loss = loss_function(model(train_features), train_actions)
        if initial_loss is None:
            initial_loss = float(loss.item())
        loss.backward()
        optimizer.step()
    torch.cuda.synchronize(device)
    duration_ms = (perf_counter() - started) * 1000
    model.eval()
    with torch.no_grad():
        final_train_loss = float(loss_function(model(train_features), train_actions).item())
        validation_loss = float(
            loss_function(model(validation_features), validation_actions).item()
        )
    if initial_loss is None or final_train_loss >= initial_loss:
        raise RuntimeError("LeRobot smoke training did not reduce loss")
    checkpoint = {
        "workload_version": WORKLOAD_VERSION,
        "dataset_selection": selection.result_fields(),
        "episode_order": episode_order,
        "seed": SEED,
        "model_state_dict": model.state_dict(),
        "feature_mean": feature_mean,
        "feature_scale": feature_scale,
        "action_mean": action_mean,
        "action_scale": action_scale,
    }
    checkpoint_hash = save_checkpoint(checkpoint)
    properties = torch.cuda.get_device_properties(device)
    return {
        "schema_version": SCHEMA_VERSION,
        "status": "succeeded",
        "completed_at": datetime.now(UTC).isoformat(),
        "workload": {
            "name": "Kratos LeRobot dataset selection smoke training",
            "version": WORKLOAD_VERSION,
        },
        "run": run_identity.result_fields(),
        "telemetry": {"resource_attributes": run_identity.otel_resource_attributes()},
        "dataset": selection.result_fields()
        | {
            "rows": int(actions.shape[0]),
            "state_width": int(features.shape[1]),
            "action_width": int(actions.shape[1]),
            "episode_order": list(episode_order),
        },
        "configuration": {
            "seed": SEED,
            "epochs": 120,
            "learning_rate": 0.01,
            "max_rows": LEROBOT_MAX_ROWS,
            "deterministic_algorithms": True,
            "cpu_fallback": False,
        },
        "metrics": {
            "initial_train_loss": round(initial_loss, 6),
            "final_train_loss": round(final_train_loss, 6),
            "validation_loss": round(validation_loss, 6),
            "training_duration_ms": round(duration_ms, 3),
        },
        "model": {
            "architecture": (f"Linear({features.shape[1]},64)-Tanh-Linear(64,{actions.shape[1]})"),
            "state_sha256": model_state_sha256(model),
            "checkpoint_sha256": checkpoint_hash,
            "checkpoint_location": str(CHECKPOINT_PATH),
            "checkpoint_staged": True,
        },
        "environment": {
            "python": platform.python_version(),
            "pytorch": torch.__version__,
            "cuda_runtime": torch.version.cuda,
            "gpu_name": properties.name,
            "gpu_compute_capability": f"{properties.major}.{properties.minor}",
        },
    }


def run_training() -> dict[str, Any]:
    run_identity = load_run_identity()
    selection = resolve_dataset_selection()
    if selection is not None:
        return run_lerobot_training(run_identity, selection)
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
    checkpoint_hash = save_checkpoint(checkpoint)
    properties = torch.cuda.get_device_properties(device)

    return {
        "schema_version": SCHEMA_VERSION,
        "status": "succeeded",
        "completed_at": datetime.now(UTC).isoformat(),
        "workload": {
            "name": "Kratos CUDA classification example",
            "version": WORKLOAD_VERSION,
        },
        "run": run_identity.result_fields(),
        "telemetry": {"resource_attributes": run_identity.otel_resource_attributes()},
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
            "checkpoint_staged": True,
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


def failure_result(error: Exception, environment: Mapping[str, str] = os.environ) -> dict[str, Any]:
    payload: dict[str, Any] = {
        "schema_version": SCHEMA_VERSION,
        "status": "failed",
        "workload_version": WORKLOAD_VERSION,
        "error_type": type(error).__name__,
        "detail": str(error)[:512],
        "cpu_fallback": False,
    }
    try:
        run_identity = load_run_identity(environment)
    except RuntimeError:
        return payload
    payload["run"] = run_identity.result_fields()
    payload["telemetry"] = {"resource_attributes": run_identity.otel_resource_attributes()}
    return payload


def main() -> int:
    try:
        result = run_training()
    except Exception as error:  # noqa: BLE001 - process boundary returns a structured failure
        print(encode_result(failure_result(error)))
        return 1
    print(encode_result(result))
    return 0


if __name__ == "__main__":
    sys.exit(main())
