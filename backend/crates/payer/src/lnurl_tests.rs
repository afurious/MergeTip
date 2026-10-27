//! LNURL-pay against a mock server, with real signed invoices.

use std::time::Duration;

use mergetip_core::{FixedClock, Msats, Sats};
use serde_json::{Value, json};
use time::OffsetDateTime;
use wiremock::{
    Mock, MockServer, ResponseTemplate,
    matchers::{method, path, query_param},
};

use super::*;
use crate::{
    InvoiceError,
    testing::{MintParams, mint_invoice},
};

const METADATA: &str = r#"[["text/plain","Sats for alice"],["text/identifier","alice@local"]]"#;

fn now() -> OffsetDateTime {
    OffsetDateTime::from_unix_timestamp(1_800_000_000).unwrap()
}

fn client() -> LnurlClient {
    LnurlClient::new(Network::Regtest, true, Arc::new(FixedClock::new(now()))).unwrap()
}

fn address(server: &MockServer) -> LightningAddress {
    format!("alice@{}", server.address()).parse().unwrap()
}

fn pay_request(server: &MockServer, comment_allowed: usize) -> Value {
    json!({
        "tag": "payRequest",
        "callback": format!("{}/lnurlp/alice/callback", server.uri()),
        "minSendable": 1_000,
        "maxSendable": 100_000_000,
        "metadata": METADATA,
        "commentAllowed": comment_allowed
    })
}

async fn serve_pay_request(server: &MockServer, body: Value) {
    Mock::given(method("GET"))
        .and(path("/.well-known/lnurlp/alice"))
        .respond_with(ResponseTemplate::new(200).set_body_json(body))
        .mount(server)
        .await;
}

async fn serve_invoice(server: &MockServer, params: &MintParams) -> String {
    let minted = mint_invoice(params);
    Mock::given(method("GET"))
        .and(path("/lnurlp/alice/callback"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(json!({ "pr": minted.bolt11, "routes": [] })),
        )
        .mount(server)
        .await;
    minted.payment_hash
}

fn mint(amount_msat: u64) -> MintParams {
    MintParams {
        amount: Msats::new(amount_msat),
        metadata: METADATA.into(),
        created_at: now(),
        expiry: Duration::from_secs(600),
        ..MintParams::default()
    }
}

#[tokio::test]
async fn resolves_an_address_to_a_validated_invoice() {
    let server = MockServer::start().await;
    serve_pay_request(&server, pay_request(&server, 20)).await;
    let hash = serve_invoice(&server, &mint(2_100_000)).await;

    let invoice = client()
        .invoice(&address(&server), Sats::new(2_100), Some("for the review"))
        .await
        .unwrap();
    assert_eq!(invoice.payment_hash, hash);
    assert_eq!(invoice.amount, Msats::new(2_100_000));

    let requests = server.received_requests().await.unwrap();
    let callback = requests
        .iter()
        .find(|r| r.url.path().ends_with("/callback"))
        .unwrap();
    let query: Vec<(String, String)> = callback.url.query_pairs().into_owned().collect();
    assert!(query.contains(&("amount".into(), "2100000".into())));
    assert!(query.contains(&("comment".into(), "for the review".into())));
}

#[tokio::test]
async fn comment_is_dropped_when_not_allowed() {
    let server = MockServer::start().await;
    serve_pay_request(&server, pay_request(&server, 0)).await;
    serve_invoice(&server, &mint(5_000)).await;

    client()
        .invoice(&address(&server), Sats::new(5), Some("hi"))
        .await
        .unwrap();
    let requests = server.received_requests().await.unwrap();
    let callback = requests
        .iter()
        .find(|r| r.url.path().ends_with("/callback"))
        .unwrap();
    assert!(!callback.url.query().unwrap().contains("comment"));
}

#[tokio::test]
async fn a_wrong_invoice_never_validates() {
    type IsExpected = fn(&InvoiceError) -> bool;
    let cases: [(MintParams, IsExpected); 4] = [
        (mint(2_100_001), |e| {
            matches!(e, InvoiceError::WrongAmount { .. })
        }),
        (
            MintParams {
                metadata: "[]".into(),
                ..mint(2_100_000)
            },
            |e| matches!(e, InvoiceError::DescriptionHashMismatch),
        ),
        (
            MintParams {
                expiry: Duration::from_secs(60),
                ..mint(2_100_000)
            },
            |e| matches!(e, InvoiceError::ExpiresTooSoon),
        ),
        (
            MintParams {
                network: Network::Bitcoin,
                ..mint(2_100_000)
            },
            |e| matches!(e, InvoiceError::WrongNetwork { .. }),
        ),
    ];
    for (minted, is_expected) in cases {
        let server = MockServer::start().await;
        serve_pay_request(&server, pay_request(&server, 0)).await;
        serve_invoice(&server, &minted).await;
        let err = client()
            .invoice(&address(&server), Sats::new(2_100), None)
            .await
            .unwrap_err();
        match &err {
            ResolveError::Invoice(inner) => assert!(is_expected(inner), "{err}"),
            other => panic!("expected an invoice error, got {other}"),
        }
        assert!(!err.is_retryable());
    }
}

#[tokio::test]
async fn amounts_outside_the_range_are_refused_before_asking() {
    let server = MockServer::start().await;
    let mut body = pay_request(&server, 0);
    body["minSendable"] = json!(10_000);
    body["maxSendable"] = json!(1_000_000);
    serve_pay_request(&server, body).await;

    for sats in [9, 1_001] {
        let err = client()
            .invoice(&address(&server), Sats::new(sats), None)
            .await
            .unwrap_err();
        assert_eq!(
            err,
            ResolveError::AmountOutOfRange {
                min: 10_000,
                max: 1_000_000
            }
        );
    }
    let callbacks = server
        .received_requests()
        .await
        .unwrap()
        .into_iter()
        .filter(|r| r.url.path().ends_with("/callback"))
        .count();
    assert_eq!(callbacks, 0);
}

#[tokio::test]
async fn refusals_and_malformed_responses_are_distinct_errors() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/.well-known/lnurlp/alice"))
        .respond_with(ResponseTemplate::new(404).set_body_json(json!({
            "status": "ERROR", "reason": "no such user"
        })))
        .mount(&server)
        .await;
    assert_eq!(
        client().probe(&address(&server)).await,
        Err(ResolveError::Refused("no such user".into()))
    );

    let server = MockServer::start().await;
    serve_pay_request(&server, json!({ "tag": "withdrawRequest" })).await;
    assert!(matches!(
        client().probe(&address(&server)).await,
        Err(ResolveError::Invalid(_))
    ));

    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/.well-known/lnurlp/alice"))
        .respond_with(ResponseTemplate::new(200).set_body_string("<html>"))
        .mount(&server)
        .await;
    assert!(matches!(
        client().probe(&address(&server)).await,
        Err(ResolveError::Invalid(_))
    ));

    let server = MockServer::start().await;
    serve_pay_request(&server, pay_request(&server, 0)).await;
    Mock::given(method("GET"))
        .and(path("/lnurlp/alice/callback"))
        .and(query_param("amount", "1000"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "status": "ERROR", "reason": "wallet offline"
        })))
        .mount(&server)
        .await;
    assert_eq!(
        client()
            .invoice(&address(&server), Sats::new(1), None)
            .await,
        Err(ResolveError::Refused("wallet offline".into()))
    );
}

