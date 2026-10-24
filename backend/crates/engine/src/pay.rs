//! Paying a tip (RFC 0002): invoice from the receiver, reservation under the sender's lock,
//! payment through the sender's own wallet, and reconciliation when the outcome is unknown.
//!
//! Money only moves from `paying`, through exactly one open attempt at a time. A timed-out
//! payment is never retried blindly: its attempt stays `unknown` until the wallet confirms it
//! settled, or its invoice expires so it never can.

use std::time::Duration;

use mergetip_core::{Msats, TipId, TipState};
use mergetip_github::Reaction;
use mergetip_payer::{
    LightningAddress, PaymentProof, ResolveError, SenderWallet, ValidatedInvoice,
    nwc::{Lookup, PayError},
};
use mergetip_store::{
    StoreError, feature_flags, installations,
    payment_attempts::{self, AttemptStatus, NewAttempt, PaymentAttempt},
    tips::{self, Tip, TipPatch},
    users, wallets,
};
use serde_json::json;
use sha2::{Digest, Sha256};
use sqlx::PgConnection;
use time::{OffsetDateTime, Time, UtcOffset};

use crate::{
    Engine, EngineError,
    comments::{enqueue_reaction, enqueue_tip_reply},
    jobs::{self, PayTip, ReconcilePayment, TipMessage},
};

/// How long the sender's wallet gets to pay before the outcome counts as unknown.
pub const PAY_TIMEOUT: Duration = Duration::from_secs(60);
/// Payment attempts per tip before it fails for good.
pub const MAX_PAYMENT_ATTEMPTS: i64 = 3;
/// Wait while payments are switched off globally.
const PAUSED_RETRY: Duration = Duration::from_secs(300);
/// First reconciliation check after a timeout, and the slowest re-check.
const RECONCILE_FIRST: Duration = Duration::from_secs(30);
const RECONCILE_MAX: Duration = Duration::from_secs(600);
/// After an invoice expires, wait this long before declaring its payment failed (clock skew).
const EXPIRY_GRACE: time::Duration = time::Duration::seconds(60);

/// What the worker should do with a reconciliation job.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReconcileOutcome {
    Done,
    CheckAgainIn(Duration),
}

impl Engine {
    /// Runs one `pay_tip` job. `final_attempt` is true on the job's last try: a failure that
    /// would otherwise be retried then fails the tip instead, so it can't be stranded in
    /// `paying` by a dead-lettered job.
    pub async fn pay_tip(&self, tip_id: TipId, final_attempt: bool) -> Result<(), EngineError> {
        if self.settings.stub_payer {
            return self.pay_tip_stub(tip_id).await;
        }
        match self.try_pay(tip_id).await {
            Err(err) if final_attempt && err.is_retryable() => {
                tracing::warn!(%tip_id, error = %err, "giving up on paying the tip");
                let tip = tips::get(&self.pool, tip_id).await?;
                if tip.state == TipState::Paying
                    && payment_attempts::open_for_tip(&self.pool, tip_id)
                        .await?
                        .is_none()
                {
                    self.fail_tip(&tip, "payment_failed", Some(err.to_string()))
                        .await?;
                }
                Ok(())
            }
            other => other,
        }
    }

