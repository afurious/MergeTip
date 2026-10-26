//! Receiving tips in local currency (FR-10, RFC 0004): supported countries, the receive form
//! generated from Bitnob's requirements, and the receiver's payout methods.
//!
//! Beneficiary details are encrypted before they are stored and never returned: responses show
//! the account's last four characters only.

use std::collections::BTreeMap;

use axum::{
    Json,
    extract::{Path, State},
    http::StatusCode,
};
use mergetip_crypto::Redacted;
use mergetip_fiat::{AccountQuery, Beneficiary, FiatError, FiatProvider, FormField};
use mergetip_store::{
    feature_flags,
    payout_methods::{self, DestinationType, NewPayoutMethod, PayoutMethod},
};
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

use crate::{ApiError, AppState, extract::AuthUser};

/// The provider, when fiat payouts are on (env and runtime flag); otherwise 404 `fiat_disabled`.
pub(crate) async fn provider(state: &AppState) -> Result<&dyn FiatProvider, ApiError> {
    let disabled = || {
        ApiError::new(
            StatusCode::NOT_FOUND,
            "fiat_disabled",
            "Local-currency payouts aren't available yet.",
        )
    };
    let fiat = state.fiat().ok_or_else(disabled)?;
    if !feature_flags::is_enabled(state.pool(), feature_flags::FIAT_PAYOUTS).await? {
        return Err(disabled());
    }
    Ok(fiat)
}

fn provider_error(err: FiatError) -> ApiError {
    match err {
        FiatError::Rejected { message, .. } => {
            tracing::info!(%message, "payout provider refused");
            ApiError::unprocessable("fiat_unsupported", "That isn't supported for payouts.")
        }
        other => {
            tracing::warn!(error = %other, "payout provider unavailable");
            ApiError::new(
                StatusCode::BAD_GATEWAY,
                "fiat_unavailable",
                "Local-currency payouts are unavailable right now. Try again shortly.",
            )
        }
    }
}

#[derive(Debug, Serialize, ToSchema)]
pub struct PayoutCountry {
    pub code: String,
    pub name: String,
    pub corridors: Vec<PayoutCorridor>,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct PayoutCorridor {
    pub currency: String,
    /// `bank` and/or `mobile_money`.
    pub destination_types: Vec<String>,
}

/// Countries receivers can be paid in, with their currencies and rails.
#[utoipa::path(
    get,
    path = "/v1/payouts/countries",
    operation_id = "list_payout_countries",
    tag = "payouts",
    responses(
        (status = 200, body = Vec<PayoutCountry>),
        (status = 404, description = "`fiat_disabled`", body = crate::error::ErrorBody),
    ),
    security(("session" = []), ("bearer" = []))
)]
pub async fn list_countries(
    State(state): State<AppState>,
    _auth: AuthUser,
) -> Result<Json<Vec<PayoutCountry>>, ApiError> {
    let countries = provider(&state)
        .await?
        .supported_countries()
        .await
        .map_err(provider_error)?;
    Ok(Json(
        countries
            .into_iter()
            .map(|c| PayoutCountry {
                code: c.code,
                name: c.name,
                corridors: c
                    .corridors
                    .into_iter()
                    .map(|k| PayoutCorridor {
                        currency: k.currency,
                        destination_types: k.destination_types,
                    })
                    .collect(),
            })
            .collect(),
    ))
}

