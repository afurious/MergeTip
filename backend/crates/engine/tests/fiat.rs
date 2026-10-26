//! Paying tips out in local currency (task 5.4): quote, initialize, pay Bitnob's invoice from the
//! sender's wallet, and wait for the payout webhook before calling the tip paid.

use std::{
    collections::VecDeque,
    str::FromStr,
    sync::{Arc, Mutex},
    time::Duration,
};

use async_trait::async_trait;
use mergetip_core::{
    FixedClock, GithubInstallationId, GithubUserId, Msats, Sats, SystemClock, ThreadRef, TipId,
    TipKind, TipState,
};
use mergetip_crypto::Keyring;
use mergetip_engine::{
    Engine, EngineSettings, Payments, ReconcileOutcome,
    jobs::{JobPayload, ReconcilePayment},
};
use mergetip_fiat::{Beneficiary, Limit, testing::FakeFiat};
use mergetip_github::testing::FakeGitHub;
use mergetip_payer::{
    LnurlClient, Network, PaymentProof, SenderWallet, WalletConnector,
    nwc::{Encryption, Lookup, PayError, WalletErrorCode, WalletInfo},
    testing::{MintParams, mint_invoice, preimage_from_invoice},
};
use mergetip_store::{
    feature_flags, installations,
    payout_methods::{self, DestinationType, NewPayoutMethod},
    tips::{self, NewTip, TipPatch},
    users::{self, GithubProfile},
    wallets::{self, NewConnection},
};
use rust_decimal::Decimal;
use serde_json::json;
use sqlx::PgPool;
use time::OffsetDateTime;

const SEED: [u8; 32] = [8; 32];
const KEY: &str = "0707070707070707070707070707070707070707070707070707070707070707";

/// What the scripted wallet does with each `pay_invoice`, in order (then: pay).
#[derive(Debug, Clone, Copy)]
enum Step {
    Pay,
    /// The invoice expired while the payment was being routed.
    Expired,
    /// Pays, but the answer is lost.
    PayAndTimeOut,
}

#[derive(Default)]
struct Script {
    steps: VecDeque<Step>,
    paid: Vec<String>,
}

#[derive(Clone, Default)]
struct ScriptedWallet(Arc<Mutex<Script>>);

impl ScriptedWallet {
    fn plan(&self, steps: &[Step]) {
        self.0.lock().unwrap().steps = steps.iter().copied().collect();
    }

    fn paid(&self) -> Vec<String> {
        self.0.lock().unwrap().paid.clone()
    }
}

#[async_trait]
impl WalletConnector for ScriptedWallet {
    async fn connect(&self, _uri: &str) -> Result<Box<dyn SenderWallet>, PayError> {
        Ok(Box::new(self.clone()))
    }
}

#[async_trait]
impl SenderWallet for ScriptedWallet {
    async fn get_info(&self) -> Result<WalletInfo, PayError> {
        Ok(WalletInfo {
            methods: vec!["pay_invoice".into(), "lookup_invoice".into()],
            encryption: Encryption::Nip44,
            wallet_pubkey: "pk".into(),
            relays: vec![],
        })
    }

    async fn pay_invoice(
        &self,
        bolt11: &str,
        payment_hash: &str,
        _timeout: Duration,
    ) -> Result<PaymentProof, PayError> {
        let step = self
            .0
            .lock()
            .unwrap()
            .steps
            .pop_front()
            .unwrap_or(Step::Pay);
        if matches!(step, Step::Expired) {
            return Err(PayError::Wallet {
                code: WalletErrorCode::PaymentFailed,
                message: "invoice expired".into(),
            });
        }
        self.0.lock().unwrap().paid.push(payment_hash.to_owned());
        if matches!(step, Step::PayAndTimeOut) {
            return Err(PayError::Timeout);
        }
        let preimage = preimage_from_invoice(&SEED, bolt11).expect("a minted invoice");
        Ok(PaymentProof {
            payment_hash: payment_hash.to_owned(),
            preimage: hex::encode(preimage),
        })
    }

