"""Constrained Docker execution for controlled worker operations."""

import json
import math
import re
from datetime import UTC, datetime, timedelta
from typing import Any
from uuid import UUID

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
MAX_FAILURE_MESSAGE_CHARS = 1_000


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

    def prepare_job(self, assignment: JobAssignment) -> JobExecutionResult | None:
        """Fetch the job image without creating a container.

        A registry that reports the image as absent is a terminal failure. Any other error
        propagates so the caller can retry while the lease remains valid.
        """
        if not IMMUTABLE_IMAGE.fullmatch(assignment.image_reference):
            raise ExecutorError("job image must use an immutable sha256 reference")
        try:
            self._client.images.pull(assignment.image_reference)
        except docker.errors.NotFound:
            return _failure(125, "job image was not found in its registry")
        return None

    def run_job(self, assignment: JobAssignment, *, may_start: bool = True) -> JobExecutionResult:
        """Supervise the attempt's container until it exits or its authority ends.

        An existing attempt-named container is always resumed rather than replaced. With
        ``may_start`` false a missing container is reported as a failure, because this worker
        has already recorded starting the attempt and cannot prove that it did not run.
        """
        if not IMMUTABLE_IMAGE.fullmatch(assignment.image_reference):
            raise ExecutorError("job image must use an immutable sha256 reference")
        container_name = _container_name(assignment.attempt_id)
        job_id = str(assignment.job_id)
        attempt_id = str(assignment.attempt_id)
        try:
            container = self._client.containers.get(container_name)
            if container.status == "created":
                return _failure(125, "job container was created but never started")
            started_at = _state_time(container, "StartedAt") or datetime.now(UTC)
        except docker.errors.NotFound:
            if datetime.now(UTC) >= assignment.lease_expires_at:
                return _failure(124, "assignment lease expired before execution", timed_out=True)
            if not may_start:
                return _failure(
                    125,
                    "attempt was already started on this worker but its container is missing; "
                    "refusing to execute it again",
                )
            try:
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
                    environment={
                        "KRATOS_JOB_ID": job_id,
                        "KRATOS_ATTEMPT_ID": attempt_id,
                        "OTEL_RESOURCE_ATTRIBUTES": (
                            f"kratos.job.id={job_id},kratos.attempt.id={attempt_id}"
                        ),
                    },
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
            except docker.errors.APIError as error:
                return _failure(125, f"job container could not be started: {error}")
            started_at = datetime.now(UTC)

        runtime_deadline = started_at + timedelta(seconds=assignment.timeout_seconds)
        deadline = min(runtime_deadline, assignment.lease_expires_at)
        remaining = (deadline - datetime.now(UTC)).total_seconds()
        timed_out = False
        failure_message: str | None = None
        try:
            # An exited container returns immediately, so a result held back by a network
            # outage is still reported with its real exit status.
            wait_result = container.wait(timeout=max(1, math.ceil(remaining)))
            exit_code = int(wait_result["StatusCode"])
        except Exception:
            timed_out = True
            exit_code = 124
            failure_message = (
                f"execution exceeded {assignment.timeout_seconds} seconds"
                if deadline == runtime_deadline
                else "assignment lease expired during execution"
            )
            try:
                container.stop(timeout=10)
            except Exception:
                failure_message = f"{failure_message}; container stop failed"
        else:
            container.reload()
            finished_at = _state_time(container, "FinishedAt")
            if finished_at is not None and finished_at > deadline:
                timed_out = True
                exit_code = 124
                failure_message = "execution continued beyond its authority while unsupervised"
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

    def remove_job_container(self, attempt_id: UUID) -> None:
        try:
            self._client.containers.get(_container_name(attempt_id)).remove(force=True)
        except docker.errors.NotFound:
            return

    @staticmethod
    def _bounded_log(container: Any, *, stdout: bool, stderr: bool) -> str:
        raw = bytes(container.logs(stdout=stdout, stderr=stderr))
        if len(raw) > MAX_RESULT_BYTES:
            raw = raw[:MAX_RESULT_BYTES]
        return raw.decode("utf-8", errors="replace")


def _container_name(attempt_id: UUID) -> str:
    return f"kratos-job-{attempt_id}"


def _failure(exit_code: int, message: str, *, timed_out: bool = False) -> JobExecutionResult:
    return JobExecutionResult(
        exit_code=exit_code,
        timed_out=timed_out,
        stdout="",
        stderr="",
        failure_message=message[:MAX_FAILURE_MESSAGE_CHARS],
    )


def _state_time(container: Any, key: str) -> datetime | None:
    """Read a Docker state timestamp; Docker reports year 1 for an event that has not happened."""
    try:
        value = datetime.fromisoformat(container.attrs["State"][key])
    except (KeyError, TypeError, ValueError):
        return None
    return value if value.year > 1 else None
