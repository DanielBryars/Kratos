from __future__ import annotations

import json
import platform
import signal
import subprocess
from collections.abc import Iterator
from datetime import UTC, datetime
from typing import Any

import pytest
import torch

import soak

ENVIRONMENT = {
    "KRATOS_JOB_ID": "22222222-2222-4222-8222-222222222222",
    "KRATOS_ATTEMPT_ID": "11111111-1111-4111-8111-111111111111",
}
RUN = {
    "job_id": "22222222-2222-4222-8222-222222222222",
    "attempt_id": "11111111-1111-4111-8111-111111111111",
}


class FakeWorkload:
    """Stands in for the GPU: every step advances a simulated clock by a fixed amount."""

    def __init__(self, seconds_per_step: float, interrupt_at_step: int | None = None) -> None:
        self.seconds_per_step = seconds_per_step
        self.interrupt_at_step = interrupt_at_step
        self.steps = 0
        self.synchronizations = 0

    def clock(self) -> float:
        return self.steps * self.seconds_per_step

    def step(self) -> None:
        self.steps += 1
        if self.steps == self.interrupt_at_step:
            signal.raise_signal(signal.SIGTERM)

    def synchronize(self) -> None:
        self.synchronizations += 1

    def loss(self) -> float:
        return 1.0 / self.steps


SETUP_PHASES = ("duration", "driver", "determinism", "device", "gpu", "workload")


class FakeSetup:
    """Stands in for every start-up phase so start-up can be driven without a GPU.

    The named phase raises SIGTERM as it runs, standing in for the agent withdrawing execution
    authority while that phase blocks; the phase still completes, as a real one would, so the stop
    is honoured at the checkpoint that follows it.
    """

    def __init__(self, interrupt_during: str | None = None) -> None:
        self.interrupt_during = interrupt_during
        self.phases_run: list[str] = []

    def install(self, monkeypatch: pytest.MonkeyPatch) -> None:
        duration = soak.SoakDuration(seconds=soak.DEFAULT_DURATION_SECONDS, source="image-default")
        gpu = {"gpu_name": "test double", "gpu_compute_capability": "12.0"}
        monkeypatch.setattr(soak, "load_duration", lambda: self.phase("duration", duration))
        monkeypatch.setattr(soak, "query_driver_version", lambda: self.phase("driver", "580.65.06"))
        monkeypatch.setattr(soak, "configure_determinism", lambda: self.phase("determinism", None))
        monkeypatch.setattr(soak, "require_cuda", lambda: self.phase("device", torch.device("cpu")))
        monkeypatch.setattr(soak, "describe_gpu", lambda _device: self.phase("gpu", gpu))
        monkeypatch.setattr(soak, "CudaTrainingWorkload", self.build_workload)
        monkeypatch.setattr(torch.cuda, "max_memory_allocated", lambda _device: 4096)

    def phase[T](self, name: str, value: T) -> T:
        self.phases_run.append(name)
        if name == self.interrupt_during:
            signal.raise_signal(signal.SIGTERM)
        return value

    def build_workload(self, _device: torch.device, stop: soak.StopRequest) -> soak.Workload:
        self.phase("workload", None)
        stop.check()
        return FakeWorkload(seconds_per_step=0.0625)

    def reached(self, phase: str) -> bool:
        return phase in self.phases_run


class Lines:
    def __init__(self) -> None:
        self.lines: list[str] = []

    def __call__(self, line: str) -> None:
        self.lines.append(line)

    def records(self) -> list[dict[str, Any]]:
        return [json.loads(line) for line in self.lines]

    def total_bytes(self) -> int:
        return sum(len(line.encode()) + 1 for line in self.lines)


@pytest.fixture
def restore_sigterm() -> Iterator[None]:
    previous = signal.getsignal(signal.SIGTERM)
    yield
    signal.signal(signal.SIGTERM, previous)


