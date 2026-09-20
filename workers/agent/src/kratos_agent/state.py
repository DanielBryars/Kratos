"""Private, atomic persistence for the worker identity and credential."""

import json
import os
import tempfile
from dataclasses import asdict, dataclass, field
from pathlib import Path
from uuid import UUID

from kratos_agent.models import JobAssignment


@dataclass(frozen=True)
class AgentState:
    agent_instance_id: UUID
    worker_id: UUID | None = None
    worker_credential: str | None = field(default=None, repr=False)
    private_key: str | None = field(default=None, repr=False)
    registration_id: UUID | None = None
    next_sequence: int = 0
    heartbeat_interval_seconds: int = 30
    # Recorded before an attempt's container is created so a restarted agent never starts it twice.
    started_attempt_id: UUID | None = None
    # The attempt's whole execution authority, so a restarted agent can enforce its bounds
    # without first reaching the control plane.
    started_assignment: JobAssignment | None = None

    @property
    def is_enrolled(self) -> bool:
        return self.worker_id is not None and self.worker_credential is not None


def load_state(path: Path) -> AgentState | None:
    if not path.exists():
        return None
    payload = json.loads(path.read_text(encoding="utf-8"))
    if payload.get("schema_version") not in (1, 2):
        raise ValueError("agent state schema is unsupported")
    return AgentState(
        agent_instance_id=UUID(payload["agent_instance_id"]),
        worker_id=UUID(payload["worker_id"]) if payload.get("worker_id") else None,
        worker_credential=payload.get("worker_credential"),
        private_key=payload.get("private_key"),
        registration_id=(
            UUID(payload["registration_id"]) if payload.get("registration_id") else None
        ),
        next_sequence=int(payload["next_sequence"]),
        heartbeat_interval_seconds=int(payload["heartbeat_interval_seconds"]),
        started_attempt_id=(
            UUID(payload["started_attempt_id"]) if payload.get("started_attempt_id") else None
        ),
        started_assignment=(
            JobAssignment.model_validate(payload["started_assignment"])
            if payload.get("started_assignment")
            else None
        ),
    )


def save_state(path: Path, state: AgentState) -> None:
    path.parent.mkdir(mode=0o700, parents=True, exist_ok=True)
    payload = asdict(state)
    payload["schema_version"] = 2
    payload["agent_instance_id"] = str(state.agent_instance_id)
    payload["worker_id"] = str(state.worker_id) if state.worker_id else None
    payload["registration_id"] = str(state.registration_id) if state.registration_id else None
    payload["started_attempt_id"] = (
        str(state.started_attempt_id) if state.started_attempt_id else None
    )
    payload["started_assignment"] = (
        state.started_assignment.model_dump(mode="json") if state.started_assignment else None
    )

    descriptor, temporary_name = tempfile.mkstemp(prefix=".state-", dir=path.parent)
    temporary_path = Path(temporary_name)
    try:
        os.chmod(temporary_path, 0o600)
        with os.fdopen(descriptor, "w", encoding="utf-8") as stream:
            json.dump(payload, stream, separators=(",", ":"))
            stream.flush()
            os.fsync(stream.fileno())
        temporary_path.replace(path)
        os.chmod(path, 0o600)
    except BaseException:
        temporary_path.unlink(missing_ok=True)
        raise


def read_enrolment_credential(path: Path) -> str:
    credential = path.read_text(encoding="utf-8").strip()
    if not credential.startswith("ken_"):
        raise ValueError("enrolment credential file is invalid")
    return credential
