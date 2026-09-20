"""Constrained Docker execution for controlled worker operations."""

import contextlib
import json
import re
import threading
import time
from collections.abc import Callable
from datetime import UTC, datetime, timedelta
from typing import Any
from uuid import UUID

import docker
from pydantic import ValidationError

from kratos_agent.logs import LineAssembler, split_timestamp
from kratos_agent.models import (
    GpuHealthEvidence,
    GpuHealthStatus,
    JobAssignment,
    JobExecutionResult,
)
from kratos_agent.observations import MAX_LINE_BYTES, Stream

# One output line, as the classifier will see it. A plain callable rather than an interface:
# the executor knows nothing about what happens to a line after it hands it over.
LogObserver = Callable[[Stream, datetime, str], None]
# Emitted in place of a resumed container's replayed output, so the absence is visible.
_RESUMED_NOTICE = (
    "kratos: attempt resumed after a restart; earlier output is not re-read, because "
    "replaying it would duplicate every observation under new sequence numbers"
)

IMMUTABLE_IMAGE = re.compile(r"^(?:[^\s@]+@)?sha256:[0-9a-f]{64}$")
MAX_RESULT_BYTES = 64 * 1024
MAX_FAILURE_MESSAGE_CHARS = 1_000
OUTPUT_MOUNT_TARGET = "/kratos/outputs"
ATTEMPT_DIRECTORY = "attempts"
# A little above the classifier's own bound, so a line truncated here is still over that
# bound once its timestamp prefix is removed and is refused rather than silently shortened.
LOG_LINE_BOUND_BYTES = MAX_LINE_BYTES + 128
# ADR-015 requires a bounded, non-blocking local log driver, and both halves matter for a
# different reason.
#
# Bounded, because a workload that prints without restraint would otherwise fill the host disk,
# and the disk it fills is the one holding the agent's own state and every attempt's outputs. Two
# files of 10 MiB is a cap of 20 MiB per container, which is generous for text and small enough
# that a runaway job cannot take the worker down with it.
#
# Non-blocking, because the default driver applies back pressure: when the logging pipe is full
# the container's write blocks, so a slow or stuck reader would stall the workload itself. That
# would make telemetry able to halt execution, which is the one thing ADR-015 forbids outright. A
# full buffer drops lines instead, and a dropped line is a diagnostic gap rather than a hung job.
JOB_LOG_CONFIG = {
    "type": "json-file",
    "config": {
        "max-size": "10m",
        "max-file": "3",
        "mode": "non-blocking",
        "max-buffer-size": "4m",
    },
}
# Volume subpath mounts require Docker Engine 26 or later.
MINIMUM_SUBPATH_ENGINE_MAJOR = 26
# Cleanup runs a known image, not the workload's: an arbitrary image need not contain a
# shell or rm, and a hostile one must never be re-entered to tidy up after itself.
CLEANUP_IMAGE = "busybox@sha256:0872fb3a7632ba9d0ae46a8e832a62b30ce83a6f220b8bb52903d9cf477dabe3"
SUPERVISION_POLL_SECONDS = 1.0
# How late a bound may be enforced: one poll plus one in-flight heartbeat and its collection.
ENFORCEMENT_TOLERANCE = timedelta(seconds=30)
# How hard the agent tries to stop a container before it admits it cannot.
KILL_ATTEMPTS = 3
KILL_RETRY_SECONDS = 1.0
ACTIVE_CONTAINER_STATUSES = frozenset({"running", "paused", "restarting"})


class ExecutorError(RuntimeError):
    """The local container executor could not produce trustworthy evidence."""


