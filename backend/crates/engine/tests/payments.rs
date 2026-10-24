//! Paying tips end to end on the payment side: a real NWC client talking to the mock wallet over
//! an in-process relay, and a mock LNURL server minting real invoices.

use std::{sync::Arc, time::Duration};

use futures::future::join_all;

use mergetip_core::{
    Clock, FixedClock, GithubInstallationId, GithubUserId, Msats, Sats, SystemClock, ThreadRef,
    TipId, TipKind, TipState,
};
use mergetip_crypto::Keyring;
use mergetip_engine::{
    Engine, EngineError, EngineSettings, Payments, ReconcileOutcome,
    jobs::{JobPayload, PayTip, PostReply, ReconcilePayment},
};
use mergetip_github::testing::FakeGitHub;
use mergetip_payer::{
    LnurlClient, Network,
    nwc::NwcConnector,
    testing::{MintParams, MockWallet, behaviour, mint_invoice},
};
use mergetip_store::{
    feature_flags, installations,
    tips::{self, NewTip, TipPatch},
    users::{self, GithubProfile},
    wallets::{self, NewConnection},
};
use nostr_sdk::prelude::MockRelay;
use serde_json::json;
use sqlx::PgPool;
use time::OffsetDateTime;
use wiremock::{
    Mock, MockServer, Request, Respond, ResponseTemplate,
    matchers::{method, path_regex},
};

const SEED: [u8; 32] = [5; 32];
const METADATA: &str = r#"[["text/plain","MergeTip test"]]"#;
const KEY: &str = "0707070707070707070707070707070707070707070707070707070707070707";

/// LNURL-pay for any user: the callback mints a real invoice for the requested amount.
/// `wrongamount` gets an invoice for 1 sat more than asked.
struct Lnurl {
    base: String,
}

impl Respond for Lnurl {
    fn respond(&self, request: &Request) -> ResponseTemplate {
        let path = request.url.path();
        if let Some(user) = path.strip_prefix("/.well-known/lnurlp/") {
            return ResponseTemplate::new(200).set_body_json(json!({
                "tag": "payRequest",
                "callback": format!("{}/cb/{user}", self.base),
                "minSendable": 1000, "maxSendable": 1_000_000_000,
                "metadata": METADATA, "commentAllowed": 140
            }));
        }
        let user = path.trim_start_matches("/cb/");
        let amount: u64 = request
            .url
            .query_pairs()
            .find(|(k, _)| k == "amount")
            .and_then(|(_, v)| v.parse().ok())
            .unwrap_or(0);
        let amount = if user == "wrongamount" {
            amount + 1_000
        } else {
            amount
        };
        let minted = mint_invoice(&MintParams {
            amount: Msats::new(amount),
            metadata: METADATA.into(),
            seed: SEED,
            ..MintParams::default()
        });
        ResponseTemplate::new(200).set_body_json(json!({ "pr": minted.bolt11, "routes": [] }))
    }
}

struct World {
    pool: PgPool,
    engine: Engine,
    clock: Arc<FixedClock>,
    wallet: MockWallet,
    lnurl: MockServer,
    _relay: MockRelay,
    keyring: Arc<Keyring>,
    next_comment: i64,
}

