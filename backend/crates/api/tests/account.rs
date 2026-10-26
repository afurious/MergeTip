//! `GET /me/tips`, `GET /users/{login}/receive-status`, `POST /tips/comment` and `DELETE /me`
//! (task 4.8).

mod common;

use std::{collections::HashSet, sync::Arc};

use axum::{Router, http::StatusCode};
use common::{FakeReceivers, MASTER_KEY, fake_app, send, session_for, state_with};
use mergetip_api::router;
use mergetip_core::{
    GithubInstallationId, Sats, SystemClock, ThreadRef, TipId, TipKind, TipState, UserId,
};
use mergetip_crypto::Keyring;
use mergetip_payer::nwc::NwcConnector;
use mergetip_store::{
    installations::{self, InstallationRepo},
    tips::{self, NewTip, TipPatch},
    users,
};
use serde_json::{Value, json};
use sqlx::PgPool;
use wiremock::{
    Mock, MockServer, ResponseTemplate,
    matchers::{body_json, header, method, path},
};

struct Fixture {
    sender: UserId,
    receiver: UserId,
    installation: mergetip_core::InstallationId,
}

async fn fixture(pool: &PgPool) -> Fixture {
    let installation = installations::upsert(pool, GithubInstallationId::new(77), "acme")
        .await
        .unwrap();
    let mut conn = pool.acquire().await.unwrap();
    installations::set_repos(
        &mut conn,
        installation.id,
        &[InstallationRepo {
            github_repo_id: 5,
            full_name: "acme/widgets".into(),
        }],
    )
    .await
    .unwrap();
    session_for(pool, 10, "maintainer").await;
    session_for(pool, 11, "reviewer").await;
    Fixture {
        sender: users::get_by_login(pool, "maintainer").await.unwrap().id,
        receiver: users::get_by_login(pool, "reviewer").await.unwrap().id,
        installation: installation.id,
    }
}

/// A tip from the maintainer to the reviewer, moved to `state` along a legal path.
async fn tip(pool: &PgPool, f: &Fixture, n: i64, state: TipState) -> TipId {
    let mut conn = pool.acquire().await.unwrap();
    let tip = tips::insert_idempotent(
        &mut conn,
        &NewTip {
            idempotency_key: format!("k{n}-{}", uuid::Uuid::new_v4()),
            installation_id: f.installation,
            sender_id: f.sender,
            receiver_login: "reviewer".into(),
            thread: ThreadRef {
                owner: "acme".into(),
                repo: "widgets".into(),
                number: 42,
                is_pr: true,
            },
            comment_id: n,
            command_index: 0,
            kind: TipKind::Now,
            amount: Sats::new(100 + u64::try_from(n).unwrap()),
            note: None,
        },
    )
    .await
    .unwrap()
    .unwrap();
    let verified = TipPatch {
        receiver_id: Some(f.receiver),
        ..TipPatch::default()
    };
    let path: &[(TipState, TipState)] = match state {
        TipState::Received => &[],
        TipState::AwaitingSender => &[(TipState::Received, TipState::AwaitingSender)],
        TipState::Held => &[
            (TipState::Received, TipState::Verifying),
            (TipState::Verifying, TipState::Held),
        ],
        TipState::Paying => &[
            (TipState::Received, TipState::Verifying),
            (TipState::Verifying, TipState::Paying),
        ],
        TipState::Pledged => &[
            (TipState::Received, TipState::Verifying),
            (TipState::Verifying, TipState::Paying),
            (TipState::Paying, TipState::Pledged),
        ],
        other => panic!("no fixture path to {other}"),
    };
    for (from, to) in path {
        // Only verification links the receiver.
        let patch = if *from == TipState::Received {
            TipPatch::default()
        } else {
            verified.clone()
        };
        tips::transition(&mut conn, tip.id, *from, *to, &patch, json!({}))
            .await
            .unwrap();
    }
    tip.id
}

