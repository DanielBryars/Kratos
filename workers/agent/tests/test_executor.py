import json
from typing import Any

import pytest

from kratos_agent.executor import DockerExecutor, ExecutorError
from kratos_agent.models import GpuHealthStatus

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
