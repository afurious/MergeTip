//! Pledge emails: the source chain, opt-out, digests and reminders (task 4.6).

mod common;

use common::{Harness, KEY, REPO};
use mergetip_crypto::Keyring;
use mergetip_engine::{
    EngineSettings,
    jobs::{JobPayload, PayTip, SendDigest},
};
use mergetip_github::{Commit, CommitDetail, GitActor, testing::FakeGitHub};
use mergetip_store::{
    feature_flags,
    users::{self, EmailSource},
};
use sqlx::PgPool;
use time::Duration;

/// Pledges `body`'s tips (the receivers have no Lightning address) and runs everything after.
async fn pledge(h: &mut Harness, comment_id: i64, body: &str) {
    h.comment(comment_id, body).await;
    h.drain().await;
    h.run_only(&[PayTip::KIND]).await;
    h.drain().await;
}

async fn harness(pool: PgPool) -> Harness {
    let h = Harness::new(pool);
    h.wallet_for(10, "maintainer").await;
    h
}

fn set_profile_email(h: &Harness, email: &str) {
    let mut account = FakeGitHub::account(11, "reviewer");
    account.email = Some(email.into());
    h.github.add_user(account);
}

fn commit_by_reviewer(h: &Harness, email: &str) {
    h.github.state().commits.insert(
        (REPO.into(), 42),
        vec![Commit {
            sha: "abc123".into(),
            author: Some(FakeGitHub::account(11, "reviewer")),
            committer: None,
            commit: CommitDetail {
                message: "fix the docs".into(),
                author: Some(GitActor {
                    name: Some("Reviewer".into()),
                    email: Some(email.into()),
                }),
                committer: None,
            },
        }],
    );
}

async fn stored_email(h: &Harness) -> Option<(String, String)> {
    let user = users::get_by_login(&h.pool, "reviewer").await.unwrap();
    let secrets = users::secrets(&h.pool, user.id).await.unwrap();
    let sealed = secrets.email_ciphertext?;
    let plain = Keyring::from_hex(KEY, None)
        .unwrap()
        .open(&sealed, format!("users.email:{}", user.id).as_bytes())
        .unwrap();
    Some((
        String::from_utf8(plain.expose().clone()).unwrap(),
        secrets.email_source.unwrap(),
    ))
}

async fn email_statuses(h: &Harness) -> Vec<Option<String>> {
    sqlx::query_scalar!("SELECT email_status FROM tips ORDER BY created_at, command_index")
        .fetch_all(&h.pool)
        .await
        .unwrap()
}

#[sqlx::test(migrator = "mergetip_store::MIGRATOR")]
async fn a_verified_email_from_sign_in_is_used_first(pool: PgPool) -> sqlx::Result<()> {
    let mut h = harness(pool).await;
    set_profile_email(&h, "public@example.com");
    // The reviewer signed in once, which stored their verified email.
    let mut conn = h.pool.acquire().await?;
    let reviewer = users::upsert_by_github_id(
        &mut conn,
        &users::GithubProfile {
            github_id: mergetip_core::GithubUserId::new(11),
            login: "reviewer".into(),
            avatar_url: None,
            created_at: None,
        },
    )
    .await
    .unwrap();
    let sealed = Keyring::from_hex(KEY, None)
        .unwrap()
        .seal(
            b"verified@example.com",
            format!("users.email:{}", reviewer.id).as_bytes(),
        )
        .unwrap();
    users::set_email(&h.pool, reviewer.id, &sealed, EmailSource::Verified)
        .await
        .unwrap();

    pledge(&mut h, 1001, "/tip @reviewer 5k for the docs").await;
    let sent = h.mailer.sent().clone();
    assert_eq!(sent.len(), 1);
    assert_eq!(sent[0].to, "verified@example.com");
    assert_eq!(sent[0].tag, "pledge");
    assert!(sent[0].text.contains("https://mergetip.dev/claim/"));
    assert!(
        sent[0]
            .headers
            .iter()
            .any(|(k, v)| k == "List-Unsubscribe-Post" && v == "List-Unsubscribe=One-Click")
    );
    assert_eq!(email_statuses(&h).await, [Some("sent".into())]);
    Ok(())
}

