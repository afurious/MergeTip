//! Tip state machine (PRD: "Payment flow and tip state machine", FR-4.1).
//!
//! All state changes go through [`transition`]; no other code updates `tips.state`.

use std::{fmt, str::FromStr};

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TipState {
    Received,
    AwaitingSender,
    Verifying,
    AwaitingConfirmation,
    Held,
    Paying,
    Pledged,
    Paid,
    Rejected,
    Cancelled,
    Expired,
    Failed,
    FiatProcessing,
    FiatStuck,
    Refunded,
}

impl TipState {
    /// Every state, in declaration order. Used by exhaustive tests and the schema `CHECK`.
    pub const ALL: [Self; 15] = [
        Self::Received,
        Self::AwaitingSender,
        Self::Verifying,
        Self::AwaitingConfirmation,
        Self::Held,
        Self::Paying,
        Self::Pledged,
        Self::Paid,
        Self::Rejected,
        Self::Cancelled,
        Self::Expired,
        Self::Failed,
        Self::FiatProcessing,
        Self::FiatStuck,
        Self::Refunded,
    ];

    /// The `snake_case` form stored in `tips.state` and returned by the API.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Received => "received",
            Self::AwaitingSender => "awaiting_sender",
            Self::Verifying => "verifying",
            Self::AwaitingConfirmation => "awaiting_confirmation",
            Self::Held => "held",
            Self::Paying => "paying",
            Self::Pledged => "pledged",
            Self::Paid => "paid",
            Self::Rejected => "rejected",
            Self::Cancelled => "cancelled",
            Self::Expired => "expired",
            Self::Failed => "failed",
            Self::FiatProcessing => "fiat_processing",
            Self::FiatStuck => "fiat_stuck",
            Self::Refunded => "refunded",
        }
    }

    /// A final state has no outgoing transitions; the tip's outcome is settled.
    pub const fn is_final(self) -> bool {
        matches!(
            self,
            Self::Paid
                | Self::Rejected
                | Self::Cancelled
                | Self::Expired
                | Self::Failed
                | Self::Refunded
        )
    }
}

impl fmt::Display for TipState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("unknown tip state {0:?}")]
pub struct UnknownTipState(pub String);

impl FromStr for TipState {
    type Err = UnknownTipState;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::ALL
            .into_iter()
            .find(|state| state.as_str() == s)
            .ok_or_else(|| UnknownTipState(s.to_owned()))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("illegal tip transition {from} -> {to}")]
pub struct IllegalTransition {
    pub from: TipState,
    pub to: TipState,
}

/// Checks a state change against the allowed-transition table.
///
/// Money moves only from `Paying`, and `Paying` is reachable only after verification, so this
/// table is the core of the "never pay unverified" guarantee.
pub fn transition(from: TipState, to: TipState) -> Result<(), IllegalTransition> {
    use TipState::*;
    let ok = matches!(
        (from, to),
        (Received, AwaitingSender | Verifying)
            | (AwaitingSender, Verifying | Cancelled)
            | (Verifying, AwaitingConfirmation | Held | Paying | Rejected)
            | (AwaitingConfirmation, Held | Paying | Cancelled)
            | (Held, Paying | Cancelled)
            | (Paying, Paid | Pledged | Failed | FiatProcessing)
            | (Pledged, Paying | Expired)
            | (FiatProcessing, Paid | FiatStuck)
            | (FiatStuck, FiatProcessing | Refunded)
    );
    if ok {
        Ok(())
    } else {
        Err(IllegalTransition { from, to })
    }
}

#[cfg(test)]
mod tests {
    use super::TipState::*;
    use super::*;

    /// The PRD's transition table, written out independently of [`transition`].
    const ALLOWED: &[(TipState, TipState)] = &[
        (Received, AwaitingSender),
        (Received, Verifying),
        (AwaitingSender, Verifying),
        (AwaitingSender, Cancelled),
        (Verifying, AwaitingConfirmation),
        (Verifying, Held),
        (Verifying, Paying),
        (Verifying, Rejected),
        (AwaitingConfirmation, Held),
        (AwaitingConfirmation, Paying),
        (AwaitingConfirmation, Cancelled),
        (Held, Paying),
        (Held, Cancelled),
        (Paying, Paid),
        (Paying, Pledged),
        (Paying, Failed),
        (Paying, FiatProcessing),
        (Pledged, Paying),
        (Pledged, Expired),
        (FiatProcessing, Paid),
        (FiatProcessing, FiatStuck),
        (FiatStuck, FiatProcessing),
        (FiatStuck, Refunded),
    ];

    #[test]
    fn every_pair_matches_the_allow_list() {
        for from in TipState::ALL {
            for to in TipState::ALL {
                let expected = ALLOWED.contains(&(from, to));
                assert_eq!(
                    transition(from, to).is_ok(),
                    expected,
                    "{from} -> {to} should be {}",
                    if expected { "allowed" } else { "rejected" }
                );
            }
        }
    }

    #[test]
    fn final_states_have_no_exits() {
        for from in TipState::ALL.into_iter().filter(|s| s.is_final()) {
            for to in TipState::ALL {
                assert!(transition(from, to).is_err(), "{from} -> {to}");
            }
        }
    }

    #[test]
    fn non_final_states_have_an_exit() {
        for from in TipState::ALL.into_iter().filter(|s| !s.is_final()) {
            assert!(
                TipState::ALL
                    .into_iter()
                    .any(|to| transition(from, to).is_ok()),
                "{from} is a dead end"
            );
        }
    }

    #[test]
    fn only_verified_states_reach_paying() {
        for from in TipState::ALL {
            if transition(from, Paying).is_ok() {
                assert!(
                    matches!(from, Verifying | AwaitingConfirmation | Held | Pledged),
                    "{from} -> paying skips verification"
                );
            }
        }
    }

    #[test]
    fn state_round_trips_through_its_string_form() {
        for state in TipState::ALL {
            assert_eq!(state.as_str().parse::<TipState>(), Ok(state));
        }
        assert!("settled".parse::<TipState>().is_err());
    }

    /// Serde's `rename_all` and `as_str` must agree, or the API and the database diverge.
    #[test]
    fn serde_name_matches_database_name() {
        use serde::de::IntoDeserializer;
        use serde::de::value::{Error, StrDeserializer};
        for state in TipState::ALL {
            let de: StrDeserializer<'_, Error> = state.as_str().into_deserializer();
            assert_eq!(TipState::deserialize(de), Ok(state));
        }
    }
}
