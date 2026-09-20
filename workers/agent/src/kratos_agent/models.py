"""Versioned messages shared with the Kratos worker API."""

from datetime import datetime
from enum import StrEnum
from typing import Annotated
from uuid import UUID

from pydantic import AfterValidator, BaseModel, ConfigDict, Field, model_validator

# 1.1 adds the durable output extension. The agent advertises it only when it can collect and
# upload outputs, because the control plane withholds jobs with output requirements from 1.0.
PROTOCOL_VERSION = "1.1"
# What an agent advertises when it cannot deliver durable outputs. The control plane never
# assigns a job with output requirements to a 1.0 worker, which is the point.
PROTOCOL_VERSION_WITHOUT_OUTPUTS = "1.0"
MAX_OUTPUT_FILES = 100
MAX_OUTPUT_FILE_BYTES = 5 * 1024**3
MAX_OUTPUT_TOTAL_BYTES = 10 * 1024**3
MAX_LOGICAL_PATH_BYTES = 240


class StrictModel(BaseModel):
    """Reject unknown fields so protocol drift fails visibly."""

    model_config = ConfigDict(extra="forbid")


class GpuCapability(StrictModel):
    index: int = Field(ge=0)
    name: str = Field(min_length=1)
    memory_total_bytes: int = Field(gt=0)
    driver_version: str = Field(min_length=1)


class GpuHealthStatus(StrEnum):
    UNAVAILABLE = "unavailable"
    UNVERIFIED = "unverified"
    HEALTHY = "healthy"
    UNHEALTHY = "unhealthy"


class GpuHealthEvidence(StrictModel):
    schema_version: str = Field(pattern=r"^1\.[0-9]+$")
    status: GpuHealthStatus
    checked_at: datetime
    image_reference: str = Field(min_length=1)
    device_index: int | None = Field(default=None, ge=0)
    device_name: str | None = None
    operation: str | None = None
    matrix_size: int | None = Field(default=None, gt=0)
    max_absolute_error: float | None = Field(default=None, ge=0)
    duration_ms: float | None = Field(default=None, ge=0)
    cuda_driver_api_version: str | None = None
    cuda_runtime_version: str | None = None
    error_type: str | None = None
    detail: str | None = None

    @model_validator(mode="after")
    def validate_status_fields(self) -> "GpuHealthEvidence":
        if self.status is GpuHealthStatus.HEALTHY:
            required = (
                self.device_index,
                self.device_name,
                self.operation,
                self.matrix_size,
                self.max_absolute_error,
                self.duration_ms,
                self.cuda_driver_api_version,
                self.cuda_runtime_version,
            )
            if any(value is None for value in required):
                raise ValueError("healthy evidence is missing computation fields")
        elif self.status is GpuHealthStatus.UNHEALTHY and not (self.error_type and self.detail):
            raise ValueError("unhealthy evidence requires error_type and detail")
        return self


class GpuHealth(StrictModel):
    status: GpuHealthStatus
    detail: str = Field(min_length=1)
    evidence: GpuHealthEvidence | None = None

    @model_validator(mode="after")
    def validate_evidence(self) -> "GpuHealth":
        verified = self.status in {GpuHealthStatus.HEALTHY, GpuHealthStatus.UNHEALTHY}
        if verified and self.evidence is None:
            raise ValueError("verified GPU health requires structured evidence")
        if not verified and self.evidence is not None:
            raise ValueError("unverified GPU health cannot include computation evidence")
        if self.evidence is not None and self.evidence.status is not self.status:
            raise ValueError("GPU health status does not match its evidence")
        return self


class WorkerCapabilities(StrictModel):
    protocol_version: str = Field(pattern=r"^1\.[0-9]+$")
    collected_at: datetime
    hostname: str = Field(min_length=1)
    operating_system: str = Field(min_length=1)
    operating_system_version: str
    architecture: str = Field(min_length=1)
    logical_cpu_count: int = Field(gt=0)
    memory_total_bytes: int = Field(gt=0)
    storage_available_bytes: int = Field(ge=0)
    python_version: str = Field(min_length=1)
    gpus: tuple[GpuCapability, ...]
    gpu_health: GpuHealth


class EnrolmentRequest(StrictModel):
    protocol_version: str = Field(pattern=r"^1\.[0-9]+$")
    agent_instance_id: UUID
    display_name: str = Field(min_length=1, max_length=100)
    capabilities: WorkerCapabilities


class EnrolmentResponse(StrictModel):
    worker_id: UUID
    worker_credential: str = Field(pattern=r"^kwc_")
    state: str
    heartbeat_interval_seconds: int = Field(gt=0)


class RegistrationRequest(StrictModel):
    protocol_version: str = Field(pattern=r"^1\.[0-9]+$")
    agent_instance_id: UUID
    display_name: str = Field(min_length=1, max_length=100)
    public_key: str = Field(min_length=43, max_length=43)
    capabilities: WorkerCapabilities


class RegistrationCreatedResponse(StrictModel):
    registration_id: UUID
    confirmation_code: str = Field(pattern=r"^[A-Z2-9]{4}-[A-Z2-9]{4}$")
    expires_at: datetime
    poll_interval_seconds: int = Field(gt=0)


class RegistrationStatusResponse(StrictModel):
    registration_id: UUID
    state: str
    claim_challenge: str | None = None
    poll_interval_seconds: int = Field(gt=0)


class ClaimRegistrationRequest(StrictModel):
    signature: str = Field(min_length=86, max_length=86)


