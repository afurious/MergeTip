//! Claiming pledges (task 4.3).

mod common;

use axum::http::StatusCode;
use common::{fake_app, send, session_for};
use mergetip_core::{
    GithubInstallationId, GithubUserId, Sats, ThreadRef, TipId, TipKind, TipState,
};
use mergetip_crypto::random_token;
use mergetip_store::{
    installations,
    tips::{self, NewTip, TipPatch},
    users::{self, GithubProfile},
};
use serde_json::json;
use sqlx::PgPool;
use time::{Duration, OffsetDateTime};

/// A pledge from `maintainer` (github 10) to `reviewer` (github 11), expiring `expires_in` from
/// now. Returns the tip and the raw claim token.
async fn pledge(pool: &PgPool, expires_in: Duration) -> (TipId, String) {
    let installation = installations::upsert(pool, GithubInstallationId::new(77), "acme")
        .await
        .unwrap();
    let mut conn = pool.acquire().await.unwrap();
    let user = |id: i64, login: &'static str| GithubProfile {
        github_id: GithubUserId::new(id),
        login: login.into(),
        avatar_url: Some(format!("https://avatars.example/{id}")),
        created_at: None,
    };
    let sender = users::upsert_by_github_id(&mut conn, &user(10, "maintainer"))
        .await
        .unwrap();
    // Created by verification, before the receiver ever signed in.
    let receiver = users::upsert_by_github_id(&mut conn, &user(11, "reviewer"))
        .await
        .unwrap();
    let tip = tips::insert_idempotent(
        &mut conn,
        &NewTip {
            idempotency_key: format!("k-{}", uuid::Uuid::new_v4()),
            installation_id: installation.id,
            sender_id: sender.id,
            receiver_login: "reviewer".into(),
            thread: ThreadRef {
                owner: "acme".into(),
                repo: "widgets".into(),
                number: 42,
                is_pr: true,
            },
            comment_id: 1001,
            command_index: 0,
            kind: TipKind::Now,
            amount: Sats::new(5_000),
            note: Some("the docs".into()),
        },
    )
    .await
    .unwrap()
    .unwrap();
    let token = random_token().unwrap();
    for (from, to, patch) in [
        (TipState::Received, TipState::Verifying, TipPatch::default()),
        (
            TipState::Verifying,
            TipState::Paying,
            TipPatch {
                receiver_id: Some(receiver.id),
                ..TipPatch::default()
            },
        ),
        (
            TipState::Paying,
            TipState::Pledged,
            TipPatch {
                expires_at: Some(OffsetDateTime::now_utc() + expires_in),
                claim_token_hash: Some(token.hash.clone()),
                claim_token_ciphertext: Some(vec![0]),
                ..TipPatch::default()
            },
        ),
    ] {
        tips::transition(&mut conn, tip.id, from, to, &patch, json!({}))
            .await
            .unwrap();
    }
    (tip.id, token.raw.expose().clone())
}