impl World {
    async fn new(pool: PgPool) -> Self {
        let relay = MockRelay::run().await.unwrap();
        let wallet = MockWallet::start(&relay.url().await.to_string(), SEED)
            .await
            .unwrap();
        let lnurl = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path_regex(".*"))
            .respond_with(Lnurl { base: lnurl.uri() })
            .mount(&lnurl)
            .await;
        let clock = Arc::new(FixedClock::new(OffsetDateTime::now_utc()));
        let keyring = Arc::new(Keyring::from_hex(KEY, None).unwrap());
        let mut settings = EngineSettings::new("https://mergetip.dev", false);
        settings.pay_timeout = Duration::from_secs(2);
        let engine = Engine::new(
            pool.clone(),
            Arc::new(FakeGitHub::new()),
            clock.clone(),
            settings,
            Payments {
                // Invoice expiry is checked against real time, as a real LNURL server mints them.
                receivers: Arc::new(
                    LnurlClient::new(Network::Regtest, true, Arc::new(SystemClock)).unwrap(),
                ),
                wallets: Arc::new(NwcConnector),
                keyring: keyring.clone(),
            },
        );
        let world = Self {
            pool,
            engine,
            clock,
            wallet,
            lnurl,
            _relay: relay,
            keyring,
            next_comment: 1,
        };
        world.sender_with_wallet().await;
        world
    }

    async fn sender_with_wallet(&self) {
        let sender = self.user(10, "maintainer").await;
        let id = uuid::Uuid::new_v4();
        let ciphertext = self
            .keyring
            .seal(
                self.wallet.uri().as_bytes(),
                format!("wallet_connections.nwc_uri:{id}").as_bytes(),
            )
            .unwrap();
        let mut conn = self.pool.acquire().await.unwrap();
        let methods = ["pay_invoice", "get_info", "lookup_invoice"].map(String::from);
        wallets::replace(
            &mut conn,
            &NewConnection {
                id,
                user_id: sender.id,
                nwc_uri_ciphertext: &ciphertext,
                wallet_pubkey: "pk",
                relays: &["wss://relay".to_owned()],
                methods: &methods,
                encryption: "nip44",
            },
            OffsetDateTime::now_utc(),
        )
        .await
        .unwrap();
    }

    async fn user(&self, github_id: i64, login: &str) -> users::User {
        let mut conn = self.pool.acquire().await.unwrap();
        users::upsert_by_github_id(
            &mut conn,
            &GithubProfile {
                github_id: GithubUserId::new(github_id),
                login: login.into(),
                avatar_url: None,
                created_at: None,
            },
        )
        .await
        .unwrap()
    }

    fn address(&self, user: &str) -> String {
        format!("{user}@{}", self.lnurl.address())
    }

    /// A verified tip in `paying`, to `receiver` (who has `address`, if any).
    async fn paying_tip(&mut self, receiver: &str, address: Option<&str>, sats: u64) -> TipId {
        let installation = installations::upsert(&self.pool, GithubInstallationId::new(77), "acme")
            .await
            .unwrap();
        let sender = users::get_by_login(&self.pool, "maintainer").await.unwrap();
        let receiver_user = self.user(100 + self.next_comment, receiver).await;
        users::set_lightning_address(&self.pool, receiver_user.id, address)
            .await
            .unwrap();
        let comment = self.next_comment;
        self.next_comment += 1;
        let mut conn = self.pool.acquire().await.unwrap();
        let tip = tips::insert_idempotent(
            &mut conn,
            &NewTip {
                idempotency_key: format!("k{comment}"),
                installation_id: installation.id,
                sender_id: sender.id,
                receiver_login: receiver.into(),
                thread: ThreadRef {
                    owner: "acme".into(),
                    repo: "widgets".into(),
                    number: 42,
                    is_pr: true,
                },
                comment_id: comment,
                command_index: 0,
                kind: TipKind::Now,
                amount: Sats::new(sats),
                note: Some("for the review".into()),
            },
        )
        .await
        .unwrap()
        .unwrap();
        tips::transition(
            &mut conn,
            tip.id,
            TipState::Received,
            TipState::Verifying,
            &TipPatch::default(),
            json!({}),
        )
        .await
        .unwrap();
        tips::transition(
            &mut conn,
            tip.id,
            TipState::Verifying,
            TipState::Paying,
            &TipPatch {
                receiver_id: Some(receiver_user.id),
                ..TipPatch::default()
            },
            json!({}),
        )
        .await
        .unwrap();
        tip.id
    }

    async fn state(&self, tip: TipId) -> (String, Option<String>) {
        let row = sqlx::query!(
            "SELECT state, error_code FROM tips WHERE id = $1",
            tip.as_uuid()
        )
        .fetch_one(&self.pool)
        .await
        .unwrap();
        (row.state, row.error_code)
    }

    async fn attempts(&self, tip: TipId) -> Vec<String> {
        sqlx::query_scalar!(
            "SELECT status FROM payment_attempts WHERE tip_id = $1 ORDER BY started_at",
            tip.as_uuid()
        )
        .fetch_all(&self.pool)
        .await
        .unwrap()
    }

    async fn queued(&self, kind: &str) -> i64 {
        sqlx::query_scalar!(r#"SELECT count(*) AS "n!" FROM jobs WHERE kind = $1"#, kind)
            .fetch_one(&self.pool)
            .await
            .unwrap()
    }

    async fn reconcile_job(&self) -> (TipId, uuid::Uuid) {
        let payload =
            sqlx::query_scalar!("SELECT payload FROM jobs WHERE kind = 'reconcile_payment'")
                .fetch_one(&self.pool)
                .await
                .unwrap();
        let job = ReconcilePayment::decode(&payload).unwrap();
        (job.tip_id, job.attempt_id)
    }
}