class HeartbeatRequest(StrictModel):
    protocol_version: str = Field(pattern=r"^1\.[0-9]+$")
    sequence: int = Field(ge=0)
    observed_at: datetime
    capabilities: WorkerCapabilities


def valid_logical_path(path: str) -> bool:
    """Apply the control plane's logical-path rules, so it never refuses a manifest's paths."""
    try:
        encoded = path.encode("utf-8")
    except UnicodeEncodeError:
        return False
    return (
        0 < len(encoded) <= MAX_LOGICAL_PATH_BYTES
        and not path.startswith("/")
        and "\\" not in path
        # The same range as Rust's char::is_control, which the control plane applies.
        and not any(character < " " or "\x7f" <= character <= "\x9f" for character in path)
        and all(segment not in ("", ".", "..") for segment in path.split("/"))
    )


def _checked_logical_path(path: str) -> str:
    if not valid_logical_path(path):
        raise ValueError("logical path is not permitted")
    return path


LogicalPath = Annotated[str, AfterValidator(_checked_logical_path)]


class JobOutputRequirement(StrictModel):
    logical_path: LogicalPath
    role: str = Field(pattern=r"^[a-z][a-z0-9_-]{0,31}$")
    media_type: str = Field(
        max_length=127, pattern=r"^[A-Za-z0-9!#$&^_.+-]+/[A-Za-z0-9!#$&^_.+-]+$"
    )
    mandatory: bool
    max_bytes: int = Field(ge=1, le=MAX_OUTPUT_FILE_BYTES)


class JobAssignment(StrictModel):
    attempt_id: UUID
    job_id: UUID
    name: str = Field(min_length=1, max_length=120)
    image_reference: str = Field(pattern=r"^[^\s@]+@sha256:[0-9a-f]{64}$")
    gpu_index: int = Field(ge=0)
    timeout_seconds: int = Field(ge=30, le=3600)
    lease_expires_at: datetime
    output_requirements: tuple[JobOutputRequirement, ...] = Field(
        default=(), max_length=MAX_OUTPUT_FILES
    )

    @model_validator(mode="after")
    def output_requirements_are_consistent(self) -> "JobAssignment":
        paths = [requirement.logical_path for requirement in self.output_requirements]
        if len(set(paths)) != len(paths):
            raise ValueError("output requirements repeat a logical path")
        declared = sum(requirement.max_bytes for requirement in self.output_requirements)
        if declared > MAX_OUTPUT_TOTAL_BYTES:
            raise ValueError("output requirements exceed the total size limit")
        return self


class HeartbeatResponse(StrictModel):
    worker_id: UUID
    state: str
    accepted_sequence: int = Field(ge=0)
    next_heartbeat_seconds: int = Field(gt=0)
    assignment: JobAssignment | None = None


class JobExecutionResult(StrictModel):
    exit_code: int
    timed_out: bool
    stdout: str = Field(max_length=65_536)
    stderr: str = Field(max_length=65_536)
    failure_message: str | None = Field(default=None, max_length=1_000)


class JobResultResponse(StrictModel):
    attempt_id: UUID
    job_id: UUID
    status: str


class ArtifactManifestFile(StrictModel):
    logical_path: LogicalPath
    byte_length: int = Field(ge=0, le=MAX_OUTPUT_FILE_BYTES)
    sha256: str = Field(pattern=r"^[0-9a-f]{64}$")
    crc32c: str = Field(pattern=r"^[A-Za-z0-9+/]{5}[AQgw]==$")


class DeclareArtifactManifestRequest(StrictModel):
    protocol_version: str = Field(pattern=r"^1\.[1-9][0-9]*$")
    manifest_id: UUID
    files: tuple[ArtifactManifestFile, ...] = Field(max_length=MAX_OUTPUT_FILES)


class ArtifactResponse(StrictModel):
    artifact_id: UUID
    logical_path: LogicalPath
    role: str
    media_type: str
    mandatory: bool
    byte_length: int = Field(ge=0, le=MAX_OUTPUT_FILE_BYTES)
    sha256: str = Field(pattern=r"^[0-9a-f]{64}$")
    crc32c: str = Field(pattern=r"^[A-Za-z0-9+/]{5}[AQgw]==$")
    object_key: str
    status: str
    storage_generation: int | None = None
    upload_started_at: datetime | None = None
    upload_completed_at: datetime | None = None
    verification_pending: bool


class ArtifactManifestResponse(StrictModel):
    manifest_id: UUID
    attempt_id: UUID
    artifacts: tuple[ArtifactResponse, ...]


class ResumableUploadSession(StrictModel):
    uri: str = Field(repr=False)
    method: str
    expires_at: datetime


class BeginArtifactUploadRequest(StrictModel):
    protocol_version: str = Field(pattern=r"^1\.[1-9][0-9]*$")


class BeginArtifactUploadResponse(StrictModel):
    artifact: ArtifactResponse
    session: ResumableUploadSession


class CompleteArtifactUploadRequest(StrictModel):
    protocol_version: str = Field(pattern=r"^1\.[1-9][0-9]*$")
    storage_generation: int
    byte_length: int = Field(ge=0, le=MAX_OUTPUT_FILE_BYTES)
    sha256: str = Field(pattern=r"^[0-9a-f]{64}$")
    crc32c: str = Field(pattern=r"^[A-Za-z0-9+/]{5}[AQgw]==$")


class AbandonArtifactUploadRequest(StrictModel):
    protocol_version: str = Field(pattern=r"^1\.[1-9][0-9]*$")
    session_uri_sha256: str = Field(pattern=r"^[0-9a-f]{64}$")
