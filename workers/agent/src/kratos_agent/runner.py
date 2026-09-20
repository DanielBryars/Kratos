"""Worker enrolment and heartbeat lifecycle."""

import base64
import json
import time
from collections.abc import Callable
from dataclasses import replace
from datetime import UTC, datetime
from pathlib import Path
from uuid import uuid4

import httpx
from cryptography.hazmat.primitives import serialization
from cryptography.hazmat.primitives.asymmetric.ed25519 import Ed25519PrivateKey
from docker.errors import DockerException

from kratos_agent.capabilities import collect_capabilities
from kratos_agent.executor import DockerExecutor, ExecutorError
from kratos_agent.models import HeartbeatResponse, JobAssignment, WorkerCapabilities
from kratos_agent.protocol import ControlPlaneError, WorkerProtocolClient
from kratos_agent.state import AgentState, load_state, read_enrolment_credential, save_state


class AgentRunner:
    def __init__(
        self,
        client: WorkerProtocolClient,
        display_name: str,
        state_path: Path,
        enrolment_credential_path: Path | None,
        capability_collector: Callable[[], WorkerCapabilities] = collect_capabilities,
        executor: DockerExecutor | None = None,
    ) -> None:
        self._client = client
        self._display_name = display_name
        self._state_path = state_path
        self._enrolment_credential_path = enrolment_credential_path
        self._collect = capability_collector
        self._executor = executor

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
        capabilities = self._collect()
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
            updated = replace(updated, started_attempt_id=None)
            save_state(self._state_path, updated)
        if assignment is not None:
            updated = self._run_assignment(updated, assignment)
        return updated

    def _run_assignment(self, state: AgentState, assignment: JobAssignment) -> AgentState:
        if self._executor is None:
            raise ExecutorError("job assignment received without a configured executor")
        worker_id, worker_credential = state.worker_id, state.worker_credential
        if worker_id is None or worker_credential is None:
            raise ValueError("worker is not enrolled")
        resuming = state.started_attempt_id == assignment.attempt_id
        result = None
        if not resuming and datetime.now(UTC) < assignment.lease_expires_at:
            result = self._executor.prepare_job(assignment)
            if result is None:
                state = replace(state, started_attempt_id=assignment.attempt_id)
                save_state(self._state_path, state)

        def still_authorised() -> bool:
            """Heartbeat while the container runs; an outage never interrupts the workload."""
            nonlocal state
            try:
                state, response = self._exchange_heartbeat(state)
            except (ControlPlaneError, httpx.TransportError) as error:
                print(json.dumps({"status": "retrying", "detail": str(error)}), flush=True)
                state = load_state(self._state_path) or state
                return not _is_rejection(error)
            held = response.assignment
            return held is not None and held.attempt_id == assignment.attempt_id

        if result is None:
            result = self._executor.run_job(
                assignment,
                may_start=not resuming,
                on_tick=still_authorised,
                tick_seconds=state.heartbeat_interval_seconds,
            )
        acknowledgement = self._client.report_job_result(
            worker_id, worker_credential, assignment.attempt_id, result
        )
        if acknowledgement.attempt_id != assignment.attempt_id:
            raise ControlPlaneError(
                200, "invalid_response", "job result acknowledgement is inconsistent"
            )
        self._executor.remove_job_container(assignment.attempt_id)
        state = replace(state, started_attempt_id=None)
        save_state(self._state_path, state)
        return state

    def step(self, state: AgentState) -> AgentState:
        """Send one heartbeat, surviving a temporary loss of the control plane or Docker.

        Every state change is persisted before the operation that depends on it, so the saved
        state is reloaded after a failure and the next heartbeat resumes the same attempt.
        """
        try:
            return self.heartbeat_once(state)
        except (ControlPlaneError, DockerException, httpx.TransportError) as error:
            if isinstance(error, ControlPlaneError) and not _is_transient(error):
                raise
            print(json.dumps({"status": "retrying", "detail": str(error)}), flush=True)
            return load_state(self._state_path) or state

    def run(self) -> None:
        state = self.ensure_enrolled()
        while True:
            state = self.step(state)
            time.sleep(state.heartbeat_interval_seconds)


def _is_transient(error: ControlPlaneError) -> bool:
    return error.status_code == 429 or error.status_code >= 500


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