/// One rail's form: fields exactly as Bitnob requires them (FR-10.2).
#[derive(Debug, Serialize, ToSchema)]
pub struct PayoutForm {
    pub destination_type: String,
    pub label: Option<String>,
    pub fields: Vec<PayoutField>,
    /// Banks to pick from, for bank rails.
    pub banks: Vec<PayoutBank>,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct PayoutField {
    pub key: String,
    pub label: String,
    #[serde(rename = "type")]
    pub kind: String,
    pub required: bool,
    pub component: Option<String>,
    pub pattern: Option<String>,
    pub min_length: Option<u32>,
    pub max_length: Option<u32>,
    pub placeholder: Option<String>,
    #[schema(value_type = Option<Object>)]
    pub options: Option<serde_json::Value>,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct PayoutBank {
    pub code: String,
    pub name: String,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct PayoutCountryForm {
    pub code: String,
    pub name: String,
    pub forms: Vec<PayoutForm>,
}

impl From<FormField> for PayoutField {
    fn from(f: FormField) -> Self {
        Self {
            key: f.key,
            label: f.label,
            kind: f.kind,
            required: f.required,
            component: f.component,
            pattern: f.pattern,
            min_length: f.min_length,
            max_length: f.max_length,
            placeholder: f.placeholder,
            options: f.options,
        }
    }
}

/// The receive form for a country, generated from Bitnob's requirements.
#[utoipa::path(
    get,
    path = "/v1/payouts/countries/{code}",
    operation_id = "get_payout_country",
    tag = "payouts",
    params(("code" = String, Path, description = "Two-letter country code")),
    responses(
        (status = 200, body = PayoutCountryForm),
        (status = 404, description = "`fiat_disabled`", body = crate::error::ErrorBody),
        (status = 422, description = "`fiat_unsupported`", body = crate::error::ErrorBody),
    ),
    security(("session" = []), ("bearer" = []))
)]
pub async fn country_form(
    State(state): State<AppState>,
    _auth: AuthUser,
    Path(code): Path<String>,
) -> Result<Json<PayoutCountryForm>, ApiError> {
    let code = country_code(&code)?;
    let requirements = provider(&state)
        .await?
        .country_requirements(&code)
        .await
        .map_err(provider_error)?;
    Ok(Json(PayoutCountryForm {
        code: requirements.code,
        name: requirements.name,
        forms: requirements
            .destination_types
            .into_iter()
            .map(|(destination_type, rail)| PayoutForm {
                destination_type,
                label: rail.label,
                fields: rail.fields.into_iter().map(Into::into).collect(),
                banks: rail
                    .banks
                    .unwrap_or_default()
                    .into_iter()
                    .map(|b| PayoutBank {
                        code: b.bank_code,
                        name: b.bank_name,
                    })
                    .collect(),
            })
            .collect(),
    }))
}

fn country_code(raw: &str) -> Result<String, ApiError> {
    let code = raw.trim().to_ascii_uppercase();
    if code.len() == 2 && code.chars().all(|c| c.is_ascii_uppercase()) {
        Ok(code)
    } else {
        Err(ApiError::unprocessable(
            "fiat_unsupported",
            "Use a two-letter country code.",
        ))
    }
}

/// A saved payout method, as its owner sees it.
#[derive(Debug, Serialize, ToSchema)]
pub struct PayoutMethodView {
    pub id: String,
    pub country: String,
    pub currency: String,
    pub destination_type: String,
    /// The account's last four characters, e.g. `****6789`.
    pub account: String,
    pub is_default: bool,
    #[serde(with = "time::serde::rfc3339::option")]
    #[schema(value_type = Option<String>, format = DateTime)]
    pub verified_at: Option<time::OffsetDateTime>,
}

fn view(state: &AppState, method: &PayoutMethod) -> Result<PayoutMethodView, ApiError> {
    let beneficiary = open_beneficiary(state, method)?;
    Ok(PayoutMethodView {
        id: method.id.to_string(),
        country: method.country.clone(),
        currency: method.currency.clone(),
        destination_type: method.destination_type.as_str().to_owned(),
        account: beneficiary.expose().masked(),
        is_default: method.is_default,
        verified_at: method.verified_at,
    })
}

fn open_beneficiary(
    state: &AppState,
    method: &PayoutMethod,
) -> Result<Redacted<Beneficiary>, ApiError> {
    let plain = state
        .keyring()
        .open(
            &method.beneficiary_ciphertext,
            format!("payout_methods.beneficiary:{}", method.id).as_bytes(),
        )
        .map_err(ApiError::internal)?;
    serde_json::from_slice(plain.expose())
        .map(Redacted::new)
        .map_err(ApiError::internal)
}

/// The receiver's payout methods.
#[utoipa::path(
    get,
    path = "/v1/me/payout-methods",
    operation_id = "list_payout_methods",
    tag = "payouts",
    responses((status = 200, body = Vec<PayoutMethodView>)),
    security(("session" = []), ("bearer" = []))
)]
pub async fn list_methods(
    State(state): State<AppState>,
    auth: AuthUser,
) -> Result<Json<Vec<PayoutMethodView>>, ApiError> {
    let methods = payout_methods::list_for_user(state.pool(), auth.user.id).await?;
    methods
        .iter()
        .map(|m| view(&state, m))
        .collect::<Result<_, _>>()
        .map(Json)
}

#[derive(Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct NewPayoutMethodRequest {
    /// Two-letter country code.
    country: String,
    /// `bank` or `mobile_money`.
    destination_type: String,
    /// The form's values, keyed by field `key` (e.g. `account_number`, `bank_code`, `network`).
    #[schema(value_type = Object)]
    fields: BTreeMap<String, Redacted<String>>,
    #[serde(default)]
    is_default: bool,
}

