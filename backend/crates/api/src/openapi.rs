//! The OpenAPI document: the contract `frontend/packages/api-client` is generated from.
//!
//! Every route must be listed here. `make openapi` writes the document to `openapi/openapi.json`
//! and CI fails when the committed file drifts from what the code generates.

use utoipa::OpenApi;

use crate::{
    error::{ErrorBody, ErrorDetail},
    routes::{
        auth,
        health::{self, Health},
        webhooks,
    },
};

#[derive(OpenApi)]
#[openapi(
    info(
        title = "MergeTip API",
        description = "Generated from the Rust backend with utoipa (`make openapi`). Do not edit by hand.",
        license(name = "MIT", identifier = "MIT"),
    ),
    paths(
        health::healthz,
        health::health,
        webhooks::github,
        auth::login,
        auth::callback,
    ),
    components(schemas(ErrorBody, ErrorDetail, Health)),
    tags(
        (name = "meta", description = "Service health"),
        (name = "webhooks", description = "Signed deliveries from external services"),
        (name = "auth", description = "Signing in and sessions"),
    )
)]
pub struct ApiDoc;

/// The document as pretty-printed JSON with a trailing newline, byte-for-byte what is committed.
pub fn json() -> String {
    let mut out = ApiDoc::openapi()
        .to_pretty_json()
        .expect("the OpenAPI document always serialises");
    out.push('\n');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn documents_every_route() {
        let doc = ApiDoc::openapi();
        for path in ["/healthz", "/v1/health", "/v1/webhooks/github"] {
            assert!(
                doc.paths.paths.contains_key(path),
                "{path} missing from OpenAPI"
            );
        }
    }

    /// Fails when `openapi/openapi.json` is stale; fix with `make openapi`.
    #[test]
    fn committed_document_is_current() {
        let committed = include_str!("../../../../openapi/openapi.json");
        assert!(
            committed == json(),
            "openapi/openapi.json is out of date; run `make openapi` and commit the result"
        );
    }
}
