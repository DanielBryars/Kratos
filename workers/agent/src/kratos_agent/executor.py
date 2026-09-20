"""Constrained Docker execution for controlled worker operations."""

import contextlib
import json
import re
import time
from collections.abc import Callable
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
OUTPUT_MOUNT_TARGET = "/kratos/outputs"
ATTEMPT_DIRECTORY = "attempts"
# Volume subpath mounts require Docker Engine 26 or later.
MINIMUM_SUBPATH_ENGINE_MAJOR = 26
SUPERVISION_POLL_SECONDS = 1.0
# How late a bound may be enforced: one poll plus one in-flight heartbeat and its collection.
ENFORCEMENT_TOLERANCE = timedelta(seconds=30)
# How hard the agent tries to stop a container before it admits it cannot.
KILL_ATTEMPTS = 3
KILL_RETRY_SECONDS = 1.0
ACTIVE_CONTAINER_STATUSES = frozenset({"running", "paused", "restarting"})


class ExecutorError(RuntimeError):
    """The local container executor could not produce trustworthy evidence."""


class CleanupError(ExecutorError):
    """An attempt's data could not be removed, so its state must not be cleared."""


class EnforcementError(ExecutorError):
    """A container outlived its authority and could not be stopped.

    This is never a job result. The attempt stays recorded so the agent retries
    enforcement rather than reporting an outcome while the workload still runs.
    """


