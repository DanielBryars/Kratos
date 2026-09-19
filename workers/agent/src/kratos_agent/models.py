"""Versioned messages shared with the Kratos worker API."""

from datetime import datetime
from enum import StrEnum

from pydantic import BaseModel, ConfigDict, Field

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
