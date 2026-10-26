mod common;

use axum::{
    Router,
    body::Body,
    http::{Request, StatusCode, header},
    response::Response,
};
use common::{MASTER_KEY, WEB};
use mergetip_crypto::Keyring;
use serde_json::json;
use sqlx::PgPool;
use tower::ServiceExt;
use wiremock::{
    Mock, MockServer, ResponseTemplate,
    matchers::{body_string_contains, header as has_header, method, path},
};

async fn github() -> MockServer {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/login/oauth/access_token"))
        .and(body_string_contains("code=good-code"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "access_token": "ghu_user_token", "expires_in": 28800,
            "refresh_token": "ghr_refresh", "refresh_token_expires_in": 15897600,
            "token_type": "bearer", "scope": ""
        })))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/login/oauth/access_token"))
        .and(body_string_contains("code=bad-code"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "error": "bad_verification_code"
        })))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/user"))
        .and(has_header("authorization", "Bearer ghu_user_token"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "id": 42, "login": "alice", "type": "User",
            "avatar_url": "https://avatars.example/42", "created_at": "2019-01-01T00:00:00Z"
        })))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/user/emails"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([
            { "email": "old@example.com", "primary": false, "verified": true },
            { "email": "alice@example.com", "primary": true, "verified": true }
        ])))
        .mount(&server)
        .await;
    server
}

async fn get(app: &Router, uri: &str, cookie: Option<&str>) -> Response {
    let mut request = Request::get(uri);
    if let Some(cookie) = cookie {
        request = request.header(header::COOKIE, cookie);
    }
    app.clone()
        .oneshot(request.body(Body::empty()).unwrap())
        .await
        .unwrap()
}

fn location(response: &Response) -> String {
    response.headers()[header::LOCATION]
        .to_str()
        .unwrap()
        .to_owned()
}

fn set_cookies(response: &Response) -> Vec<String> {
    response
        .headers()
        .get_all(header::SET_COOKIE)
        .iter()
        .map(|v| v.to_str().unwrap().to_owned())
        .collect()
}

/// Starts a sign-in and returns the `mt_oauth` cookie pair and the state sent to GitHub.
async fn start(app: &Router, redirect: &str) -> (String, String) {
    let response = get(
        app,
        &format!("/v1/auth/github/login?redirect={redirect}"),
        None,
    )
    .await;
    assert_eq!(response.status(), StatusCode::SEE_OTHER);
    let target = location(&response);
    let state = target
        .split("state=")
        .nth(1)
        .expect("state in the authorize URL")
        .split('&')
        .next()
        .unwrap()
        .to_owned();
    let cookie = set_cookies(&response)
        .into_iter()
        .find(|c| c.starts_with("mt_oauth="))
        .unwrap();
    (cookie.split(';').next().unwrap().to_owned(), state)
}

#[sqlx::test(migrator = "mergetip_store::MIGRATOR")]
async fn sign_in_creates_a_user_and_a_session(pool: PgPool) -> sqlx::Result<()> {
    let gh = github().await;
    let app = common::app(pool.clone(), &gh.uri());

    let (oauth_cookie, state) = start(&app, "/claim/abc").await;
    let login = get(&app, "/v1/auth/github/login?redirect=/claim/abc", None).await;
    let authorize = location(&login);
    assert!(authorize.starts_with(&format!("{}/login/oauth/authorize?", gh.uri())));
    assert!(authorize.contains("client_id=Iv1.test"));
    assert!(authorize.contains(&format!(
        "redirect_uri={}",
        "http%3A%2F%2Flocalhost%3A8080%2Fv1%2Fauth%2Fgithub%2Fcallback"
    )));

    let callback = get(
        &app,
        &format!("/v1/auth/github/callback?code=good-code&state={state}"),
        Some(&oauth_cookie),
    )
    .await;
    assert_eq!(callback.status(), StatusCode::SEE_OTHER);
    assert_eq!(location(&callback), format!("{WEB}/claim/abc"));
    let cookies = set_cookies(&callback);
    let session = cookies
        .iter()
        .find(|c| c.starts_with("mt_session="))
        .expect("session cookie");
    assert!(session.contains("HttpOnly") && session.contains("SameSite=Lax"));
    assert!(
        cookies
            .iter()
            .any(|c| c.starts_with("mt_oauth=;") && c.contains("Max-Age=0"))
    );

    let user = sqlx::query!(
        "SELECT id, github_login, github_token_ciphertext, github_refresh_token_ciphertext,
                email_ciphertext, email_source FROM users WHERE github_id = 42"
    )
    .fetch_one(&pool)
    .await?;
    assert_eq!(user.github_login, "alice");
    assert_eq!(user.email_source.as_deref(), Some("verified"));
    let keyring = Keyring::from_hex(MASTER_KEY, None).unwrap();
    let token = keyring
        .open(
            &user.github_token_ciphertext.unwrap(),
            format!("users.github_token:{}", user.id).as_bytes(),
        )
        .unwrap();
    assert_eq!(token.expose().as_slice(), b"ghu_user_token");
    let email = keyring
        .open(
            &user.email_ciphertext.unwrap(),
            format!("users.email:{}", user.id).as_bytes(),
        )
        .unwrap();
    assert_eq!(email.expose().as_slice(), b"alice@example.com");
    assert!(user.github_refresh_token_ciphertext.is_some());

    let raw = session
        .split(';')
        .next()
        .unwrap()
        .trim_start_matches("mt_session=");
    let stored = sqlx::query_scalar!("SELECT token_hash FROM sessions")
        .fetch_one(&pool)
        .await?;
    assert_eq!(
        stored,
        mergetip_crypto::hash_token(raw),
        "only the hash is stored"
    );
    Ok(())
}