    async fn try_pay(&self, tip_id: TipId) -> Result<(), EngineError> {
        if !feature_flags::is_enabled(&self.pool, feature_flags::PAYMENTS_ENABLED).await? {
            return Err(EngineError::Retry {
                reason: "payments are paused".into(),
                after: PAUSED_RETRY,
            });
        }
        let tip = tips::get(&self.pool, tip_id).await?;
        if tip.state != TipState::Paying {
            return Ok(());
        }
        let installation = installations::get(&self.pool, tip.installation_id).await?;
        if installation.suspended_at.is_some() {
            return self.stop_for_suspension(&tip, TipState::Failed).await;
        }
        // A crash after reserving left an attempt open: reconcile it, never pay a second time.
        if let Some(open) = payment_attempts::open_for_tip(&self.pool, tip_id).await? {
            let mut conn = self.pool.acquire().await?;
            enqueue_reconcile(&mut conn, &tip, &open, Duration::ZERO).await?;
            return Ok(());
        }

        // 1. Who gets paid, and from which wallet.
        let receiver = match tip.receiver_id {
            Some(id) => users::get(&self.pool, id).await?,
            None => {
                return Err(EngineError::Malformed(
                    "verified tip without a receiver".into(),
                ));
            }
        };
        let Some(address) = receiver.lightning_address.clone() else {
            return self.pledge(&tip).await;
        };
        let Some((connection_id, uri_ciphertext)) =
            wallets::active_secret(&self.pool, tip.sender_id).await?
        else {
            return self.fail_tip(&tip, "no_wallet", None).await;
        };

        // 2. An invoice for exactly this tip, validated before anything is reserved.
        let invoice = match self.invoice(&tip, &address).await {
            Ok(invoice) => invoice,
            Err(err) if err.is_retryable() => {
                return Err(EngineError::Retry {
                    reason: err.to_string(),
                    after: RECONCILE_FIRST,
                });
            }
            Err(err) => {
                return self
                    .fail_tip(&tip, "receiver_unpayable", Some(err.to_string()))
                    .await;
            }
        };

        // 3. Reserve: one transaction, sender row locked so caps can't be raced.
        let attempt = match self.reserve(&tip, &invoice).await? {
            Reservation::Reserved(attempt) => attempt,
            Reservation::OverCap(code) => return self.fail_tip(&tip, code, None).await,
            Reservation::Moved => return Ok(()),
        };

        // 4. Pay. Only here is the NWC URI decrypted, and it never leaves this function.
        let uri = self
            .payments
            .keyring
            .open(
                &uri_ciphertext,
                format!("wallet_connections.nwc_uri:{connection_id}").as_bytes(),
            )
            .map_err(|err| EngineError::Malformed(format!("cannot decrypt NWC URI: {err}")))?;
        let uri = String::from_utf8(uri.expose().clone())
            .map_err(|_| EngineError::Malformed("NWC URI is not UTF-8".into()))?;
        let wallet = match self.payments.wallets.connect(&uri).await {
            Ok(wallet) => wallet,
            Err(err) => {
                // Nothing was sent: the attempt is cleanly failed.
                self.finish_attempt(&attempt, AttemptStatus::Failed, Some("CONNECT"))
                    .await?;
                return self
                    .after_failed_attempt(&tip, &err, "wallet_invalid")
                    .await;
            }
        };
        let result = wallet
            .pay_invoice(
                &invoice.bolt11,
                &invoice.payment_hash,
                self.settings.pay_timeout,
            )
            .await;
        wallet.close().await;
        self.record_payment(&tip, &attempt, result).await
    }

    async fn invoice(&self, tip: &Tip, address: &str) -> Result<ValidatedInvoice, ResolveError> {
        let address: LightningAddress = address.parse()?;
        self.payments
            .receivers
            .invoice_for(&address, tip.amount, tip.note.as_deref())
            .await
    }

