from datetime import UTC, datetime

import pytest
from pydantic import ValidationError

from kratos_agent.cli import _reported_health
from kratos_agent.models import GpuHealth, GpuHealthEvidence, GpuHealthStatus


def test_healthy_evidence_is_attached_to_reported_health() -> None:
    evidence = GpuHealthEvidence(
        schema_version="1.0",
        status=GpuHealthStatus.HEALTHY,
        checked_at=datetime.now(UTC),
        image_reference="sha256:" + ("a" * 64),
        device_index=0,
        device_name="Test GPU",
        operation="matrix multiplication",
        matrix_size=512,
        max_absolute_error=0.0,
        duration_ms=12.5,
        cuda_driver_api_version="13.3",
        cuda_runtime_version="12.9",
    )

    health = _reported_health(evidence)

    assert health.status is GpuHealthStatus.HEALTHY
    assert health.evidence is evidence
    assert health.detail == "GPU computation passed on Test GPU in 12.500 ms"


def test_verified_health_without_evidence_is_rejected() -> None:
    with pytest.raises(ValidationError, match="requires structured evidence"):
        GpuHealth(status=GpuHealthStatus.HEALTHY, detail="claimed without proof")
