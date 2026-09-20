import json
import time
from collections.abc import Callable
from datetime import UTC, datetime, timedelta
from types import SimpleNamespace
from typing import Any
from uuid import UUID

import docker
import pytest
from pydantic import ValidationError

from kratos_agent.executor import (
    CLEANUP_IMAGE,
    MAX_RESULT_BYTES,
    AuthorityLost,
    DockerExecutor,
    EnforcementError,
    ExecutorError,
)
from kratos_agent.models import (
    GpuHealthStatus,
    JobAssignment,
    JobExecutionResult,
    JobOutputRequirement,
)

IMAGE_ID = "sha256:" + ("a" * 64)


class FakeContainer:
    def __init__(self, status_code: int, payload: dict[str, Any]) -> None:
        self.status_code = status_code
        self.payload = payload
        self.removed = False

    def wait(self, timeout: int) -> dict[str, int]:
        assert timeout == 120
        return {"StatusCode": self.status_code}

    def logs(self, *, stdout: bool, stderr: bool) -> bytes:
        assert stdout is True
        assert stderr is False
        return json.dumps(self.payload).encode()

    def remove(self, *, force: bool) -> None:
        assert force is True
        self.removed = True


class FakeContainers:
    def __init__(self, container: FakeContainer) -> None:
        self.container = container
        self.options: dict[str, Any] | None = None

    def run(self, image: str, **options: Any) -> FakeContainer:
        assert image == IMAGE_ID
        self.options = options
        return self.container


class FakeClient:
    def __init__(self, container: FakeContainer) -> None:
        self.containers = FakeContainers(container)


def healthy_payload() -> dict[str, Any]:
    return {
        "schema_version": "1.0",
        "status": "healthy",
        "checked_at": "2026-09-19T10:51:31Z",
        "device_index": 0,
        "device_name": "Test GPU",
        "operation": "matrix multiplication",
        "matrix_size": 512,
        "max_absolute_error": 0.0,
        "duration_ms": 10.0,
        "cuda_driver_api_version": "13.3",
        "cuda_runtime_version": "12.9",
    }


def test_health_check_uses_constrained_sibling_container() -> None:
    container = FakeContainer(0, healthy_payload())
    client = FakeClient(container)

    evidence = DockerExecutor(client).run_gpu_health_check(IMAGE_ID)

    assert evidence.status is GpuHealthStatus.HEALTHY
    assert evidence.image_reference == IMAGE_ID
    assert container.removed is True
    options = client.containers.options
    assert options is not None
    assert options["network_disabled"] is True
    assert options["read_only"] is True
    assert options["cap_drop"] == ["ALL"]
    assert options["security_opt"] == ["no-new-privileges"]
    assert options["pids_limit"] == 128
    assert options["labels"]["com.kratos.role"] == "gpu-health-check"


def test_mutable_image_tag_is_rejected_before_docker_access() -> None:
    container = FakeContainer(0, healthy_payload())
    client = FakeClient(container)

    with pytest.raises(ExecutorError, match="immutable sha256"):
        DockerExecutor(client).run_gpu_health_check("kratos-gpu-health-check:latest")

    assert client.containers.options is None


def test_structured_unhealthy_result_is_preserved() -> None:
    container = FakeContainer(
        1,
        {
            "schema_version": "1.0",
            "status": "unhealthy",
            "checked_at": "2026-09-19T10:51:31Z",
            "error_type": "CUDARuntimeError",
            "detail": "no GPU available",
        },
    )

    evidence = DockerExecutor(FakeClient(container)).run_gpu_health_check(IMAGE_ID)

    assert evidence.status is GpuHealthStatus.UNHEALTHY
    assert evidence.detail == "no GPU available"
    assert container.removed is True


def test_invalid_output_is_rejected_and_container_removed() -> None:
    container = FakeContainer(0, {"unexpected": "value"})

    with pytest.raises(ExecutorError, match="invalid structured evidence"):
        DockerExecutor(FakeClient(container)).run_gpu_health_check(IMAGE_ID)

    assert container.removed is True


