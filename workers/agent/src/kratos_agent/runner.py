"""Worker enrolment and heartbeat lifecycle."""

import base64
import json
import time
from collections.abc import Callable
from dataclasses import replace
from datetime import UTC, datetime, timedelta
from pathlib import Path
from uuid import UUID, uuid4, uuid5

import httpx
from cryptography.hazmat.primitives import serialization
from cryptography.hazmat.primitives.asymmetric.ed25519 import Ed25519PrivateKey
from docker.errors import DockerException

from kratos_agent.capabilities import collect_capabilities
from kratos_agent.courier import ObservationCourier
from kratos_agent.executor import (
    ATTEMPT_DIRECTORY,
    AuthorityLost,
    CleanupError,
    DockerExecutor,
    EnforcementError,
    ExecutorError,
)
from kratos_agent.models import (
    ArtifactResponse,
    HeartbeatResponse,
    JobAssignment,
    JobExecutionResult,
    WorkerCapabilities,
)
from kratos_agent.observations import ObservationCollector
from kratos_agent.outputs import (
    OutputError,
    VerifiedOutput,
    build_manifest,
    create_attempt_tree,
    discard_tree,
)
from kratos_agent.protocol import ControlPlaneError, WorkerProtocolClient
from kratos_agent.pump import CONTROL_PLANE_SINK, ObservationPump, batch_records
from kratos_agent.spool import Batch, ObservationSpool
from kratos_agent.state import AgentState, load_state, read_enrolment_credential, save_state
from kratos_agent.uploads import UploadConflict, UploadError, upload_object

# A fixed namespace makes the manifest identifier a pure function of the attempt, so a retry
# after a lost response declares the same manifest rather than conflicting with itself.
MANIFEST_NAMESPACE = UUID("6f5bb2ac-0f0a-4c6f-9a4a-0a5f1e0c7d21")
# One replacement session per artefact per pass; a further failure waits for the next tick.
UPLOAD_SESSION_ATTEMPTS = 2
VERIFIED = "verified"
REJECTED = "rejected"
# Every state from which this worker may still act. Anything else is a contract change and is
# treated as an invalid response rather than guessed at.
DELIVERABLE = frozenset({"declared", "uploading"})


class CapabilityCollectionError(RuntimeError):
    """This host's capabilities could not be observed for one heartbeat."""


