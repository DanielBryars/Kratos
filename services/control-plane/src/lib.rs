use std::path::PathBuf;

use axum::{
    Json, Router,
    extract::State,
    http::StatusCode,
    routing::{get, post, put},
};
use serde::Serialize;
use sqlx::PgPool;
use tower_http::services::{ServeDir, ServeFile};
use utoipa::{
    Modify, OpenApi, ToSchema,
    openapi::security::{HttpAuthScheme, HttpBuilder, SecurityScheme},
};
use utoipa_swagger_ui::SwaggerUi;

pub mod credentials;
pub mod database;
pub mod human_auth;
pub mod migration;
mod operator;
mod registry;

use human_auth::{ClientAuthConfig, HumanAuth};
use operator::{
    ApproveWorkerRequest, CreateEnrolmentRequest, CreateEnrolmentResponse, OperatorWorkerResponse,
    PendingRegistrationResponse, RegistrationDecisionResponse, WorkerActionResponse,
    WorkerConnectivity, WorkerGroupResponse,
};
use registry::{
    ClaimRegistrationRequest, EnrolmentRequest, EnrolmentResponse, ErrorResponse, GpuCapability,
    GpuHealth, GpuHealthEvidence, GpuHealthStatus, HeartbeatRequest, HeartbeatResponse,
    RegistrationCreatedResponse, RegistrationRequest, RegistrationState,
    RegistrationStatusResponse, VerificationGate, WorkerCapabilities, WorkerState,
};

#[derive(Debug, Serialize, ToSchema)]
pub struct HealthResponse {
    status: &'static str,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct VersionResponse {
    name: &'static str,
    version: &'static str,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct ReadinessResponse {
    status: &'static str,
    database: &'static str,
}

#[derive(Clone)]
pub(crate) struct AppState {
    pub(crate) database: Option<PgPool>,
    pub(crate) human_auth: Option<HumanAuth>,
    pub(crate) verification_gate: VerificationGate,
}

#[derive(OpenApi)]
#[openapi(
    paths(
        health,
        readiness,
        version,
        auth_config,
        operator::create_worker_enrolment,
        operator::list_worker_registration_requests,
        operator::approve_worker_registration,
        operator::reject_worker_registration,
        operator::list_workers,
        operator::approve_worker,
        operator::quarantine_worker,
        operator::revoke_worker,
        registry::enrol_worker,
        registry::request_registration,
        registry::registration_status,
        registry::claim_registration,
        registry::heartbeat
    ),
    components(schemas(
        HealthResponse, ReadinessResponse, VersionResponse, ClientAuthConfig, EnrolmentRequest,
        EnrolmentResponse, HeartbeatRequest, HeartbeatResponse, ErrorResponse,
        WorkerCapabilities, GpuCapability, GpuHealth, GpuHealthEvidence, GpuHealthStatus, WorkerState,
        CreateEnrolmentRequest, CreateEnrolmentResponse, RegistrationRequest,
        RegistrationCreatedResponse, RegistrationStatusResponse, RegistrationState,
        ClaimRegistrationRequest, PendingRegistrationResponse, RegistrationDecisionResponse,
        ApproveWorkerRequest, OperatorWorkerResponse, WorkerActionResponse, WorkerConnectivity,
        WorkerGroupResponse
    )),
    tags(
        (name = "system", description = "Control-plane status"),
        (name = "operator", description = "Human-authorised administration"),
        (name = "workers", description = "Worker enrolment and liveness")
    ),
    modifiers(&SecurityAddon)
)]
struct ApiDoc;

struct SecurityAddon;

impl Modify for SecurityAddon {
    fn modify(&self, openapi: &mut utoipa::openapi::OpenApi) {
        if let Some(components) = openapi.components.as_mut() {
            components.add_security_scheme(
                "bearer_credential",
                SecurityScheme::Http(
                    HttpBuilder::new()
                        .scheme(HttpAuthScheme::Bearer)
                        .bearer_format("Kratos opaque credential")
                        .build(),
                ),
            );
            components.add_security_scheme(
                "human_bearer",
                SecurityScheme::Http(
                    HttpBuilder::new()
                        .scheme(HttpAuthScheme::Bearer)
                        .bearer_format("Identity Platform ID token")
                        .build(),
                ),
            );
        }
    }
}

#[utoipa::path(
    get,
    path = "/healthz",
    tag = "system",
    responses((status = 200, description = "Service is healthy", body = HealthResponse))
)]
async fn health() -> Json<HealthResponse> {
    Json(HealthResponse { status: "ok" })
}

