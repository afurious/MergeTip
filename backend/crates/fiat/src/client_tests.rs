//! `BitnobClient` against a `wiremock` Bitnob. Fixtures follow the field names in Bitnob's API
//! reference; anything assumed is marked in `client.rs`.

use std::{str::FromStr, time::Duration};

use mergetip_crypto::Redacted;
use rust_decimal::Decimal;
use serde_json::{Value, json};
use wiremock::{
    Match, Mock, MockServer, Request, ResponseTemplate,
    matchers::{method, path, query_param},
};

use super::*;
use crate::{Beneficiary, signing};

const CLIENT: &str = "client_1";
const SECRET: &str = "secret_1";

/// Matches only requests whose `X-Auth-Signature` is right for their headers and exact body.
struct SignedCorrectly;

impl Match for SignedCorrectly {
    fn matches(&self, request: &Request) -> bool {
        let header = |name: &str| {
            request
                .headers
                .get(name)
                .and_then(|v| v.to_str().ok())
                .map(str::to_owned)
        };
        let (Some(client), Some(ts), Some(nonce), Some(signature)) = (
            header("x-auth-client"),
            header("x-auth-timestamp"),
            header("x-auth-nonce"),
            header("x-auth-signature"),
        ) else {
            return false;
        };
        let body = String::from_utf8(request.body.clone()).unwrap_or_default();
        let expected =
            signing::sign_request(&client, SECRET, ts.parse().unwrap_or(0), &nonce, &body);
        client == CLIENT && nonce.len() == 32 && signature == expected.signature
    }
}

async fn bitnob() -> (MockServer, BitnobClient) {
    let server = MockServer::start().await;
    let client = BitnobClient::new(BitnobConfig {
        base_url: server.uri(),
        client_id: CLIENT.into(),
        client_secret: Redacted::new(SECRET.into()),
        retry_backoff: Duration::ZERO,
    })
    .unwrap();
    (server, client)
}

fn ok(data: Value) -> ResponseTemplate {
    ResponseTemplate::new(200)
        .set_body_json(json!({ "success": true, "message": "ok", "data": data }))
}

fn beneficiary() -> Beneficiary {
    Beneficiary {
        destination_type: "bank".into(),
        country: "NG".into(),
        account_name: "Ada Obi".into(),
        account_number: "0123456789".into(),
        bank_code: "058".into(),
        network: None,
    }
}

#[tokio::test]
async fn lists_countries_requirements_and_limits() {
    let (server, client) = bitnob().await;
    Mock::given(method("GET"))
        .and(path("/api/payouts/supported-countries"))
        .and(SignedCorrectly)
        .respond_with(ok(json!({ "countries": [
            { "code": "NG", "name": "Nigeria", "flag": "🇳🇬", "dial_code": "+234",
              "corridors": [{ "currency": "NGN", "destination_types": ["bank"] }] }
        ]})))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/payouts/supported-countries/GH"))
        .and(SignedCorrectly)
        .respond_with(ok(json!({
            "code": "GH", "name": "Ghana", "flag": "🇬🇭", "dial_code": "+233",
            "destination_types": { "mobile_money": {
                "label": "Mobile money",
                "fields": [{ "key": "account_number", "label": "Phone number", "type": "string",
                             "required": true, "component": "input", "pattern": "^\\+233\\d{9}$" },
                           { "key": "network", "label": "Network", "type": "string",
                             "required": true, "component": "select",
                             "options": [{ "label": "MTN", "value": "MTN" }] }],
                "limits": { "min": 10, "max": 5000 }
            }}
        })))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/payouts/limits"))
        .and(SignedCorrectly)
        .respond_with(ok(json!({ "limits": [
            { "country": "NG", "currency": "NGN", "min_amount": "1000", "max_amount": 5000000,
              "min_amount_usd": "1", "max_amount_usd": "3000" }
        ]})))
        .mount(&server)
        .await;

    let countries = client.supported_countries().await.unwrap();
    assert_eq!(countries[0].code, "NG");
    assert_eq!(countries[0].corridors[0].destination_types, ["bank"]);
    let ghana = client.country_requirements("GH").await.unwrap();
    let rail = &ghana.destination_types["mobile_money"];
    assert_eq!(rail.fields.len(), 2);
    assert_eq!(rail.fields[0].pattern.as_deref(), Some("^\\+233\\d{9}$"));
    let limits = client.limits().await.unwrap();
    assert_eq!(limits[0].min_amount, Decimal::from(1000));
    assert_eq!(limits[0].max_amount, Decimal::from(5_000_000));
    assert!(
        client.country_requirements("../x").await.is_err(),
        "path-safe IDs only"
    );
}

#[tokio::test]
async fn looks_up_accounts() {
    let (server, client) = bitnob().await;
    Mock::given(method("GET"))
        .and(path("/api/payouts/account-lookup"))
        .and(query_param("country", "GH"))
        .and(query_param("type", "mobile_money"))
        .and(query_param("bank_code", "MTN"))
        .and(query_param("account_number", "+233241234567"))
        .and(SignedCorrectly)
        .respond_with(ok(json!({
            "account_name": "Kofi Mensah", "account_number": "+233241234567",
            "bank_code": "MTN", "bank_name": "MTN Mobile Money", "country": "GH", "is_verified": true
        })))
        .mount(&server)
        .await;
    let resolved = client
        .account_lookup(&AccountQuery {
            country: "GH".into(),
            kind: Some("mobile_money".into()),
            bank_code: "MTN".into(),
            account_number: "+233241234567".into(),
        })
        .await
        .unwrap();
    assert_eq!(resolved.account_name, "Kofi Mensah");
    assert!(resolved.is_verified);
}