class AgentRunner:
    def __init__(
        self,
        client: WorkerProtocolClient,
        display_name: str,
        state_path: Path,
        enrolment_credential_path: Path | None,
        capability_collector: Callable[[], WorkerCapabilities] = collect_capabilities,
        executor: DockerExecutor | None = None,
        clock: Callable[[], datetime] = lambda: datetime.now(UTC),
    ) -> None:
        self._client = client
        self._display_name = display_name
        self._state_path = state_path
        self._enrolment_credential_path = enrolment_credential_path
        self._collect = capability_collector
        self._executor = executor
        # One courier for the agent, not one per assignment. Created on first use.
        self._courier: ObservationCourier | None = None
        self._clock = clock

    def ensure_enrolled(self) -> AgentState:
        existing = load_state(self._state_path)
        if existing is not None and existing.is_enrolled:
            return existing
        if self._enrolment_credential_path is not None:
            credential = read_enrolment_credential(self._enrolment_credential_path)
            agent_instance_id = uuid4()
            capabilities = self._collect()
            response = self._client.enrol(
                credential, agent_instance_id, self._display_name, capabilities
            )
            state = AgentState(
                agent_instance_id=agent_instance_id,
                worker_id=response.worker_id,
                worker_credential=response.worker_credential,
                heartbeat_interval_seconds=response.heartbeat_interval_seconds,
            )
            save_state(self._state_path, state)
            return state

        pending = existing or self._new_registration_identity()
        if pending.private_key is None:
            raise ValueError("pending registration has no private key")
        private_key = Ed25519PrivateKey.from_private_bytes(_decode(pending.private_key))
        public_key_bytes = private_key.public_key().public_bytes(
            serialization.Encoding.Raw, serialization.PublicFormat.Raw
        )
        public_key = _encode(public_key_bytes)
        local_code = _confirmation_code(public_key_bytes)
        registration_id = pending.registration_id
        if registration_id is None:
            capabilities = self._collect()
            created = self._client.request_registration(
                pending.agent_instance_id, self._display_name, public_key, capabilities
            )
            if created.confirmation_code != local_code:
                raise ControlPlaneError(
                    200, "invalid_response", "confirmation code is inconsistent"
                )
            registration_id = created.registration_id
            pending = AgentState(
                agent_instance_id=pending.agent_instance_id,
                private_key=pending.private_key,
                registration_id=registration_id,
            )
            save_state(self._state_path, pending)
        print(
            f"Registration requested for {self._display_name}. "
            f"Confirm code {local_code} at the Kratos operator console.",
            flush=True,
        )
        while True:
            status = self._client.registration_status(registration_id)
            if status.state in {"approved", "claimed"}:
                if status.claim_challenge is None:
                    raise ControlPlaneError(
                        200, "invalid_response", "approved request has no challenge"
                    )
                message = (
                    f"kratos-worker-claim-v1\n{registration_id}\n{status.claim_challenge}"
                ).encode()
                signature = _encode(private_key.sign(message))
                response = self._client.claim_registration(registration_id, signature)
                break
            if status.state == "expired":
                created = self._client.request_registration(
                    pending.agent_instance_id,
                    self._display_name,
                    public_key,
                    self._collect(),
                )
                if created.confirmation_code != local_code:
                    raise ControlPlaneError(
                        200, "invalid_response", "confirmation code is inconsistent"
                    )
                registration_id = created.registration_id
                pending = AgentState(
                    agent_instance_id=pending.agent_instance_id,
                    private_key=pending.private_key,
                    registration_id=registration_id,
                )
                save_state(self._state_path, pending)
                continue
            if status.state == "rejected":
                raise ControlPlaneError(
                    409,
                    f"registration_{status.state}",
                    f"registration request is {status.state}",
                )
            time.sleep(status.poll_interval_seconds)

        state = AgentState(
            agent_instance_id=pending.agent_instance_id,
            worker_id=response.worker_id,
            worker_credential=response.worker_credential,
            private_key=pending.private_key,
            heartbeat_interval_seconds=response.heartbeat_interval_seconds,
        )
        save_state(self._state_path, state)
        return state

    def _new_registration_identity(self) -> AgentState:
        private_key = Ed25519PrivateKey.generate()
        encoded = _encode(
            private_key.private_bytes(
                serialization.Encoding.Raw,
                serialization.PrivateFormat.Raw,
                serialization.NoEncryption(),
            )
        )
        state = AgentState(agent_instance_id=uuid4(), private_key=encoded)
        save_state(self._state_path, state)
        return state

    def _exchange_heartbeat(self, state: AgentState) -> tuple[AgentState, HeartbeatResponse]:
        if state.worker_id is None or state.worker_credential is None:
            raise ValueError("worker is not enrolled")
        try:
            capabilities = self._collect()
        except Exception as error:
            raise CapabilityCollectionError(f"capability collection failed: {error}") from error
        response = self._client.heartbeat(
            state.worker_id,
            state.worker_credential,
            state.next_sequence,
            capabilities,
        )
        if (
            response.worker_id != state.worker_id
            or response.accepted_sequence != state.next_sequence
        ):
            raise ControlPlaneError(
                200, "invalid_response", "heartbeat acknowledgement is inconsistent"
            )
        updated = replace(
            state,
            next_sequence=response.accepted_sequence + 1,
            heartbeat_interval_seconds=response.next_heartbeat_seconds,
        )
        save_state(self._state_path, updated)
        return updated, response

    def heartbeat_once(self, state: AgentState) -> AgentState:
        updated, response = self._exchange_heartbeat(state)
        assignment = response.assignment
        stale_attempt_id = updated.started_attempt_id
        if (
            self._executor is not None
            and stale_attempt_id is not None
            and (assignment is None or assignment.attempt_id != stale_attempt_id)
        ):
            # The control plane no longer holds this attempt open, so nothing authorises its
            # container to keep running and its retained evidence is no longer required.
            self._executor.remove_job_container(stale_attempt_id)
            updated = replace(
                updated,
                started_attempt_id=None,
                started_assignment=None,
                retained_attempt_ids=_with(updated.retained_attempt_ids, stale_attempt_id),
            )
            save_state(self._state_path, updated)
            updated = self._clear_retained(updated)
        if assignment is not None:
            updated = self._run_assignment(updated, assignment)
        return updated

    def _observations_root(self) -> Path:
        return self._state_path.parent / "observations"

    def _observation_directory(self, attempt_id: UUID) -> Path:
        return self._observations_root() / str(attempt_id)

    def _courier_for(self, state: AgentState) -> ObservationCourier | None:
        """The agent's one courier, created on first use and never stopped for a job.

        Delivery deliberately does not belong to the assignment. The first version of this gave
        each attempt its own thread and stopped it on the result path, so a control plane that was
        briefly unreachable could let the job report its result, watch the attempt disappear from
        polling, and leave records nothing would ever send.
        """
        worker_id, credential = state.worker_id, state.worker_credential
        if worker_id is None or credential is None:
            return None
        if self._courier is None:
            self._courier = ObservationCourier(
                self._observations_root(), self._submit_batch(worker_id, credential)
            )
            # Anything an earlier process left behind is picked up here, before any job runs.
            self._courier.discover()
            self._courier.start()
        return self._courier

    def _submit_batch(self, worker_id: UUID, credential: str) -> Callable[[UUID, Batch], int]:
        """Address a batch by the stream the spool recorded, not by an assignment.

        The courier delivers spools whose attempt finished long ago, possibly in another process,
        so the stream identifier comes from the spool rather than from anything still in memory.
        """

        def submit(stream_id: UUID, batch: Batch) -> int:
            response = self._client.submit_observation_batch(
                worker_id,
                credential,
                stream_id,
                batch.batch_id,
                batch.first_sequence,
                batch_records(batch),
            )
            if response.stream_id != stream_id or response.batch_id != batch.batch_id:
                raise ControlPlaneError(
                    200, "invalid_response", "observation acknowledgement is inconsistent"
                )
            return response.accepted_through_sequence

        return submit

    def _build_pump(self, state: AgentState, assignment: JobAssignment) -> ObservationPump | None:
        """Create the pump for this attempt, or decline when there is nowhere to send.

        No stream means the control plane did not allocate one, which is how it addresses an
        agent below protocol 1.2 and how it behaves before its own side is deployed. Collecting
        observations with nowhere to put them would fill the state volume for nothing.
        """
        stream_id = assignment.observation_stream_id
        courier = self._courier_for(state)
        if stream_id is None or courier is None:
            return None
        directory = self._observation_directory(assignment.attempt_id)
        try:
            spool = ObservationSpool(directory, (CONTROL_PLANE_SINK,))
            # Recorded before a single line is collected. A spool that cannot say where it is
            # addressed can never be delivered by a later process.
            spool.record_stream(stream_id)
        except Exception as error:  # noqa: BLE001 - telemetry never stops a job starting
            print(json.dumps({"status": "observations_unavailable", "detail": str(error)}))
            return None

        courier.adopt(spool, directory)
        return ObservationPump(
            spool=spool,
            collector=ObservationCollector(clock=time.monotonic),
            clock=time.monotonic,
        )

    def _attempt_directory_for(self, attempt_id: UUID) -> Path:
        return self._state_path.parent / ATTEMPT_DIRECTORY / str(attempt_id)

    def _attempt_directory(self, assignment: JobAssignment) -> Path:
        return self._attempt_directory_for(assignment.attempt_id)

    def _outputs_directory(self, assignment: JobAssignment) -> Path:
        return self._attempt_directory(assignment) / "outputs"

    def _clear_retained(self, state: AgentState) -> AgentState:
        """Remove every output tree this agent still owes, and keep what it could not."""
        remaining = tuple(
            attempt_id
            for attempt_id in state.retained_attempt_ids
            if not self._discard_outputs_for(attempt_id)
        )
        if remaining != state.retained_attempt_ids:
            state = replace(state, retained_attempt_ids=remaining)
            save_state(self._state_path, state)
        return state

    def _discard_outputs_for(self, attempt_id: UUID) -> bool:
        """Remove one attempt's outputs, escalating past a tree the agent cannot traverse.

        A workload runs as an arbitrary user and can leave such a tree, so a failure escalates
        to a throwaway container bounded to this attempt's subpath and running a known tool
        image rather than the workload's own.
        """
        if discard_tree(self._attempt_directory_for(attempt_id)):
            return True
        if self._executor is None:
            return False
        if not self._executor.discard_attempt_outputs(attempt_id):
            return False
        return discard_tree(self._attempt_directory_for(attempt_id))

    def _delivery_tick(
        self, state_holder: list[AgentState], assignment: JobAssignment
    ) -> Callable[[], bool]:
        """Heartbeat while delivering, and report whether the attempt is still held.

        Delivery runs after the container has stopped and can outlast several heartbeat intervals,
        so the worker must keep reporting or it reads as stale and cannot observe a cancellation.
        The execution lease is never extended: this only proves that the control plane still holds
        the attempt, which is the server's own delivery window rather than the workload's.
        """
        due = [self._clock() + timedelta(seconds=state_holder[0].heartbeat_interval_seconds)]

        def tick() -> bool:
            if self._clock() < due[0]:
                return True
            try:
                state_holder[0], response = self._exchange_heartbeat(state_holder[0])
            except Exception as error:
                print(json.dumps({"status": "retrying", "detail": str(error)}), flush=True)
                state_holder[0] = load_state(self._state_path) or state_holder[0]
                due[0] = self._clock() + timedelta(
                    seconds=state_holder[0].heartbeat_interval_seconds
                )
                # Only an explicit refusal withdraws authority; being unreachable does not.
                return not _is_rejection(error)
            due[0] = self._clock() + timedelta(seconds=state_holder[0].heartbeat_interval_seconds)
            held = response.assignment
            return held is not None and held.attempt_id == assignment.attempt_id

        return tick

    def _deliver_outputs(
        self, state: AgentState, assignment: JobAssignment, result: JobExecutionResult
    ) -> tuple[AgentState, JobExecutionResult]:
        """Declare and upload this attempt's outputs before its result is reported.

        A job that produced no usable outputs becomes a failure: the control plane refuses a
        successful result while a mandatory artefact is unverified, so reporting success here
        would leave the attempt stuck rather than honestly failed.
        """
        worker_id, credential = state.worker_id, state.worker_credential
        if worker_id is None or credential is None:
            raise ValueError("worker is not enrolled")
        state_holder = [state]
        tick = self._delivery_tick(state_holder, assignment)
        try:
            outputs = build_manifest(
                self._outputs_directory(assignment),
                assignment.output_requirements,
                still_authorised=tick,
            )
        except OutputError as error:
            if result.exit_code == 0 and not result.timed_out:
                return state_holder[0], result.model_copy(
                    update={
                        "exit_code": 125,
                        "failure_message": f"outputs could not be collected: {error}"[:1000],
                    }
                )
            # The workload had already failed; its missing outputs are not the reason.
            return state_holder[0], result

        manifest = self._client.declare_artifact_manifest(
            worker_id,
            credential,
            assignment.attempt_id,
            _manifest_id(assignment.attempt_id),
            tuple(output.file for output in outputs),
        )
        by_path = {output.file.logical_path: output for output in outputs}
        for artifact in manifest.artifacts:
            if artifact.status == VERIFIED:
                continue
            if artifact.status == REJECTED:
                # The control plane has durably refused this object. Retrying can never verify it,
                # and raising would restart this attempt forever, so the execution becomes a
                # bounded delivery failure instead.
                return state_holder[0], result.model_copy(
                    update={
                        "exit_code": 125,
                        "failure_message": (
                            f"output {artifact.logical_path!r} was rejected by the control plane"
                        )[:1000],
                    }
                )
            if artifact.status not in DELIVERABLE:
                raise ControlPlaneError(
                    200, "invalid_response", f"artefact is in unknown state {artifact.status!r}"
                )
            output = by_path.get(artifact.logical_path)
            if output is None:
                raise ControlPlaneError(
                    200, "invalid_response", "manifest named an output this worker did not declare"
                )
            if artifact.storage_generation is not None:
                # The bytes already reached Cloud Storage and the server recorded the generation;
                # only the completion acknowledgement was lost. Beginning again would be refused.
                generation = artifact.storage_generation
            else:
                generation = self._transfer(
                    state_holder, assignment, artifact, output, worker_id, credential, tick
                )
            completed = self._client.complete_artifact_upload(
                worker_id,
                credential,
                assignment.attempt_id,
                output.file,
                artifact.artifact_id,
                generation,
            )
            _check_completed(completed, artifact, generation)
        return state_holder[0], result

    def _transfer(
        self,
        state_holder: list[AgentState],
        assignment: JobAssignment,
        artifact: ArtifactResponse,
        output: VerifiedOutput,
        worker_id: UUID,
        credential: str,
        still_authorised: Callable[[], bool],
    ) -> int:
        """Send one artefact, replacing a session Cloud Storage has permanently rejected."""
        source = self._outputs_directory(assignment) / artifact.logical_path
        for remaining in range(UPLOAD_SESSION_ATTEMPTS - 1, -1, -1):
            begun = self._client.begin_artifact_upload(
                worker_id, credential, assignment.attempt_id, artifact.artifact_id
            )
            try:
                completed = upload_object(
                    self._client.storage,
                    begun.session.uri,
                    source,
                    output.identity,
                    artifact.byte_length,
                    still_authorised=still_authorised,
                )
            except UploadConflict:
                if not remaining:
                    raise
                # A dead session is only replaced once the control plane has consumed it.
                self._client.abandon_artifact_upload(
                    worker_id,
                    credential,
                    assignment.attempt_id,
                    artifact.artifact_id,
                    begun.session.uri,
                )
                continue
            return completed.storage_generation
        raise UploadError("no usable upload session for this artefact")

    def _run_assignment(self, state: AgentState, assignment: JobAssignment) -> AgentState:
        if self._executor is None:
            raise ExecutorError("job assignment received without a configured executor")
        worker_id, worker_credential = state.worker_id, state.worker_credential
        if worker_id is None or worker_credential is None:
            raise ValueError("worker is not enrolled")
        resuming = state.started_attempt_id == assignment.attempt_id
        if not resuming and state.retained_attempt_ids:
            # Taking new work while an earlier attempt's data is still on disk is how the
            # state volume fills unnoticed. Retry that first; the lease will be reassigned.
            raise CleanupError(
                f"{len(state.retained_attempt_ids)} attempt output trees are still retained"
            )
        result = None
        authorised = True
        pump = self._build_pump(state, assignment)
        if not resuming and datetime.now(UTC) < assignment.lease_expires_at:
            # The pull can take minutes for a multi-gigabyte image, so it heartbeats too.
            pull_state = [state]
            result = self._executor.prepare_job(
                assignment,
                still_authorised=self._delivery_tick(pull_state, assignment),
                tick_seconds=state.heartbeat_interval_seconds,
            )
            state = pull_state[0]
            if result is None:
                if assignment.output_requirements:
                    # Docker needs the subpath to exist before it creates the container.
                    create_attempt_tree(self._attempt_directory(assignment))
                state = replace(
                    state, started_attempt_id=assignment.attempt_id, started_assignment=assignment
                )
                save_state(self._state_path, state)
        if result is None:
            state, result, authorised = self._supervise(
                state, assignment, may_start=not resuming, pump=pump
            )
        # A snapshot, taken now. Delivery is the courier's business and carries on without this
        # result, past it, and if necessary into another process.
        result = _with_observations(result, pump)
        succeeded = result.exit_code == 0 and not result.timed_out and not result.failure_message
        if assignment.output_requirements and authorised and succeeded:
            # Only a successful result is gated on verified outputs. A failed or timed-out
            # workload is reported through the critical path, so storage being unavailable can
            # never hide the failure or leave the worker occupied.
            # Delivery advances the heartbeat sequence, so its state is what continues.
            state, result = self._deliver_outputs(state, assignment, result)
        acknowledgement = self._client.report_job_result(
            worker_id, worker_credential, assignment.attempt_id, result
        )
        if acknowledgement.attempt_id != assignment.attempt_id:
            raise ControlPlaneError(
                200, "invalid_response", "job result acknowledgement is inconsistent"
            )
        self._executor.remove_job_container(assignment.attempt_id)
        # Record the obligation before clearing the attempt: the assignment that named this
        # tree is gone once its result is acknowledged, but the tree is not.
        state = replace(
            state,
            started_attempt_id=None,
            started_assignment=None,
            retained_attempt_ids=_with(state.retained_attempt_ids, assignment.attempt_id),
        )
        save_state(self._state_path, state)
        state = self._clear_retained(state)
        return state

    def _supervise(
        self,
        state: AgentState,
        assignment: JobAssignment,
        *,
        may_start: bool,
        pump: ObservationPump | None = None,
    ) -> tuple[AgentState, JobExecutionResult, bool]:
        """Run the attempt's container to the end of its authority, heartbeating meanwhile."""
        if self._executor is None:
            raise ExecutorError("job assignment received without a configured executor")

        authorised = True

        def still_authorised() -> bool:
            nonlocal state, authorised
            try:
                state, response = self._exchange_heartbeat(state)
            except Exception as error:
                # Nothing that goes wrong in a heartbeat may abandon supervision of the container,
                # and an outage never interrupts the workload.
                print(json.dumps({"status": "retrying", "detail": str(error)}), flush=True)
                state = load_state(self._state_path) or state
                return not _is_rejection(error)
            held = response.assignment
            authorised = held is not None and held.attempt_id == assignment.attempt_id
            return authorised

        result = self._executor.run_job(
            assignment,
            may_start=may_start,
            on_tick=still_authorised,
            tick_seconds=state.heartbeat_interval_seconds,
            observe=None if pump is None else pump.ingest,
        )
        return state, result, authorised

    def step(self, state: AgentState) -> AgentState:
        """Send one heartbeat, surviving a temporary loss of the control plane or Docker.

        Every state change is persisted before the operation that depends on it, so the saved
        state is reloaded after a failure and the next heartbeat resumes the same attempt.
        """
        try:
            recorded_attempt_id = state.started_attempt_id
            if self._executor is not None and recorded_attempt_id is not None:
                # A recorded attempt is supervised to the end of its authority before anything
                # that needs the control plane, so a restart during an outage still enforces
                # its bounds. The heartbeat below then reports the finished container.
                if state.started_assignment is not None:
                    state = self._supervise(state, state.started_assignment, may_start=False)[0]
                else:
                    # A state file written before the bounds were recorded.
                    self._executor.stop_unbounded_attempt(recorded_attempt_id)
            state = self._clear_retained(state)
            return self.heartbeat_once(state)
        except (
            AuthorityLost,
            CapabilityCollectionError,
            CleanupError,
            DockerException,
            EnforcementError,
            UploadError,
            httpx.TransportError,
            ControlPlaneError,
        ) as error:
            if isinstance(error, ControlPlaneError) and not _is_transient(error):
                raise
            if isinstance(error, UploadConflict):
                # The session is gone. The next pass requests a fresh one for this attempt.
                print(json.dumps({"status": "upload_session_lost"}), flush=True)
            print(json.dumps({"status": "retrying", "detail": str(error)}), flush=True)
            return load_state(self._state_path) or state

    def run(self) -> None:
        state = self.ensure_enrolled()
        while True:
            state = self.step(state)
            time.sleep(state.heartbeat_interval_seconds)