    async fn lookup_invoice(&self, payment_hash: &str) -> Result<Lookup, PayError> {
        // Only invoices minted here are looked up; their preimage is re-derivable from the seed,
        // but the wallet only knows hashes, so settle with what it recorded.
        let paid = self
            .0
            .lock()
            .unwrap()
            .paid
            .contains(&payment_hash.to_owned());
        Ok(if paid {
            Lookup::Settled {
                preimage: PREIMAGES
                    .lock()
                    .unwrap()
                    .get(payment_hash)
                    .cloned()
                    .unwrap_or_default(),
            }
        } else {
            Lookup::Pending
        })
    }
}

/// Preimages of the invoices handed to the fake Bitnob, by payment hash.
static PREIMAGES: std::sync::LazyLock<Mutex<std::collections::HashMap<String, String>>> =
    std::sync::LazyLock::new(Default::default);

struct World {
    pool: PgPool,
    engine: Engine,
    fiat: Arc<FakeFiat>,
    wallet: ScriptedWallet,
    receiver: users::User,
}

impl World {
    async fn new(pool: PgPool) -> Self {
        let fiat = Arc::new(FakeFiat::new());
        {
            let mut state = fiat.state();
            // 100,000,000 NGN per BTC: one sat is one naira.
            state.rate = Decimal::from(100_000_000);
            state.limits = vec![Limit {
                country: "NG".into(),
                currency: "NGN".into(),
                min_amount: Decimal::from(1_000),
                max_amount: Decimal::from(5_000_000),
            }];
        }
        let wallet = ScriptedWallet::default();
        let keyring = Arc::new(Keyring::from_hex(KEY, None).unwrap());
        let mut settings = EngineSettings::new("https://mergetip.dev", false);
        settings.api_base_url = "https://api.mergetip.dev".into();
        let engine = Engine::new(
            pool.clone(),
            Arc::new(FakeGitHub::new()),
            Arc::new(FixedClock::new(OffsetDateTime::now_utc())),
            settings,
            Payments {
                receivers: Arc::new(
                    LnurlClient::new(Network::Regtest, true, Arc::new(SystemClock)).unwrap(),
                ),
                wallets: Arc::new(wallet.clone()),
                keyring: keyring.clone(),
            },
        )
        .with_fiat(fiat.clone(), Network::Regtest);
        feature_flags::set(&pool, feature_flags::FIAT_PAYOUTS, true)
            .await
            .unwrap();

        let mut conn = pool.acquire().await.unwrap();
        let profile = |id: i64, login: &str| GithubProfile {
            github_id: GithubUserId::new(id),
            login: login.into(),
            avatar_url: None,
            created_at: None,
        };
        let sender = users::upsert_by_github_id(&mut conn, &profile(10, "maintainer"))
            .await
            .unwrap();
        let receiver = users::upsert_by_github_id(&mut conn, &profile(11, "reviewer"))
            .await
            .unwrap();
        let methods = ["pay_invoice", "lookup_invoice"].map(String::from);
        let connection = uuid::Uuid::new_v4();
        let sealed_uri = keyring
            .seal(
                b"nostr+walletconnect://scripted",
                format!("wallet_connections.nwc_uri:{connection}").as_bytes(),
            )
            .unwrap();
        wallets::replace(
            &mut conn,
            &NewConnection {
                id: connection,
                user_id: sender.id,
                nwc_uri_ciphertext: &sealed_uri,
                wallet_pubkey: "pk",
                relays: &["wss://relay".to_owned()],
                methods: &methods,
                encryption: "nip44",
            },
            OffsetDateTime::now_utc(),
        )
        .await
        .unwrap();

        // The reviewer is paid out to a Nigerian bank account by default.
        let method = uuid::Uuid::new_v4();
        let beneficiary = Beneficiary {
            destination_type: "bank".into(),
            country: "NG".into(),
            account_name: "Ada Obi".into(),
            account_number: "0123456789".into(),
            bank_code: "058".into(),
            network: None,
        };
        let sealed = keyring
            .seal(
                &serde_json::to_vec(&beneficiary).unwrap(),
                format!("payout_methods.beneficiary:{method}").as_bytes(),
            )
            .unwrap();
        payout_methods::insert(
            &mut conn,
            &NewPayoutMethod {
                id: method,
                user_id: receiver.id,
                country: "NG",
                currency: "NGN",
                destination_type: DestinationType::Bank,
                beneficiary_ciphertext: &sealed,
                provider_customer_id: None,
                verified_at: OffsetDateTime::now_utc(),
                is_default: true,
            },
        )
        .await
        .unwrap();
        Self {
            pool,
            engine,
            fiat,
            wallet,
            receiver,
        }
    }

