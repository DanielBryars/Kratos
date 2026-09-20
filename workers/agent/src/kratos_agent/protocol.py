"""HTTPS client for the versioned worker protocol."""

import hashlib
from dataclasses import dataclass
from typing import TypeVar
from urllib.parse import urlparse
from uuid import UUID

import httpx
from pydantic import ValidationError

from kratos_agent.models import (
    PROTOCOL_VERSION,
    AbandonArtifactUploadRequest,
    ArtifactManifestFile,
    ArtifactManifestResponse,
    ArtifactResponse,
    BeginArtifactUploadRequest,
    BeginArtifactUploadResponse,
    ClaimRegistrationRequest,
    CompleteArtifactUploadRequest,
    DeclareArtifactManifestRequest,
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
    ArtifactResponse,
    EnrolmentResponse,
    HeartbeatResponse,
    RegistrationCreatedResponse,
    RegistrationStatusResponse,
    JobResultResponse,
    ArtifactManifestResponse,
    BeginArtifactUploadResponse,
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
        # Object bytes go straight to Cloud Storage, not through the control plane, so they use a
        # separate client with no base URL and no Kratos credential attached.
        self.storage = httpx.Client(follow_redirects=False, transport=transport)

    def close(self) -> None:
        self._client.close()
        self.storage.close()

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

    def declare_artifact_manifest(
        self,
        worker_id: UUID,
        credential: str,
        attempt_id: UUID,
        manifest_id: UUID,
        files: tuple[ArtifactManifestFile, ...],
    ) -> ArtifactManifestResponse:
        request = DeclareArtifactManifestRequest(
            protocol_version=PROTOCOL_VERSION, manifest_id=manifest_id, files=files
        )
        response = self._client.put(
            f"/api/v1/workers/{worker_id}/job-attempts/{attempt_id}/artifact-manifest",
            headers={"Authorization": f"Bearer {credential}"},
            json=request.model_dump(mode="json"),
        )
        return self._parse(response, ArtifactManifestResponse)

    def begin_artifact_upload(
        self, worker_id: UUID, credential: str, attempt_id: UUID, artifact_id: UUID
    ) -> BeginArtifactUploadResponse:
        request = BeginArtifactUploadRequest(protocol_version=PROTOCOL_VERSION)
        response = self._client.put(
            f"/api/v1/workers/{worker_id}/job-attempts/{attempt_id}/artifacts/{artifact_id}/upload",
            headers={"Authorization": f"Bearer {credential}"},
            json=request.model_dump(mode="json"),
        )
        return self._parse(response, BeginArtifactUploadResponse)

    def complete_artifact_upload(
        self,
        worker_id: UUID,
        credential: str,
        attempt_id: UUID,
        artifact: ArtifactManifestFile,
        artifact_id: UUID,
        storage_generation: int,
    ) -> ArtifactResponse:
        request = CompleteArtifactUploadRequest(
            protocol_version=PROTOCOL_VERSION,
            storage_generation=storage_generation,
            byte_length=artifact.byte_length,
            sha256=artifact.sha256,
            crc32c=artifact.crc32c,
        )
        response = self._client.put(
            f"/api/v1/workers/{worker_id}/job-attempts/{attempt_id}"
            f"/artifacts/{artifact_id}/complete-upload",
            headers={"Authorization": f"Bearer {credential}"},
            json=request.model_dump(mode="json"),
        )
        return self._parse(response, ArtifactResponse)

    def abandon_artifact_upload(
        self,
        worker_id: UUID,
        credential: str,
        attempt_id: UUID,
        artifact_id: UUID,
        session_uri: str,
    ) -> None:
        """Consume a session Cloud Storage has rejected, so a replacement may be issued.

        The URI is a credential, so only its fingerprint is sent. That fingerprint also stops a
        delayed request cancelling a session issued after it.
        """
        request = AbandonArtifactUploadRequest(
            protocol_version=PROTOCOL_VERSION,
            session_uri_sha256=hashlib.sha256(session_uri.encode("utf-8")).hexdigest(),
        )
        response = self._client.put(
            f"/api/v1/workers/{worker_id}/job-attempts/{attempt_id}"
            f"/artifacts/{artifact_id}/abandon-upload",
            headers={"Authorization": f"Bearer {credential}"},
            json=request.model_dump(mode="json"),
        )
        if response.status_code != 204:
            self._parse(response, ArtifactManifestResponse)

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
