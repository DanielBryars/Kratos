import json
from datetime import UTC, datetime, timedelta
from typing import Any
from uuid import UUID

import docker
import pytest

from kratos_agent.executor import DockerExecutor, ExecutorError
from kratos_agent.models import GpuHealthStatus, JobAssignment

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

    def __call__(self) -> datetime:
        return self.now

    def sleep(self, seconds: float) -> None:
        self.slept.append(seconds)
        self.now += timedelta(seconds=seconds)


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