T0 = datetime(2026, 9, 20, 12, 0, tzinfo=UTC)


class FakeClock:
    def __init__(self) -> None:
        self.now = T0
        self.slept: list[float] = []
        self.on_sleep: Callable[[], None] | None = None

    def __call__(self) -> datetime:
        return self.now

    def sleep(self, seconds: float) -> None:
        self.slept.append(seconds)
        self.now += timedelta(seconds=seconds)
        if self.on_sleep is not None:
            self.on_sleep()


class JobContainer:
    def __init__(
        self,
        clock: FakeClock,
        *,
        status: str = "running",
        exit_code: int = 0,
        started_at: datetime | None = None,
        exits_at: datetime | None = None,
    ) -> None:
        self.clock = clock
        self.status = status
        self.exit_code = exit_code
        self.started_at = started_at
        self.exits_at = exits_at
        self.killed = False
        self.unkillable = False
        self.attrs: dict[str, Any] = {}
        self._publish()

    def _publish(self) -> None:
        exited = self.status == "exited"
        self.attrs = {
            "State": {
                "StartedAt": _docker_time(self.started_at),
                "FinishedAt": _docker_time(self.exits_at if exited else None),
            }
        }

    def reload(self) -> None:
        if self.status == "created":
            self.status = "running"
        if self.exits_at is not None and self.clock.now >= self.exits_at:
            self.status = "exited"
        self._publish()

    def wait(self, timeout: int) -> dict[str, int]:
        assert self.status == "exited"
        return {"StatusCode": self.exit_code}

    def stop(self, *, timeout: int) -> None:
        # SIGTERM followed by a grace period, which a workload may simply sit out.
        self.clock.now += timedelta(seconds=timeout)
        self.kill()

    def kill(self) -> None:
        self.killed = True
        if self.unkillable:
            raise docker.errors.APIError("cannot kill container")
        self.status = "exited"
        self.exits_at = self.clock.now
        self._publish()

    def logs(self, *, stdout: bool, stderr: bool) -> bytes:
        return b"completed\n" if stdout else b""


def _docker_time(value: datetime | None) -> str:
    # Docker reports year 1 for a start or finish that has not happened.
    return "0001-01-01T00:00:00Z" if value is None else value.isoformat().replace("+00:00", "Z")


class JobContainers:
    def __init__(self, clock: FakeClock, existing: JobContainer | None = None) -> None:
        self.existing = existing
        # docker-py returns a freshly run container with the status it had when created.
        self.container = JobContainer(clock, status="created", exits_at=T0 + timedelta(seconds=5))
        self.options: dict[str, Any] | None = None
        self.start_error: Exception | None = None

    def get(self, _: str) -> JobContainer:
        if self.existing is None:
            raise docker.errors.NotFound("missing")
        return self.existing

    def run(self, _: str, **options: Any) -> JobContainer:
        if self.start_error is not None:
            raise self.start_error
        self.options = options
        return self.container


class JobImages:
    def __init__(self) -> None:
        self.pulled: list[str] = []
        self.pull_error: Exception | None = None

    def pull(self, image: str) -> None:
        if self.pull_error is not None:
            raise self.pull_error
        self.pulled.append(image)


class JobClient:
    def __init__(self, clock: FakeClock, existing: JobContainer | None = None) -> None:
        self.containers = JobContainers(clock, existing)
        self.images = JobImages()


JOB_IMAGE = "example.test/work@sha256:" + ("b" * 64)


def job_assignment(*, lease: timedelta = timedelta(minutes=5)) -> JobAssignment:
    return JobAssignment(
        attempt_id=UUID("11111111-1111-4111-8111-111111111111"),
        job_id=UUID("22222222-2222-4222-8222-222222222222"),
        name="Matrix check",
        image_reference=JOB_IMAGE,
        gpu_index=0,
        timeout_seconds=120,
        lease_expires_at=T0 + lease,
    )