async fn page(app: &Router, session: &str, query: &str) -> (Vec<String>, Option<String>) {
    let (status, body) = send(
        app,
        "GET",
        &format!("/v1/me/tips?{query}"),
        Some(session),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let ids = body["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["id"].as_str().unwrap().to_owned())
        .collect();
    (ids, body["next_cursor"].as_str().map(str::to_owned))
}

#[sqlx::test(migrator = "mergetip_store::MIGRATOR")]
async fn tips_page_newest_first_with_a_stable_cursor(pool: PgPool) -> sqlx::Result<()> {
    let app = fake_app(pool.clone());
    let f = fixture(&pool).await;
    let session = session_for(&pool, 10, "maintainer").await;
    let mut created = Vec::new();
    for n in 0..120 {
        created.push(tip(&pool, &f, n, TipState::Received).await.to_string());
    }

    let (first, cursor) = page(&app, &session, "direction=sent").await;
    assert_eq!(first.len(), 50);
    assert_eq!(first[0], created[119], "newest first");

    // New tips arrive while the client pages; the pages it already has stay valid.
    for n in 120..130 {
        tip(&pool, &f, n, TipState::Received).await;
    }
    let cursor = cursor.expect("more pages");
    let (second, cursor) = page(&app, &session, &format!("direction=sent&cursor={cursor}")).await;
    let (third, cursor) = page(
        &app,
        &session,
        &format!("direction=sent&cursor={}", cursor.unwrap()),
    )
    .await;
    assert_eq!((second.len(), third.len(), cursor), (50, 20, None));
    let seen: Vec<String> = first.into_iter().chain(second).chain(third).collect();
    let unique: HashSet<&String> = seen.iter().collect();
    assert_eq!(unique.len(), 120, "no tip repeated");
    let mut expected: Vec<String> = created.clone();
    expected.reverse();
    assert_eq!(seen, expected, "none skipped, newest first");
    Ok(())
}

#[sqlx::test(migrator = "mergetip_store::MIGRATOR")]
async fn tips_filter_by_direction_and_state(pool: PgPool) -> sqlx::Result<()> {
    let app = fake_app(pool.clone());
    let f = fixture(&pool).await;
    let maintainer = session_for(&pool, 10, "maintainer").await;
    let reviewer = session_for(&pool, 11, "reviewer").await;
    let held = tip(&pool, &f, 1, TipState::Held).await;
    tip(&pool, &f, 2, TipState::AwaitingSender).await;

    assert_eq!(page(&app, &maintainer, "direction=sent").await.0.len(), 2);
    // The second tip has no receiver until it is verified.
    assert_eq!(
        page(&app, &reviewer, "direction=received").await.0,
        [held.to_string()]
    );
    assert_eq!(page(&app, &reviewer, "direction=sent").await.0.len(), 0);
    assert_eq!(
        page(&app, &maintainer, "state=held").await.0,
        [held.to_string()]
    );

    let (status, body) = send(&app, "GET", "/v1/me/tips", Some(&maintainer), None).await;
    assert_eq!(status, StatusCode::OK);
    let item = &body["items"][1];
    assert_eq!(item["state"], "held");
    assert_eq!(item["receiver"]["login"], "reviewer");
    assert_eq!(
        item["thread_url"],
        "https://github.com/acme/widgets/pull/42"
    );

    for bad in ["direction=up", "state=nope", "cursor=%%%"] {
        let (status, body) = send(
            &app,
            "GET",
            &format!("/v1/me/tips?{bad}"),
            Some(&maintainer),
            None,
        )
        .await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{bad}");
        assert_eq!(body["error"]["code"], "invalid_query");
    }
    let (status, _) = send(&app, "GET", "/v1/me/tips", None, None).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    Ok(())
}

#[sqlx::test(migrator = "mergetip_store::MIGRATOR")]
async fn receive_status_says_whether_a_tip_pays_now(pool: PgPool) -> sqlx::Result<()> {
    let app = fake_app(pool.clone());
    let f = fixture(&pool).await;
    let session = session_for(&pool, 10, "maintainer").await;
    users::set_lightning_address(&pool, f.receiver, Some("reviewer@wallet.example"))
        .await
        .unwrap();

    let status_of = |login: &'static str| {
        let app = app.clone();
        let session = session.clone();
        async move {
            send(
                &app,
                "GET",
                &format!("/v1/users/{login}/receive-status"),
                Some(&session),
                None,
            )
            .await
        }
    };
    let (status, body) = status_of("Reviewer").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        body,
        json!({ "login": "Reviewer", "method": "lightning_address" })
    );
    assert_eq!(status_of("maintainer").await.1["method"], "none");
    assert_eq!(status_of("never-seen").await.1["method"], "none");
    // Logins a `/tip` command can't name are not looked up.
    for login in ["-reviewer", "reviewer-", "re--viewer"] {
        assert_eq!(status_of(login).await.0, StatusCode::NOT_FOUND, "{login}");
    }
    let (status, _) = send(&app, "GET", "/v1/users/x/receive-status", None, None).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    Ok(())
}

