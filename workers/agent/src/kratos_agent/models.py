"""Versioned messages shared with the Kratos worker API."""

from datetime import datetime
from enum import StrEnum

from pydantic import BaseModel, ConfigDict, Field, model_validator

PROTOCOL_VERSION = "1.0"


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


class GpuHealth(StrictModel):
    status: GpuHealthStatus
    detail: str = Field(min_length=1)


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
