use std::path::PathBuf;

use axum::{Json, Router, extract::State, http::StatusCode, routing::get};
use serde::Serialize;
use sqlx::PgPool;
use tower_http::services::{ServeDir, ServeFile};
use utoipa::{OpenApi, ToSchema};
use utoipa_swagger_ui::SwaggerUi;

pub mod credentials;
pub mod database;

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
struct AppState {
    database: Option<PgPool>,
}

#[derive(OpenApi)]
#[openapi(
    paths(health, readiness, version),
    components(schemas(HealthResponse, ReadinessResponse, VersionResponse)),
    tags((name = "system", description = "Control-plane status"))
)]
struct ApiDoc;

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

pub fn app(web_root: Option<PathBuf>, database: Option<PgPool>) -> Router {
    let router = Router::new()
        .route("/healthz", get(health))
        .route("/readyz", get(readiness))
        .route("/api/v1/version", get(version))
        .merge(SwaggerUi::new("/swagger-ui").url("/api-docs/openapi.json", ApiDoc::openapi()))
        .with_state(AppState { database });

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
}
