//! Rate limits, security headers and body limits (task 5.8).

mod common;

use axum::{
    Router,
    body::Body,
    http::{Request, StatusCode, header},
    response::Response,
};
use common::{WEB, fake_app, session_for};
use sqlx::PgPool;
use tower::ServiceExt;

async fn post(app: &Router, uri: &str, ip: &str, session: Option<&str>, body: &str) -> Response {
    let mut request = Request::post(uri)
        .header("fly-client-ip", ip)
        .header(header::ORIGIN, WEB)
        .header(header::CONTENT_TYPE, "application/json");
    if let Some(session) = session {
        request = request.header(header::COOKIE, format!("mt_session={session}"));
    }
    app.clone()
        .oneshot(request.body(Body::from(body.to_owned())).unwrap())
        .await
        .unwrap()
}

async fn code(response: Response) -> String {
    let bytes = axum::body::to_bytes(response.into_body(), 1 << 16)
        .await
        .unwrap();
    serde_json::from_slice::<serde_json::Value>(&bytes).unwrap()["error"]["code"]
        .as_str()
        .unwrap_or_default()
        .to_owned()
}

#[sqlx::test(migrator = "mergetip_store::MIGRATOR")]
async fn confirmation_codes_are_rate_limited_per_ip(pool: PgPool) -> sqlx::Result<()> {
    let app = fake_app(pool.clone());
    let uri = "/v1/confirmations/00000000-0000-0000-0000-000000000000/otp";
    for _ in 0..5 {
        let response = post(&app, uri, "203.0.113.7", None, "").await;
        assert_ne!(response.status(), StatusCode::TOO_MANY_REQUESTS);
    }
    // One more request refills every 12 s; allow a slow machine a little slack.
    let mut limited = None;
    for _ in 0..5 {
        let response = post(&app, uri, "203.0.113.7", None, "").await;
        if response.status() == StatusCode::TOO_MANY_REQUESTS {
            limited = Some(response);
            break;
        }
    }
    let limited = limited.expect("the per-IP limit applies");
    let retry_after: u64 = limited.headers()[header::RETRY_AFTER]
        .to_str()
        .unwrap()
        .parse()
        .unwrap();
    assert!((1..=60).contains(&retry_after), "{retry_after}");
    assert_eq!(code(limited).await, "rate_limited");
    // Another address is unaffected.
    let other = post(&app, uri, "198.51.100.9", None, "").await;
    assert_ne!(other.status(), StatusCode::TOO_MANY_REQUESTS);
    Ok(())
}

#[sqlx::test(migrator = "mergetip_store::MIGRATOR")]
async fn one_user_cannot_spread_claims_across_addresses(pool: PgPool) -> sqlx::Result<()> {
    let app = fake_app(pool.clone());
    let session = session_for(&pool, 11, "reviewer").await;
    for n in 0..10 {
        let response = post(
            &app,
            "/v1/claims/nope",
            &format!("192.0.2.{n}"),
            Some(&session),
            "{}",
        )
        .await;
        assert_ne!(
            response.status(),
            StatusCode::TOO_MANY_REQUESTS,
            "request {n}"
        );
    }
    // The quota refills one request every 6 s; a slow machine may earn one or two more, but a
    // burst from new addresses is still cut off well before 20.
    let mut limited = None;
    for n in 0..10 {
        let response = post(
            &app,
            "/v1/claims/nope",
            &format!("198.51.100.{n}"),
            Some(&session),
            "{}",
        )
        .await;
        if response.status() == StatusCode::TOO_MANY_REQUESTS {
            limited = Some(response);
            break;
        }
    }
    let limited = limited.expect("the per-user limit applies across addresses");
    assert_eq!(code(limited).await, "rate_limited");
    Ok(())
}

#[sqlx::test(migrator = "mergetip_store::MIGRATOR")]
async fn every_response_carries_security_headers(pool: PgPool) -> sqlx::Result<()> {
    let app = fake_app(pool.clone());
    let public = app
        .clone()
        .oneshot(Request::get("/v1/health").body(Body::empty()).unwrap())
        .await
        .unwrap();
    let headers = public.headers();
    assert_eq!(
        headers[header::STRICT_TRANSPORT_SECURITY],
        "max-age=63072000; includeSubDomains"
    );
    assert_eq!(headers[header::X_CONTENT_TYPE_OPTIONS], "nosniff");
    assert_eq!(headers[header::REFERRER_POLICY], "no-referrer");
    assert!(
        !headers.contains_key(header::CACHE_CONTROL),
        "public responses may be cached"
    );

    let session = session_for(&pool, 11, "reviewer").await;
    let private = app
        .clone()
        .oneshot(
            Request::get("/v1/me")
                .header(header::COOKIE, format!("mt_session={session}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(private.status(), StatusCode::OK);
    assert_eq!(private.headers()[header::CACHE_CONTROL], "no-store");
    // Errors too.
    let missing = app
        .clone()
        .oneshot(Request::get("/v1/nope").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(missing.headers()[header::X_CONTENT_TYPE_OPTIONS], "nosniff");
    Ok(())
}

#[sqlx::test(migrator = "mergetip_store::MIGRATOR")]
async fn bodies_are_capped_per_route(pool: PgPool) -> sqlx::Result<()> {
    let app = fake_app(pool.clone());
    let session = session_for(&pool, 11, "reviewer").await;
    let big_note = format!(
        r#"{{"per_tip_max_sats": 1, "pad": "{}"}}"#,
        "x".repeat(70 * 1024)
    );
    let response = app
        .clone()
        .oneshot(
            Request::patch("/v1/me/settings")
                .header(header::COOKIE, format!("mt_session={session}"))
                .header(header::ORIGIN, WEB)
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(big_note))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::PAYLOAD_TOO_LARGE);

    // GitHub deliveries may be large: a 1 MiB body is read (and refused for its signature).
    let big = format!(r#"{{"pad":"{}"}}"#, "x".repeat(1024 * 1024));
    let github = app
        .clone()
        .oneshot(
            Request::post("/v1/webhooks/github")
                .header("x-hub-signature-256", "sha256=00")
                .body(Body::from(big.clone()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(github.status(), StatusCode::UNAUTHORIZED);
    let resend = app
        .clone()
        .oneshot(
            Request::post("/v1/webhooks/resend")
                .header("svix-id", "a")
                .header("svix-timestamp", "1")
                .header("svix-signature", "v1,AA==")
                .body(Body::from(big))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resend.status(), StatusCode::PAYLOAD_TOO_LARGE);
    Ok(())
}