def job_executor(clock: FakeClock, client: JobClient) -> DockerExecutor:
    return DockerExecutor(client, clock=clock, sleep=clock.sleep)


def test_job_uses_immutable_image_and_constrained_gpu_container() -> None:
    assignment = job_assignment()
    clock = FakeClock()
    client = JobClient(clock)
    executor = job_executor(clock, client)

    assert executor.prepare_job(assignment) is None
    result = executor.run_job(assignment)

    assert result.exit_code == 0
    assert result.stdout == "completed\n"
    assert client.images.pulled == [JOB_IMAGE]
    assert clock.now == T0 + timedelta(seconds=5)
    options = client.containers.options
    assert options is not None
    assert options["name"] == f"kratos-job-{assignment.attempt_id}"
    assert options["network_disabled"] is True
    assert options["read_only"] is True
    assert options["cap_drop"] == ["ALL"]
    assert options["device_requests"][0].device_ids == ["0"]
    assert options["labels"]["com.kratos.role"] == "job"
    assert options["environment"] == {
        "KRATOS_JOB_ID": "22222222-2222-4222-8222-222222222222",
        "KRATOS_ATTEMPT_ID": "11111111-1111-4111-8111-111111111111",
        "OTEL_RESOURCE_ATTRIBUTES": (
            "kratos.job.id=22222222-2222-4222-8222-222222222222,"
            "kratos.attempt.id=11111111-1111-4111-8111-111111111111"
        ),
    }


def test_image_absent_from_registry_is_a_terminal_failure() -> None:
    clock = FakeClock()
    client = JobClient(clock)
    client.images.pull_error = docker.errors.NotFound("manifest unknown")

    result = job_executor(clock, client).prepare_job(job_assignment())

    assert result is not None
    assert result.exit_code == 125
    assert result.failure_message == "job image was not found in its registry"


def test_registry_outage_is_left_for_the_caller_to_retry() -> None:
    clock = FakeClock()
    client = JobClient(clock)
    client.images.pull_error = docker.errors.APIError("registry unreachable")

    with pytest.raises(docker.errors.APIError):
        job_executor(clock, client).prepare_job(job_assignment())


def test_exited_container_is_reported_not_restarted_after_lease_expiry() -> None:
    # A result withheld by a network outage: the work finished inside its lease, but the
    # worker could only reach the control plane after the lease had passed.
    clock = FakeClock()
    existing = JobContainer(
        clock,
        status="exited",
        started_at=T0 - timedelta(minutes=10),
        exits_at=T0 - timedelta(minutes=9),
    )
    client = JobClient(clock, existing)
    assignment = job_assignment(lease=timedelta(minutes=-5))

    result = job_executor(clock, client).run_job(assignment, may_start=False)

    assert (result.exit_code, result.timed_out, result.failure_message) == (0, False, None)
    assert result.stdout == "completed\n"
    assert client.containers.options is None
    assert existing.killed is False


def test_result_log_stays_within_byte_limit_when_cut_splits_utf8() -> None:
    payload = (b"x" * (MAX_RESULT_BYTES - 1)) + b"\xe2\x82\xac" + b"ignored"
    container = SimpleNamespace(logs=lambda **_: payload)

    result = DockerExecutor._bounded_log(container, stdout=False, stderr=True)

    assert len(result.encode("utf-8")) <= MAX_RESULT_BYTES
    assert result == "x" * (MAX_RESULT_BYTES - 1)


def test_missing_container_is_never_started_twice() -> None:
    clock = FakeClock()
    client = JobClient(clock)

    result = job_executor(clock, client).run_job(job_assignment(), may_start=False)

    assert result.exit_code == 125
    assert result.failure_message is not None
    assert "refusing to execute it again" in result.failure_message
    assert client.containers.options is None


def test_expired_lease_does_not_start_a_container() -> None:
    clock = FakeClock()
    client = JobClient(clock)

    result = job_executor(clock, client).run_job(job_assignment(lease=timedelta(seconds=-1)))

    assert (result.exit_code, result.timed_out) == (124, True)
    assert result.failure_message == "assignment lease expired before execution"
    assert client.containers.options is None


