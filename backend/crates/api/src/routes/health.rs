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

/// Liveness for load balancers (Fly health checks); deliberately touches no dependencies.
pub async fn healthz() -> &'static str {
    "ok"
}

pub async fn health() -> Json<Health> {
    Json(Health {
        status: "ok",
        version: env!("CARGO_PKG_VERSION"),
    })
}