class AuthorityLost(ExecutorError):
    """The control plane stopped holding this attempt before its container was created."""


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

    def prepare_job(
        self,
        assignment: JobAssignment,
        still_authorised: Callable[[], bool] | None = None,
        tick_seconds: float = 30,
    ) -> JobExecutionResult | None:
        """Fetch the job image without creating a container.

        A registry that reports the image as absent is a terminal failure. Any other error
        propagates so the caller can retry while the lease remains valid.

        The pull runs on its own thread so ``still_authorised`` can be called about every
        ``tick_seconds`` while it proceeds. A multi-gigabyte image on a home link takes minutes,
        and without this the worker sends no heartbeat for all of it: it reads stale and then
        offline exactly as a job begins, and cannot observe a cancellation until the pull ends.
        Losing authority stops this before any container is created.
        """
        if not IMMUTABLE_IMAGE.fullmatch(assignment.image_reference):
            raise ExecutorError("job image must use an immutable sha256 reference")
        failure: list[BaseException] = []

        def pull() -> None:
            try:
                self._client.images.pull(assignment.image_reference)
            except BaseException as error:  # re-raised on the calling thread below
                failure.append(error)

        thread = threading.Thread(target=pull, name="kratos-image-pull", daemon=True)
        thread.start()
        next_tick = self._clock() + timedelta(seconds=tick_seconds)
        while thread.is_alive():
            if still_authorised is not None and self._clock() >= next_tick:
                if not still_authorised():
                    # The pull continues on its daemon thread and is simply abandoned; nothing
                    # has been created, so there is nothing to clean up.
                    raise AuthorityLost(
                        "the control plane stopped holding this attempt during the image pull"
                    )
                next_tick = self._clock() + timedelta(seconds=tick_seconds)
            self._sleep(SUPERVISION_POLL_SECONDS)
        thread.join()
        if failure:
            if isinstance(failure[0], docker.errors.NotFound):
                return _failure(125, "job image was not found in its registry")
            raise failure[0]
        return None

    def run_job(
        self,
        assignment: JobAssignment,
        *,
        may_start: bool = True,
        on_tick: Callable[[], bool] | None = None,
        tick_seconds: float = 30,
        observe: "LogObserver | None" = None,
    ) -> JobExecutionResult:
        """Supervise the attempt's container until it exits or its authority ends.

        An existing attempt-named container is always resumed rather than replaced. With
        ``may_start`` false a missing container is reported as a failure, because this worker
        has already recorded starting the attempt and cannot prove that it did not run.

        ``on_tick`` is called about every ``tick_seconds`` while the container runs. Returning
        false means the control plane no longer holds the attempt, which ends its authority.

        Authority ends without a grace period: the container is killed, not asked to stop,
        so a workload that ignores SIGTERM cannot run past its bound.

        ``observe`` receives each output line on a separate thread, so nothing about
        telemetry can delay the supervision loop. It is only attached to a container this
        call created: see ``_start_log_reader``.
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
            observed_start = _state_time(container, "StartedAt")
            started_at = observed_start or self._clock()
            resumed = True
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
                    log_config=JOB_LOG_CONFIG,
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
            # Read back rather than assumed: the runtime's own clock is the evidence.
            container.reload()
            observed_start = _state_time(container, "StartedAt")
            resumed = False

        self._start_log_reader(container, observe, resumed=resumed)

        runtime_deadline = started_at + timedelta(seconds=assignment.timeout_seconds)
        deadline = min(runtime_deadline, assignment.lease_expires_at)
        observed_finish: datetime | None = None
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
            # The kill has happened, so the runtime now knows when it finished.
            with contextlib.suppress(Exception):
                container.reload()
                observed_finish = _state_time(container, "FinishedAt")
        else:
            # A container that has already exited is reported with its real exit status, even
            # when a network outage held the result back beyond the lease.
            exit_code = int(container.wait(timeout=10)["StatusCode"])
            finished_at = _state_time(container, "FinishedAt")
            observed_finish = finished_at
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
        # The reader is deliberately left to drain Docker's finite log stream on its daemon
        # thread. The result goes now without waiting for it. Asking it to stop here used to race
        # a short-lived container: Docker could yield the first buffered chunk after supervision
        # observed the exit, at which point the stop flag discarded every line the job wrote.
        # Both or neither: an interval with one end missing is not evidence.
        whole = observed_start is not None and observed_finish is not None
        return JobExecutionResult(
            exit_code=exit_code,
            timed_out=timed_out,
            stdout=stdout,
            stderr=stderr,
            failure_message=failure_message,
            execution_started_at=observed_start if whole else None,
            execution_finished_at=observed_finish if whole else None,
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
        # Cleanup runs after a job has already succeeded, so it must not depend on a registry
        # then. The tool image is fetched now, while the worker is still free to decline the
        # capability, and cleanup only ever uses the local copy.
        try:
            self._client.images.get(CLEANUP_IMAGE)
        except Exception:
            try:
                self._client.images.pull(CLEANUP_IMAGE)
                self._client.images.get(CLEANUP_IMAGE)
            except Exception as error:
                return f"the cleanup image could not be prefetched: {type(error).__name__}"
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

    def discard_attempt_outputs(self, attempt_id: UUID) -> bool:
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
            # Deliberately no pull: the image was prefetched before 1.1 was advertised, so a
            # registry outage cannot strand a worker that has just finished a job.
            container = self._client.containers.run(
                CLEANUP_IMAGE,
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

    def _start_log_reader(
        self,
        container: Any,
        observe: "LogObserver | None",
        *,
        resumed: bool,
    ) -> None:
        """Follow the container's output on its own thread, or decline to.

        A resumed container is deliberately **not** followed. Docker replays a container's log
        from the beginning, and every replayed line would be given a fresh sequence, so the
        control plane would receive the same observations twice under different numbers. Batch
        idempotency does not help: it is keyed on sequence, and these would be new ones. Losing
        telemetry for a resumed attempt is the lesser fault, and it is counted rather than silent.
        """
        if observe is None:
            return
        if resumed:
            observe(Stream.STDERR, self._clock(), _RESUMED_NOTICE)
            return
        thread = threading.Thread(
            target=self._follow_logs,
            args=(container, observe),
            name="kratos-log-reader",
            daemon=True,
        )
        thread.start()

    def _follow_logs(self, container: Any, observe: "LogObserver") -> None:
        """Read until Docker closes the exited container's finite log stream. Never raises."""
        assemblers = {
            Stream.STDOUT: LineAssembler(LOG_LINE_BOUND_BYTES),
            Stream.STDERR: LineAssembler(LOG_LINE_BOUND_BYTES),
        }

        def emit(which: Stream, line: str) -> None:
            at, text = split_timestamp(line, fallback=self._clock())
            observe(which, at, text)

        try:
            for out, err in container.logs(
                stdout=True, stderr=True, stream=True, follow=True, timestamps=True, demux=True
            ):
                for data, which in ((out, Stream.STDOUT), (err, Stream.STDERR)):
                    if not data:
                        continue
                    for line in assemblers[which].feed(data):
                        emit(which, line)
        except Exception:  # noqa: BLE001 - reading output must never end a run
            pass
        finally:
            # A container that exits mid-line still wrote that line, and it is often the one
            # worth having.
            with contextlib.suppress(Exception):
                for which, assembler in assemblers.items():
                    for line in assembler.flush():
                        emit(which, line)

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