def test_resumed_container_is_stopped_when_its_lease_expires() -> None:
    clock = FakeClock()
    existing = JobContainer(clock, started_at=T0 - timedelta(seconds=10))
    assignment = job_assignment(lease=timedelta(seconds=20))

    result = job_executor(clock, JobClient(clock, existing)).run_job(assignment, may_start=False)

    assert clock.now == T0 + timedelta(seconds=20)
    assert existing.killed is True
    assert (result.exit_code, result.timed_out) == (124, True)
    assert result.failure_message == "assignment lease expired during execution"


def test_resumed_container_keeps_its_original_runtime_bound() -> None:
    clock = FakeClock()
    existing = JobContainer(clock, started_at=T0 - timedelta(seconds=100))

    result = job_executor(clock, JobClient(clock, existing)).run_job(
        job_assignment(), may_start=False
    )

    assert clock.now == T0 + timedelta(seconds=20)
    assert existing.killed is True
    assert result.failure_message == "execution exceeded 120 seconds"


def test_unsupervised_overrun_is_not_reported_as_success() -> None:
    clock = FakeClock()
    existing = JobContainer(
        clock,
        status="exited",
        started_at=T0 - timedelta(minutes=30),
        exits_at=T0 - timedelta(minutes=1),
    )

    result = job_executor(clock, JobClient(clock, existing)).run_job(
        job_assignment(), may_start=False
    )

    assert (result.exit_code, result.timed_out) == (124, True)
    assert result.failure_message == "execution continued beyond its authority while unsupervised"


def test_container_that_never_started_is_a_failure() -> None:
    clock = FakeClock()
    existing = JobContainer(clock, status="created")

    result = job_executor(clock, JobClient(clock, existing)).run_job(
        job_assignment(), may_start=False
    )

    assert result.exit_code == 125
    assert result.failure_message == "job container was created but never started"
    assert clock.slept == []


def test_container_start_failure_is_reported() -> None:
    clock = FakeClock()
    client = JobClient(clock)
    client.containers.start_error = docker.errors.APIError("could not select device driver")

    result = job_executor(clock, client).run_job(job_assignment())

    assert result.exit_code == 125
    assert result.failure_message is not None
    assert result.failure_message.startswith("job container could not be started")


def test_heartbeats_continue_while_the_container_runs() -> None:
    clock = FakeClock()
    existing = JobContainer(clock, started_at=T0, exits_at=T0 + timedelta(seconds=100))
    ticks: list[datetime] = []

    def on_tick() -> bool:
        ticks.append(clock.now)
        return True

    result = job_executor(clock, JobClient(clock, existing)).run_job(
        job_assignment(), may_start=False, on_tick=on_tick, tick_seconds=30
    )

    assert ticks == [T0 + timedelta(seconds=seconds) for seconds in (30, 60, 90)]
    assert (result.exit_code, result.timed_out, result.failure_message) == (0, False, None)
    assert existing.killed is False


def test_container_is_stopped_when_the_control_plane_closes_its_attempt() -> None:
    clock = FakeClock()
    existing = JobContainer(clock, started_at=T0)

    result = job_executor(clock, JobClient(clock, existing)).run_job(
        job_assignment(), may_start=False, on_tick=lambda: False, tick_seconds=30
    )

    assert clock.now == T0 + timedelta(seconds=30)
    assert existing.killed is True
    assert (result.exit_code, result.timed_out) == (125, False)
    assert result.failure_message == "control plane no longer holds this attempt"


def test_slow_heartbeat_does_not_extend_the_runtime_bound() -> None:
    clock = FakeClock()
    existing = JobContainer(clock, started_at=T0)

    def slow_tick() -> bool:
        clock.now += timedelta(seconds=15)
        return True

    result = job_executor(clock, JobClient(clock, existing)).run_job(
        job_assignment(), may_start=False, on_tick=slow_tick, tick_seconds=30
    )

    assert existing.killed is True
    assert result.failure_message == "execution exceeded 120 seconds"
    assert clock.now <= T0 + timedelta(seconds=120 + 15)