    /// Bitnob will hand out `count` invoices for `sats`, payable by the scripted wallet.
    fn invoices(&self, sats: u64, count: usize) {
        let mut state = self.fiat.state();
        for _ in 0..count {
            let minted = mint_invoice(&MintParams {
                amount: Msats::new(sats * 1_000),
                metadata: "bitnob payout".into(),
                seed: SEED,
                expiry: Duration::from_secs(900),
                ..MintParams::default()
            });
            PREIMAGES
                .lock()
                .unwrap()
                .insert(minted.payment_hash.clone(), minted.preimage.clone());
            state.invoices.push_back((
                minted.bolt11,
                OffsetDateTime::now_utc() + time::Duration::minutes(15),
            ));
        }
    }

    /// A verified tip of `sats` to the reviewer, in `paying`.
    async fn paying_tip(&self, sats: u64) -> TipId {
        let installation = installations::upsert(&self.pool, GithubInstallationId::new(77), "acme")
            .await
            .unwrap();
        let sender = users::get_by_login(&self.pool, "maintainer").await.unwrap();
        let mut conn = self.pool.acquire().await.unwrap();
        let tip = tips::insert_idempotent(
            &mut conn,
            &NewTip {
                idempotency_key: format!("k{}", uuid::Uuid::new_v4()),
                installation_id: installation.id,
                sender_id: sender.id,
                receiver_login: "reviewer".into(),
                thread: ThreadRef {
                    owner: "acme".into(),
                    repo: "widgets".into(),
                    number: 42,
                    is_pr: true,
                },
                comment_id: 1,
                command_index: 0,
                kind: TipKind::Now,
                amount: Sats::new(sats),
                note: None,
            },
        )
        .await
        .unwrap()
        .unwrap();
        for (from, to, patch) in [
            (TipState::Received, TipState::Verifying, TipPatch::default()),
            (
                TipState::Verifying,
                TipState::Paying,
                TipPatch {
                    receiver_id: Some(self.receiver.id),
                    ..TipPatch::default()
                },
            ),
        ] {
            tips::transition(&mut conn, tip.id, from, to, &patch, json!({}))
                .await
                .unwrap();
        }
        tip.id
    }

    async fn tip(&self, id: TipId) -> tips::Tip {
        tips::get(&self.pool, id).await.unwrap()
    }

    async fn receipts(&self) -> i64 {
        sqlx::query_scalar!(
            r#"SELECT count(*) AS "n!" FROM jobs WHERE kind = 'post_reply'
               AND payload->'reply'->>'message' = 'paid'"#
        )
        .fetch_one(&self.pool)
        .await
        .unwrap()
    }
}

