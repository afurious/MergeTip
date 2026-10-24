mod common;

use std::sync::Arc;

use axum::{
    Router,
    body::{Body, to_bytes},
    http::{Request, StatusCode, header},
    response::Response,
};
use common::{MASTER_KEY, WEB};
use mergetip_api::{github_token, router};
use mergetip_core::{FixedClock, GithubUserId, SystemClock};
use mergetip_crypto::{Keyring, random_token};
use mergetip_store::{
    sessions::{self, SessionKind},
    users::{self, GithubProfile, User},
};
use serde_json::{Value, json};
use sqlx::PgPool;
use time::{Duration, OffsetDateTime};
use tower::ServiceExt;
use wiremock::{
    Mock, MockServer, ResponseTemplate,
    matchers::{body_string_contains, method, path},
};

async fn user(pool: &PgPool) -> User {
    let mut conn = pool.acquire().await.unwrap();
    users::upsert_by_github_id(
        &mut conn,
        &GithubProfile {
            github_id: GithubUserId::new(42),
            login: "alice".into(),
            avatar_url: Some("https://avatars.example/42".into()),
            created_at: None,
        },
    )
    .await
    .unwrap()
}

async fn session(pool: &PgPool, user: &User, kind: SessionKind, ttl: Duration) -> String {
    let token = random_token().unwrap();
    sessions::create(
        pool,
        user.id,
        kind,
        &token.hash,
        OffsetDateTime::now_utc() + ttl,
        None,
    )
    .await
    .unwrap();
    token.raw.expose().clone()
}

async fn send(app: &Router, request: Request<Body>) -> Response {
    app.clone().oneshot(request).await.unwrap()
}

async fn json(response: Response) -> Value {
    serde_json::from_slice(&to_bytes(response.into_body(), 1 << 16).await.unwrap()).unwrap()
}

fn with_cookie(method: &str, uri: &str, token: &str, origin: Option<&str>) -> Request<Body> {
    let mut request = Request::builder()
        .method(method)
        .uri(uri)
        .header(header::COOKIE, format!("mt_session={token}"));
    if let Some(origin) = origin {
        request = request.header(header::ORIGIN, origin);
    }
    request.body(Body::empty()).unwrap()
}

fn with_bearer(method: &str, uri: &str, token: &str) -> Request<Body> {
    Request::builder()
        .method(method)
        .uri(uri)
        .header(header::AUTHORIZATION, format!("Bearer {token}"))
        .body(Body::empty())
        .unwrap()
}

#[sqlx::test(migrator = "mergetip_store::MIGRATOR")]
async fn me_describes_the_signed_in_user(pool: PgPool) -> sqlx::Result<()> {
    let app = common::app(pool.clone(), "http://127.0.0.1:9");
    let alice = user(&pool).await;
    let keyring = Keyring::from_hex(MASTER_KEY, None).unwrap();
    let email = keyring
        .seal(
            b"alice@example.com",
            format!("users.email:{}", alice.id).as_bytes(),
        )
        .unwrap();
    users::set_email(&pool, alice.id, &email, users::EmailSource::Verified)
        .await
        .unwrap();
    let token = session(&pool, &alice, SessionKind::Web, Duration::days(30)).await;

    let response = send(&app, with_cookie("GET", "/v1/me", &token, None)).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        json(response).await,
        json!({
            "id": alice.id.to_string(),
            "github_login": "alice",
            "avatar_url": "https://avatars.example/42",
            "email": "alice@example.com",
            "lightning_address": null,
            "wallet": { "connected": false, "relay": null, "methods": [] },
            "caps": { "per_tip_max_sats": 10000, "daily_max_sats": 50000, "spent_today_sats": 0 },
            "confirm_threshold_sats": 50000,
            "confirm_otp_enabled": false,
            "email_opt_out": false
        })
    );
    Ok(())
}

#[sqlx::test(migrator = "mergetip_store::MIGRATOR")]
async fn credentials_must_match_their_channel(pool: PgPool) -> sqlx::Result<()> {
    let app = common::app(pool.clone(), "http://127.0.0.1:9");
    let alice = user(&pool).await;
    let web = session(&pool, &alice, SessionKind::Web, Duration::days(30)).await;
    let extension = session(&pool, &alice, SessionKind::Extension, Duration::days(7)).await;
    let expired = session(&pool, &alice, SessionKind::Web, -Duration::seconds(1)).await;

    let cases = [
        (with_bearer("GET", "/v1/me", &extension), StatusCode::OK),
        (with_bearer("GET", "/v1/me", &web), StatusCode::UNAUTHORIZED),
        (
            with_cookie("GET", "/v1/me", &extension, None),
            StatusCode::UNAUTHORIZED,
        ),
        (
            with_cookie("GET", "/v1/me", &expired, None),
            StatusCode::UNAUTHORIZED,
        ),
        (
            with_cookie("GET", "/v1/me", "made-up", None),
            StatusCode::UNAUTHORIZED,
        ),
        (
            Request::get("/v1/me").body(Body::empty()).unwrap(),
            StatusCode::UNAUTHORIZED,
        ),
        // Session-only endpoints refuse extension tokens.
        (
            with_bearer("POST", "/v1/auth/extension-token", &extension),
            StatusCode::UNAUTHORIZED,
        ),
    ];
    for (request, expected) in cases {
        let uri = request.uri().clone();
        let response = send(&app, request).await;
        assert_eq!(response.status(), expected, "{uri}");
        if expected == StatusCode::UNAUTHORIZED {
            assert_eq!(json(response).await["error"]["code"], "unauthenticated");
        }
    }

    sqlx::query!("UPDATE users SET deleted_at = now()")
        .execute(&pool)
        .await?;
    let response = send(&app, with_cookie("GET", "/v1/me", &web, None)).await;
    assert_eq!(
        response.status(),
        StatusCode::UNAUTHORIZED,
        "deleted users are signed out"
    );
    Ok(())
}