/// Add a payout method. The account is name-checked with the provider before it is saved.
#[utoipa::path(
    post,
    path = "/v1/me/payout-methods",
    operation_id = "add_payout_method",
    tag = "payouts",
    request_body = NewPayoutMethodRequest,
    responses(
        (status = 201, body = PayoutMethodView),
        (status = 404, description = "`fiat_disabled`", body = crate::error::ErrorBody),
        (status = 422, description = "`account_lookup_failed` or `fiat_unsupported`", body = crate::error::ErrorBody),
    ),
    security(("session" = []), ("bearer" = []))
)]
pub async fn add_method(
    State(state): State<AppState>,
    auth: AuthUser,
    Json(body): Json<NewPayoutMethodRequest>,
) -> Result<(StatusCode, Json<PayoutMethodView>), ApiError> {
    let fiat = provider(&state).await?;
    let country = country_code(&body.country)?;
    let destination: DestinationType = body
        .destination_type
        .parse()
        .map_err(|_| ApiError::unprocessable("fiat_unsupported", "Unknown destination type."))?;
    let currency = fiat
        .supported_countries()
        .await
        .map_err(provider_error)?
        .into_iter()
        .find(|c| c.code.eq_ignore_ascii_case(&country))
        .and_then(|c| {
            c.corridors.into_iter().find(|k| {
                k.destination_types
                    .iter()
                    .any(|d| d == destination.as_str())
            })
        })
        .map(|k| k.currency)
        .ok_or_else(|| {
            ApiError::unprocessable("fiat_unsupported", "Payouts there aren't supported yet.")
        })?;

    let field = |key: &str| {
        body.fields
            .get(key)
            .map(|v| v.expose().trim().to_owned())
            .filter(|v| !v.is_empty())
    };
    let account_number = field("account_number").ok_or_else(lookup_failed)?;
    let network = field("network");
    let bank_code = field("bank_code")
        .or_else(|| network.clone())
        .ok_or_else(lookup_failed)?;

    let resolved = fiat
        .account_lookup(&AccountQuery {
            country: country.clone(),
            kind: (destination == DestinationType::MobileMoney).then(|| "mobile_money".into()),
            bank_code: bank_code.clone(),
            account_number: account_number.clone(),
        })
        .await
        .map_err(|err| match err {
            FiatError::Rejected { .. } => lookup_failed(),
            other => provider_error(other),
        })?;
    if !resolved.is_verified {
        return Err(lookup_failed());
    }

    let beneficiary = Redacted::new(Beneficiary {
        destination_type: destination.as_str().to_owned(),
        country: country.clone(),
        account_name: resolved.account_name,
        account_number,
        bank_code: resolved.bank_code.unwrap_or(bank_code),
        network,
    });
    let id = uuid::Uuid::new_v4();
    let plain =
        Redacted::new(serde_json::to_vec(beneficiary.expose()).map_err(ApiError::internal)?);
    let sealed = state
        .keyring()
        .seal(
            plain.expose(),
            format!("payout_methods.beneficiary:{id}").as_bytes(),
        )
        .map_err(ApiError::internal)?;
    let mut conn = state.pool().acquire().await.map_err(ApiError::internal)?;
    let saved = payout_methods::insert(
        &mut conn,
        &NewPayoutMethod {
            id,
            user_id: auth.user.id,
            country: &country,
            currency: &currency,
            destination_type: destination,
            beneficiary_ciphertext: &sealed,
            provider_customer_id: None,
            verified_at: state.clock().now(),
            is_default: body.is_default,
        },
    )
    .await?;
    tracing::info!(user_id = %auth.user.id, method_id = %saved.id, %country, "payout method added");
    Ok((StatusCode::CREATED, Json(view(&state, &saved)?)))
}

fn lookup_failed() -> ApiError {
    ApiError::unprocessable(
        "account_lookup_failed",
        "We couldn't verify that account. Check the details and try again.",
    )
}

fn method_id(raw: &str) -> Result<uuid::Uuid, ApiError> {
    raw.parse().map_err(|_| ApiError::not_found())
}

/// Remove a payout method.
#[utoipa::path(
    delete,
    path = "/v1/me/payout-methods/{id}",
    operation_id = "delete_payout_method",
    tag = "payouts",
    params(("id" = String, Path)),
    responses((status = 204, description = "Removed"), (status = 404, description = "No such method", body = crate::error::ErrorBody)),
    security(("session" = []), ("bearer" = []))
)]
pub async fn delete_method(
    State(state): State<AppState>,
    auth: AuthUser,
    Path(id): Path<String>,
) -> Result<StatusCode, ApiError> {
    if payout_methods::delete(state.pool(), auth.user.id, method_id(&id)?).await? {
        Ok(StatusCode::NO_CONTENT)
    } else {
        Err(ApiError::not_found())
    }
}

#[derive(Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct UpdatePayoutMethodRequest {
    /// Make this the default (tips are paid out here), or stop it being the default (tips go to
    /// the Lightning address).
    is_default: bool,
}

/// Choose the default payout method.
#[utoipa::path(
    patch,
    path = "/v1/me/payout-methods/{id}",
    operation_id = "update_payout_method",
    tag = "payouts",
    params(("id" = String, Path)),
    request_body = UpdatePayoutMethodRequest,
    responses((status = 200, body = PayoutMethodView), (status = 404, description = "No such method", body = crate::error::ErrorBody)),
    security(("session" = []), ("bearer" = []))
)]
pub async fn update_method(
    State(state): State<AppState>,
    auth: AuthUser,
    Path(id): Path<String>,
    Json(body): Json<UpdatePayoutMethodRequest>,
) -> Result<Json<PayoutMethodView>, ApiError> {
    let id = method_id(&id)?;
    let mut conn = state.pool().acquire().await.map_err(ApiError::internal)?;
    if !payout_methods::set_default(&mut conn, auth.user.id, id, body.is_default).await? {
        return Err(ApiError::not_found());
    }
    let method = payout_methods::list_for_user(state.pool(), auth.user.id)
        .await?
        .into_iter()
        .find(|m| m.id == id)
        .ok_or_else(ApiError::not_found)?;
    view(&state, &method).map(Json)
}
