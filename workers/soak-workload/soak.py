"""Run Kratos' fixed-duration, provenance-reporting CUDA soak workload."""

from __future__ import annotations

import json
import math
import os
import platform
import random
import signal
import subprocess
import sys
from collections.abc import Callable, Mapping
from dataclasses import dataclass, field
from datetime import UTC, datetime
from time import perf_counter
from types import FrameType
from typing import Any, Protocol, cast
from uuid import UUID

import torch
from torch import Tensor, nn

SCHEMA_VERSION = "1.0"
WORKLOAD_VERSION = "kratos-soak-workload-v1"
SEED = 20260920
DURATION_VARIABLE = "KRATOS_SOAK_SECONDS"
DEFAULT_DURATION_SECONDS = 600
MIN_DURATION_SECONDS = 10
MAX_DURATION_SECONDS = 3300
MAX_PROGRESS_RECORDS = 20
STEPS_PER_SYNCHRONIZE = 10
BATCH_SIZE = 4096
FEATURES = 256
HIDDEN = 2048
CLASSES = 16
LABEL_NOISE = 8.0
LEARNING_RATE = 0.001
THROUGHPUT_UNIT = "steps/s"
# The worker keeps only the first 64 KiB of standard output. Every record this workload prints is
# counted against a much smaller total, and part of that total is reserved for the final result so
# progress records can never crowd it out.
OUTPUT_LIMIT_BYTES = 16 * 1024
RESULT_RESERVE_BYTES = 4 * 1024
PROGRESS_RECORD_LIMIT_BYTES = 512
EXIT_INTERRUPTED = 128 + signal.SIGTERM
# The agent allows a short grace between SIGTERM and SIGKILL, so no single start-up phase may take
# anything like that long; the driver probe is a subprocess and is bounded tightly.
DRIVER_PROBE_TIMEOUT_SECONDS = 3


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


@dataclass(frozen=True)
class SoakDuration:
    seconds: int
    source: str

    @property
    def progress_interval_seconds(self) -> float:
        """Space progress evenly so no duration can produce more than the permitted records."""
        return self.seconds / MAX_PROGRESS_RECORDS


def load_duration(environment: Mapping[str, str] = os.environ) -> SoakDuration:
    """Resolve the soak duration; an invalid override fails rather than silently defaulting."""
    raw = environment.get(DURATION_VARIABLE)
    if raw is None:
        return SoakDuration(seconds=DEFAULT_DURATION_SECONDS, source="image-default")
    if not (raw.isascii() and raw.isdigit()):
        raise RuntimeError(
            f"{DURATION_VARIABLE} must be a whole number of seconds between "
            f"{MIN_DURATION_SECONDS} and {MAX_DURATION_SECONDS}"
        )
    seconds = int(raw)
    if not MIN_DURATION_SECONDS <= seconds <= MAX_DURATION_SECONDS:
        raise RuntimeError(
            f"{DURATION_VARIABLE} is {seconds}; it must be between "
            f"{MIN_DURATION_SECONDS} and {MAX_DURATION_SECONDS}"
        )
    return SoakDuration(seconds=seconds, source=DURATION_VARIABLE)


def write_line(line: str) -> None:
    # Flush every record so a witness following the container log sees progress as it happens.
    print(line, flush=True)


class OutputBudget:
    """Bound everything the workload writes to standard output."""

    def __init__(
        self,
        writer: Callable[[str], None] = write_line,
        limit_bytes: int = OUTPUT_LIMIT_BYTES,
        result_reserve_bytes: int = RESULT_RESERVE_BYTES,
    ) -> None:
        self._writer = writer
        self._limit_bytes = limit_bytes
        self._result_reserve_bytes = result_reserve_bytes
        self.bytes_written = 0
        self.progress_records_emitted = 0
        self.progress_records_suppressed = 0

    def emit_progress(self, payload: dict[str, Any]) -> bool:
        """Print a progress record, or suppress it when it would consume the result's reserve."""
        encoded = encode_record(payload)
        size = len(encoded.encode("utf-8")) + 1
        available = self._limit_bytes - self._result_reserve_bytes - self.bytes_written
        if size > PROGRESS_RECORD_LIMIT_BYTES or size > available:
            self.progress_records_suppressed += 1
            return False
        self._writer(encoded)
        self.bytes_written += size
        self.progress_records_emitted += 1
        return True

    def emit_result(self, payload: dict[str, Any]) -> None:
        encoded = encode_record(payload)
        size = len(encoded.encode("utf-8")) + 1
        if size > self._limit_bytes - self.bytes_written:
            raise RuntimeError(
                f"structured result would exceed the {self._limit_bytes} byte output budget"
            )
        self._writer(encoded)
        self.bytes_written += size