def result_for(outcome: soak.SoakOutcome, budget: soak.OutputBudget) -> dict[str, Any]:
    return soak.result_record(
        soak.load_run_identity(ENVIRONMENT),
        soak.load_duration({}),
        outcome,
        budget,
        started_at=datetime(2026, 9, 20, 12, 0, 0, tzinfo=UTC),
        finished_at=datetime(2026, 9, 20, 12, 10, 0, tzinfo=UTC),
        environment={"gpu_name": "test double", "nvidia_driver": None},
        peak_gpu_memory_bytes=0,
    )


def test_cuda_is_required_without_cpu_fallback(monkeypatch: pytest.MonkeyPatch) -> None:
    monkeypatch.setattr(torch.cuda, "is_available", lambda: False)
    with pytest.raises(RuntimeError, match="CPU fallback is disabled"):
        soak.require_cuda()


def test_missing_cuda_is_a_structured_failure(
    monkeypatch: pytest.MonkeyPatch,
    capsys: pytest.CaptureFixture[str],
    restore_sigterm: None,
) -> None:
    monkeypatch.setattr(torch.cuda, "is_available", lambda: False)
    monkeypatch.setattr(torch, "use_deterministic_algorithms", lambda _enabled: None)
    for name, value in ENVIRONMENT.items():
        monkeypatch.setenv(name, value)
    monkeypatch.delenv(soak.DURATION_VARIABLE, raising=False)

    assert soak.main() == 1

    lines = capsys.readouterr().out.splitlines()
    assert len(lines) == 1
    failure = json.loads(lines[0])
    assert failure["record"] == "result"
    assert failure["status"] == "failed"
    assert failure["cpu_fallback"] is False
    assert failure["workload_version"] == "kratos-soak-workload-v1"
    assert failure["run"] == RUN
    assert "CPU fallback is disabled" in failure["detail"]


def test_run_identity_is_validated_and_exposes_correlation_attributes() -> None:
    identity = soak.load_run_identity(ENVIRONMENT)

    assert identity.result_fields() == RUN
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
        soak.load_run_identity(environment)


def test_failure_result_preserves_valid_run_identity() -> None:
    failure = soak.failure_result(RuntimeError("soak failed"), ENVIRONMENT)

    assert failure["run"] == RUN
    assert failure["telemetry"]["resource_attributes"]["kratos.attempt.id"] == (
        "11111111-1111-4111-8111-111111111111"
    )


def test_duration_defaults_to_the_value_baked_into_the_image() -> None:
    duration = soak.load_duration({})
    assert duration == soak.SoakDuration(seconds=600, source="image-default")
    assert duration.progress_interval_seconds == 30


@pytest.mark.parametrize("raw, seconds", [("10", 10), ("30", 30), ("3300", 3300)])
def test_duration_override_accepts_the_inclusive_bounds(raw: str, seconds: int) -> None:
    duration = soak.load_duration({soak.DURATION_VARIABLE: raw})
    assert duration == soak.SoakDuration(seconds=seconds, source="KRATOS_SOAK_SECONDS")


@pytest.mark.parametrize("raw", ["", "9", "3301", "0", "-30", "+30", "30.0", "3_0", " 30", "ten"])
def test_invalid_duration_override_is_rejected_rather_than_defaulted(raw: str) -> None:
    with pytest.raises(RuntimeError, match=soak.DURATION_VARIABLE):
        soak.load_duration({soak.DURATION_VARIABLE: raw})


def test_invalid_duration_is_a_structured_failure(
    monkeypatch: pytest.MonkeyPatch,
    capsys: pytest.CaptureFixture[str],
    restore_sigterm: None,
) -> None:
    for name, value in ENVIRONMENT.items():
        monkeypatch.setenv(name, value)
    monkeypatch.setenv(soak.DURATION_VARIABLE, "3600")

    assert soak.main() == 1

    failure = json.loads(capsys.readouterr().out)
    assert failure["status"] == "failed"
    assert failure["run"] == RUN
    assert "3600" in failure["detail"]