fn state(s: &str, code: Option<&str>) -> (String, Option<String>) {
    (s.to_owned(), code.map(str::to_owned))
}

#[sqlx::test(migrator = "mergetip_store::MIGRATOR")]
async fn a_verified_tip_is_paid_from_the_senders_wallet(pool: PgPool) -> sqlx::Result<()> {
    let mut w = World::new(pool).await;
    let address = w.address("reviewer");
    let tip = w.paying_tip("reviewer", Some(&address), 2_100).await;

    w.engine.pay_tip(tip, false).await.unwrap();

    assert_eq!(w.state(tip).await, state("paid", None));
    assert_eq!(w.attempts(tip).await, ["succeeded"]);
    let row = sqlx::query!(
        "SELECT payment_hash, preimage FROM tips WHERE id = $1",
        tip.as_uuid()
    )
    .fetch_one(&w.pool)
    .await?;
    assert_eq!(w.wallet.paid_hashes(), [row.payment_hash.unwrap()]);
    assert!(row.preimage.is_some());
    assert_eq!(w.queued(PostReply::KIND).await, 2, "receipt and 🚀");

    // Running the job again (a redelivery) pays nothing more.
    w.engine.pay_tip(tip, false).await.unwrap();
    assert_eq!(w.wallet.paid_hashes().len(), 1);
    Ok(())
}

#[sqlx::test(migrator = "mergetip_store::MIGRATOR")]
async fn a_wrong_invoice_is_never_paid(pool: PgPool) -> sqlx::Result<()> {
    let mut w = World::new(pool).await;
    let address = w.address("wrongamount");
    let tip = w.paying_tip("wrongamount", Some(&address), 2_100).await;

    w.engine.pay_tip(tip, false).await.unwrap();

    assert_eq!(
        w.state(tip).await,
        state("failed", Some("receiver_unpayable"))
    );
    assert!(
        w.attempts(tip).await.is_empty(),
        "no attempt is ever reserved"
    );
    assert!(w.wallet.paid_hashes().is_empty());
    Ok(())
}

#[sqlx::test(migrator = "mergetip_store::MIGRATOR")]
async fn terminal_wallet_errors_fail_the_tip(pool: PgPool) -> sqlx::Result<()> {
    let mut w = World::new(pool).await;
    for (sats, code) in [
        (behaviour::INSUFFICIENT_BALANCE_SATS, "insufficient_balance"),
        (behaviour::QUOTA_EXCEEDED_SATS, "quota_exceeded"),
    ] {
        let address = w.address("reviewer");
        let tip = w.paying_tip("reviewer", Some(&address), sats).await;
        w.engine.pay_tip(tip, false).await.unwrap();
        assert_eq!(w.state(tip).await, state("failed", Some(code)));
        assert_eq!(w.attempts(tip).await, ["failed"]);
    }
    Ok(())
}