def encode_record(payload: dict[str, Any]) -> str:
    return json.dumps(payload, separators=(",", ":"), sort_keys=True)


class StopRequested(Exception):
    """Abandon start-up because termination was requested before the soak loop began."""


class StopRequest:
    """Remember a termination signal so the soak loop can finish its record and exit."""

    def __init__(self) -> None:
        self.signal_name: str | None = None

    @property
    def requested(self) -> bool:
        return self.signal_name is not None

    def install(self) -> None:
        # The workload is PID 1 in its container, where SIGTERM is ignored unless handled.
        signal.signal(signal.SIGTERM, self._handle)

    def check(self) -> None:
        """Give up at a start-up phase boundary when a stop has already been requested."""
        if self.requested:
            raise StopRequested

    def _handle(self, signal_number: int, _frame: FrameType | None) -> None:
        self.signal_name = signal.Signals(signal_number).name


class Workload(Protocol):
    def step(self) -> None: ...

    def synchronize(self) -> None: ...

    def loss(self) -> float: ...


class Classifier(nn.Module):
    """A modest multilayer classifier; large enough to keep a GPU busy, small in memory."""

    def __init__(self) -> None:
        super().__init__()
        self.layers = nn.Sequential(
            nn.Linear(FEATURES, HIDDEN),
            nn.Tanh(),
            nn.Linear(HIDDEN, HIDDEN),
            nn.Tanh(),
            nn.Linear(HIDDEN, CLASSES),
        )

    def forward(self, features: Tensor) -> Tensor:
        return cast(Tensor, self.layers(features))


class CudaTrainingWorkload:
    """Train continuously on a synthetic stream that is generated on, and never leaves, the GPU.

    Every step draws a fresh batch and labels it with a fixed, noisy teacher, so the classifier can
    never simply memorise its data and the loss stays meaningful for the whole soak.
    """

    def __init__(self, device: torch.device, stop: StopRequest) -> None:
        self._device = device
        self._generator = torch.Generator(device=device)
        self._generator.manual_seed(SEED)
        # Allocating the teacher and moving the model are separate multi-second phases on a cold
        # device, so a stop request is honoured between them rather than at the first training step.
        self._teacher = self._randn(FEATURES, CLASSES)
        if not self._teacher.is_cuda:
            raise RuntimeError("soak data was not placed on CUDA; CPU fallback is disabled")
        stop.check()
        self._model = Classifier().to(device)
        if not all(parameter.is_cuda for parameter in self._model.parameters()):
            raise RuntimeError("model was not placed on CUDA; CPU fallback is disabled")
        stop.check()
        self._optimizer = torch.optim.Adam(self._model.parameters(), lr=LEARNING_RATE)
        self._loss_function = nn.CrossEntropyLoss()
        self._last_loss: Tensor | None = None
        self._model.train()

    def _randn(self, *shape: int) -> Tensor:
        return torch.randn(*shape, device=self._device, generator=self._generator)

    def step(self) -> None:
        features = self._randn(BATCH_SIZE, FEATURES)
        noise = LABEL_NOISE * self._randn(BATCH_SIZE, CLASSES)
        labels = (features @ self._teacher + noise).argmax(dim=1)
        self._optimizer.zero_grad(set_to_none=True)
        loss = self._loss_function(self._model(features), labels)
        loss.backward()
        self._optimizer.step()
        self._last_loss = loss.detach()

    def synchronize(self) -> None:
        torch.cuda.synchronize(self._device)

    def loss(self) -> float:
        if self._last_loss is None:
            raise RuntimeError("no training step has completed")
        value = float(self._last_loss.item())
        if not math.isfinite(value):
            raise RuntimeError("training loss is not finite")
        return value


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


def query_driver_version() -> str | None:
    """Ask the NVIDIA driver for its version; report nothing rather than guess when it fails.

    The probe is a subprocess that cannot be interrupted by the workload's own signal handler, so a
    tight timeout keeps a wedged `nvidia-smi` from holding up a stop request.
    """
    try:
        completed = subprocess.run(
            ["nvidia-smi", "--query-gpu=driver_version", "--format=csv,noheader"],
            capture_output=True,
            check=True,
            text=True,
            timeout=DRIVER_PROBE_TIMEOUT_SECONDS,
        )
    except (OSError, subprocess.SubprocessError):
        return None
    lines = completed.stdout.strip().splitlines()
    return lines[0].strip()[:64] if lines and lines[0].strip() else None