async fn pay_jobs(pool: &PgPool) -> i64 {
    sqlx::query_scalar!(r#"SELECT count(*) AS "n!" FROM jobs WHERE kind = 'pay_tip'"#)
        .fetch_one(pool)
        .await
        .unwrap()
}

#[sqlx::test(migrator = "mergetip_store::MIGRATOR")]
async fn the_claim_page_shows_the_pledge_to_anyone(pool: PgPool) -> sqlx::Result<()> {
    let app = fake_app(pool.clone());
    let (tip_id, token) = pledge(&pool, Duration::days(30)).await;

    let (status, body) = send(&app, "GET", &format!("/v1/claims/{token}"), None, None).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let tip = &body["tip"];
    assert_eq!(tip["id"], tip_id.to_string());
    assert_eq!(tip["state"], "pledged");
    assert_eq!(tip["amount_sats"], 5_000);
    assert_eq!(tip["kind"], "now");
    assert_eq!(tip["sender"]["login"], "maintainer");
    assert_eq!(tip["receiver"]["login"], "reviewer");
    assert_eq!(tip["thread_url"], "https://github.com/acme/widgets/pull/42");
    assert!(tip["expires_at"].is_string());

    let (status, body) = send(&app, "GET", "/v1/claims/not-a-token", None, None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(body["error"]["code"], "claim_not_found");
    Ok(())
}

#[sqlx::test(migrator = "mergetip_store::MIGRATOR")]
async fn the_receiver_claims_once_and_payment_starts(pool: PgPool) -> sqlx::Result<()> {
    let app = fake_app(pool.clone());
    let (tip_id, token) = pledge(&pool, Duration::days(30)).await;
    // Signing in finds the row verification created, by GitHub account ID.
    let receiver = session_for(&pool, 11, "reviewer").await;
    let rows = sqlx::query_scalar!(r#"SELECT count(*) AS "n!" FROM users WHERE github_id = 11"#)
        .fetch_one(&pool)
        .await?;
    assert_eq!(rows, 1);

    let uri = format!("/v1/claims/{token}");
    let address = json!({ "lightning_address": "reviewer@wallet.example" });
    let (status, body) = send(&app, "POST", &uri, Some(&receiver), Some(address.clone())).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["tip"]["state"], "paying");
    assert_eq!(pay_jobs(&pool).await, 1);
    let saved = sqlx::query_scalar!("SELECT lightning_address FROM users WHERE github_id = 11")
        .fetch_one(&pool)
        .await?;
    assert_eq!(saved.as_deref(), Some("reviewer@wallet.example"));
    let token_left = sqlx::query!(
        "SELECT claim_token_hash, claim_token_ciphertext FROM tips WHERE id = $1",
        tip_id.as_uuid()
    )
    .fetch_one(&pool)
    .await?;
    assert!(token_left.claim_token_hash.is_none() && token_left.claim_token_ciphertext.is_none());

    // The link is single use: it no longer finds anything.
    let (status, _) = send(&app, "POST", &uri, Some(&receiver), Some(address)).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (status, _) = send(&app, "GET", &uri, None, None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(pay_jobs(&pool).await, 1);
    Ok(())
}

#[sqlx::test(migrator = "mergetip_store::MIGRATOR")]
async fn two_claims_at_once_pay_once(pool: PgPool) -> sqlx::Result<()> {
    let app = fake_app(pool.clone());
    let (_, token) = pledge(&pool, Duration::days(30)).await;
    let receiver = session_for(&pool, 11, "reviewer").await;
    let uri = format!("/v1/claims/{token}");
    let body = json!({ "lightning_address": "reviewer@wallet.example" });

    let (a, b) = tokio::join!(
        send(&app, "POST", &uri, Some(&receiver), Some(body.clone())),
        send(&app, "POST", &uri, Some(&receiver), Some(body.clone())),
    );
    let mut statuses = [a.0, b.0];
    statuses.sort();
    assert_eq!(statuses[0], StatusCode::OK);
    assert!(
        [StatusCode::CONFLICT, StatusCode::NOT_FOUND].contains(&statuses[1]),
        "{statuses:?}"
    );
    if statuses[1] == StatusCode::CONFLICT {
        let loser = if a.0 == StatusCode::CONFLICT {
            &a.1
        } else {
            &b.1
        };
        assert_eq!(loser["error"]["code"], "already_claimed");
    }
    assert_eq!(pay_jobs(&pool).await, 1);
    Ok(())
}

#[sqlx::test(migrator = "mergetip_store::MIGRATOR")]
async fn only_the_receiver_can_claim(pool: PgPool) -> sqlx::Result<()> {
    let app = fake_app(pool.clone());
    let (_, token) = pledge(&pool, Duration::days(30)).await;
    let uri = format!("/v1/claims/{token}");
    let body = json!({ "lightning_address": "thief@wallet.example" });

    let stranger = session_for(&pool, 99, "stranger").await;
    let (status, reply) = send(&app, "POST", &uri, Some(&stranger), Some(body.clone())).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(reply["error"]["code"], "not_receiver");
    // Not even the sender.
    let sender = session_for(&pool, 10, "maintainer").await;
    let (status, _) = send(&app, "POST", &uri, Some(&sender), Some(body.clone())).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    // Not signed in.
    let (status, _) = send(&app, "POST", &uri, None, Some(body)).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(pay_jobs(&pool).await, 0);
    Ok(())
}

#[sqlx::test(migrator = "mergetip_store::MIGRATOR")]
async fn an_expired_pledge_cannot_be_claimed(pool: PgPool) -> sqlx::Result<()> {
    let app = fake_app(pool.clone());
    // Past its deadline, but the expiry job hasn't run yet.
    let (_, token) = pledge(&pool, -Duration::minutes(1)).await;
    let receiver = session_for(&pool, 11, "reviewer").await;
    let uri = format!("/v1/claims/{token}");

    let (status, body) = send(&app, "GET", &uri, None, None).await;
    assert_eq!(status, StatusCode::GONE);
    assert_eq!(body["error"]["code"], "claim_expired");
    let body = json!({ "lightning_address": "reviewer@wallet.example" });
    let (status, _) = send(&app, "POST", &uri, Some(&receiver), Some(body)).await;
    assert_eq!(status, StatusCode::GONE);
    assert_eq!(pay_jobs(&pool).await, 0);
    Ok(())
}

#[sqlx::test(migrator = "mergetip_store::MIGRATOR")]
async fn a_claim_needs_a_working_address(pool: PgPool) -> sqlx::Result<()> {
    let app = fake_app(pool.clone());
    let (_, token) = pledge(&pool, Duration::days(30)).await;
    let receiver = session_for(&pool, 11, "reviewer").await;
    let uri = format!("/v1/claims/{token}");

    let (status, body) = send(&app, "POST", &uri, Some(&receiver), None).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(body["error"]["code"], "no_address");
    let bad = json!({ "lightning_address": "down@wallet.example" });
    let (status, body) = send(&app, "POST", &uri, Some(&receiver), Some(bad)).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(body["error"]["code"], "lnurl_unreachable");
    assert_eq!(pay_jobs(&pool).await, 0);

    // An address saved in settings is enough, with no body at all.
    sqlx::query!(
        "UPDATE users SET lightning_address = 'reviewer@wallet.example' WHERE github_id = 11"
    )
    .execute(&pool)
    .await?;
    let (status, body) = send(&app, "POST", &uri, Some(&receiver), None).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(pay_jobs(&pool).await, 1);
    Ok(())
}