#[sqlx::test(migrator = "mergetip_store::MIGRATOR")]
async fn a_fiat_tip_pays_bitnobs_invoice_and_waits_for_the_payout(
    pool: PgPool,
) -> sqlx::Result<()> {
    let w = World::new(pool).await;
    w.invoices(5_000, 1);
    let tip = w.paying_tip(5_000).await;

    w.engine.pay_tip(tip, false).await.unwrap();
    let paid = w.tip(tip).await;
    assert_eq!(
        paid.state,
        TipState::FiatProcessing,
        "paid only once Bitnob confirms"
    );
    assert_eq!(
        w.wallet.paid().len(),
        1,
        "the sender's wallet paid Bitnob's invoice once"
    );
    assert_eq!(w.receipts().await, 0, "no receipt before the payout lands");
    assert_eq!(paid.fiat.currency.as_deref(), Some("NGN"));
    assert_eq!(paid.fiat.amount, Some(Decimal::from_str("5000").unwrap()));
    assert_eq!(paid.fiat.payout_id.as_deref(), Some("po_1"));
    assert!(paid.preimage.is_some());

    let state = w.fiat.state();
    assert_eq!(state.quotes.len(), 1);
    assert_eq!(
        state.quotes[0].reference,
        tip.to_string(),
        "the quote references the tip"
    );
    assert_eq!(
        state.quotes[0].amount,
        Decimal::from_str("0.00005").unwrap()
    );
    assert_eq!(state.initialized[0].2.account_number, "0123456789");
    assert!(state.initialized[0].1.starts_with(&tip.to_string()));
    Ok(())
}

#[sqlx::test(migrator = "mergetip_store::MIGRATOR")]
async fn a_tip_below_the_corridor_minimum_falls_back(pool: PgPool) -> sqlx::Result<()> {
    let w = World::new(pool).await;
    // 500 sats is 500 NGN, under the 1,000 NGN minimum, and the reviewer has no Lightning
    // address: the tip waits as a pledge instead of being paid out.
    let tip = w.paying_tip(500).await;
    w.engine.pay_tip(tip, false).await.unwrap();
    assert_eq!(w.tip(tip).await.state, TipState::Pledged);
    assert!(w.wallet.paid().is_empty());
    assert!(
        w.fiat.state().initialized.is_empty(),
        "no payout was initialized"
    );
    Ok(())
}

#[sqlx::test(migrator = "mergetip_store::MIGRATOR")]
async fn an_expired_quote_is_requoted_once(pool: PgPool) -> sqlx::Result<()> {
    let w = World::new(pool).await;
    w.invoices(5_000, 1);
    w.fiat.state().expire_initializes = 1;
    let tip = w.paying_tip(5_000).await;
    w.engine.pay_tip(tip, false).await.unwrap();
    assert_eq!(w.tip(tip).await.state, TipState::FiatProcessing);
    assert_eq!(w.fiat.state().quotes.len(), 2, "one re-quote");
    assert_eq!(w.wallet.paid().len(), 1);
    Ok(())
}

#[sqlx::test(migrator = "mergetip_store::MIGRATOR")]
async fn a_quote_that_expires_twice_fails_cleanly(pool: PgPool) -> sqlx::Result<()> {
    let w = World::new(pool).await;
    w.fiat.state().expire_initializes = 2;
    let tip = w.paying_tip(5_000).await;
    w.engine.pay_tip(tip, false).await.unwrap();
    let failed = w.tip(tip).await;
    assert_eq!(failed.state, TipState::Failed);
    assert_eq!(failed.error_code.as_deref(), Some("fiat_quote_expired"));
    assert!(w.wallet.paid().is_empty(), "nothing was charged");
    Ok(())
}

