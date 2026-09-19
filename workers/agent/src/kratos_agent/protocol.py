"""HTTPS client for the versioned worker protocol."""

from dataclasses import dataclass
from typing import TypeVar
from urllib.parse import urlparse
from uuid import UUID

import httpx
from pydantic import ValidationError

from kratos_agent.models import (
    ClaimRegistrationRequest,
    EnrolmentRequest,
    EnrolmentResponse,
    HeartbeatRequest,
    HeartbeatResponse,
    JobExecutionResult,
    JobResultResponse,
    RegistrationCreatedResponse,
    RegistrationRequest,
    RegistrationStatusResponse,
    WorkerCapabilities,
)

ResponseModel = TypeVar(
    "ResponseModel",
    EnrolmentResponse,
    HeartbeatResponse,
    RegistrationCreatedResponse,
    RegistrationStatusResponse,
    JobResultResponse,
)


@dataclass(frozen=True)
class ControlPlaneError(RuntimeError):
    status_code: int
    code: str
    message: str

    def __str__(self) -> str:
        return f"control plane returned {self.status_code} {self.code}: {self.message}"


class WorkerProtocolClient:
    def __init__(self, base_url: str, *, transport: httpx.BaseTransport | None = None) -> None:
        parsed = urlparse(base_url)
        if parsed.scheme != "https" or not parsed.netloc or parsed.username or parsed.password:
            raise ValueError("control-plane URL must be an HTTPS origin without user information")
        self._client = httpx.Client(
            base_url=base_url.rstrip("/"),
            timeout=httpx.Timeout(15),
            follow_redirects=False,
            transport=transport,
        )

    def close(self) -> None:
        self._client.close()

    def __enter__(self) -> "WorkerProtocolClient":
        return self

    def __exit__(self, *_: object) -> None:
        self.close()

    def enrol(
        self,
        credential: str,
        agent_instance_id: UUID,
        display_name: str,
        capabilities: WorkerCapabilities,
    ) -> EnrolmentResponse:
        request = EnrolmentRequest(
            protocol_version=capabilities.protocol_version,
            agent_instance_id=agent_instance_id,
            display_name=display_name,
            capabilities=capabilities,
        )
        response = self._client.post(
            "/api/v1/worker-enrolments",
            headers={"Authorization": f"Bearer {credential}"},
            json=request.model_dump(mode="json"),
        )
        return self._parse(response, EnrolmentResponse)

    def request_registration(
        self,
        agent_instance_id: UUID,
        display_name: str,
        public_key: str,
        capabilities: WorkerCapabilities,
    ) -> RegistrationCreatedResponse:
        request = RegistrationRequest(
            protocol_version=capabilities.protocol_version,
            agent_instance_id=agent_instance_id,
            display_name=display_name,
            public_key=public_key,
            capabilities=capabilities,
        )
        response = self._client.post(
            "/api/v1/worker-registration-requests",
            json=request.model_dump(mode="json"),
        )
        return self._parse(response, RegistrationCreatedResponse)

    def registration_status(self, registration_id: UUID) -> RegistrationStatusResponse:
        response = self._client.get(f"/api/v1/worker-registration-requests/{registration_id}")
        return self._parse(response, RegistrationStatusResponse)

    def claim_registration(self, registration_id: UUID, signature: str) -> EnrolmentResponse:
        request = ClaimRegistrationRequest(signature=signature)
        response = self._client.post(
            f"/api/v1/worker-registration-requests/{registration_id}/claim",
            json=request.model_dump(mode="json"),
        )
        return self._parse(response, EnrolmentResponse)

    def heartbeat(
        self,
        worker_id: UUID,
        credential: str,
        sequence: int,
        capabilities: WorkerCapabilities,
    ) -> HeartbeatResponse:
        request = HeartbeatRequest(
            protocol_version=capabilities.protocol_version,
            sequence=sequence,
            observed_at=capabilities.collected_at,
            capabilities=capabilities,
        )
        response = self._client.put(
            f"/api/v1/workers/{worker_id}/heartbeat",
            headers={"Authorization": f"Bearer {credential}"},
            json=request.model_dump(mode="json"),
        )
        return self._parse(response, HeartbeatResponse)

    def report_job_result(
        self,
        worker_id: UUID,
        credential: str,
        attempt_id: UUID,
        result: JobExecutionResult,
    ) -> JobResultResponse:
        response = self._client.put(
            f"/api/v1/workers/{worker_id}/job-attempts/{attempt_id}/result",
            headers={"Authorization": f"Bearer {credential}"},
            json=result.model_dump(mode="json"),
        )
        return self._parse(response, JobResultResponse)

    @staticmethod
    def _parse(response: httpx.Response, model: type[ResponseModel]) -> ResponseModel:
        if response.is_success:
            try:
                return model.model_validate_json(response.content)
            except ValidationError as error:
                raise ControlPlaneError(
                    response.status_code, "invalid_response", "response schema is invalid"
                ) from error
        try:
            payload = response.json()
            code = str(payload.get("code", "request_failed"))
            message = str(payload.get("message", "request failed"))
        except ValueError:
            code = "request_failed"
            message = "control plane returned a non-JSON error"
        raise ControlPlaneError(response.status_code, code, message)