class DockerExecutor:
    def __init__(
        self,
        client: Any,
        *,
        clock: Callable[[], datetime] = lambda: datetime.now(UTC),
        sleep: Callable[[float], None] = time.sleep,
        state_volume: str | None = None,
    ) -> None:
        self._client = client
        self._clock = clock
        self._sleep = sleep
        # The agent drives sibling containers, so a job's output directory cannot be a path inside
        # this container. Only the per-attempt subpath of the state volume is exposed, never its
        # root, which holds the worker credential (ADR-014).
        self._state_volume = state_volume

    @classmethod
    def from_environment(cls, *, state_volume: str | None = None) -> "DockerExecutor":
        return cls(docker.from_env(), state_volume=state_volume)

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

    def run_job(
        self,
        assignment: JobAssignment,
        *,
        may_start: bool = True,
        on_tick: Callable[[], bool] | None = None,
        tick_seconds: float = 30,
    ) -> JobExecutionResult:
        """Supervise the attempt's container until it exits or its authority ends.

        An existing attempt-named container is always resumed rather than replaced. With
        ``may_start`` false a missing container is reported as a failure, because this worker
        has already recorded starting the attempt and cannot prove that it did not run.

        ``on_tick`` is called about every ``tick_seconds`` while the container runs. Returning
        false means the control plane no longer holds the attempt, which ends its authority.

        Authority ends without a grace period: the container is killed, not asked to stop,
        so a workload that ignores SIGTERM cannot run past its bound.
        """
        if not IMMUTABLE_IMAGE.fullmatch(assignment.image_reference):
            raise ExecutorError("job image must use an immutable sha256 reference")
        container_name = _container_name(assignment.attempt_id)
        logical_name = f"attempt {assignment.attempt_id}"
        job_id = str(assignment.job_id)
        attempt_id = str(assignment.attempt_id)
        try:
            container = self._client.containers.get(container_name)
            if container.status == "created":
                return _failure(125, "job container was created but never started")
            started_at = _state_time(container, "StartedAt") or self._clock()
        except docker.errors.NotFound:
            if self._clock() >= assignment.lease_expires_at:
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
                    mounts=self._output_mounts(assignment),
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
            started_at = self._clock()

        runtime_deadline = started_at + timedelta(seconds=assignment.timeout_seconds)
        deadline = min(runtime_deadline, assignment.lease_expires_at)
        bound_message = (
            f"execution exceeded {assignment.timeout_seconds} seconds"
            if deadline == runtime_deadline
            else "assignment lease expired during execution"
        )
        tick = timedelta(seconds=tick_seconds)
        next_tick = self._clock() + tick
        timed_out = False
        failure_message: str | None = None
        while True:
            container.reload()
            if container.status not in ACTIVE_CONTAINER_STATUSES:
                break
            now = self._clock()
            if now >= deadline:
                timed_out = True
                failure_message = bound_message
                break
            if on_tick is not None and now >= next_tick:
                authorised = on_tick()
                next_tick = self._clock() + tick
                if not authorised:
                    failure_message = "control plane no longer holds this attempt"
                    break
                continue
            self._sleep(min(SUPERVISION_POLL_SECONDS, (deadline - now).total_seconds()))
        if failure_message is not None:
            exit_code = 124 if timed_out else 125
            self._kill(container, logical_name)
        else:
            # A container that has already exited is reported with its real exit status, even
            # when a network outage held the result back beyond the lease.
            exit_code = int(container.wait(timeout=10)["StatusCode"])
            finished_at = _state_time(container, "FinishedAt")
            if finished_at is not None and finished_at > deadline:
                # A container this agent killed at its bound finishes just after it; a later
                # finish means nothing was enforcing the bound at the time.
                timed_out = True
                exit_code = 124
                failure_message = (
                    bound_message
                    if finished_at <= deadline + ENFORCEMENT_TOLERANCE
                    else "execution continued beyond its authority while unsupervised"
                )
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

    def durable_output_support(self) -> str | None:
        """Why this agent cannot deliver durable outputs, or None when it can.

        Checked once at startup so the agent advertises 1.1 only when an output job would
        actually succeed, rather than being assigned work it must then reject.
        """
        if self._state_volume is None:
            return "the agent was started without --state-volume"
        try:
            version = self._client.version()
            engine = str(version.get("Version", "0"))
            major = int(engine.split(".")[0])
        except Exception as error:
            return f"the Docker Engine version could not be read: {type(error).__name__}"
        if major < MINIMUM_SUBPATH_ENGINE_MAJOR:
            return f"Docker Engine {engine} cannot mount a volume subpath"
        try:
            self._client.volumes.get(self._state_volume)
        except Exception:
            return f"the state volume {self._state_volume!r} does not exist"
        return None

    def _output_mounts(self, assignment: JobAssignment) -> list[Any]:
        """Expose only this attempt's outputs subdirectory, writable, at /kratos/outputs."""
        if not assignment.output_requirements:
            return []
        if self._state_volume is None:
            raise ExecutorError(
                "a job with output requirements needs the agent state volume name; "
                "reinstall the agent so it can pass --state-volume"
            )
        return [
            docker.types.Mount(
                target=OUTPUT_MOUNT_TARGET,
                source=self._state_volume,
                type="volume",
                read_only=False,
                # Docker Engine 26 or later. Without subpath support the whole volume, including
                # the worker credential, would be visible to the job.
                subpath=f"{ATTEMPT_DIRECTORY}/{assignment.attempt_id}/outputs",
            )
        ]

    def discard_attempt_outputs(self, attempt_id: UUID, image_reference: str) -> bool:
        """Remove an attempt's output tree that this agent cannot remove itself.

        A workload runs as an arbitrary user and can leave a nested directory the agent may not
        traverse. The removal therefore runs in a throwaway container as root, but it is bound to
        the one attempt: the container sees only that attempt's subpath of the state volume, never
        the volume root, and it has no network and no capabilities beyond the two it needs to
        traverse and unlink what another user owns.
        """
        if self._state_volume is None:
            return False
        try:
            container = self._client.containers.run(
                image_reference,
                command=["sh", "-c", "rm -rf /attempt/* /attempt/.[!.]* 2>/dev/null; true"],
                detach=True,
                network_disabled=True,
                read_only=True,
                cap_drop=["ALL"],
                cap_add=["DAC_OVERRIDE", "DAC_READ_SEARCH"],
                security_opt=["no-new-privileges"],
                mem_limit="128m",
                pids_limit=32,
                mounts=[
                    docker.types.Mount(
                        target="/attempt",
                        source=self._state_volume,
                        type="volume",
                        read_only=False,
                        subpath=f"{ATTEMPT_DIRECTORY}/{attempt_id}",
                    )
                ],
                labels={"com.kratos.role": "cleanup", "com.kratos.managed": "true"},
            )
        except docker.errors.APIError:
            return False
        try:
            return int(container.wait(timeout=60)["StatusCode"]) == 0
        except Exception:
            return False
        finally:
            with contextlib.suppress(Exception):
                container.remove(force=True)

    def _kill(self, container: Any, logical_name: str) -> None:
        """Stop a container whose authority has ended, or refuse to report a result."""
        for remaining in range(KILL_ATTEMPTS - 1, -1, -1):
            # The container may simply have exited between the check and the kill.
            with contextlib.suppress(Exception):
                container.kill()
            try:
                container.reload()
                if container.status not in ACTIVE_CONTAINER_STATUSES:
                    return
            except docker.errors.NotFound:
                return
            except Exception:
                # Without a confirmed status the agent cannot claim the workload stopped.
                pass
            if remaining:
                self._sleep(KILL_RETRY_SECONDS)
        raise EnforcementError(f"{logical_name} outlived its authority and could not be stopped")

    def remove_job_container(self, attempt_id: UUID) -> None:
        try:
            self._client.containers.get(_container_name(attempt_id)).remove(force=True)
        except docker.errors.NotFound:
            return

    def stop_unbounded_attempt(self, attempt_id: UUID) -> None:
        """Stop a container whose recorded authority is unknown, keeping it for reporting.

        An agent upgraded in place during an outage can hold an attempt identifier from an
        older state file without the bounds needed to supervise it. It cannot show the
        container is still authorised, so it stops it and leaves it for the control plane.
        """
        try:
            container = self._client.containers.get(_container_name(attempt_id))
        except docker.errors.NotFound:
            return
        container.reload()
        if container.status in ACTIVE_CONTAINER_STATUSES:
            self._kill(container, f"attempt {attempt_id}")

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