/// The wallet paid but its answer was lost: the attempt stays open (so nothing pays twice) and
/// reconciliation finds the payment.
#[sqlx::test(migrator = "mergetip_store::MIGRATOR")]
async fn a_lost_answer_is_reconciled_not_repaid(pool: PgPool) -> sqlx::Result<()> {
    let mut w = World::new(pool).await;
    let address = w.address("reviewer");
    let tip = w
        .paying_tip("reviewer", Some(&address), behaviour::PAYS_SILENTLY_SATS)
        .await;

    w.engine.pay_tip(tip, false).await.unwrap();
    assert_eq!(w.state(tip).await, state("paying", None));
    assert_eq!(w.attempts(tip).await, ["unknown"]);
    assert_eq!(w.queued(ReconcilePayment::KIND).await, 1);

    // The pay job running again (crash, redelivery) must not pay a second time.
    w.engine.pay_tip(tip, false).await.unwrap();
    assert_eq!(w.wallet.paid_hashes().len(), 1);
    assert_eq!(w.attempts(tip).await, ["unknown"]);

    let (tip_id, attempt_id) = w.reconcile_job().await;
    let outcome = w
        .engine
        .reconcile_payment(tip_id, attempt_id)
        .await
        .unwrap();
    assert_eq!(outcome, ReconcileOutcome::Done);
    assert_eq!(w.state(tip).await, state("paid", None));
    assert_eq!(w.attempts(tip).await, ["succeeded"]);
    assert_eq!(w.wallet.paid_hashes().len(), 1);
    Ok(())
}

/// No answer and no payment: wait until the invoice expires, then allow a fresh attempt.
#[sqlx::test(migrator = "mergetip_store::MIGRATOR")]
async fn an_unanswered_payment_is_retried_only_after_its_invoice_expires(
    pool: PgPool,
) -> sqlx::Result<()> {
    let mut w = World::new(pool).await;
    let address = w.address("reviewer");
    let tip = w
        .paying_tip("reviewer", Some(&address), behaviour::NEVER_ANSWERS_SATS)
        .await;
    w.engine.pay_tip(tip, false).await.unwrap();
    let (tip_id, attempt_id) = w.reconcile_job().await;
    sqlx::query!("DELETE FROM jobs").execute(&w.pool).await?;

    let outcome = w
        .engine
        .reconcile_payment(tip_id, attempt_id)
        .await
        .unwrap();
    assert!(
        matches!(outcome, ReconcileOutcome::CheckAgainIn(_)),
        "{outcome:?}"
    );
    assert_eq!(w.attempts(tip).await, ["unknown"]);
    assert_eq!(
        w.queued(PayTip::KIND).await,
        0,
        "no new attempt while one may be in flight"
    );

    w.clock.advance(time::Duration::minutes(12)); // past the 10-minute invoice expiry + grace
    let outcome = w
        .engine
        .reconcile_payment(tip_id, attempt_id)
        .await
        .unwrap();
    assert_eq!(outcome, ReconcileOutcome::Done);
    assert_eq!(w.attempts(tip).await, ["failed"]);
    assert_eq!(w.state(tip).await, state("paying", None));
    assert_eq!(
        w.queued(PayTip::KIND).await,
        1,
        "a fresh attempt with a new invoice"
    );
    Ok(())
}

#[sqlx::test(migrator = "mergetip_store::MIGRATOR")]
async fn concurrent_payments_never_exceed_the_daily_cap(pool: PgPool) -> sqlx::Result<()> {
    let mut w = World::new(pool).await;
    sqlx::query!("UPDATE users SET daily_max_sats = 3000 WHERE github_login = 'maintainer'")
        .execute(&w.pool)
        .await?;
    let address = w.address("reviewer");
    let mut ids = Vec::new();
    for _ in 0..5 {
        ids.push(w.paying_tip("reviewer", Some(&address), 1_000).await);
    }
    // Polled together, so the five payments really race for the sender's lock.
    let engine = &w.engine;
    let results = join_all(ids.iter().map(|id| engine.pay_tip(*id, false))).await;
    assert!(results.iter().all(Result::is_ok), "{results:?}");

    let mut paid = 0;
    let mut capped = 0;
    for id in &ids {
        match w.state(*id).await {
            (s, None) if s == "paid" => paid += 1,
            (s, Some(code)) if s == "failed" && code == "daily_cap_reached" => capped += 1,
            other => panic!("unexpected {other:?}"),
        }
    }
    assert_eq!((paid, capped), (3, 2));
    assert_eq!(w.wallet.paid_hashes().len(), 3);
    Ok(())
}

