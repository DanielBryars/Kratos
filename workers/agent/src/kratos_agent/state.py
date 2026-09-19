"""Private, atomic persistence for the worker identity and credential."""

import json
import os
import tempfile
from dataclasses import asdict, dataclass, field
from pathlib import Path
from uuid import UUID


@dataclass(frozen=True)
class AgentState:
    agent_instance_id: UUID
    worker_id: UUID
    worker_credential: str = field(repr=False)
    next_sequence: int = 0
    heartbeat_interval_seconds: int = 30


def load_state(path: Path) -> AgentState | None:
    if not path.exists():
        return None
    payload = json.loads(path.read_text(encoding="utf-8"))
    if payload.get("schema_version") != 1:
        raise ValueError("agent state schema is unsupported")
    return AgentState(
        agent_instance_id=UUID(payload["agent_instance_id"]),
        worker_id=UUID(payload["worker_id"]),
        worker_credential=payload["worker_credential"],
        next_sequence=int(payload["next_sequence"]),
        heartbeat_interval_seconds=int(payload["heartbeat_interval_seconds"]),
    )


def save_state(path: Path, state: AgentState) -> None:
    path.parent.mkdir(mode=0o700, parents=True, exist_ok=True)
    payload = asdict(state)
    payload["schema_version"] = 1
    payload["agent_instance_id"] = str(state.agent_instance_id)
    payload["worker_id"] = str(state.worker_id)

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