#[tokio::test]
async fn server_errors_are_retried_then_reported_as_unreachable() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/.well-known/lnurlp/alice"))
        .respond_with(ResponseTemplate::new(503))
        .expect(3)
        .mount(&server)
        .await;
    let err = client().probe(&address(&server)).await.unwrap_err();
    assert!(matches!(err, ResolveError::Unreachable(_)));
    assert!(err.is_retryable());
}

#[tokio::test]
async fn plain_http_is_refused_for_public_hosts_and_without_dev_mode() {
    let strict =
        LnurlClient::new(Network::Regtest, false, Arc::new(FixedClock::new(now()))).unwrap();
    let local: LightningAddress = "alice@127.0.0.1:9".parse().unwrap();
    assert!(matches!(
        strict.probe(&local).await,
        Err(ResolveError::InvalidAddress(_))
    ));

    // Even the local mock can't hand out a plain-HTTP callback on a public host.
    let server = MockServer::start().await;
    let mut body = pay_request(&server, 0);
    body["callback"] = json!("http://evil.example/cb");
    serve_pay_request(&server, body).await;
    assert_eq!(
        client().probe(&address(&server)).await,
        Err(ResolveError::Invalid("callback is not HTTPS".into()))
    );
}

/// M3 review (SSRF): the address's server chooses the callback; it must not aim the worker at
/// an internal address such as the cloud metadata service.
#[tokio::test]
async fn callbacks_to_internal_hosts_are_refused() {
    let server = MockServer::start().await;
    for callback in [
        "https://169.254.169.254/latest/meta-data",
        "https://10.0.0.5/cb",
        "https://[::1]/cb",
        "https://localhost/cb",
    ] {
        let mut body = pay_request(&server, 0);
        body["callback"] = json!(callback);
        server.reset().await;
        serve_pay_request(&server, body).await;
        let err = client().probe(&address(&server)).await.unwrap_err();
        assert!(
            matches!(&err, ResolveError::Invalid(why) if why.contains("non-public")),
            "{callback}: {err:?}"
        );
    }
}

#[tokio::test]
async fn addresses_on_internal_ip_literals_are_refused() {
    let production =
        LnurlClient::new(Network::Regtest, false, Arc::new(FixedClock::new(now()))).unwrap();
    for raw in [
        "alice@10.0.0.5",
        "alice@169.254.169.254",
        "alice@192.168.1.10:8080",
    ] {
        let address: LightningAddress = raw.parse().unwrap();
        assert!(
            matches!(
                production.probe(&address).await,
                Err(ResolveError::InvalidAddress(_))
            ),
            "{raw}"
        );
    }
}
