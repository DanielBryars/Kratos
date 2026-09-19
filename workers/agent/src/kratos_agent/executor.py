"""Constrained Docker execution for controlled worker operations."""

import json
import re
from typing import Any

import docker
from pydantic import ValidationError

from kratos_agent.models import (
    GpuHealthEvidence,
    GpuHealthStatus,
    JobAssignment,
    JobExecutionResult,
)

IMMUTABLE_IMAGE = re.compile(r"^(?:[^\s@]+@)?sha256:[0-9a-f]{64}$")
MAX_RESULT_BYTES = 64 * 1024


class ExecutorError(RuntimeError):
    """The local container executor could not produce trustworthy evidence."""


class DockerExecutor:
    def __init__(self, client: Any) -> None:
        self._client = client

    @classmethod
    def from_environment(cls) -> "DockerExecutor":
        return cls(docker.from_env())

    def run_gpu_health_check(
        self, image_reference: str, gpu_index: int = 0, timeout_seconds: int = 120
    ) -> GpuHealthEvidence:
        if not IMMUTABLE_IMAGE.fullmatch(image_reference):
            raise ExecutorError("health-check image must use an immutable sha256 reference")
        if gpu_index < 0:
            raise ExecutorError("GPU index must be non-negative")

        container = self._client.containers.run(
            image_reference,
            detach=True,
            auto_remove=False,
            network_disabled=True,
            read_only=True,
            cap_drop=["ALL"],
            security_opt=["no-new-privileges"],
            mem_limit="1g",
            nano_cpus=1_000_000_000,
            pids_limit=128,
            tmpfs={
                "/tmp": "rw,noexec,nosuid,size=64m",
                "/var/lib/kratos-health": "rw,noexec,nosuid,size=64m",
            },
            device_requests=[
                docker.types.DeviceRequest(
                    device_ids=[str(gpu_index)],
                    capabilities=[["gpu"]],
                )
            ],
            labels={
                "com.kratos.role": "gpu-health-check",
                "com.kratos.managed": "true",
            },
        )

        try:
            wait_result = container.wait(timeout=timeout_seconds)
            raw_logs = container.logs(stdout=True, stderr=False)
            if len(raw_logs) > MAX_RESULT_BYTES:
                raise ExecutorError("health-check result exceeded its size limit")

            payload = json.loads(raw_logs.decode("utf-8"))
            payload["image_reference"] = image_reference
            evidence = GpuHealthEvidence.model_validate(payload)

            status_code = int(wait_result["StatusCode"])
            if status_code == 0 and evidence.status is not GpuHealthStatus.HEALTHY:
                raise ExecutorError("health-check exited successfully without healthy evidence")
            if status_code != 0 and evidence.status is not GpuHealthStatus.UNHEALTHY:
                raise ExecutorError("failed health-check did not return unhealthy evidence")
            return evidence
        except (KeyError, UnicodeDecodeError, json.JSONDecodeError, ValidationError) as error:
            raise ExecutorError("health-check returned invalid structured evidence") from error
        finally:
            container.remove(force=True)

    def run_job(self, assignment: JobAssignment) -> JobExecutionResult:
        if not IMMUTABLE_IMAGE.fullmatch(assignment.image_reference):
            raise ExecutorError("job image must use an immutable sha256 reference")
        container_name = f"kratos-job-{assignment.attempt_id}"
        try:
            container = self._client.containers.get(container_name)
        except docker.errors.NotFound:
            self._client.images.pull(assignment.image_reference)
            container = self._client.containers.run(
                assignment.image_reference,
                name=container_name,
                detach=True,
                auto_remove=False,
                network_disabled=True,
                read_only=True,
                cap_drop=["ALL"],
                security_opt=["no-new-privileges"],
                mem_limit="8g",
                nano_cpus=4_000_000_000,
                pids_limit=512,
                tmpfs={"/tmp": "rw,noexec,nosuid,size=1g"},
                device_requests=[
                    docker.types.DeviceRequest(
                        device_ids=[str(assignment.gpu_index)], capabilities=[["gpu"]]
                    )
                ],
                labels={
                    "com.kratos.role": "job",
                    "com.kratos.managed": "true",
                    "com.kratos.job-id": str(assignment.job_id),
                    "com.kratos.attempt-id": str(assignment.attempt_id),
                },
            )
        timed_out = False
        failure_message: str | None = None
        try:
            wait_result = container.wait(timeout=assignment.timeout_seconds)
            exit_code = int(wait_result["StatusCode"])
        except Exception:
            timed_out = True
            exit_code = 124
            failure_message = f"execution exceeded {assignment.timeout_seconds} seconds"
            try:
                container.stop(timeout=10)
            except Exception:
                failure_message = f"{failure_message}; container stop failed"
        stdout = self._bounded_log(container, stdout=True, stderr=False)
        stderr = self._bounded_log(container, stdout=False, stderr=True)
        if exit_code != 0 and failure_message is None:
            failure_message = f"container exited with code {exit_code}"
        return JobExecutionResult(
            exit_code=exit_code,
            timed_out=timed_out,
            stdout=stdout,
            stderr=stderr,
            failure_message=failure_message,
        )

    def remove_job_container(self, assignment: JobAssignment) -> None:
        try:
            self._client.containers.get(f"kratos-job-{assignment.attempt_id}").remove(force=True)
        except docker.errors.NotFound:
            return

    @staticmethod
    def _bounded_log(container: Any, *, stdout: bool, stderr: bool) -> str:
        raw = bytes(container.logs(stdout=stdout, stderr=stderr))
        if len(raw) > MAX_RESULT_BYTES:
            raw = raw[:MAX_RESULT_BYTES]
        return raw.decode("utf-8", errors="replace")