def test_workload_ignoring_sigterm_gets_no_time_beyond_its_bound() -> None:
    clock = FakeClock()
    existing = JobContainer(clock, started_at=T0)

    result = job_executor(clock, JobClient(clock, existing)).run_job(
        job_assignment(), may_start=False
    )

    assert existing.killed is True
    assert existing.exits_at == T0 + timedelta(seconds=120)
    assert result.failure_message == "execution exceeded 120 seconds"


def test_container_killed_at_its_bound_is_described_the_same_way_when_reported_later() -> None:
    # The first report was lost, so a later pass finds the container already killed, a moment
    # after its deadline. That is enforcement, not an unsupervised overrun.
    clock = FakeClock()
    existing = JobContainer(
        clock,
        status="exited",
        exit_code=137,
        started_at=T0 - timedelta(seconds=300),
        exits_at=T0 - timedelta(seconds=178),
    )

    result = job_executor(clock, JobClient(clock, existing)).run_job(
        job_assignment(), may_start=False
    )

    assert (result.exit_code, result.timed_out) == (124, True)
    assert result.failure_message == "execution exceeded 120 seconds"


def test_container_that_cannot_be_killed_is_not_reported_as_a_result() -> None:
    # Reporting a result here would tell the control plane the attempt is over while the
    # workload is still running beyond its authority.
    clock = FakeClock()
    existing = JobContainer(clock, started_at=T0 - timedelta(seconds=100))
    existing.unkillable = True

    with pytest.raises(EnforcementError, match="outlived its authority and could not be stopped"):
        job_executor(clock, JobClient(clock, existing)).run_job(job_assignment(), may_start=False)

    assert existing.status == "running"
    assert clock.slept[-2:] == [1.0, 1.0]


def test_kill_that_wins_on_a_later_attempt_is_reported_normally() -> None:
    clock = FakeClock()
    existing = JobContainer(clock, started_at=T0 - timedelta(seconds=100))
    existing.unkillable = True

    def relent() -> None:
        existing.unkillable = False

    clock.on_sleep = relent

    result = job_executor(clock, JobClient(clock, existing)).run_job(
        job_assignment(), may_start=False
    )

    assert existing.status == "exited"
    assert (result.exit_code, result.timed_out) == (124, True)
    assert result.failure_message == "execution exceeded 120 seconds"


def test_attempt_without_recorded_bounds_is_stopped_but_kept() -> None:
    clock = FakeClock()
    existing = JobContainer(clock, started_at=T0 - timedelta(seconds=10))
    client = JobClient(clock, existing)

    job_executor(clock, client).stop_unbounded_attempt(job_assignment().attempt_id)

    assert existing.killed is True
    assert existing.status == "exited"


def test_stopping_an_absent_unbounded_attempt_is_harmless() -> None:
    clock = FakeClock()

    job_executor(clock, JobClient(clock)).stop_unbounded_attempt(job_assignment().attempt_id)


def output_assignment() -> JobAssignment:
    return job_assignment().model_copy(
        update={
            "output_requirements": (
                JobOutputRequirement(
                    logical_path="model.pt",
                    role="model",
                    media_type="application/octet-stream",
                    mandatory=True,
                    max_bytes=1024,
                ),
            )
        }
    )


def test_only_this_attempts_outputs_subpath_is_exposed_to_the_job() -> None:
    clock = FakeClock()
    client = JobClient(clock)
    assignment = output_assignment()

    DockerExecutor(
        client, clock=clock, sleep=clock.sleep, state_volume="kratos-agent-state"
    ).run_job(assignment)

    options = client.containers.options
    assert options is not None
    (mount,) = options["mounts"]
    assert mount["Target"] == "/kratos/outputs"
    assert mount["Source"] == "kratos-agent-state"
    assert mount["ReadOnly"] is False
    # The volume root holds the worker credential and must never be what the job sees.
    assert mount["VolumeOptions"]["Subpath"] == f"attempts/{assignment.attempt_id}/outputs"