#[utoipa::path(
    get,
    path = "/readyz",
    tag = "system",
    responses(
        (status = 200, description = "Configured dependencies are ready", body = ReadinessResponse),
        (status = 503, description = "A configured dependency is unavailable", body = ReadinessResponse)
    )
)]
async fn readiness(State(state): State<AppState>) -> (StatusCode, Json<ReadinessResponse>) {
    let Some(database) = state.database else {
        return (
            StatusCode::OK,
            Json(ReadinessResponse {
                status: "ok",
                database: "disabled",
            }),
        );
    };

    match sqlx::query_scalar::<_, i32>("SELECT 1")
        .fetch_one(&database)
        .await
    {
        Ok(1) => (
            StatusCode::OK,
            Json(ReadinessResponse {
                status: "ok",
                database: "ready",
            }),
        ),
        Ok(_) | Err(_) => (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(ReadinessResponse {
                status: "unavailable",
                database: "unavailable",
            }),
        ),
    }
}

#[utoipa::path(
    get,
    path = "/api/v1/version",
    tag = "system",
    responses((status = 200, description = "Service build version", body = VersionResponse))
)]
async fn version() -> Json<VersionResponse> {
    Json(VersionResponse {
        name: "kratos-control-plane",
        version: env!("CARGO_PKG_VERSION"),
    })
}

#[utoipa::path(
    get,
    path = "/api/v1/auth/config",
    tag = "system",
    responses(
        (status = 200, description = "Public Identity Platform browser configuration", body = ClientAuthConfig),
        (status = 503, description = "Human authentication is not configured", body = ErrorResponse)
    )
)]
async fn auth_config(
    State(state): State<AppState>,
) -> Result<Json<ClientAuthConfig>, (StatusCode, Json<ErrorResponse>)> {
    state
        .human_auth
        .map(|auth| Json(auth.client_config()))
        .ok_or({
            (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(ErrorResponse {
                    code: "authentication_unavailable",
                    message: "Human authentication is unavailable.",
                }),
            )
        })
}

pub fn app(web_root: Option<PathBuf>, database: Option<PgPool>) -> Router {
    app_with_human_auth(web_root, database, None)
}

pub fn app_with_human_auth(
    web_root: Option<PathBuf>,
    database: Option<PgPool>,
    human_auth: Option<HumanAuth>,
) -> Router {
    let router = Router::new()
        .route("/healthz", get(health))
        .route("/readyz", get(readiness))
        .route("/api/v1/version", get(version))
        .route("/api/v1/auth/config", get(auth_config))
        .route(
            "/api/v1/operator/worker-enrolments",
            post(operator::create_worker_enrolment),
        )
        .route(
            "/api/v1/operator/worker-registration-requests",
            get(operator::list_worker_registration_requests),
        )
        .route(
            "/api/v1/operator/worker-registration-requests/{registration_id}/approve",
            post(operator::approve_worker_registration),
        )
        .route(
            "/api/v1/operator/worker-registration-requests/{registration_id}/reject",
            post(operator::reject_worker_registration),
        )
        .route("/api/v1/operator/workers", get(operator::list_workers))
        .route(
            "/api/v1/operator/workers/{worker_id}/approve",
            post(operator::approve_worker),
        )
        .route(
            "/api/v1/operator/workers/{worker_id}/quarantine",
            post(operator::quarantine_worker),
        )
        .route(
            "/api/v1/operator/workers/{worker_id}/revoke",
            post(operator::revoke_worker),
        )
        .route("/api/v1/worker-enrolments", post(registry::enrol_worker))
        .route(
            "/api/v1/worker-registration-requests",
            post(registry::request_registration),
        )
        .route(
            "/api/v1/worker-registration-requests/{registration_id}",
            get(registry::registration_status),
        )
        .route(
            "/api/v1/worker-registration-requests/{registration_id}/claim",
            post(registry::claim_registration),
        )
        .route(
            "/api/v1/workers/{worker_id}/heartbeat",
            put(registry::heartbeat),
        )
        .merge(SwaggerUi::new("/swagger-ui").url("/api-docs/openapi.json", ApiDoc::openapi()))
        .with_state(AppState {
            database,
            human_auth,
            verification_gate: VerificationGate::default(),
        });

    if let Some(root) = web_root {
        let index = root.join("index.html");
        router.fallback_service(ServeDir::new(root).fallback(ServeFile::new(index)))
    } else {
        router
    }
}

#[cfg(test)]
mod tests {
    use axum::{body::Body, http::Request};
    use tower::ServiceExt;

    use super::app;

    #[tokio::test]
    async fn health_endpoint_is_available() {
        let response = app(None, None)
            .oneshot(Request::get("/healthz").body(Body::empty()).unwrap())
            .await
            .unwrap();

        assert_eq!(response.status(), 200);
    }

    #[tokio::test]
    async fn openapi_document_is_available() {
        let response = app(None, None)
            .oneshot(
                Request::get("/api-docs/openapi.json")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), 200);
    }

    #[tokio::test]
    async fn readiness_reports_when_persistence_is_disabled() {
        let response = app(None, None)
            .oneshot(Request::get("/readyz").body(Body::empty()).unwrap())
            .await
            .unwrap();

        assert_eq!(response.status(), 200);
    }

    #[tokio::test]
    async fn worker_routes_fail_closed_when_persistence_is_disabled() {
        let response = app(None, None)
            .oneshot(
                Request::post("/api/v1/worker-enrolments")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), 503);
    }
}