def _check_completed(
    completed: ArtifactResponse, declared: ArtifactResponse, generation: int
) -> None:
    """Refuse to treat an upload as done unless the server says this exact object is verified."""
    if completed.artifact_id != declared.artifact_id:
        raise ControlPlaneError(200, "invalid_response", "completion named a different artefact")
    if completed.storage_generation != generation:
        raise ControlPlaneError(
            200, "invalid_response", "completion named a different object generation"
        )
    if completed.sha256 != declared.sha256 or completed.crc32c != declared.crc32c:
        raise ControlPlaneError(
            200, "invalid_response", "completion returned different content checksums"
        )
    if completed.status != VERIFIED or completed.verification_pending:
        raise ControlPlaneError(
            200,
            "invalid_response",
            f"artefact is {completed.status!r} after completion, not verified",
        )


def _with(existing: tuple[UUID, ...], attempt_id: UUID) -> tuple[UUID, ...]:
    return existing if attempt_id in existing else (*existing, attempt_id)


def _manifest_id(attempt_id: UUID) -> UUID:
    """One stable manifest identifier per attempt, so a replay is the same manifest."""
    return uuid5(MANIFEST_NAMESPACE, str(attempt_id))


def _is_transient(error: ControlPlaneError) -> bool:
    return error.status_code == 429 or error.status_code >= 500


