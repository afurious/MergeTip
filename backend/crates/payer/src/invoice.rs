//! BOLT11 checks run before an invoice reaches the sender's wallet (PRD threat: "Malicious
//! Lightning address returns a wrong invoice").

use std::time::Duration;

use lightning_invoice::{Bolt11Invoice, Bolt11InvoiceDescriptionRef};
use mergetip_core::Msats;
use sha2::{Digest, Sha256};
use time::OffsetDateTime;

use crate::{Network, ValidatedInvoice};

/// An invoice must stay payable at least this long: enough for the wallet to route it, with
/// margin for a slow relay.
pub const MIN_TIME_TO_EXPIRY: Duration = Duration::from_secs(120);

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum InvoiceError {
    /// Not a BOLT11 invoice, or its signature doesn't verify.
    #[error("unparseable invoice: {0}")]
    Unparseable(String),
    #[error("invoice is for {actual:?} msat, expected {expected} msat")]
    WrongAmount { expected: u64, actual: Option<u64> },
    #[error("invoice is for the {actual:?} network, expected {expected}")]
    WrongNetwork {
        expected: Network,
        actual: lightning_invoice::Currency,
    },
    #[error("invoice expires too soon")]
    ExpiresTooSoon,
    #[error("invoice description hash does not match the LNURL metadata")]
    DescriptionHashMismatch,
}

/// Accepts `bolt11` only if it pays exactly `expected`, on `network`, stays payable for at least
/// [`MIN_TIME_TO_EXPIRY`] after `now`, and commits to `metadata` (LUD-06: the description hash is
/// SHA-256 of the metadata string). The signature is checked while parsing.
pub fn validate(
    bolt11: &str,
    expected: Msats,
    metadata: &str,
    network: Network,
    now: OffsetDateTime,
) -> Result<ValidatedInvoice, InvoiceError> {
    check(bolt11, expected, Some(metadata), network, now)
}

/// The same checks for an invoice from a payout provider (Bitnob), which commits to no LNURL
/// metadata: exact amount, network, signature and at least [`MIN_TIME_TO_EXPIRY`] left. The
/// amount check is what stops a provider from charging the sender more than the tip.
pub fn validate_provider_invoice(
    bolt11: &str,
    expected: Msats,
    network: Network,
    now: OffsetDateTime,
) -> Result<ValidatedInvoice, InvoiceError> {
    check(bolt11, expected, None, network, now)
}

fn check(
    bolt11: &str,
    expected: Msats,
    metadata: Option<&str>,
    network: Network,
    now: OffsetDateTime,
) -> Result<ValidatedInvoice, InvoiceError> {
    let invoice: Bolt11Invoice = bolt11
        .trim()
        .parse()
        .map_err(|err| InvoiceError::Unparseable(format!("{err}")))?;

    let actual = invoice.amount_milli_satoshis();
    if actual != Some(expected.get()) {
        return Err(InvoiceError::WrongAmount {
            expected: expected.get(),
            actual,
        });
    }
    if invoice.currency() != network.currency() {
        return Err(InvoiceError::WrongNetwork {
            expected: network,
            actual: invoice.currency(),
        });
    }

    let expires_at = expires_at(&invoice).ok_or(InvoiceError::ExpiresTooSoon)?;
    if expires_at - now < MIN_TIME_TO_EXPIRY {
        return Err(InvoiceError::ExpiresTooSoon);
    }

    if let Some(metadata) = metadata {
        let expected_hash = Sha256::digest(metadata.as_bytes());
        match invoice.description() {
            Bolt11InvoiceDescriptionRef::Hash(hash)
                if AsRef::<[u8]>::as_ref(&hash.0) == expected_hash.as_slice() => {}
            _ => return Err(InvoiceError::DescriptionHashMismatch),
        }
    }

    Ok(ValidatedInvoice {
        bolt11: bolt11.trim().to_owned(),
        payment_hash: hex::encode(invoice.payment_hash().as_ref() as &[u8]),
        amount: expected,
        expires_at,
    })
}