#[sqlx::test(migrator = "mergetip_store::MIGRATOR")]
async fn the_public_profile_email_comes_next(pool: PgPool) -> sqlx::Result<()> {
    let mut h = harness(pool).await;
    set_profile_email(&h, "public@example.com");
    commit_by_reviewer(&h, "commit@example.com");
    pledge(&mut h, 1001, "/tip @reviewer 5k").await;
    assert_eq!(
        stored_email(&h).await,
        Some(("public@example.com".into(), "public_profile".into()))
    );
    assert_eq!(h.mailer.sent()[0].to, "public@example.com");
    Ok(())
}

#[sqlx::test(migrator = "mergetip_store::MIGRATOR")]
async fn then_the_commit_email_when_allowed(pool: PgPool) -> sqlx::Result<()> {
    let mut h = harness(pool).await;
    commit_by_reviewer(&h, "commit@example.com");
    pledge(&mut h, 1001, "/tip @reviewer 5k").await;
    assert_eq!(
        stored_email(&h).await,
        Some(("commit@example.com".into(), "commit".into()))
    );
    assert_eq!(h.mailer.sent().len(), 1);
    Ok(())
}

#[sqlx::test(migrator = "mergetip_store::MIGRATOR")]
async fn commit_emails_follow_the_flag_and_default_off_in_production(
    pool: PgPool,
) -> sqlx::Result<()> {
    let mut settings = EngineSettings::new("https://mergetip.dev", false);
    settings.production = true;
    let mut h = Harness::with_settings(pool, settings);
    h.wallet_for(10, "maintainer").await;
    commit_by_reviewer(&h, "commit@example.com");
    pledge(&mut h, 1001, "/tip @reviewer 5k").await;
    assert_eq!(stored_email(&h).await, None, "off by default in production");
    assert_eq!(email_statuses(&h).await, [Some("none".into())]);

    feature_flags::set(&h.pool, feature_flags::EMAIL_FROM_COMMITS, true)
        .await
        .unwrap();
    pledge(&mut h, 1002, "/tip @reviewer 1k").await;
    assert_eq!(
        stored_email(&h).await,
        Some(("commit@example.com".into(), "commit".into())),
        "an explicit flag wins"
    );
    Ok(())
}

#[sqlx::test(migrator = "mergetip_store::MIGRATOR")]
async fn noreply_addresses_are_never_used(pool: PgPool) -> sqlx::Result<()> {
    let mut h = harness(pool).await;
    set_profile_email(&h, "11+reviewer@users.noreply.github.com");
    commit_by_reviewer(&h, "11+reviewer@users.noreply.github.com");
    pledge(&mut h, 1001, "/tip @reviewer 5k").await;
    assert_eq!(stored_email(&h).await, None);
    assert_eq!(email_statuses(&h).await, [Some("none".into())]);
    assert!(h.mailer.sent().is_empty());
    // The GitHub mention still happened.
    assert!(
        h.github
            .posted_bodies()
            .iter()
            .any(|b| b.starts_with("⚡ @reviewer"))
    );
    Ok(())
}

#[sqlx::test(migrator = "mergetip_store::MIGRATOR")]
async fn opted_out_receivers_get_no_email(pool: PgPool) -> sqlx::Result<()> {
    let mut h = harness(pool).await;
    set_profile_email(&h, "public@example.com");
    sqlx::query!(
        "INSERT INTO users (github_id, github_login, email_opt_out) VALUES (11, 'reviewer', true)"
    )
    .execute(&h.pool)
    .await?;
    pledge(&mut h, 1001, "/tip @reviewer 5k").await;
    assert!(h.mailer.sent().is_empty());
    assert_eq!(email_statuses(&h).await, [Some("none".into())]);
    Ok(())
}

