"""Worker enrolment and heartbeat lifecycle."""

import time
from collections.abc import Callable
from pathlib import Path
from uuid import uuid4

from kratos_agent.capabilities import collect_capabilities
from kratos_agent.models import WorkerCapabilities
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
    ) -> None:
        self._client = client
        self._display_name = display_name
        self._state_path = state_path
        self._enrolment_credential_path = enrolment_credential_path
        self._collect = capability_collector

    def ensure_enrolled(self) -> AgentState:
        existing = load_state(self._state_path)
        if existing is not None:
            return existing
        if self._enrolment_credential_path is None:
            raise ValueError("an enrolment credential file is required for first registration")

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

    def heartbeat_once(self, state: AgentState) -> AgentState:
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
        updated = AgentState(
            agent_instance_id=state.agent_instance_id,
            worker_id=state.worker_id,
            worker_credential=state.worker_credential,
            next_sequence=response.accepted_sequence + 1,
            heartbeat_interval_seconds=response.next_heartbeat_seconds,
        )
        save_state(self._state_path, updated)
        return updated

    def run(self) -> None:
        state = self.ensure_enrolled()
        while True:
            state = self.heartbeat_once(state)
            time.sleep(state.heartbeat_interval_seconds)