def test_a_job_without_declared_outputs_gets_no_output_mount() -> None:
    clock = FakeClock()
    client = JobClient(clock)

    DockerExecutor(
        client, clock=clock, sleep=clock.sleep, state_volume="kratos-agent-state"
    ).run_job(job_assignment())

    options = client.containers.options
    assert options is not None
    assert options["mounts"] == []


def test_a_job_with_outputs_refuses_to_start_without_a_known_state_volume() -> None:
    clock = FakeClock()
    client = JobClient(clock)

    with pytest.raises(ExecutorError, match="needs the agent state volume name"):
        DockerExecutor(client, clock=clock, sleep=clock.sleep).run_job(output_assignment())

    assert client.containers.options is None


class PreflightImages(JobImages):
    """Local image store with a registry that may be unreachable."""

    def __init__(self, *, local: bool, registry_up: bool = True) -> None:
        super().__init__()
        self.local = local
        self.registry_up = registry_up
        self.gets = 0

    def get(self, image: str) -> str:
        self.gets += 1
        if not self.local:
            raise docker.errors.ImageNotFound(image)
        return image

    def pull(self, image: str) -> None:
        if not self.registry_up:
            raise docker.errors.APIError("registry unreachable")
        self.local = True
        self.pulled.append(image)


class PreflightClient(JobClient):
    def __init__(self, clock: FakeClock, images: PreflightImages) -> None:
        super().__init__(clock)
        self.images = images
        # A cleanup container is waited on after it has finished, unlike a supervised job.
        self.containers.container.status = "exited"
        self.volumes = SimpleNamespace(get=lambda name: name)
        self.version = lambda: {"Version": "27.0.1"}


def test_durable_outputs_are_declined_when_the_cleanup_image_cannot_be_fetched() -> None:
    # Cleanup happens after a job has succeeded, so a registry outage then would strand the
    # worker. The capability is declined now instead.
    clock = FakeClock()
    images = PreflightImages(local=False, registry_up=False)
    executor = DockerExecutor(
        PreflightClient(clock, images), clock=clock, sleep=clock.sleep, state_volume="state"
    )

    reason = executor.durable_output_support()

    assert reason is not None
    assert "cleanup image could not be prefetched" in reason


def test_a_cached_cleanup_image_survives_an_offline_registry() -> None:
    clock = FakeClock()
    images = PreflightImages(local=True, registry_up=False)
    client = PreflightClient(clock, images)
    executor = DockerExecutor(client, clock=clock, sleep=clock.sleep, state_volume="state")

    assert executor.durable_output_support() is None
    assert executor.discard_attempt_outputs(job_assignment().attempt_id) is True

    # Nothing was pulled, at preflight or during cleanup.
    assert images.pulled == []


def test_the_cleanup_image_is_prefetched_before_the_capability_is_offered() -> None:
    clock = FakeClock()
    images = PreflightImages(local=False, registry_up=True)
    executor = DockerExecutor(
        PreflightClient(clock, images), clock=clock, sleep=clock.sleep, state_volume="state"
    )

    assert executor.durable_output_support() is None
    assert images.pulled == [CLEANUP_IMAGE]


def test_a_completed_run_reports_the_runtime_s_own_interval() -> None:
    clock = FakeClock()
    started, finished = T0 - timedelta(seconds=40), T0 - timedelta(seconds=10)
    existing = JobContainer(clock, status="exited", started_at=started, exits_at=finished)

    result = job_executor(clock, JobClient(clock, existing)).run_job(
        job_assignment(), may_start=False
    )

    # The container runtime's timestamps, not the agent's clock or the assignment.
    assert result.execution_started_at == started
    assert result.execution_finished_at == finished


def test_a_failure_before_the_container_started_reports_no_interval() -> None:
    clock = FakeClock()

    result = job_executor(clock, JobClient(clock)).run_job(job_assignment(), may_start=False)

    assert result.exit_code == 125
    assert result.execution_started_at is None
    assert result.execution_finished_at is None