/// Critical test 9: the quote's invoice expires while the wallet is paying it. One re-quote,
/// then a clean failure, and the sender is never charged.
#[sqlx::test(migrator = "mergetip_store::MIGRATOR")]
async fn a_quote_expiring_during_payment_requotes_once_then_fails(
    pool: PgPool,
) -> sqlx::Result<()> {
    let w = World::new(pool).await;
    w.invoices(5_000, 2);
    w.wallet.plan(&[Step::Expired, Step::Expired]);
    let tip = w.paying_tip(5_000).await;

    let first = w.engine.pay_tip(tip, false).await;
    assert!(
        first.is_err_and(|e| e.is_retryable()),
        "retried with a fresh quote"
    );
    assert_eq!(w.tip(tip).await.state, TipState::Paying);
    w.engine.pay_tip(tip, false).await.unwrap();

    let failed = w.tip(tip).await;
    assert_eq!(failed.state, TipState::Failed);
    assert_eq!(failed.error_code.as_deref(), Some("fiat_quote_expired"));
    assert_eq!(w.fiat.state().quotes.len(), 2, "exactly one re-quote");
    assert!(w.wallet.paid().is_empty(), "nothing was charged");
    Ok(())
}

#[sqlx::test(migrator = "mergetip_store::MIGRATOR")]
async fn a_lost_wallet_answer_is_reconciled_not_paid_again(pool: PgPool) -> sqlx::Result<()> {
    let w = World::new(pool).await;
    w.invoices(5_000, 2);
    w.wallet.plan(&[Step::PayAndTimeOut]);
    let tip = w.paying_tip(5_000).await;

    w.engine.pay_tip(tip, false).await.unwrap();
    assert_eq!(w.tip(tip).await.state, TipState::Paying, "outcome unknown");
    let payload = sqlx::query_scalar!("SELECT payload FROM jobs WHERE kind = 'reconcile_payment'")
        .fetch_one(&w.pool)
        .await?;
    let job = ReconcilePayment::decode(&payload).unwrap();
    assert_eq!(
        w.engine
            .reconcile_payment(job.tip_id, job.attempt_id)
            .await
            .unwrap(),
        ReconcileOutcome::Done
    );
    assert_eq!(w.tip(tip).await.state, TipState::FiatProcessing);
    assert_eq!(w.wallet.paid().len(), 1, "paid exactly once");
    assert_eq!(w.fiat.state().initialized.len(), 1, "no second payout");
    Ok(())
}

/// Stores and applies a Bitnob webhook, as the endpoint and worker do.
async fn webhook(w: &World, event_id: &str, event: &str, payout: &str, reference: &str) {
    let payload = json!({
        "event": event, "event_id": event_id,
        "data": { "id": payout, "reference": reference, "status": "x" }
    });
    mergetip_store::bitnob_events::insert_if_new(&w.pool, event_id, &payload)
        .await
        .unwrap();
    w.engine.process_bitnob_event(event_id).await.unwrap();
}

async fn funded(w: &World) -> (TipId, String) {
    w.invoices(5_000, 1);
    let tip = w.paying_tip(5_000).await;
    w.engine.pay_tip(tip, false).await.unwrap();
    assert_eq!(w.tip(tip).await.state, TipState::FiatProcessing);
    (tip, format!("{tip}:1:1"))
}

async fn paid_out_replies(w: &World) -> i64 {
    sqlx::query_scalar!(
        r#"SELECT count(*) AS "n!" FROM jobs WHERE kind = 'post_reply'
           AND payload->'reply'->>'message' = 'paid_out'"#
    )
    .fetch_one(&w.pool)
    .await
    .unwrap()
}

#[sqlx::test(migrator = "mergetip_store::MIGRATOR")]
async fn the_success_webhook_marks_the_tip_paid_with_a_receipt(pool: PgPool) -> sqlx::Result<()> {
    let w = World::new(pool).await;
    let (tip, reference) = funded(&w).await;
    webhook(&w, "ev1", "payouts.processing", "po_1", &reference).await;
    assert_eq!(w.tip(tip).await.state, TipState::FiatProcessing);
    webhook(&w, "ev2", "payouts.withdrawal.success", "po_1", &reference).await;
    let paid = w.tip(tip).await;
    assert_eq!(paid.state, TipState::Paid);
    assert_eq!(paid.fiat.provider_status.as_deref(), Some("success"));
    assert_eq!(paid_out_replies(&w).await, 1);

    // Retried and late events change nothing.
    webhook(&w, "ev2", "payouts.withdrawal.success", "po_1", &reference).await;
    webhook(&w, "ev3", "payouts.processing", "po_1", &reference).await;
    webhook(&w, "ev4", "payouts.withdrawal.expired", "po_1", &reference).await;
    let still = w.tip(tip).await;
    assert_eq!(still.state, TipState::Paid);
    assert_eq!(still.fiat.provider_status.as_deref(), Some("success"));
    assert_eq!(paid_out_replies(&w).await, 1);
    Ok(())
}

