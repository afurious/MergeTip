//! High-value tip confirmation (FR-12, RFC 0003 "High-value confirmation and OTP").
//!
//! Only the signed-in sender, in the web app, can release a tip from `awaiting_confirmation`.
//! These functions are called by the API inside the transaction that holds the tip's row lock;
//! nothing in the comment handler calls them, so a GitHub comment can never confirm a tip.

use mergetip_core::{TipId, TipKind, TipState};
use mergetip_crypto::{Keyring, constant_time_eq, hmac_sha256, random_bytes};
use mergetip_store::{
    installations,
    tips::{self, Tip, TipPatch},
};
use serde_json::json;
use sqlx::PgConnection;
use time::{Duration, OffsetDateTime};

use crate::{
    EngineError,
    comments::{cancel_tip, enqueue_tip_reply},
    jobs::{self, PayTip, ResolveHeld, TipMessage},
};

/// How long an emailed code works.
pub const OTP_TTL: Duration = Duration::minutes(10);
/// At most one new code per tip this often.
pub const OTP_RESEND_INTERVAL: Duration = Duration::seconds(60);
/// Wrong guesses before the code is thrown away.
pub const OTP_MAX_ATTEMPTS: i32 = 5;
const OTP_PURPOSE: &str = "otp";

/// A uniformly random 6-digit code from the OS CSPRNG. Values at or above the largest multiple
/// of 1,000,000 that fits in a `u32` are redrawn, so no code is likelier than another.
pub fn new_otp() -> Result<String, EngineError> {
    const LIMIT: u32 = u32::MAX - (u32::MAX % 1_000_000);
    loop {
        let bytes = random_bytes::<4>()
            .map_err(|err| EngineError::Malformed(format!("no randomness for an OTP: {err}")))?;
        let value = u32::from_le_bytes(bytes);
        if value < LIMIT {
            return Ok(format!("{:06}", value % 1_000_000));
        }
    }
}

/// What is stored for `code`: an HMAC under the OTP subkey, bound to the tip so a code for one
/// tip can't confirm another.
pub fn otp_mac(keyring: &Keyring, tip_id: TipId, code: &str) -> String {
    let key = keyring.subkey(OTP_PURPOSE);
    hex::encode(hmac_sha256(
        key.expose(),
        format!("{tip_id}:{code}").as_bytes(),
    ))
}

/// Whether `code` matches the stored MAC, under the current master key or the previous one
/// during a rotation. Constant-time.
pub fn otp_matches(keyring: &Keyring, tip_id: TipId, code: &str, stored: &str) -> bool {
    let code = code.trim();
    if code.len() != 6 || !code.bytes().all(|b| b.is_ascii_digit()) {
        return false;
    }
    keyring.verification_subkeys(OTP_PURPOSE).iter().any(|key| {
        let mac = hex::encode(hmac_sha256(
            key.expose(),
            format!("{tip_id}:{code}").as_bytes(),
        ));
        constant_time_eq(&mac, stored)
    })
}

/// Releases a confirmed tip: a `now` tip goes to `paying`, an `on-merge` tip to `held`. A held
/// tip also gets a `resolve_held` job, so a PR that merged before the confirmation pays at once.
/// `tip` must be locked by the caller and in `awaiting_confirmation`.
pub async fn confirm(
    conn: &mut PgConnection,
    tip: &Tip,
    now: OffsetDateTime,
) -> Result<Tip, EngineError> {
    let patch = TipPatch {
        confirmed_at: Some(now),
        ..TipPatch::default()
    };
    let installation = installations::get(&mut *conn, tip.installation_id).await?;
    match tip.kind {
        TipKind::Now => {
            let paying = tips::transition(
                conn,
                tip.id,
                TipState::AwaitingConfirmation,
                TipState::Paying,
                &patch,
                json!({ "reason": "confirmed" }),
            )
            .await?;
            jobs::enqueue(conn, &PayTip { tip_id: tip.id }).await?;
            Ok(paying)
        }
        TipKind::OnMerge => {
            let held = tips::transition(
                conn,
                tip.id,
                TipState::AwaitingConfirmation,
                TipState::Held,
                &patch,
                json!({ "reason": "confirmed" }),
            )
            .await?;
            enqueue_tip_reply(
                conn,
                installation.github_installation_id,
                &held,
                TipMessage::Held,
            )
            .await?;
            jobs::enqueue(
                conn,
                &ResolveHeld {
                    installation: installation.github_installation_id,
                    repo: held.thread.full_name(),
                    number: held.thread.number,
                },
            )
            .await?;
            Ok(held)
        }
    }
}

/// The sender declined their own high-value tip. Returns `false` if it had already moved.
pub async fn cancel_by_sender(conn: &mut PgConnection, tip: &Tip) -> Result<bool, EngineError> {
    cancel_tip(
        conn,
        tip,
        TipState::AwaitingConfirmation,
        "cancelled_by_sender",
    )
    .await
}

#[cfg(test)]
mod tests {
    use super::*;

    fn keyring() -> Keyring {
        Keyring::from_hex(&"07".repeat(32), None).unwrap()
    }

    #[test]
    fn codes_are_six_digits() {
        for _ in 0..200 {
            let code = new_otp().unwrap();
            assert_eq!(code.len(), 6);
            assert!(code.bytes().all(|b| b.is_ascii_digit()));
        }
    }

    #[test]
    fn a_code_only_confirms_its_own_tip() {
        let keyring = keyring();
        let tip = TipId::new_random();
        let mac = otp_mac(&keyring, tip, "123456");
        assert!(otp_matches(&keyring, tip, "123456", &mac));
        assert!(otp_matches(&keyring, tip, " 123456 ", &mac));
        assert!(!otp_matches(&keyring, tip, "123457", &mac));
        assert!(!otp_matches(&keyring, TipId::new_random(), "123456", &mac));
        assert!(!otp_matches(&keyring, tip, "12345", &mac));
        assert!(!otp_matches(&keyring, tip, "abcdef", &mac));
    }

    #[test]
    fn codes_survive_a_master_key_rotation() {
        let old = keyring();
        let tip = TipId::new_random();
        let mac = otp_mac(&old, tip, "000042");
        let rotated = Keyring::from_hex(&"08".repeat(32), Some(&"07".repeat(32))).unwrap();
        assert!(otp_matches(&rotated, tip, "000042", &mac));
    }
}