    async fn reserve(
        &self,
        tip: &Tip,
        invoice: &ValidatedInvoice,
    ) -> Result<Reservation, EngineError> {
        let mut tx = self.pool.begin().await?;
        let locked = tips::lock_for_update(&mut tx, tip.id).await?;
        if locked.state != TipState::Paying {
            return Ok(Reservation::Moved);
        }
        users::lock(&mut tx, tip.sender_id).await?;
        let sender = users::get(&mut *tx, tip.sender_id).await?;
        let amount = tip.amount.to_msats().unwrap_or(Msats::new(u64::MAX));
        if tip.amount > sender.per_tip_max {
            return Ok(Reservation::OverCap("over_per_tip_cap"));
        }
        let midnight = self
            .clock
            .now()
            .to_offset(UtcOffset::UTC)
            .replace_time(Time::MIDNIGHT);
        let reserved = payment_attempts::reserved_since(&mut *tx, tip.sender_id, midnight).await?;
        let daily = sender.daily_max.to_msats().unwrap_or(Msats::new(u64::MAX));
        if reserved.get().saturating_add(amount.get()) > daily.get() {
            return Ok(Reservation::OverCap("daily_cap_reached"));
        }
        let attempt = match payment_attempts::insert_pending(
            &mut *tx,
            &NewAttempt {
                tip_id: tip.id,
                payment_hash: &invoice.payment_hash,
                bolt11: &invoice.bolt11,
                amount: invoice.amount,
                invoice_expires_at: invoice.expires_at,
            },
        )
        .await
        {
            Ok(attempt) => attempt,
            // Another worker opened an attempt first; it owns this payment.
            Err(StoreError::Conflict) => return Ok(Reservation::Moved),
            Err(other) => return Err(other.into()),
        };
        tips::set_payment_hash(&mut *tx, tip.id, &invoice.payment_hash).await?;
        tx.commit().await?;
        Ok(Reservation::Reserved(attempt))
    }

    async fn record_payment(
        &self,
        tip: &Tip,
        attempt: &PaymentAttempt,
        result: Result<PaymentProof, PayError>,
    ) -> Result<(), EngineError> {
        match result {
            Ok(proof) => self.mark_paid(tip, attempt, &proof.preimage).await,
            Err(PayError::Wallet { code, message }) => {
                tracing::info!(tip_id = %tip.id, %code, message, "wallet refused the payment");
                self.finish_attempt(attempt, AttemptStatus::Failed, Some(code.as_str()))
                    .await?;
                let err = PayError::Wallet { code, message };
                self.after_failed_attempt(tip, &err, &code.as_str().to_ascii_lowercase())
                    .await
            }
            // Sent, but no usable answer: the payment may have happened. Reconcile.
            Err(err @ (PayError::Timeout | PayError::Relay(_) | PayError::Protocol(_))) => {
                tracing::warn!(tip_id = %tip.id, error = %err, "payment outcome unknown");
                let mut tx = self.pool.begin().await?;
                let attempt = payment_attempts::set_status(
                    &mut *tx,
                    attempt.id,
                    AttemptStatus::Unknown,
                    None,
                    self.clock.now(),
                )
                .await?;
                enqueue_reconcile(&mut tx, tip, &attempt, RECONCILE_FIRST).await?;
                tx.commit().await?;
                Ok(())
            }
            Err(PayError::InvalidUri) => {
                self.finish_attempt(attempt, AttemptStatus::Failed, Some("INVALID_URI"))
                    .await?;
                self.fail_tip(tip, "wallet_invalid", None).await
            }
        }
    }

    /// A failed attempt: terminal wallet errors fail the tip; others retry with a fresh invoice
    /// until the attempt budget is spent.
    async fn after_failed_attempt(
        &self,
        tip: &Tip,
        err: &PayError,
        terminal_code: &str,
    ) -> Result<(), EngineError> {
        let terminal = match err {
            PayError::Wallet { code, .. } => code.is_terminal(),
            PayError::InvalidUri => true,
            _ => false,
        };
        if terminal {
            return self
                .fail_tip(tip, terminal_code, Some(err.to_string()))
                .await;
        }
        if payment_attempts::count_for_tip(&self.pool, tip.id).await? >= MAX_PAYMENT_ATTEMPTS {
            return self
                .fail_tip(tip, "payment_failed", Some(err.to_string()))
                .await;
        }
        Err(EngineError::Retry {
            reason: err.to_string(),
            after: RECONCILE_FIRST,
        })
    }