#[sqlx::test(migrator = "mergetip_store::MIGRATOR")]
async fn cookie_writes_need_the_web_app_origin(pool: PgPool) -> sqlx::Result<()> {
    let app = common::app(pool.clone(), "http://127.0.0.1:9");
    let alice = user(&pool).await;
    let token = session(&pool, &alice, SessionKind::Web, Duration::days(30)).await;

    for origin in [
        None,
        Some("https://evil.example"),
        Some("http://localhost:5173.evil.example"),
    ] {
        let response = send(&app, with_cookie("POST", "/v1/auth/logout", &token, origin)).await;
        assert_eq!(response.status(), StatusCode::FORBIDDEN, "{origin:?}");
        assert_eq!(json(response).await["error"]["code"], "csrf_rejected");
    }

    let response = send(
        &app,
        with_cookie("POST", "/v1/auth/logout", &token, Some(WEB)),
    )
    .await;
    assert_eq!(response.status(), StatusCode::NO_CONTENT);
    let cleared = response.headers()[header::SET_COOKIE].to_str().unwrap();
    assert!(cleared.starts_with("mt_session=;") && cleared.contains("Max-Age=0"));
    let response = send(&app, with_cookie("GET", "/v1/me", &token, None)).await;
    assert_eq!(
        response.status(),
        StatusCode::UNAUTHORIZED,
        "the session is gone"
    );
    Ok(())
}

#[sqlx::test(migrator = "mergetip_store::MIGRATOR")]
async fn extension_tokens_work_as_bearer_tokens(pool: PgPool) -> sqlx::Result<()> {
    let app = common::app(pool.clone(), "http://127.0.0.1:9");
    let alice = user(&pool).await;
    let web = session(&pool, &alice, SessionKind::Web, Duration::days(30)).await;

    let response = send(
        &app,
        with_cookie("POST", "/v1/auth/extension-token", &web, Some(WEB)),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let body = json(response).await;
    let token = body["token"].as_str().unwrap();
    let expires = OffsetDateTime::parse(
        body["expires_at"].as_str().unwrap(),
        &time::format_description::well_known::Rfc3339,
    )
    .unwrap();
    let days = (expires - OffsetDateTime::now_utc()).whole_days();
    assert!((6..=7).contains(&days), "valid for {days} days");

    let response = send(&app, with_bearer("GET", "/v1/me", token)).await;
    assert_eq!(response.status(), StatusCode::OK);
    Ok(())
}

#[sqlx::test(migrator = "mergetip_store::MIGRATOR")]
async fn sessions_slide_forward_when_used(pool: PgPool) -> sqlx::Result<()> {
    let clock = Arc::new(FixedClock::new(OffsetDateTime::now_utc()));
    let app = router(common::state(
        pool.clone(),
        "http://127.0.0.1:9",
        clock.clone(),
    ));
    let alice = user(&pool).await;
    let token = session(&pool, &alice, SessionKind::Web, Duration::days(1)).await;
    sqlx::query!("UPDATE sessions SET last_seen_at = now() - interval '2 hours'")
        .execute(&pool)
        .await?;

    let response = send(&app, with_cookie("GET", "/v1/me", &token, None)).await;
    assert_eq!(response.status(), StatusCode::OK);
    let expires = sqlx::query_scalar!("SELECT expires_at FROM sessions")
        .fetch_one(&pool)
        .await?;
    assert!(expires - OffsetDateTime::now_utc() > Duration::days(29));
    Ok(())
}

#[sqlx::test(migrator = "mergetip_store::MIGRATOR")]
async fn expiring_github_tokens_are_refreshed(pool: PgPool) -> sqlx::Result<()> {
    let gh = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/login/oauth/access_token"))
        .and(body_string_contains("grant_type=refresh_token"))
        .and(body_string_contains("refresh_token=ghr_old"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "access_token": "ghu_new", "expires_in": 28800, "refresh_token": "ghr_new"
        })))
        .expect(1)
        .mount(&gh)
        .await;
    let state = common::state(pool.clone(), &gh.uri(), Arc::new(SystemClock));
    let alice = user(&pool).await;
    let keyring = Keyring::from_hex(MASTER_KEY, None).unwrap();
    let seal = |v: &[u8], col: &str| {
        keyring
            .seal(v, format!("users.{col}:{}", alice.id).as_bytes())
            .unwrap()
    };
    users::set_github_tokens(
        &pool,
        alice.id,
        &seal(b"ghu_old", "github_token"),
        Some(&seal(b"ghr_old", "github_refresh_token")),
        Some(OffsetDateTime::now_utc() + Duration::minutes(2)),
    )
    .await
    .unwrap();

    let token = github_token::for_user(&state, alice.id).await.unwrap();
    assert_eq!(token.expose(), "ghu_new");
    // The refreshed token is stored and reused without another refresh.
    let again = github_token::for_user(&state, alice.id).await.unwrap();
    assert_eq!(again.expose(), "ghu_new");
    Ok(())
}