#[sqlx::test(migrator = "mergetip_store::MIGRATOR")]
async fn three_pledges_make_one_email(pool: PgPool) -> sqlx::Result<()> {
    let mut h = harness(pool).await;
    set_profile_email(&h, "public@example.com");
    h.github.add_user(FakeGitHub::account(13, "docs-writer"));
    // Three pledges arrive before the digest runs.
    h.comment(
        1001,
        "/tip @reviewer 5k for the review\n/tip @reviewer 1k\n/tip @reviewer 21",
    )
    .await;
    h.drain().await;
    h.run_only(&[PayTip::KIND]).await;
    h.drain().await;

    let sent = h.mailer.sent().clone();
    assert_eq!(sent.len(), 1, "one digest, not three emails");
    assert_eq!(sent[0].tag, "digest");
    assert_eq!(sent[0].subject, "You have 3 tips waiting on MergeTip");
    assert_eq!(
        sent[0]
            .text
            .matches("Claim: https://mergetip.dev/claim/")
            .count(),
        3
    );
    assert_eq!(email_statuses(&h).await, vec![Some("sent".to_owned()); 3]);
    let messages = sqlx::query_scalar!("SELECT cardinality(tip_ids) FROM email_messages")
        .fetch_all(&h.pool)
        .await?;
    assert_eq!(messages, [Some(3)]);
    assert_eq!(h.queued(SendDigest::KIND).await, 0, "nothing left to send");
    Ok(())
}

#[sqlx::test(migrator = "mergetip_store::MIGRATOR")]
async fn a_second_pledge_the_same_day_waits_a_day(pool: PgPool) -> sqlx::Result<()> {
    let mut h = harness(pool).await;
    set_profile_email(&h, "public@example.com");
    pledge(&mut h, 1001, "/tip @reviewer 5k").await;
    assert_eq!(h.mailer.sent().len(), 1);

    h.clock.advance(Duration::hours(2));
    pledge(&mut h, 1002, "/tip @reviewer 1k").await;
    assert_eq!(h.mailer.sent().len(), 1, "no second email within 24 h");
    assert_eq!(h.queued(SendDigest::KIND).await, 1);

    h.clock.advance(Duration::hours(22));
    h.release_jobs_due_by_clock().await;
    h.drain().await;
    let sent = h.mailer.sent().clone();
    assert_eq!(sent.len(), 2);
    // Lists both open pledges; only the new one is due.
    assert_eq!(sent[1].tag, "digest");
    assert!(sent[1].text.contains("new, claim by"));
    Ok(())
}

#[sqlx::test(migrator = "mergetip_store::MIGRATOR")]
async fn the_reminder_goes_out_on_day_23_once(pool: PgPool) -> sqlx::Result<()> {
    let mut h = harness(pool).await;
    set_profile_email(&h, "public@example.com");
    pledge(&mut h, 1001, "/tip @reviewer 5k").await;
    assert_eq!(h.mailer.sent().len(), 1);

    h.clock.advance(Duration::days(22) + Duration::hours(23));
    assert_eq!(h.engine.schedule_reminders().await.unwrap(), 0, "too early");

    h.clock.advance(Duration::hours(1));
    assert_eq!(h.engine.schedule_reminders().await.unwrap(), 1);
    h.release_jobs_due_by_clock().await;
    h.drain().await;
    let sent = h.mailer.sent().clone();
    assert_eq!(sent.len(), 2);
    assert_eq!(sent[1].tag, "reminder");
    assert!(
        sent[1].subject.contains("expires in 7 days"),
        "{}",
        sent[1].subject
    );

    h.clock.advance(Duration::hours(1));
    assert_eq!(h.engine.schedule_reminders().await.unwrap(), 0, "sent once");
    Ok(())
}
