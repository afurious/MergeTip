//! Fixtures shared by the store integration tests.

#![allow(dead_code)]

use mergetip_core::{GithubInstallationId, GithubUserId, Sats, ThreadRef, TipKind};
use mergetip_store::{
    installations::{self, Installation},
    tips::{self, NewTip},
    users::{self, GithubProfile, User},
};
use sqlx::PgPool;

pub async fn user(pool: &PgPool, github_id: i64, login: &str) -> User {
    users::upsert_by_github_id(
        pool,
        &GithubProfile {
            github_id: GithubUserId::new(github_id),
            login: login.to_owned(),
            avatar_url: Some(format!("https://avatars.example/{login}")),
            created_at: None,
        },
    )
    .await
    .unwrap()
}

pub async fn installation(pool: &PgPool, github_id: i64) -> Installation {
    installations::upsert(pool, GithubInstallationId::new(github_id), "acme")
        .await
        .unwrap()
}

pub fn new_tip(installation: &Installation, sender: &User, comment_id: i64, index: u16) -> NewTip {
    NewTip {
        idempotency_key: tips::idempotency_key(
            installation.github_installation_id,
            comment_id,
            index,
        ),
        installation_id: installation.id,
        sender_id: sender.id,
        receiver_login: "alice".into(),
        thread: ThreadRef {
            owner: "acme".into(),
            repo: "widgets".into(),
            number: 42,
            is_pr: true,
        },
        comment_id,
        command_index: index,
        kind: TipKind::Now,
        amount: Sats::new(2_100),
        note: Some("for the review".into()),
    }
}
