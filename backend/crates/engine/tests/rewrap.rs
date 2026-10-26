//! Master-key rotation (runbook `key-rotation.md`): `ops rewrap` moves every encrypted value to
//! the new key, so the old one can be removed.

use mergetip_core::{GithubUserId, UserId};
use mergetip_crypto::Keyring;
use mergetip_engine::ops::{RewrapReport, rewrap};
use mergetip_store::{
    users::{self, EmailSource, GithubProfile},
    wallets::{self, NewConnection},
};
use sqlx::PgPool;
use time::OffsetDateTime;

const OLD: &str = "0101010101010101010101010101010101010101010101010101010101010101";
const NEW: &str = "0202020202020202020202020202020202020202020202020202020202020202";

#[sqlx::test(migrator = "mergetip_store::MIGRATOR")]
async fn rotation_rewraps_every_secret_under_the_new_key(pool: PgPool) -> sqlx::Result<()> {
    let old = Keyring::from_hex(OLD, None).unwrap();
    let mut conn = pool.acquire().await?;
    let mut people: Vec<UserId> = Vec::new();
    for n in 0..3 {
        let user = users::upsert_by_github_id(
            &mut conn,
            &GithubProfile {
                github_id: GithubUserId::new(n),
                login: format!("user{n}"),
                avatar_url: None,
                created_at: None,
            },
        )
        .await
        .unwrap();
        let seal = |value: &str, column: &str| {
            old.seal(
                value.as_bytes(),
                format!("users.{column}:{}", user.id).as_bytes(),
            )
            .unwrap()
        };
        users::set_email(
            &pool,
            user.id,
            &seal(&format!("u{n}@example.com"), "email"),
            EmailSource::Verified,
        )
        .await
        .unwrap();
        users::set_github_tokens(
            &pool,
            user.id,
            &seal("ghu_x", "github_token"),
            Some(&seal("ghr_x", "github_refresh_token")),
            None,
        )
        .await
        .unwrap();
        let wallet = uuid::Uuid::new_v4();
        let uri = old
            .seal(
                b"nostr+walletconnect://x",
                format!("wallet_connections.nwc_uri:{wallet}").as_bytes(),
            )
            .unwrap();
        wallets::replace(
            &mut conn,
            &NewConnection {
                id: wallet,
                user_id: user.id,
                nwc_uri_ciphertext: &uri,
                wallet_pubkey: "pk",
                relays: &[],
                methods: &["pay_invoice".to_owned()],
                encryption: "nip44",
            },
            OffsetDateTime::now_utc(),
        )
        .await
        .unwrap();
        people.push(user.id);
    }

    // MASTER_KEY is now NEW, with OLD as MASTER_KEY_PREVIOUS.
    let rotated = Keyring::from_hex(NEW, Some(OLD)).unwrap();
    let report = rewrap(&pool, &rotated, "ada").await.unwrap();
    assert_eq!(
        report,
        RewrapReport {
            checked: 12,
            rewrapped: 12,
            failed: 0
        }
    );
    assert_eq!(
        rewrap(&pool, &rotated, "ada").await.unwrap().rewrapped,
        0,
        "a second pass has nothing left to do"
    );

    // The old key can go: everything opens with the new key alone.
    let new_only = Keyring::from_hex(NEW, None).unwrap();
    for id in people {
        let secrets = users::secrets(&pool, id).await.unwrap();
        for (blob, column) in [
            (secrets.email_ciphertext, "email"),
            (secrets.github_token_ciphertext, "github_token"),
            (
                secrets.github_refresh_token_ciphertext,
                "github_refresh_token",
            ),
        ] {
            new_only
                .open(&blob.unwrap(), format!("users.{column}:{id}").as_bytes())
                .unwrap();
        }
        let (wallet, blob) = wallets::active_secret(&pool, id).await.unwrap().unwrap();
        let uri = new_only
            .open(
                &blob,
                format!("wallet_connections.nwc_uri:{wallet}").as_bytes(),
            )
            .unwrap();
        assert_eq!(uri.expose().as_slice(), b"nostr+walletconnect://x");
    }
    Ok(())
}