#[sqlx::test(migrator = "mergetip_store::MIGRATOR")]
async fn the_kill_switch_holds_payments(pool: PgPool) -> sqlx::Result<()> {
    let mut w = World::new(pool).await;
    feature_flags::set(&w.pool, feature_flags::PAYMENTS_ENABLED, false)
        .await
        .unwrap();
    let address = w.address("reviewer");
    let tip = w.paying_tip("reviewer", Some(&address), 21).await;

    let err = w.engine.pay_tip(tip, false).await.unwrap_err();
    assert!(matches!(err, EngineError::Retry { .. }), "{err:?}");
    assert_eq!(w.state(tip).await, state("paying", None));
    assert!(w.attempts(tip).await.is_empty());
    Ok(())
}

#[sqlx::test(migrator = "mergetip_store::MIGRATOR")]
async fn an_open_attempt_is_reconciled_instead_of_paid_again(pool: PgPool) -> sqlx::Result<()> {
    let mut w = World::new(pool).await;
    let address = w.address("reviewer");
    let tip = w.paying_tip("reviewer", Some(&address), 21).await;
    // A worker crashed right after reserving.
    sqlx::query!(
        "INSERT INTO payment_attempts (tip_id, payment_hash, bolt11, amount_msat, invoice_expires_at, status)
         VALUES ($1, 'h', 'lnbcrt...', 21000, now() + interval '10 minutes', 'pending')",
        tip.as_uuid()
    )
    .execute(&w.pool)
    .await?;

    w.engine.pay_tip(tip, false).await.unwrap();
    assert!(
        w.wallet.paid_hashes().is_empty(),
        "nothing is sent while an attempt is open"
    );
    assert_eq!(w.queued(ReconcilePayment::KIND).await, 1);
    Ok(())
}

#[sqlx::test(migrator = "mergetip_store::MIGRATOR")]
async fn without_an_address_the_tip_becomes_a_pledge(pool: PgPool) -> sqlx::Result<()> {
    let mut w = World::new(pool).await;
    let tip = w.paying_tip("newdev", None, 5_000).await;
    w.engine.pay_tip(tip, false).await.unwrap();
    assert_eq!(w.state(tip).await, state("pledged", None));
    assert!(w.attempts(tip).await.is_empty());
    assert_eq!(w.queued(PostReply::KIND).await, 1);
    Ok(())
}

#[sqlx::test(migrator = "mergetip_store::MIGRATOR")]
async fn a_disconnected_wallet_fails_the_tip(pool: PgPool) -> sqlx::Result<()> {
    let mut w = World::new(pool).await;
    let address = w.address("reviewer");
    let tip = w.paying_tip("reviewer", Some(&address), 21).await;
    let sender = users::get_by_login(&w.pool, "maintainer").await.unwrap();
    wallets::revoke(&w.pool, sender.id, w.clock.now())
        .await
        .unwrap();

    w.engine.pay_tip(tip, false).await.unwrap();
    assert_eq!(w.state(tip).await, state("failed", Some("no_wallet")));
    Ok(())
}

#[sqlx::test(migrator = "mergetip_store::MIGRATOR")]
async fn the_last_try_fails_the_tip_instead_of_stranding_it(pool: PgPool) -> sqlx::Result<()> {
    let mut w = World::new(pool).await;
    let tip = w
        .paying_tip("reviewer", Some("reviewer@127.0.0.1:9"), 21)
        .await; // nothing listens

    assert!(
        w.engine
            .pay_tip(tip, false)
            .await
            .unwrap_err()
            .is_retryable()
    );
    assert_eq!(w.state(tip).await, state("paying", None));
    w.engine.pay_tip(tip, true).await.unwrap();
    assert_eq!(w.state(tip).await, state("failed", Some("payment_failed")));
    Ok(())
}
