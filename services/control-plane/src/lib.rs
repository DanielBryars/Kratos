use std::path::PathBuf;

use axum::{Json, Router, routing::get};
use serde::Serialize;
use tower_http::services::{ServeDir, ServeFile};
use utoipa::{OpenApi, ToSchema};
use utoipa_swagger_ui::SwaggerUi;

#[derive(Debug, Serialize, ToSchema)]
pub struct HealthResponse {
    status: &'static str,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct VersionResponse {
    name: &'static str,
    version: &'static str,
}

#[derive(OpenApi)]
#[openapi(
    paths(health, version),
    components(schemas(HealthResponse, VersionResponse)),
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

pub fn app(web_root: Option<PathBuf>) -> Router {
    let router = Router::new()
        .route("/healthz", get(health))
        .route("/api/v1/version", get(version))
        .merge(SwaggerUi::new("/swagger-ui").url("/api-docs/openapi.json", ApiDoc::openapi()));

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
        let response = app(None)
            .oneshot(Request::get("/healthz").body(Body::empty()).unwrap())
            .await
            .unwrap();

        assert_eq!(response.status(), 200);
    }

    #[tokio::test]
    async fn openapi_document_is_available() {
        let response = app(None)
            .oneshot(
                Request::get("/api-docs/openapi.json")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), 200);
    }
}