def _with_observations(
    result: JobExecutionResult, pump: "ObservationPump | None"
) -> JobExecutionResult:
    """Put the observation counters on the result, if there are any.

    A snapshot, taken now, because the result is reported without waiting for delivery to finish.
    The field is omitted when nothing was counted: the model refuses an empty object, so an
    absent field and "nothing happened" are the same thing rather than two.
    """
    if pump is None:
        return result
    try:
        counters = pump.counters()
    except Exception:  # noqa: BLE001 - a result is never lost over its telemetry
        return result
    if not counters:
        return result
    return result.model_copy(update={"observation_counters": counters})


def _is_rejection(error: Exception) -> bool:
    """Whether the control plane explicitly refused this worker, rather than being unreachable."""
    return (
        isinstance(error, ControlPlaneError)
        and 400 <= error.status_code < 500
        and error.status_code != 429
    )


def _encode(value: bytes) -> str:
    return base64.urlsafe_b64encode(value).rstrip(b"=").decode("ascii")


def _decode(value: str) -> bytes:
    return base64.urlsafe_b64decode(value + "=" * (-len(value) % 4))


def _confirmation_code(public_key: bytes) -> str:
    alphabet = "ABCDEFGHJKLMNPQRSTUVWXYZ23456789"
    value = int.from_bytes(public_key[:5], "big")
    characters: list[str] = []
    for _ in range(8):
        characters.append(alphabet[value & 31])
        value >>= 5
    return "".join(characters[:4]) + "-" + "".join(characters[4:])