#[sqlx::test(migrator = "mergetip_store::MIGRATOR")]
async fn an_expired_payout_leaves_the_tip_stuck_for_an_operator(pool: PgPool) -> sqlx::Result<()> {
    let w = World::new(pool).await;
    let (tip, reference) = funded(&w).await;
    webhook(&w, "ev1", "payouts.withdrawal.expired", "po_1", &reference).await;
    let stuck = w.tip(tip).await;
    assert_eq!(stuck.state, TipState::FiatStuck);
    assert_eq!(stuck.error_code.as_deref(), Some("payout_failed"));
    // A late success doesn't silently paper over it: an operator resolves stuck payouts.
    webhook(&w, "ev2", "payouts.withdrawal.success", "po_1", &reference).await;
    assert_eq!(w.tip(tip).await.state, TipState::FiatStuck);
    Ok(())
}

#[sqlx::test(migrator = "mergetip_store::MIGRATOR")]
async fn events_for_unknown_payouts_are_ignored(pool: PgPool) -> sqlx::Result<()> {
    let w = World::new(pool).await;
    let (tip, _) = funded(&w).await;
    webhook(&w, "ev1", "payouts.withdrawal.success", "po_other", "junk").await;
    webhook(
        &w,
        "ev2",
        "payouts.withdrawal.success",
        "po_9",
        &format!("{tip}:9:9"),
    )
    .await;
    assert_eq!(w.tip(tip).await.state, TipState::FiatProcessing);
    let processed = sqlx::query_scalar!(
        r#"SELECT count(*) AS "n!" FROM bitnob_events WHERE processed_at IS NOT NULL"#
    )
    .fetch_one(&w.pool)
    .await?;
    assert_eq!(processed, 2);
    Ok(())
}

/// Bitnob confirms delivery before the sender's wallet answers: the success is recorded and
/// applied as soon as the payment is.
#[sqlx::test(migrator = "mergetip_store::MIGRATOR")]
async fn a_success_that_beats_the_wallet_still_lands(pool: PgPool) -> sqlx::Result<()> {
    let w = World::new(pool).await;
    w.invoices(5_000, 1);
    w.wallet.plan(&[Step::PayAndTimeOut]);
    let tip = w.paying_tip(5_000).await;
    w.engine.pay_tip(tip, false).await.unwrap();
    assert_eq!(w.tip(tip).await.state, TipState::Paying);

    webhook(
        &w,
        "ev1",
        "payouts.withdrawal.success",
        "po_1",
        &format!("{tip}:1:1"),
    )
    .await;
    assert_eq!(
        w.tip(tip).await.state,
        TipState::Paying,
        "waits for the payment proof"
    );

    let payload = sqlx::query_scalar!("SELECT payload FROM jobs WHERE kind = 'reconcile_payment'")
        .fetch_one(&w.pool)
        .await?;
    let job = ReconcilePayment::decode(&payload).unwrap();
    w.engine
        .reconcile_payment(job.tip_id, job.attempt_id)
        .await
        .unwrap();
    assert_eq!(w.tip(tip).await.state, TipState::Paid);
    assert_eq!(paid_out_replies(&w).await, 1);
    Ok(())
}