def throughput(steps_completed: int, elapsed_seconds: float) -> dict[str, Any]:
    """Mean throughput since the soak began, always carrying its unit of measurement."""
    value = steps_completed / elapsed_seconds if elapsed_seconds > 0 else 0.0
    return {"value": round(value, 3), "unit": THROUGHPUT_UNIT}


def progress_record(
    run_identity: RunIdentity,
    duration: SoakDuration,
    steps_completed: int,
    elapsed_seconds: float,
    loss: float,
) -> dict[str, Any]:
    return {
        "record": "progress",
        "schema_version": SCHEMA_VERSION,
        "workload_version": WORKLOAD_VERSION,
        "run": run_identity.result_fields(),
        "configured_duration_seconds": duration.seconds,
        "elapsed_seconds": round(elapsed_seconds, 3),
        "steps_completed": steps_completed,
        "throughput": throughput(steps_completed, elapsed_seconds),
        "train_loss": round(loss, 6),
    }


@dataclass(frozen=True)
class SoakOutcome:
    status: str
    signal_name: str | None
    steps_completed: int
    elapsed_seconds: float
    initial_loss: float | None
    final_loss: float | None


def soak_loop(
    workload: Workload,
    run_identity: RunIdentity,
    duration: SoakDuration,
    stop: StopRequest,
    budget: OutputBudget,
    clock: Callable[[], float] = perf_counter,
) -> SoakOutcome:
    """Do GPU work until the configured wall-clock duration has elapsed or a stop is requested."""
    interval = duration.progress_interval_seconds
    next_progress = 1
    steps_completed = 0
    initial_loss: float | None = None
    started = clock()
    while not stop.requested and clock() - started < duration.seconds:
        workload.step()
        steps_completed += 1
        if initial_loss is None:
            initial_loss = workload.loss()
        if steps_completed % STEPS_PER_SYNCHRONIZE != 0:
            continue
        # Elapsed time is only trusted after the GPU has actually finished the queued steps.
        workload.synchronize()
        elapsed = clock() - started
        if next_progress * interval <= elapsed < duration.seconds:
            budget.emit_progress(
                progress_record(run_identity, duration, steps_completed, elapsed, workload.loss())
            )
            next_progress = math.floor(elapsed / interval) + 1
    workload.synchronize()
    elapsed = clock() - started
    return SoakOutcome(
        status="interrupted" if stop.requested else "succeeded",
        signal_name=stop.signal_name,
        steps_completed=steps_completed,
        elapsed_seconds=elapsed,
        initial_loss=initial_loss,
        final_loss=workload.loss() if steps_completed else None,
    )


def result_record(
    run_identity: RunIdentity,
    duration: SoakDuration,
    outcome: SoakOutcome,
    budget: OutputBudget,
    started_at: datetime,
    finished_at: datetime,
    environment: dict[str, Any],
    peak_gpu_memory_bytes: int | None,
    deterministic_algorithms: bool = True,
) -> dict[str, Any]:
    payload: dict[str, Any] = {
        "record": "result",
        "schema_version": SCHEMA_VERSION,
        "status": outcome.status,
        "workload_name": "Kratos CUDA soak workload",
        "workload_version": WORKLOAD_VERSION,
        "started_at": started_at.isoformat(),
        "finished_at": finished_at.isoformat(),
        "run": run_identity.result_fields(),
        "telemetry": {"resource_attributes": run_identity.otel_resource_attributes()},
        "configuration": {
            "seed": SEED,
            "configured_duration_seconds": duration.seconds,
            "duration_source": duration.source,
            "progress_interval_seconds": duration.progress_interval_seconds,
            "batch_size": BATCH_SIZE,
            "label_noise": LABEL_NOISE,
            "learning_rate": LEARNING_RATE,
            "architecture": (
                f"Linear({FEATURES},{HIDDEN})-Tanh-Linear({HIDDEN},{HIDDEN})-Tanh-"
                f"Linear({HIDDEN},{CLASSES})"
            ),
            "deterministic_algorithms": deterministic_algorithms,
            "cublas_workspace_config": os.environ.get("CUBLAS_WORKSPACE_CONFIG"),
            "cpu_fallback": False,
        },
        "metrics": {
            "actual_duration_seconds": round(outcome.elapsed_seconds, 3),
            "steps_completed": outcome.steps_completed,
            "throughput": throughput(outcome.steps_completed, outcome.elapsed_seconds),
            "initial_train_loss": _rounded(outcome.initial_loss),
            "final_train_loss": _rounded(outcome.final_loss),
            "peak_gpu_memory_bytes": peak_gpu_memory_bytes,
        },
        "output": {
            "limit_bytes": OUTPUT_LIMIT_BYTES,
            "bytes_before_result": budget.bytes_written,
            "progress_records_emitted": budget.progress_records_emitted,
            "progress_records_suppressed": budget.progress_records_suppressed,
        },
        "environment": environment,
        "cpu_fallback": False,
        "repeatability_note": (
            "The seed and deterministic settings are recorded, but the loop is bounded by "
            "wall-clock time: the number of steps, and therefore the final loss and model state, "
            "vary with hardware and load and are not claimed to be reproducible."
        ),
    }
    if outcome.signal_name is not None:
        payload["signal"] = outcome.signal_name
    return payload