#[sqlx::test(migrator = "mergetip_store::MIGRATOR")]
async fn a_mismatched_state_is_refused(pool: PgPool) -> sqlx::Result<()> {
    let gh = github().await;
    let app = common::app(pool.clone(), &gh.uri());
    let (oauth_cookie, _state) = start(&app, "/dashboard").await;

    for (uri, cookie) in [
        (
            "/v1/auth/github/callback?code=good-code&state=forged",
            Some(oauth_cookie.as_str()),
        ),
        (
            "/v1/auth/github/callback?code=good-code&state=anything",
            None,
        ),
    ] {
        let response = get(&app, uri, cookie).await;
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    }
    let users = sqlx::query_scalar!(r#"SELECT count(*) AS "n!" FROM users"#)
        .fetch_one(&pool)
        .await?;
    assert_eq!(users, 0);
    Ok(())
}

#[sqlx::test(migrator = "mergetip_store::MIGRATOR")]
async fn open_redirects_fall_back_to_the_dashboard(pool: PgPool) -> sqlx::Result<()> {
    let gh = github().await;
    let app = common::app(pool.clone(), &gh.uri());
    let (oauth_cookie, state) = start(&app, "//evil.example").await;
    let callback = get(
        &app,
        &format!("/v1/auth/github/callback?code=good-code&state={state}"),
        Some(&oauth_cookie),
    )
    .await;
    assert_eq!(location(&callback), format!("{WEB}/dashboard"));
    Ok(())
}

#[sqlx::test(migrator = "mergetip_store::MIGRATOR")]
async fn declined_or_bad_codes_do_not_sign_in(pool: PgPool) -> sqlx::Result<()> {
    let gh = github().await;
    let app = common::app(pool.clone(), &gh.uri());

    let (cookie, state) = start(&app, "/dashboard").await;
    let declined = get(
        &app,
        &format!("/v1/auth/github/callback?error=access_denied&state={state}"),
        Some(&cookie),
    )
    .await;
    assert_eq!(
        location(&declined),
        format!("{WEB}/?auth_error=access_denied")
    );

    let (cookie, state) = start(&app, "/dashboard").await;
    let bad = get(
        &app,
        &format!("/v1/auth/github/callback?code=bad-code&state={state}"),
        Some(&cookie),
    )
    .await;
    assert_eq!(bad.status(), StatusCode::BAD_GATEWAY);
    let sessions = sqlx::query_scalar!(r#"SELECT count(*) AS "n!" FROM sessions"#)
        .fetch_one(&pool)
        .await?;
    assert_eq!(sessions, 0);
    Ok(())
}

#[sqlx::test(migrator = "mergetip_store::MIGRATOR")]
async fn a_deleted_account_can_sign_up_again(pool: PgPool) -> sqlx::Result<()> {
    let gh = github().await;
    let app = common::app(pool.clone(), &gh.uri());
    let sign_in = || async {
        let (oauth_cookie, state) = start(&app, "/dashboard").await;
        let callback = get(
            &app,
            &format!("/v1/auth/github/callback?code=good-code&state={state}"),
            Some(&oauth_cookie),
        )
        .await;
        set_cookies(&callback)
            .into_iter()
            .find(|c| c.starts_with("mt_session="))
            .unwrap()
            .split(';')
            .next()
            .unwrap()
            .to_owned()
    };

    let session = sign_in().await;
    let delete = Request::delete("/v1/me")
        .header(header::COOKIE, &session)
        .header(header::ORIGIN, WEB)
        .body(Body::empty())
        .unwrap();
    let response = app.clone().oneshot(delete).await.unwrap();
    assert_eq!(response.status(), StatusCode::NO_CONTENT);
    assert_eq!(
        get(&app, "/v1/me", Some(&session)).await.status(),
        StatusCode::UNAUTHORIZED
    );

    let again = sign_in().await;
    let me = get(&app, "/v1/me", Some(&again)).await;
    assert_eq!(me.status(), StatusCode::OK);
    let rows = sqlx::query!("SELECT github_login, deleted_at FROM users WHERE github_id = 42")
        .fetch_all(&pool)
        .await?;
    assert_eq!(rows.len(), 1, "the same row, reactivated");
    assert_eq!(rows[0].github_login, "alice");
    assert!(rows[0].deleted_at.is_none());
    Ok(())
}