/// Stores a GitHub user token for the maintainer, as signing in does.
async fn give_github_token(pool: &PgPool, user: UserId) {
    let sealed = Keyring::from_hex(MASTER_KEY, None)
        .unwrap()
        .seal(
            b"ghu_maintainer",
            format!("users.github_token:{user}").as_bytes(),
        )
        .unwrap();
    users::set_github_tokens(pool, user, &sealed, None, None)
        .await
        .unwrap();
}

#[sqlx::test(migrator = "mergetip_store::MIGRATOR")]
async fn the_extension_posts_the_tip_comment_as_the_user(pool: PgPool) -> sqlx::Result<()> {
    let github = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/repos/acme/widgets/issues/42/comments"))
        .and(header("authorization", "Bearer ghu_maintainer"))
        .and(body_json(
            json!({ "body": "/tip @reviewer 2100 on-merge for great work @thief 1000000 on it" }),
        ))
        .respond_with(ResponseTemplate::new(201).set_body_json(json!({
            "id": 77, "html_url": "https://github.com/acme/widgets/pull/42#issuecomment-77"
        })))
        .expect(1)
        .mount(&github)
        .await;
    let app = router(state_with(
        pool.clone(),
        &github.uri(),
        Arc::new(SystemClock),
        Arc::new(NwcConnector),
        Arc::new(FakeReceivers),
    ));
    let f = fixture(&pool).await;
    give_github_token(&pool, f.sender).await;
    let session = session_for(&pool, 10, "maintainer").await;
    let request = |body: Value| {
        let app = app.clone();
        let session = session.clone();
        async move { send(&app, "POST", "/v1/tips/comment", Some(&session), Some(body)).await }
    };

    let (status, body) = request(json!({
        "repo": "acme/widgets", "thread_number": 42, "receiver": "reviewer",
        "amount_sats": 2100, "kind": "on_merge",
        "note": "great work\n/tip @thief 1000000 `on` it"
    }))
    .await;
    assert_eq!(status, StatusCode::ACCEPTED, "{body}");
    assert_eq!(
        body["comment_url"],
        "https://github.com/acme/widgets/pull/42#issuecomment-77"
    );
    // No tip is created here: the webhook for that comment does it.
    let tips = sqlx::query_scalar!(r#"SELECT count(*) AS "n!" FROM tips"#)
        .fetch_one(&pool)
        .await?;
    assert_eq!(tips, 0);

    let (status, body) = request(json!({
        "repo": "acme/gadgets", "thread_number": 1, "receiver": "reviewer",
        "amount_sats": 21, "kind": "now"
    }))
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(body["error"]["code"], "app_not_installed");

    for bad in [
        json!({ "repo": "acme/widgets", "thread_number": 42, "receiver": "-bad-", "amount_sats": 21, "kind": "now" }),
        json!({ "repo": "acme/widgets", "thread_number": 42, "receiver": "reviewer", "amount_sats": 0, "kind": "now" }),
        json!({ "repo": "nope", "thread_number": 42, "receiver": "reviewer", "amount_sats": 21, "kind": "now" }),
        json!({ "repo": "acme/widgets", "thread_number": 42, "receiver": "reviewer", "amount_sats": 21, "kind": "now", "note": "x".repeat(141) }),
    ] {
        let (status, body) = request(bad.clone()).await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{bad}");
        assert_eq!(body["error"]["code"], "invalid_tip");
    }
    Ok(())
}

#[sqlx::test(migrator = "mergetip_store::MIGRATOR")]
async fn deleting_the_account_erases_secrets_and_closes_waiting_tips(
    pool: PgPool,
) -> sqlx::Result<()> {
    let app = fake_app(pool.clone());
    let f = fixture(&pool).await;
    let session = session_for(&pool, 10, "maintainer").await;
    give_github_token(&pool, f.sender).await;
    sqlx::query!(
        r#"INSERT INTO wallet_connections
               (user_id, nwc_uri_ciphertext, wallet_pubkey, relays, methods, encryption)
           VALUES ($1, '\x00', 'pk', '{wss://relay}', '{pay_invoice}', 'nip44')"#,
        f.sender.as_uuid()
    )
    .execute(&pool)
    .await?;
    let waiting = tip(&pool, &f, 1, TipState::AwaitingSender).await;
    let held = tip(&pool, &f, 2, TipState::Held).await;
    let pledged = tip(&pool, &f, 3, TipState::Pledged).await;
    let paying = tip(&pool, &f, 4, TipState::Paying).await;

    let (status, _) = send(&app, "DELETE", "/v1/me", Some(&session), None).await;
    assert_eq!(status, StatusCode::NO_CONTENT);

    let (status, _) = send(&app, "GET", "/v1/me", Some(&session), None).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED, "signed out everywhere");
    let user = sqlx::query!(
        "SELECT github_login, deleted_at, github_token_ciphertext, email_ciphertext,
                lightning_address, avatar_url FROM users WHERE id = $1",
        f.sender.as_uuid()
    )
    .fetch_one(&pool)
    .await?;
    assert!(
        user.github_login.starts_with("deleted-"),
        "{}",
        user.github_login
    );
    assert!(user.deleted_at.is_some());
    assert!(user.github_token_ciphertext.is_none() && user.email_ciphertext.is_none());
    let active_wallets = sqlx::query_scalar!(
        r#"SELECT count(*) AS "n!" FROM wallet_connections WHERE revoked_at IS NULL"#
    )
    .fetch_one(&pool)
    .await?;
    assert_eq!(active_wallets, 0);

    let state = |id: TipId| {
        let pool = pool.clone();
        async move {
            sqlx::query_scalar!("SELECT state FROM tips WHERE id = $1", id.as_uuid())
                .fetch_one(&pool)
                .await
                .unwrap()
        }
    };
    assert_eq!(state(waiting).await, "cancelled");
    assert_eq!(state(held).await, "cancelled");
    assert_eq!(state(pledged).await, "expired");
    // M4 review: its deadline is still ahead, so it must not linger as an unsent expiry notice.
    let notice = sqlx::query!(
        "SELECT last_notified_at, expires_at, email_status FROM tips WHERE id = $1",
        pledged.as_uuid()
    )
    .fetch_one(&pool)
    .await?;
    assert!(notice.last_notified_at >= notice.expires_at);
    assert_eq!(notice.email_status.as_deref(), Some("none"));
    assert_eq!(
        state(paying).await,
        "paying",
        "a payment in flight finishes on its own"
    );
    let rows = sqlx::query_scalar!(r#"SELECT count(*) AS "n!" FROM tips"#)
        .fetch_one(&pool)
        .await?;
    assert_eq!(rows, 4, "tip rows are kept for idempotency");
    Ok(())
}

/// M4 review: a stolen session must not be able to switch confirmation codes off (then confirm
/// a large tip) or raise limits. Loosening needs the web cookie from a recent sign-in.
#[sqlx::test(migrator = "mergetip_store::MIGRATOR")]
async fn loosening_protections_needs_a_recent_web_sign_in(pool: PgPool) -> sqlx::Result<()> {
    let app = fake_app(pool.clone());
    let session = session_for(&pool, 10, "maintainer").await;
    let patch = |body: Value, session: String| {
        let app = app.clone();
        async move { send(&app, "PATCH", "/v1/me/settings", Some(&session), Some(body)).await }
    };
    sqlx::query!("UPDATE users SET confirm_otp_enabled = true WHERE github_id = 10")
        .execute(&pool)
        .await?;

    // A session from yesterday can tighten, not loosen.
    sqlx::query!("UPDATE sessions SET created_at = now() - interval '1 day'")
        .execute(&pool)
        .await?;
    for loosen in [
        json!({ "confirm_otp_enabled": false }),
        json!({ "confirm_threshold_sats": 100_000 }),
        json!({ "per_tip_max_sats": 20_000 }),
        json!({ "daily_max_sats": 60_000 }),
    ] {
        let (status, body) = patch(loosen.clone(), session.clone()).await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{loosen}");
        assert_eq!(body["error"]["code"], "reauth_required");
    }
    let (status, _) = patch(
        json!({ "per_tip_max_sats": 5_000, "confirm_threshold_sats": 20_000 }),
        session.clone(),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "tightening is always fine");

    // A fresh sign-in can loosen.
    let fresh = session_for(&pool, 10, "maintainer").await;
    let (status, body) = patch(
        json!({ "confirm_otp_enabled": false, "per_tip_max_sats": 20_000 }),
        fresh,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["confirm_otp_enabled"], false);

    // The extension's token never can, however fresh.
    let token = mergetip_crypto::random_token().unwrap();
    let user = users::get_by_login(&pool, "maintainer").await.unwrap();
    mergetip_store::sessions::create(
        &pool,
        user.id,
        mergetip_store::sessions::SessionKind::Extension,
        &token.hash,
        time::OffsetDateTime::now_utc() + time::Duration::days(1),
        None,
    )
    .await
    .unwrap();
    let request = axum::http::Request::patch("/v1/me/settings")
        .header("authorization", format!("Bearer {}", token.raw.expose()))
        .header("content-type", "application/json")
        .body(axum::body::Body::from(
            json!({ "daily_max_sats": 90_000 }).to_string(),
        ))
        .unwrap();
    let response = tower::ServiceExt::oneshot(app.clone(), request)
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
    Ok(())
}

/// M3 review: sessions slide in the database; the browser's cookie must slide with them.
#[sqlx::test(migrator = "mergetip_store::MIGRATOR")]
async fn using_a_session_renews_its_cookie(pool: PgPool) -> sqlx::Result<()> {
    let app = fake_app(pool.clone());
    let session = session_for(&pool, 10, "maintainer").await;
    let get = |cookie: String, uri: &'static str| {
        let app = app.clone();
        async move {
            let request = axum::http::Request::get(uri)
                .header("cookie", format!("mt_session={cookie}"))
                .body(axum::body::Body::empty())
                .unwrap();
            tower::ServiceExt::oneshot(app, request).await.unwrap()
        }
    };
    let set_cookies = |response: &axum::response::Response| -> Vec<String> {
        response
            .headers()
            .get_all("set-cookie")
            .iter()
            .map(|v| v.to_str().unwrap().to_owned())
            .collect()
    };

    let me = get(session.clone(), "/v1/me").await;
    assert_eq!(me.status(), StatusCode::OK);
    let renewed = set_cookies(&me);
    assert_eq!(renewed.len(), 1, "{renewed:?}");
    assert!(renewed[0].starts_with(&format!("mt_session={session};")));
    assert!(renewed[0].contains("Max-Age=2592000") && renewed[0].contains("HttpOnly"));

    // A cookie that doesn't authenticate is never echoed back.
    let bogus = get("not-a-session".into(), "/v1/me").await;
    assert_eq!(bogus.status(), StatusCode::UNAUTHORIZED);
    assert!(set_cookies(&bogus).is_empty());
    let public = get("not-a-session".into(), "/v1/health").await;
    assert!(set_cookies(&public).is_empty());

    // Signing out still only clears it.
    let (status, _) = send(&app, "POST", "/v1/auth/logout", Some(&session), None).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    Ok(())
}
