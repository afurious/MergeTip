//! The OpenAPI document: the contract `frontend/packages/api-client` is generated from.
//!
//! Every route must be listed here. `make openapi` writes the document to `openapi/openapi.json`
//! and CI fails when the committed file drifts from what the code generates.

use utoipa::{
    Modify, OpenApi,
    openapi::security::{ApiKey, ApiKeyValue, HttpAuthScheme, HttpBuilder, SecurityScheme},
};

use crate::{
    error::{ErrorBody, ErrorDetail},
    routes::{
        auth::{self, ExtensionToken},
        claims::{self, ClaimRequest},
        confirmations::{self, ConfirmRequest},
        email,
        health::{self, Health},
        me::{self, Caps, Me, WalletStatus},
        settings::{self, AddressRequest, SettingsRequest, WalletRequest},
        tips::{Tip, TipEnvelope, TipKindBody, TipState, UserRef},
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
        auth::logout,
        auth::extension_token,
        me::get_me,
        settings::connect_wallet,
        settings::disconnect_wallet,
        settings::set_lightning_address,
        settings::update_settings,
        claims::get_claim,
        claims::claim_tip,
        confirmations::get_confirmation,
        confirmations::send_code,
        confirmations::confirm_tip,
        confirmations::cancel_tip,
        email::unsubscribe_link,
        email::unsubscribe_one_click,
        webhooks::resend,
    ),
    components(schemas(
        ErrorBody,
        ErrorDetail,
        Health,
        ExtensionToken,
        Me,
        WalletStatus,
        Caps,
        WalletRequest,
        AddressRequest,
        SettingsRequest,
        Tip,
        TipState,
        TipKindBody,
        UserRef,
        TipEnvelope,
        ClaimRequest,
        ConfirmRequest,
    )),
    modifiers(&SecuritySchemes),
    tags(
        (name = "meta", description = "Service health"),
        (name = "webhooks", description = "Signed deliveries from external services"),
        (name = "auth", description = "Signing in and sessions"),
        (name = "me", description = "The signed-in user's account and settings"),
        (name = "claims", description = "Claiming pledged tips"),
        (name = "confirmations", description = "Confirming high-value tips"),
        (name = "email", description = "Email preferences reachable from emails"),
    )
)]
pub struct ApiDoc;

/// `session`: the web app's `mt_session` cookie. `bearer`: an extension token.
struct SecuritySchemes;

impl Modify for SecuritySchemes {
    fn modify(&self, openapi: &mut utoipa::openapi::OpenApi) {
        let components = openapi.components.get_or_insert_with(Default::default);
        components.add_security_scheme(
            "session",
            SecurityScheme::ApiKey(ApiKey::Cookie(ApiKeyValue::new("mt_session"))),
        );
        components.add_security_scheme(
            "bearer",
            SecurityScheme::Http(HttpBuilder::new().scheme(HttpAuthScheme::Bearer).build()),
        );
    }
}

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
