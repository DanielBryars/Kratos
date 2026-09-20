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


class JobContainer:
    def __init__(
        self,
        *,
        status: str = "running",
        exit_code: int = 0,
        started_at: datetime | None = None,
        finished_at: datetime | None = None,
        exits: bool = True,
    ) -> None:
        self.status = status
        self.exit_code = exit_code
        self.exits = exits
        self.wait_timeouts: list[int] = []
        self.stopped = False
        self.attrs = {
            "State": {
                "StartedAt": _docker_time(started_at),
                "FinishedAt": _docker_time(finished_at),
            }
        }

    def wait(self, timeout: int) -> dict[str, int]:
        self.wait_timeouts.append(timeout)
        if not self.exits:
            raise TimeoutError("container is still running")
        return {"StatusCode": self.exit_code}

    def reload(self) -> None:
        return None

    def stop(self, *, timeout: int) -> None:
        assert timeout == 10
        self.stopped = True

    def logs(self, *, stdout: bool, stderr: bool) -> bytes:
        return b"completed\n" if stdout else b""


def _docker_time(value: datetime | None) -> str:
    # Docker reports year 1 for a start or finish that has not happened.
    return "0001-01-01T00:00:00Z" if value is None else value.isoformat().replace("+00:00", "Z")


class JobContainers:
    def __init__(self, existing: JobContainer | None = None) -> None:
        self.existing = existing
        self.container = JobContainer()
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
    def __init__(self, existing: JobContainer | None = None) -> None:
        self.containers = JobContainers(existing)
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
        lease_expires_at=datetime.now(UTC) + lease,
    )


def test_job_uses_immutable_image_and_constrained_gpu_container() -> None:
    assignment = job_assignment()
    client = JobClient()
    executor = DockerExecutor(client)

    assert executor.prepare_job(assignment) is None
    result = executor.run_job(assignment)

    assert result.exit_code == 0
    assert result.stdout == "completed\n"
    assert client.images.pulled == [JOB_IMAGE]
    assert client.containers.container.wait_timeouts == [120]
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
    client = JobClient()
    client.images.pull_error = docker.errors.NotFound("manifest unknown")

    result = DockerExecutor(client).prepare_job(job_assignment())

    assert result is not None
    assert result.exit_code == 125
    assert result.failure_message == "job image was not found in its registry"


def test_registry_outage_is_left_for_the_caller_to_retry() -> None:
    client = JobClient()
    client.images.pull_error = docker.errors.APIError("registry unreachable")

    with pytest.raises(docker.errors.APIError):
        DockerExecutor(client).prepare_job(job_assignment())


def test_exited_container_is_reported_not_restarted_after_lease_expiry() -> None:
    # A result withheld by a network outage: the work finished inside its lease, but the
    # worker could only reach the control plane after the lease had passed.
    now = datetime.now(UTC)
    existing = JobContainer(
        status="exited",
        started_at=now - timedelta(minutes=10),
        finished_at=now - timedelta(minutes=9),
    )
    client = JobClient(existing)
    assignment = job_assignment(lease=timedelta(minutes=-5))

    result = DockerExecutor(client).run_job(assignment, may_start=False)

    assert (result.exit_code, result.timed_out, result.failure_message) == (0, False, None)
    assert result.stdout == "completed\n"
    assert client.containers.options is None
    assert existing.stopped is False


def test_missing_container_is_never_started_twice() -> None:
    client = JobClient()

    result = DockerExecutor(client).run_job(job_assignment(), may_start=False)

    assert result.exit_code == 125
    assert result.failure_message is not None
    assert "refusing to execute it again" in result.failure_message
    assert client.containers.options is None


def test_expired_lease_does_not_start_a_container() -> None:
    client = JobClient()

    result = DockerExecutor(client).run_job(job_assignment(lease=timedelta(seconds=-1)))

    assert (result.exit_code, result.timed_out) == (124, True)
    assert result.failure_message == "assignment lease expired before execution"
    assert client.containers.options is None


def test_resumed_container_is_stopped_when_its_lease_expires() -> None:
    existing = JobContainer(started_at=datetime.now(UTC) - timedelta(seconds=10), exits=False)
    assignment = job_assignment(lease=timedelta(seconds=20))

    result = DockerExecutor(JobClient(existing)).run_job(assignment, may_start=False)

    assert existing.wait_timeouts == [20]
    assert existing.stopped is True
    assert (result.exit_code, result.timed_out) == (124, True)
    assert result.failure_message == "assignment lease expired during execution"


def test_resumed_container_keeps_its_original_runtime_bound() -> None:
    existing = JobContainer(started_at=datetime.now(UTC) - timedelta(seconds=100), exits=False)

    result = DockerExecutor(JobClient(existing)).run_job(job_assignment(), may_start=False)

    assert existing.wait_timeouts == [20]
    assert existing.stopped is True
    assert result.failure_message == "execution exceeded 120 seconds"


def test_unsupervised_overrun_is_not_reported_as_success() -> None:
    now = datetime.now(UTC)
    existing = JobContainer(
        status="exited",
        started_at=now - timedelta(minutes=30),
        finished_at=now - timedelta(minutes=1),
    )

    result = DockerExecutor(JobClient(existing)).run_job(job_assignment(), may_start=False)

    assert (result.exit_code, result.timed_out) == (124, True)
    assert result.failure_message == "execution continued beyond its authority while unsupervised"


def test_container_that_never_started_is_a_failure() -> None:
    existing = JobContainer(status="created")

    result = DockerExecutor(JobClient(existing)).run_job(job_assignment(), may_start=False)

    assert result.exit_code == 125
    assert result.failure_message == "job container was created but never started"
    assert existing.wait_timeouts == []


def test_container_start_failure_is_reported() -> None:
    client = JobClient()
    client.containers.start_error = docker.errors.APIError("could not select device driver")

    result = DockerExecutor(client).run_job(job_assignment())

    assert result.exit_code == 125
    assert result.failure_message is not None
    assert result.failure_message.startswith("job container could not be started")