async fn stuck(w: &World) -> TipId {
    let (tip, reference) = funded(w).await;
    webhook(
        w,
        "ev-stuck",
        "payouts.withdrawal.expired",
        "po_1",
        &reference,
    )
    .await;
    assert_eq!(w.tip(tip).await.state, TipState::FiatStuck);
    tip
}

async fn last_event_detail(w: &World, tip: TipId) -> serde_json::Value {
    sqlx::query_scalar!(
        "SELECT detail FROM tip_events WHERE tip_id = $1 ORDER BY id DESC LIMIT 1",
        tip.as_uuid()
    )
    .fetch_one(&w.pool)
    .await
    .unwrap()
}

#[sqlx::test(migrator = "mergetip_store::MIGRATOR")]
async fn an_operator_retry_lands_a_delivered_payout(pool: PgPool) -> sqlx::Result<()> {
    let w = World::new(pool).await;
    let tip = stuck(&w).await;
    // Bitnob delivered after all.
    w.fiat.state().payouts.get_mut("po_1").unwrap().status = "COMPLETED".into();

    let state = mergetip_engine::ops::fiat_retry(&w.pool, w.fiat.as_ref(), tip, "ada")
        .await
        .unwrap();
    assert_eq!(state, TipState::Paid);
    let events = sqlx::query_scalar!(
        "SELECT detail FROM tip_events WHERE tip_id = $1 AND from_state = 'fiat_stuck'",
        tip.as_uuid()
    )
    .fetch_one(&w.pool)
    .await?;
    assert_eq!(events, json!({ "operator": "ada", "action": "fiat-retry" }));
    Ok(())
}

#[sqlx::test(migrator = "mergetip_store::MIGRATOR")]
async fn a_retry_of_a_still_failed_payout_is_stuck_again(pool: PgPool) -> sqlx::Result<()> {
    let w = World::new(pool).await;
    let tip = stuck(&w).await;
    w.fiat.state().payouts.get_mut("po_1").unwrap().status = "FAILED".into();
    let state = mergetip_engine::ops::fiat_retry(&w.pool, w.fiat.as_ref(), tip, "ada")
        .await
        .unwrap();
    assert_eq!(state, TipState::FiatStuck);
    Ok(())
}

#[sqlx::test(migrator = "mergetip_store::MIGRATOR")]
async fn a_refund_is_recorded_only_for_the_full_tip(pool: PgPool) -> sqlx::Result<()> {
    let w = World::new(pool).await;
    let tip = stuck(&w).await;
    let invoice = |sats: u64| {
        mint_invoice(&MintParams {
            amount: Msats::new(sats * 1_000),
            seed: SEED,
            ..MintParams::default()
        })
    };
    let now = OffsetDateTime::now_utc();
    let short = mergetip_engine::ops::fiat_refund(
        &w.pool,
        tip,
        &invoice(4_000).bolt11,
        Network::Regtest,
        "ada",
        now,
    )
    .await;
    assert!(matches!(
        short,
        Err(mergetip_engine::ops::OpsError::BadRefund(_))
    ));
    assert_eq!(w.tip(tip).await.state, TipState::FiatStuck);

    let refund = invoice(5_000);
    let state = mergetip_engine::ops::fiat_refund(
        &w.pool,
        tip,
        &refund.bolt11,
        Network::Regtest,
        "ada",
        now,
    )
    .await
    .unwrap();
    assert_eq!(state, TipState::Refunded);
    let detail = last_event_detail(&w, tip).await;
    assert_eq!(detail["operator"], "ada");
    assert_eq!(detail["action"], "fiat-refund");
    assert_eq!(detail["refund_payment_hash"], refund.payment_hash);

    // Only stuck payouts can be retried or refunded.
    let again = mergetip_engine::ops::fiat_retry(&w.pool, w.fiat.as_ref(), tip, "ada").await;
    assert!(matches!(
        again,
        Err(mergetip_engine::ops::OpsError::NotStuck(TipState::Refunded))
    ));
    Ok(())
}