def _rounded(value: float | None) -> float | None:
    return None if value is None else round(value, 6)


def describe_runtime() -> dict[str, Any]:
    """Describe what is known before any device is touched; none of this can block."""
    return {
        "python": platform.python_version(),
        "platform": platform.platform()[:128],
        "pytorch": torch.__version__,
        "cuda_runtime": torch.version.cuda,
    }


def describe_gpu(device: torch.device) -> dict[str, Any]:
    properties = torch.cuda.get_device_properties(device)
    return {
        "gpu_name": properties.name,
        "gpu_compute_capability": f"{properties.major}.{properties.minor}",
    }


@dataclass
class SetupFacts:
    """What start-up has established so far.

    A stop during start-up reports these and nothing else: whatever had not been observed by then
    is absent from the result rather than guessed at.
    """

    environment: dict[str, Any] = field(default_factory=dict)
    deterministic_algorithms: bool = False
    device: torch.device | None = None

    def peak_gpu_memory_bytes(self) -> int | None:
        """Report peak device memory, or nothing when no device was ever selected."""
        if self.device is None:
            return None
        return int(torch.cuda.max_memory_allocated(self.device))


def prepare_workload(stop: StopRequest, facts: SetupFacts) -> Workload:
    """Bring the GPU up one phase at a time, giving up at the first phase boundary after a stop.

    The driver probe, CUDA initialisation and the first device allocation each take seconds and
    none of them can be abandoned part way through, so the termination flag is checked between
    every phase instead of only once the soak loop starts.
    """
    facts.environment.update(describe_runtime())
    stop.check()
    # The driver is queried before the clock starts so a stop request is never delayed by it.
    facts.environment["nvidia_driver"] = query_driver_version()
    stop.check()
    configure_determinism()
    facts.deterministic_algorithms = True
    stop.check()
    facts.device = require_cuda()
    facts.environment.update(describe_gpu(facts.device))
    stop.check()
    return CudaTrainingWorkload(facts.device, stop)


def interrupted_during_setup(stop: StopRequest) -> SoakOutcome:
    """Describe a run stopped before the loop began: no step ran, so no measurement is claimed."""
    return SoakOutcome(
        status="interrupted",
        signal_name=stop.signal_name,
        steps_completed=0,
        elapsed_seconds=0.0,
        initial_loss=None,
        final_loss=None,
    )


def run_soak(budget: OutputBudget, stop: StopRequest) -> dict[str, Any]:
    run_identity = load_run_identity()
    duration = load_duration()
    facts = SetupFacts()
    setup_started_at = datetime.now(UTC)
    try:
        workload = prepare_workload(stop, facts)
    except StopRequested:
        # Start-up never reached the loop, so the run began when start-up did.
        outcome = interrupted_during_setup(stop)
        started_at, finished_at = setup_started_at, datetime.now(UTC)
    else:
        started_at = datetime.now(UTC)
        outcome = soak_loop(workload, run_identity, duration, stop, budget)
        finished_at = datetime.now(UTC)
    return result_record(
        run_identity,
        duration,
        outcome,
        budget,
        started_at,
        finished_at,
        facts.environment,
        peak_gpu_memory_bytes=facts.peak_gpu_memory_bytes(),
        deterministic_algorithms=facts.deterministic_algorithms,
    )


def failure_result(error: Exception, environment: Mapping[str, str] = os.environ) -> dict[str, Any]:
    payload: dict[str, Any] = {
        "record": "result",
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
    budget = OutputBudget()
    stop = StopRequest()
    stop.install()
    try:
        result = run_soak(budget, stop)
        budget.emit_result(result)
    except Exception as error:  # noqa: BLE001 - process boundary returns a structured failure
        budget.emit_result(failure_result(error))
        return 1
    return EXIT_INTERRUPTED if result["status"] == "interrupted" else 0


if __name__ == "__main__":
    sys.exit(main())