def test_full_soak_emits_sparse_progress_with_throughput_units() -> None:
    lines = Lines()
    budget = soak.OutputBudget(writer=lines)
    workload = FakeWorkload(seconds_per_step=0.0625)

    outcome = soak.soak_loop(
        workload,
        soak.load_run_identity(ENVIRONMENT),
        soak.load_duration({}),
        soak.StopRequest(),
        budget,
        clock=workload.clock,
    )

    assert outcome.status == "succeeded"
    assert outcome.signal_name is None
    assert outcome.steps_completed == 9600
    assert outcome.elapsed_seconds == pytest.approx(600)
    assert workload.synchronizations >= outcome.steps_completed // soak.STEPS_PER_SYNCHRONIZE
    progress = lines.records()
    assert len(progress) == soak.MAX_PROGRESS_RECORDS - 1
    assert [record["elapsed_seconds"] for record in progress] == [
        pytest.approx(30 * index) for index in range(1, soak.MAX_PROGRESS_RECORDS)
    ]
    for record in progress:
        assert record["record"] == "progress"
        assert record["schema_version"] == soak.SCHEMA_VERSION
        assert record["workload_version"] == "kratos-soak-workload-v1"
        assert record["run"] == RUN
        assert record["configured_duration_seconds"] == 600
        assert record["steps_completed"] == round(record["elapsed_seconds"] / 0.0625)
        assert record["throughput"] == {"value": pytest.approx(16), "unit": "steps/s"}
    assert all(len(line.encode()) + 1 <= soak.PROGRESS_RECORD_LIMIT_BYTES for line in lines.lines)

    budget.emit_result(result_for(outcome, budget))
    assert lines.total_bytes() == budget.bytes_written
    assert budget.bytes_written <= soak.OUTPUT_LIMIT_BYTES


def test_longest_permitted_soak_stays_within_the_output_budget() -> None:
    lines = Lines()
    budget = soak.OutputBudget(writer=lines)
    workload = FakeWorkload(seconds_per_step=0.5)
    duration = soak.load_duration({soak.DURATION_VARIABLE: str(soak.MAX_DURATION_SECONDS)})

    outcome = soak.soak_loop(
        workload,
        soak.load_run_identity(ENVIRONMENT),
        duration,
        soak.StopRequest(),
        budget,
        clock=workload.clock,
    )
    budget.emit_result(result_for(outcome, budget))

    assert budget.progress_records_emitted == soak.MAX_PROGRESS_RECORDS - 1
    assert budget.progress_records_suppressed == 0
    assert lines.total_bytes() <= soak.OUTPUT_LIMIT_BYTES


def test_a_slow_gpu_does_not_replay_missed_progress_records() -> None:
    lines = Lines()
    workload = FakeWorkload(seconds_per_step=7)

    soak.soak_loop(
        workload,
        soak.load_run_identity(ENVIRONMENT),
        soak.load_duration({}),
        soak.StopRequest(),
        soak.OutputBudget(writer=lines),
        clock=workload.clock,
    )

    # Synchronisation happens every 70 simulated seconds, so several 30 second marks are skipped.
    assert [record["elapsed_seconds"] for record in lines.records()] == [
        pytest.approx(70 * index) for index in range(1, 9)
    ]


def test_sigterm_stops_promptly_and_reports_an_interrupted_result(restore_sigterm: None) -> None:
    lines = Lines()
    budget = soak.OutputBudget(writer=lines)
    stop = soak.StopRequest()
    stop.install()
    workload = FakeWorkload(seconds_per_step=0.0625, interrupt_at_step=1234)

    outcome = soak.soak_loop(
        workload,
        soak.load_run_identity(ENVIRONMENT),
        soak.load_duration({}),
        stop,
        budget,
        clock=workload.clock,
    )

    assert outcome.status == "interrupted"
    assert outcome.signal_name == "SIGTERM"
    assert outcome.steps_completed == 1234
    assert outcome.elapsed_seconds == pytest.approx(77.125)
    result = result_for(outcome, budget)
    assert result["status"] == "interrupted"
    assert result["signal"] == "SIGTERM"
    assert result["metrics"]["steps_completed"] == 1234
    assert result["output"]["progress_records_emitted"] == 2
    assert soak.EXIT_INTERRUPTED == 143