#[tokio::test]
async fn quotes_initializes_and_reads_payouts() {
    let (server, client) = bitnob().await;
    Mock::given(method("POST"))
        .and(path("/api/payouts/quotes"))
        .and(SignedCorrectly)
        .and(wiremock::matchers::body_json(json!({
            "from_asset": "btc", "to_currency": "NGN", "source": "onchain", "chain": "LIGHTNING",
            "amount": "0.00021", "payment_reason": "MergeTip tip", "reference": "tip-1", "country": "NG"
        })))
        .respond_with(ok(json!({
            "id": "6f1c", "quote_id": "QT2_123", "status": "QUOTE", "amount": "0.00021",
            "settlement_amount": "34567.80", "exchange_rate": { "rate": "164608571.43" },
            "fees": "0.10", "expires_at": "2026-11-02T10:15:00Z"
        })))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/api/payouts/QT2_123/initialize"))
        .and(SignedCorrectly)
        .and(wiremock::matchers::body_partial_json(json!({
            "quote_id": "QT2_123", "reference": "tip-1:1",
            "callback_url": "https://api.mergetip.dev/v1/webhooks/bitnob",
            "beneficiary": { "account_number": "0123456789", "bank_code": "058" }
        })))
        .respond_with(ok(json!({ "payout": {
            "id": "po_9", "status": "INITIATED", "payment_request": "lnbc210n1example",
            "expires_at": "2026-11-02T10:15:00Z"
        }})))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/payouts/po_9"))
        .and(SignedCorrectly)
        .respond_with(ok(json!({
            "id": "po_9", "reference": "tip-1:1", "status": "PROCESSING",
            "settlement_amount": "34567.80"
        })))
        .mount(&server)
        .await;

    let quote = client
        .create_quote(&QuoteRequest {
            from_asset: "btc".into(),
            to_currency: "NGN".into(),
            source: crate::LIGHTNING_SOURCE.into(),
            chain: Some(crate::LIGHTNING_CHAIN.into()),
            amount: Decimal::from_str("0.00021").unwrap(),
            payment_reason: "MergeTip tip".into(),
            reference: "tip-1".into(),
            country: "NG".into(),
        })
        .await
        .unwrap();
    assert_eq!(quote.quote_id, "QT2_123");
    assert_eq!(
        quote.settlement_amount,
        Decimal::from_str("34567.80").unwrap()
    );
    let payout = client
        .initialize_payout(
            &quote.quote_id,
            &InitializeRequest {
                customer_id: None,
                beneficiary: Redacted::new(beneficiary()),
                reference: "tip-1:1".into(),
                payment_reason: "MergeTip tip".into(),
                callback_url: "https://api.mergetip.dev/v1/webhooks/bitnob".into(),
            },
        )
        .await
        .unwrap();
    assert_eq!(payout.id, "po_9");
    assert_eq!(payout.payment_request, "lnbc210n1example");
    let status = client.get_payout("po_9").await.unwrap();
    assert_eq!(status.status, "PROCESSING");
}

#[tokio::test]
async fn outages_are_retried_but_refusals_are_not() {
    let (server, client) = bitnob().await;
    Mock::given(method("GET"))
        .and(path("/api/payouts/limits"))
        .respond_with(ResponseTemplate::new(502))
        .up_to_n_times(2)
        .expect(2)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/payouts/limits"))
        .respond_with(ok(json!({ "limits": [] })))
        .mount(&server)
        .await;
    assert_eq!(client.limits().await.unwrap(), []);

    Mock::given(method("GET"))
        .and(path("/api/payouts/account-lookup"))
        .respond_with(ResponseTemplate::new(400).set_body_json(json!({
            "success": false, "code": "VALIDATION_ERROR",
            "message": "Ghana bank accounts are not supported"
        })))
        .expect(1)
        .mount(&server)
        .await;
    let refused = client
        .account_lookup(&AccountQuery {
            country: "GH".into(),
            kind: None,
            bank_code: "030".into(),
            account_number: "1".into(),
        })
        .await
        .unwrap_err();
    assert_eq!(
        refused,
        FiatError::Rejected {
            status: 400,
            code: Some("VALIDATION_ERROR".into()),
            message: "Ghana bank accounts are not supported".into()
        }
    );

    Mock::given(method("GET"))
        .and(path("/api/payouts/supported-countries"))
        .respond_with(ResponseTemplate::new(503))
        .expect(3)
        .mount(&server)
        .await;
    assert!(
        client
            .supported_countries()
            .await
            .unwrap_err()
            .is_retryable()
    );
}

#[tokio::test]
async fn expired_quotes_and_garbage_are_distinct_errors() {
    let (server, client) = bitnob().await;
    Mock::given(method("POST"))
        .and(path("/api/payouts/QT2_1/initialize"))
        .respond_with(ResponseTemplate::new(400).set_body_json(json!({
            "success": false, "message": "Quote has expired"
        })))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/payouts/po_1"))
        .respond_with(ResponseTemplate::new(200).set_body_string("<html>oops</html>"))
        .mount(&server)
        .await;
    let request = InitializeRequest {
        customer_id: None,
        beneficiary: Redacted::new(beneficiary()),
        reference: "r".into(),
        payment_reason: "MergeTip tip".into(),
        callback_url: "https://x".into(),
    };
    assert_eq!(
        client.initialize_payout("QT2_1", &request).await,
        Err(FiatError::QuoteExpired)
    );
    assert!(matches!(
        client.get_payout("po_1").await,
        Err(FiatError::Decode(_))
    ));
}