    async fn mark_paid(
        &self,
        tip: &Tip,
        attempt: &PaymentAttempt,
        preimage: &str,
    ) -> Result<(), EngineError> {
        let preimage = preimage.trim().to_ascii_lowercase();
        let valid = hex::decode(&preimage)
            .map(|bytes| hex::encode(Sha256::digest(bytes)) == attempt.payment_hash)
            .unwrap_or(false);
        if !valid {
            return Err(EngineError::Malformed(
                "preimage does not match the payment hash".into(),
            ));
        }
        let mut tx = self.pool.begin().await?;
        payment_attempts::set_status(
            &mut *tx,
            attempt.id,
            AttemptStatus::Succeeded,
            None,
            self.clock.now(),
        )
        .await?;
        let paid = tips::transition(
            &mut tx,
            tip.id,
            TipState::Paying,
            TipState::Paid,
            &TipPatch {
                payment_hash: Some(attempt.payment_hash.clone()),
                preimage: Some(preimage),
                ..TipPatch::default()
            },
            json!({ "attempt_id": attempt.id }),
        )
        .await?;
        let installation = installations::get(&mut *tx, tip.installation_id).await?;
        enqueue_tip_reply(
            &mut tx,
            installation.github_installation_id,
            &paid,
            TipMessage::Paid,
        )
        .await?;
        enqueue_reaction(
            &mut tx,
            installation.github_installation_id,
            &paid.thread,
            paid.comment_id,
            Reaction::Rocket,
        )
        .await?;
        tx.commit().await?;
        tracing::info!(tip_id = %tip.id, sats = tip.amount.get(), "tip paid");
        Ok(())
    }

    async fn finish_attempt(
        &self,
        attempt: &PaymentAttempt,
        status: AttemptStatus,
        code: Option<&str>,
    ) -> Result<(), EngineError> {
        payment_attempts::set_status(&self.pool, attempt.id, status, code, self.clock.now())
            .await?;
        Ok(())
    }

    /// `paying → failed` with the reason, a reply and a 😕.
    async fn fail_tip(
        &self,
        tip: &Tip,
        code: &str,
        detail: Option<String>,
    ) -> Result<(), EngineError> {
        let mut tx = self.pool.begin().await?;
        let failed = tips::transition(
            &mut tx,
            tip.id,
            TipState::Paying,
            TipState::Failed,
            &TipPatch::error(code, detail),
            json!({ "reason": code }),
        )
        .await?;
        let installation = installations::get(&mut *tx, tip.installation_id).await?;
        enqueue_tip_reply(
            &mut tx,
            installation.github_installation_id,
            &failed,
            TipMessage::Failed,
        )
        .await?;
        enqueue_reaction(
            &mut tx,
            installation.github_installation_id,
            &failed.thread,
            failed.comment_id,
            Reaction::Confused,
        )
        .await?;
        tx.commit().await?;
        tracing::info!(tip_id = %tip.id, code, "tip payment failed");
        Ok(())
    }

    /// The receiver has no Lightning address: nothing is charged, the tip waits for them.
    async fn pledge(&self, tip: &Tip) -> Result<(), EngineError> {
        let mut tx = self.pool.begin().await?;
        let pledged = tips::transition(
            &mut tx,
            tip.id,
            TipState::Paying,
            TipState::Pledged,
            &TipPatch::default(),
            json!({ "reason": "no_lightning_address" }),
        )
        .await?;
        let installation = installations::get(&mut *tx, tip.installation_id).await?;
        enqueue_tip_reply(
            &mut tx,
            installation.github_installation_id,
            &pledged,
            TipMessage::Pledged,
        )
        .await?;
        tx.commit().await?;
        Ok(())
    }