fn expires_at(invoice: &Bolt11Invoice) -> Option<OffsetDateTime> {
    let at = invoice
        .duration_since_epoch()
        .checked_add(invoice.expiry_time())?;
    OffsetDateTime::from_unix_timestamp(i64::try_from(at.as_secs()).ok()?).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::{MintParams, mint_invoice};

    const METADATA: &str =
        r#"[["text/plain","Tip for alice"],["text/identifier","alice@example.com"]]"#;

    fn now() -> OffsetDateTime {
        OffsetDateTime::from_unix_timestamp(1_800_000_000).unwrap()
    }

    fn params() -> MintParams {
        MintParams {
            amount: Msats::new(2_100_000),
            metadata: METADATA.to_owned(),
            created_at: now(),
            expiry: Duration::from_secs(600),
            ..MintParams::default()
        }
    }

    #[test]
    fn provider_invoices_skip_only_the_metadata_check() {
        let any_metadata = MintParams {
            metadata: "whatever bitnob commits to".into(),
            ..params()
        };
        let bolt11 = mint_invoice(&any_metadata).bolt11;
        assert!(
            validate_provider_invoice(&bolt11, Msats::new(2_100_000), Network::Regtest, now())
                .is_ok()
        );
        assert!(matches!(
            validate_provider_invoice(&bolt11, Msats::new(2_000_000), Network::Regtest, now()),
            Err(InvoiceError::WrongAmount { .. })
        ));
        assert!(matches!(
            validate_provider_invoice(&bolt11, Msats::new(2_100_000), Network::Bitcoin, now()),
            Err(InvoiceError::WrongNetwork { .. })
        ));
        let soon = mint_invoice(&MintParams {
            expiry: Duration::from_secs(60),
            ..any_metadata
        });
        assert_eq!(
            validate_provider_invoice(&soon.bolt11, Msats::new(2_100_000), Network::Regtest, now()),
            Err(InvoiceError::ExpiresTooSoon)
        );
    }

    fn check(params: &MintParams) -> Result<ValidatedInvoice, InvoiceError> {
        validate(
            &mint_invoice(params).bolt11,
            Msats::new(2_100_000),
            METADATA,
            Network::Regtest,
            now(),
        )
    }

    #[test]
    fn accepts_a_matching_invoice() {
        let minted = mint_invoice(&params());
        let valid = validate(
            &minted.bolt11,
            Msats::new(2_100_000),
            METADATA,
            Network::Regtest,
            now(),
        )
        .unwrap();
        assert_eq!(valid.payment_hash, minted.payment_hash);
        assert_eq!(valid.amount, Msats::new(2_100_000));
        assert_eq!(valid.expires_at, now() + time::Duration::seconds(600));
    }

    #[test]
    fn rejects_the_wrong_amount() {
        let result = check(&MintParams {
            amount: Msats::new(2_100_001),
            ..params()
        });
        assert_eq!(
            result,
            Err(InvoiceError::WrongAmount {
                expected: 2_100_000,
                actual: Some(2_100_001)
            })
        );
    }

    #[test]
    fn rejects_the_wrong_network() {
        let result = check(&MintParams {
            network: Network::Bitcoin,
            ..params()
        });
        assert!(
            matches!(result, Err(InvoiceError::WrongNetwork { .. })),
            "{result:?}"
        );
    }

    #[test]
    fn rejects_invoices_about_to_expire() {
        for expiry in [0, 60, 119] {
            let result = check(&MintParams {
                expiry: Duration::from_secs(expiry),
                ..params()
            });
            assert_eq!(result, Err(InvoiceError::ExpiresTooSoon), "{expiry}s");
        }
        let created_long_ago = check(&MintParams {
            created_at: now() - time::Duration::hours(2),
            expiry: Duration::from_secs(3_600),
            ..params()
        });
        assert_eq!(created_long_ago, Err(InvoiceError::ExpiresTooSoon));
    }

    #[test]
    fn rejects_a_different_description() {
        let result = check(&MintParams {
            metadata: "[]".into(),
            ..params()
        });
        assert_eq!(result, Err(InvoiceError::DescriptionHashMismatch));
    }

    #[test]
    fn rejects_garbage_and_tampered_invoices() {
        assert!(matches!(
            validate(
                "lnbc1nope",
                Msats::new(1),
                METADATA,
                Network::Regtest,
                now()
            ),
            Err(InvoiceError::Unparseable(_))
        ));
        let mut tampered = mint_invoice(&params()).bolt11.into_bytes();
        let i = tampered.len() - 20;
        tampered[i] = if tampered[i] == b'q' { b'p' } else { b'q' };
        let tampered = String::from_utf8(tampered).unwrap();
        assert!(
            validate(
                &tampered,
                Msats::new(2_100_000),
                METADATA,
                Network::Regtest,
                now()
            )
            .is_err()
        );
    }
}