def test_a_stop_requested_before_the_loop_completes_no_steps() -> None:
    stop = soak.StopRequest()
    stop.signal_name = "SIGTERM"
    workload = FakeWorkload(seconds_per_step=0.0625)

    outcome = soak.soak_loop(
        workload,
        soak.load_run_identity(ENVIRONMENT),
        soak.load_duration({}),
        stop,
        soak.OutputBudget(writer=Lines()),
        clock=workload.clock,
    )

    assert outcome.status == "interrupted"
    assert outcome.steps_completed == 0
    assert outcome.initial_loss is None
    assert outcome.final_loss is None
    assert soak.throughput(0, 0.0) == {"value": 0.0, "unit": "steps/s"}


@pytest.mark.parametrize("interrupted_phase", SETUP_PHASES)
def test_sigterm_during_start_up_still_reports_a_single_interrupted_result(
    interrupted_phase: str,
    monkeypatch: pytest.MonkeyPatch,
    capsys: pytest.CaptureFixture[str],
    restore_sigterm: None,
) -> None:
    setup = FakeSetup(interrupt_during=interrupted_phase)
    setup.install(monkeypatch)
    for name, value in ENVIRONMENT.items():
        monkeypatch.setenv(name, value)

    assert soak.main() == soak.EXIT_INTERRUPTED

    expected = SETUP_PHASES[: SETUP_PHASES.index(interrupted_phase) + 1]
    if interrupted_phase == "device":
        # Describing the GPU costs nothing once it is selected, so it shares that checkpoint.
        expected += ("gpu",)
    assert setup.phases_run == list(expected)
    lines = capsys.readouterr().out.splitlines()
    assert len(lines) == 1
    result = json.loads(lines[0])
    assert result["record"] == "result"
    assert result["status"] == "interrupted"
    assert result["signal"] == "SIGTERM"
    assert result["run"] == RUN
    assert result["cpu_fallback"] is False
    assert result["configuration"]["configured_duration_seconds"] == 600
    assert result["metrics"]["steps_completed"] == 0
    assert result["metrics"]["actual_duration_seconds"] == 0.0
    assert result["metrics"]["throughput"] == {"value": 0.0, "unit": "steps/s"}
    # Nothing start-up had not yet observed may appear in the record.
    assert result["metrics"]["initial_train_loss"] is None
    assert result["metrics"]["final_train_loss"] is None
    assert result["environment"]["python"] == platform.python_version()
    assert ("nvidia_driver" in result["environment"]) is setup.reached("driver")
    assert ("gpu_name" in result["environment"]) is setup.reached("gpu")
    assert result["configuration"]["deterministic_algorithms"] is setup.reached("determinism")
    assert (result["metrics"]["peak_gpu_memory_bytes"] is None) is not setup.reached("device")


def test_a_start_up_phase_is_only_abandoned_once_a_stop_has_been_requested() -> None:
    stop = soak.StopRequest()

    stop.check()

    stop.signal_name = "SIGTERM"
    with pytest.raises(soak.StopRequested):
        stop.check()


def test_a_hanging_driver_probe_neither_blocks_nor_fails_the_run(
    monkeypatch: pytest.MonkeyPatch,
    capsys: pytest.CaptureFixture[str],
    restore_sigterm: None,
) -> None:
    probe = soak.query_driver_version
    recorded: dict[str, Any] = {}

    def wedged(*_args: object, **kwargs: Any) -> None:
        recorded.update(kwargs)
        raise subprocess.TimeoutExpired(cmd="nvidia-smi", timeout=kwargs["timeout"])

    setup = FakeSetup(interrupt_during="device")
    setup.install(monkeypatch)
    monkeypatch.setattr(soak, "query_driver_version", probe)
    monkeypatch.setattr(subprocess, "run", wedged)
    for name, value in ENVIRONMENT.items():
        monkeypatch.setenv(name, value)

    assert soak.main() == soak.EXIT_INTERRUPTED

    assert recorded["timeout"] == soak.DRIVER_PROBE_TIMEOUT_SECONDS
    assert soak.DRIVER_PROBE_TIMEOUT_SECONDS <= 5
    result = json.loads(capsys.readouterr().out)
    assert result["status"] == "interrupted"
    assert result["environment"]["nvidia_driver"] is None