    /// Decides an attempt whose outcome was unknown, from the wallet's own records.
    pub async fn reconcile_payment(
        &self,
        tip_id: TipId,
        attempt_id: uuid::Uuid,
    ) -> Result<ReconcileOutcome, EngineError> {
        let attempt = payment_attempts::get(&self.pool, attempt_id).await?;
        if !matches!(
            attempt.status,
            AttemptStatus::Pending | AttemptStatus::Unknown
        ) {
            return Ok(ReconcileOutcome::Done);
        }
        let tip = tips::get(&self.pool, tip_id).await?;
        let now = self.clock.now();
        let expired = now > attempt.invoice_expires_at + EXPIRY_GRACE;

        let lookup = match self.lookup(&tip, &attempt).await {
            Ok(lookup) => Some(lookup),
            Err(err) => {
                tracing::info!(%tip_id, error = %err, "lookup_invoice unavailable");
                None
            }
        };
        match lookup {
            Some(Lookup::Settled { preimage }) => {
                self.mark_paid(&tip, &attempt, &preimage).await?;
                Ok(ReconcileOutcome::Done)
            }
            Some(Lookup::Failed) => {
                self.finish_attempt(&attempt, AttemptStatus::Failed, Some("PAYMENT_FAILED"))
                    .await?;
                self.retry_or_fail(&tip).await?;
                Ok(ReconcileOutcome::Done)
            }
            // An expired invoice can no longer be paid, so an unsettled attempt has failed.
            _ if expired => {
                self.finish_attempt(&attempt, AttemptStatus::Failed, Some("EXPIRED"))
                    .await?;
                self.retry_or_fail(&tip).await?;
                Ok(ReconcileOutcome::Done)
            }
            _ => Ok(ReconcileOutcome::CheckAgainIn(next_check(now, &attempt))),
        }
    }

    async fn lookup(&self, tip: &Tip, attempt: &PaymentAttempt) -> Result<Lookup, EngineError> {
        let Some((connection_id, ciphertext)) =
            wallets::active_secret(&self.pool, tip.sender_id).await?
        else {
            return Err(EngineError::Malformed("no wallet to ask".into()));
        };
        let connection = wallets::active(&self.pool, tip.sender_id).await?;
        if !connection.is_some_and(|c| c.methods.iter().any(|m| m == "lookup_invoice")) {
            return Err(EngineError::Malformed(
                "wallet does not support lookup_invoice".into(),
            ));
        }
        let uri = self
            .payments
            .keyring
            .open(
                &ciphertext,
                format!("wallet_connections.nwc_uri:{connection_id}").as_bytes(),
            )
            .map_err(|err| EngineError::Malformed(format!("cannot decrypt NWC URI: {err}")))?;
        let uri = String::from_utf8(uri.expose().clone())
            .map_err(|_| EngineError::Malformed("NWC URI is not UTF-8".into()))?;
        let wallet: Box<dyn SenderWallet> = self
            .payments
            .wallets
            .connect(&uri)
            .await
            .map_err(|err| EngineError::Malformed(err.to_string()))?;
        let result = wallet.lookup_invoice(&attempt.payment_hash).await;
        wallet.close().await;
        result.map_err(|err| EngineError::Malformed(err.to_string()))
    }

    /// After a definitely-failed attempt: a fresh attempt if the budget allows, else fail.
    async fn retry_or_fail(&self, tip: &Tip) -> Result<(), EngineError> {
        let tip = tips::get(&self.pool, tip.id).await?;
        if tip.state != TipState::Paying {
            return Ok(());
        }
        if payment_attempts::count_for_tip(&self.pool, tip.id).await? >= MAX_PAYMENT_ATTEMPTS {
            return self.fail_tip(&tip, "payment_failed", None).await;
        }
        let mut conn = self.pool.acquire().await?;
        jobs::enqueue(&mut conn, &PayTip { tip_id: tip.id }).await?;
        Ok(())
    }
}