def test_an_expired_lease_before_execution_reports_no_interval() -> None:
    clock = FakeClock()

    result = job_executor(clock, JobClient(clock)).run_job(
        job_assignment(lease=timedelta(seconds=-1))
    )

    assert result.timed_out is True
    assert result.execution_started_at is None


def test_a_killed_run_reports_the_interval_the_runtime_recorded() -> None:
    clock = FakeClock()
    existing = JobContainer(clock, started_at=T0 - timedelta(seconds=100))

    result = job_executor(clock, JobClient(clock, existing)).run_job(
        job_assignment(), may_start=False
    )

    assert result.failure_message == "execution exceeded 120 seconds"
    # A killed container has a finish time, and it is the runtime's, not the deadline.
    assert result.execution_started_at == T0 - timedelta(seconds=100)
    assert result.execution_finished_at == existing.exits_at


def test_an_interval_is_never_sent_with_one_end_missing() -> None:
    with pytest.raises(ValidationError, match="sent as a pair"):
        JobExecutionResult(
            exit_code=0,
            timed_out=False,
            stdout="",
            stderr="",
            execution_started_at=T0,
        )


def test_an_inverted_interval_is_refused_before_it_is_sent() -> None:
    with pytest.raises(ValidationError, match="finished before it started"):
        JobExecutionResult(
            exit_code=0,
            timed_out=False,
            stdout="",
            stderr="",
            execution_started_at=T0,
            execution_finished_at=T0 - timedelta(seconds=1),
        )


class SlowImages(JobImages):
    """A registry that takes its time, like a cold multi-gigabyte pull on a home link."""

    def __init__(self, clock: FakeClock, seconds: float = 300) -> None:
        super().__init__()
        self.clock = clock
        self.finish_at = clock.now + timedelta(seconds=seconds)

    def pull(self, image: str) -> None:
        while self.clock.now < self.finish_at:
            time.sleep(0.001)
        self.pulled.append(image)


def test_a_long_pull_keeps_the_worker_reporting() -> None:
    # Without this the worker sends nothing for the whole pull and reads offline as a job starts.
    clock = FakeClock()
    client = JobClient(clock)
    client.images = SlowImages(clock)
    ticks: list[datetime] = []

    def on_tick() -> bool:
        ticks.append(clock.now)
        return True

    result = job_executor(clock, client).prepare_job(
        job_assignment(), still_authorised=on_tick, tick_seconds=30
    )

    assert result is None
    assert client.images.pulled == [JOB_IMAGE]
    # Roughly one heartbeat per interval across a five minute pull, not silence.
    assert len(ticks) >= 5
    spacing = zip(ticks, ticks[1:], strict=False)
    assert all(later - earlier >= timedelta(seconds=30) for earlier, later in spacing)


def test_a_cancellation_during_the_pull_creates_no_container() -> None:
    clock = FakeClock()
    client = JobClient(clock)
    client.images = SlowImages(clock)

    with pytest.raises(AuthorityLost, match="during the image pull"):
        job_executor(clock, client).prepare_job(
            job_assignment(), still_authorised=lambda: False, tick_seconds=30
        )

    # Nothing was created, so there is nothing to reconcile or clean up.
    assert client.containers.options is None


def test_a_pull_without_an_authority_check_still_works() -> None:
    clock = FakeClock()
    client = JobClient(clock)

    assert job_executor(clock, client).prepare_job(job_assignment()) is None
    assert client.images.pulled == [JOB_IMAGE]


def test_a_pull_of_a_mutable_reference_is_refused_before_any_thread_starts() -> None:
    clock = FakeClock()
    client = JobClient(clock)
    mutable = job_assignment().model_copy(update={"image_reference": "example.test/work:latest"})

    with pytest.raises(ExecutorError, match="immutable sha256"):
        job_executor(clock, client).prepare_job(mutable)

    assert client.images.pulled == []