def test_result_record_reports_provenance_without_claiming_determinism() -> None:
    budget = soak.OutputBudget(writer=Lines())
    outcome = soak.SoakOutcome(
        status="succeeded",
        signal_name=None,
        steps_completed=12000,
        elapsed_seconds=600.004,
        initial_loss=2.772589,
        final_loss=0.123456,
    )

    result = result_for(outcome, budget)

    assert result["record"] == "result"
    assert result["schema_version"] == "1.0"
    assert result["status"] == "succeeded"
    assert result["workload_version"] == "kratos-soak-workload-v1"
    assert result["run"] == RUN
    assert result["telemetry"]["resource_attributes"]["kratos.job.id"] == RUN["job_id"]
    assert result["started_at"] == "2026-09-20T12:00:00+00:00"
    assert result["finished_at"] == "2026-09-20T12:10:00+00:00"
    assert result["configuration"]["seed"] == soak.SEED
    assert result["configuration"]["configured_duration_seconds"] == 600
    assert result["configuration"]["duration_source"] == "image-default"
    assert result["configuration"]["cpu_fallback"] is False
    assert result["cpu_fallback"] is False
    assert result["metrics"]["actual_duration_seconds"] == 600.004
    assert result["metrics"]["steps_completed"] == 12000
    assert result["metrics"]["throughput"] == {
        "value": pytest.approx(20, abs=0.01),
        "unit": "steps/s",
    }
    assert result["environment"] == {"gpu_name": "test double", "nvidia_driver": None}
    assert "not claimed to be reproducible" in result["repeatability_note"]
    assert "signal" not in result


def test_records_are_compact_and_stable() -> None:
    encoded = soak.encode_record({"status": "succeeded", "schema_version": "1.0"})
    assert encoded == '{"schema_version":"1.0","status":"succeeded"}'


def test_progress_cannot_consume_the_space_reserved_for_the_result() -> None:
    lines = Lines()
    budget = soak.OutputBudget(writer=lines, limit_bytes=1024, result_reserve_bytes=512)
    record = {"record": "progress", "padding": "x" * 200}

    assert budget.emit_progress(record) is True
    assert budget.emit_progress(record) is True
    assert budget.emit_progress(record) is False
    assert budget.progress_records_emitted == 2
    assert budget.progress_records_suppressed == 1
    assert len(lines.lines) == 2

    budget.emit_result({"record": "result", "padding": "x" * 400})
    assert lines.total_bytes() == budget.bytes_written
    assert budget.bytes_written <= 1024


def test_oversized_progress_record_is_suppressed() -> None:
    lines = Lines()
    budget = soak.OutputBudget(writer=lines)

    assert budget.emit_progress({"padding": "x" * soak.PROGRESS_RECORD_LIMIT_BYTES}) is False
    assert lines.lines == []
    assert budget.bytes_written == 0


def test_oversized_result_is_rejected() -> None:
    lines = Lines()
    budget = soak.OutputBudget(writer=lines)

    with pytest.raises(RuntimeError, match="output budget"):
        budget.emit_result({"detail": "x" * soak.OUTPUT_LIMIT_BYTES})
    assert lines.lines == []
    assert budget.bytes_written == 0


def test_driver_version_is_not_guessed_when_the_driver_cannot_be_queried(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    def missing(*_args: object, **_kwargs: object) -> None:
        raise FileNotFoundError("nvidia-smi")

    monkeypatch.setattr(subprocess, "run", missing)
    assert soak.query_driver_version() is None