// The staging stub (`STUB_PAYER=true`): marks the tip paid with a random but internally
// consistent proof, so the whole flow up to the receipt runs without a wallet.
impl Engine {
    /// Settles a tip in `paying` with a fake payment. Refuses unless `stub_payer` is on.
    pub(crate) async fn pay_tip_stub(
        &self,
        tip_id: mergetip_core::TipId,
    ) -> Result<(), EngineError> {
        if !self.settings.stub_payer {
            return Err(EngineError::Malformed(
                "the stub payer is disabled; real payments are not available yet".into(),
            ));
        }
        let tip = tips::get(&self.pool, tip_id).await?;
        if tip.state != TipState::Paying {
            return Ok(());
        }
        let installation = installations::get(&self.pool, tip.installation_id).await?;
        if installation.suspended_at.is_some() {
            return self.stop_for_suspension(&tip, TipState::Failed).await;
        }

        let preimage = mergetip_crypto::random_bytes::<32>()
            .map_err(|err| EngineError::Malformed(err.to_string()))?;
        let payment_hash = hex::encode(Sha256::digest(preimage));

        let mut tx = self.pool.begin().await?;
        let paid = tips::transition(
            &mut tx,
            tip.id,
            TipState::Paying,
            TipState::Paid,
            &TipPatch {
                payment_hash: Some(payment_hash),
                preimage: Some(hex::encode(preimage)),
                ..TipPatch::default()
            },
            json!({ "payer": "stub" }),
        )
        .await?;
        let github_installation = installation.github_installation_id;
        enqueue_tip_reply(&mut tx, github_installation, &paid, TipMessage::Paid).await?;
        enqueue_reaction(
            &mut tx,
            github_installation,
            &paid.thread,
            paid.comment_id,
            Reaction::Rocket,
        )
        .await?;
        tx.commit().await?;
        tracing::info!(tip_id = %tip.id, "tip paid by the stub payer");
        Ok(())
    }
}

enum Reservation {
    Reserved(PaymentAttempt),
    OverCap(&'static str),
    /// The tip left `paying`, or another worker holds the open attempt.
    Moved,
}

async fn enqueue_reconcile(
    conn: &mut PgConnection,
    tip: &Tip,
    attempt: &PaymentAttempt,
    delay: Duration,
) -> Result<(), EngineError> {
    let payload = ReconcilePayment {
        tip_id: tip.id,
        attempt_id: attempt.id,
    };
    let job = jobs::JobPayload::job(&payload)
        .run_at(OffsetDateTime::now_utc() + delay)
        .dedupe_key(format!("reconcile:{}", attempt.id));
    mergetip_store::jobs::enqueue(conn, &job).await?;
    Ok(())
}

/// Checks back sooner while the payment is fresh, then less often, and never long after the
/// invoice expires (the point at which the outcome becomes certain).
fn next_check(now: OffsetDateTime, attempt: &PaymentAttempt) -> Duration {
    let age = Duration::try_from(now - attempt.started_at).unwrap_or(Duration::ZERO);
    let backoff = (age / 2).clamp(RECONCILE_FIRST, RECONCILE_MAX);
    let until_certain = attempt.invoice_expires_at + EXPIRY_GRACE - now;
    let until_certain = Duration::try_from(until_certain).unwrap_or(Duration::ZERO);
    backoff.min(until_certain.max(Duration::from_secs(1)))
}

#[cfg(test)]
mod tests {
    use time::macros::datetime;

    use super::*;

    fn attempt(started: OffsetDateTime, expires: OffsetDateTime) -> PaymentAttempt {
        PaymentAttempt {
            id: uuid::Uuid::nil(),
            tip_id: TipId::new_random(),
            payment_hash: String::new(),
            bolt11: String::new(),
            amount: Msats::new(1),
            invoice_expires_at: expires,
            status: AttemptStatus::Unknown,
            nwc_error_code: None,
            started_at: started,
            finished_at: None,
        }
    }

    #[test]
    fn reconciliation_backs_off_but_not_past_expiry() {
        let start = datetime!(2026-11-02 12:00 UTC);
        let a = attempt(start, start + time::Duration::hours(1));
        assert_eq!(
            next_check(start + time::Duration::seconds(10), &a),
            RECONCILE_FIRST
        );
        assert_eq!(
            next_check(start + time::Duration::minutes(10), &a),
            Duration::from_secs(300)
        );
        assert_eq!(
            next_check(start + time::Duration::minutes(50), &a),
            RECONCILE_MAX
        );
        // Close to expiry: check right after it (plus grace), not 10 minutes later.
        let near = start + time::Duration::minutes(59);
        assert_eq!(next_check(near, &a), Duration::from_secs(120));
    }
}
