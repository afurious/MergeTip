use axum::Json;
use serde::Serialize;
use utoipa::ToSchema;

#[derive(Debug, Serialize, ToSchema)]
pub struct Health {
    /// Always `ok` when the process can serve requests.
    pub status: &'static str,
    /// The running build's version.
    pub version: &'static str,
}

/// Liveness probe for load balancers.
///
/// Deliberately touches no dependencies, so a database outage doesn't take every machine out of
/// rotation at once.
#[utoipa::path(
    get,
    path = "/healthz",
    operation_id = "healthz",
    tag = "meta",
    responses((status = 200, description = "The process is up", body = String, content_type = "text/plain"))
)]
pub async fn healthz() -> &'static str {
    "ok"
}

/// Service status and build version.
#[utoipa::path(
    get,
    path = "/v1/health",
    operation_id = "health",
    tag = "meta",
    responses((status = 200, description = "The service is up", body = Health))
)]
pub async fn health() -> Json<Health> {
    Json(Health {
        status: "ok",
        version: env!("CARGO_PKG_VERSION"),
    })
}
