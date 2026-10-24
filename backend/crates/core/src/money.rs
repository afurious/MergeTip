//! Money newtypes. Amounts are integers end to end; floats never touch a payment path.

use std::fmt;

use serde::{Deserialize, Serialize};

/// An amount that does not fit the target representation (e.g. a negative database value).
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("amount out of range")]
pub struct AmountOutOfRange;

/// Whole satoshis: the unit users see and the unit tips are denominated in.
#[derive(
    Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize,
)]
#[serde(transparent)]
pub struct Sats(u64);

impl Sats {
    pub const ZERO: Self = Self(0);

    pub const fn new(sats: u64) -> Self {
        Self(sats)
    }

    pub const fn get(self) -> u64 {
        self.0
    }

    /// Millisatoshis for LNURL and BOLT11; `None` only on overflow.
    pub const fn to_msats(self) -> Option<Msats> {
        match self.0.checked_mul(1_000) {
            Some(msats) => Some(Msats(msats)),
            None => None,
        }
    }

    pub const fn checked_add(self, other: Self) -> Option<Self> {
        match self.0.checked_add(other.0) {
            Some(sum) => Some(Self(sum)),
            None => None,
        }
    }

    pub const fn checked_sub(self, other: Self) -> Option<Self> {
        match self.0.checked_sub(other.0) {
            Some(diff) => Some(Self(diff)),
            None => None,
        }
    }
}

impl fmt::Display for Sats {
    /// Formats with thousands separators and the unit, e.g. `2,100 sats`, as the PRD requires.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let digits = self.0.to_string();
        let mut grouped = String::with_capacity(digits.len() + digits.len() / 3);
        for (i, ch) in digits.chars().enumerate() {
            if i > 0 && (digits.len() - i).is_multiple_of(3) {
                grouped.push(',');
            }
            grouped.push(ch);
        }
        let unit = if self.0 == 1 { "sat" } else { "sats" };
        write!(f, "{grouped} {unit}")
    }
}

/// Postgres stores amounts as `BIGINT`.
impl TryFrom<i64> for Sats {
    type Error = AmountOutOfRange;

    fn try_from(value: i64) -> Result<Self, Self::Error> {
        u64::try_from(value).map(Self).map_err(|_| AmountOutOfRange)
    }
}

impl TryFrom<Sats> for i64 {
    type Error = AmountOutOfRange;

    fn try_from(value: Sats) -> Result<Self, Self::Error> {
        i64::try_from(value.0).map_err(|_| AmountOutOfRange)
    }
}

/// Millisatoshis: the unit of LNURL `amount` parameters and BOLT11 invoices.
#[derive(
    Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize,
)]
#[serde(transparent)]
pub struct Msats(u64);

impl Msats {
    pub const fn new(msats: u64) -> Self {
        Self(msats)
    }

    pub const fn get(self) -> u64 {
        self.0
    }

    /// Whole sats, or `None` when a sub-satoshi remainder would be lost. An invoice with such a
    /// remainder can never equal a tip amount, so callers treat `None` as a mismatch.
    pub const fn to_sats_exact(self) -> Option<Sats> {
        if self.0.is_multiple_of(1_000) {
            Some(Sats(self.0 / 1_000))
        } else {
            None
        }
    }
}

impl TryFrom<i64> for Msats {
    type Error = AmountOutOfRange;

    fn try_from(value: i64) -> Result<Self, Self::Error> {
        u64::try_from(value).map(Self).map_err(|_| AmountOutOfRange)
    }
}

impl TryFrom<Msats> for i64 {
    type Error = AmountOutOfRange;

    fn try_from(value: Msats) -> Result<Self, Self::Error> {
        i64::try_from(value.0).map_err(|_| AmountOutOfRange)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sats_convert_to_msats_and_back() {
        let msats = Sats::new(2_100).to_msats().unwrap();
        assert_eq!(msats, Msats::new(2_100_000));
        assert_eq!(msats.to_sats_exact(), Some(Sats::new(2_100)));
    }

    #[test]
    fn msats_with_remainder_are_not_whole_sats() {
        assert_eq!(Msats::new(2_100_001).to_sats_exact(), None);
    }

    #[test]
    fn overflow_is_reported_not_wrapped() {
        assert_eq!(Sats::new(u64::MAX).to_msats(), None);
        assert_eq!(Sats::new(u64::MAX).checked_add(Sats::new(1)), None);
        assert_eq!(Sats::ZERO.checked_sub(Sats::new(1)), None);
    }

    #[test]
    fn database_values_are_range_checked() {
        assert_eq!(Sats::try_from(-1_i64), Err(AmountOutOfRange));
        assert_eq!(i64::try_from(Sats::new(u64::MAX)), Err(AmountOutOfRange));
        assert_eq!(Sats::try_from(21_i64), Ok(Sats::new(21)));
    }

    #[test]
    fn display_groups_thousands() {
        assert_eq!(Sats::new(1).to_string(), "1 sat");
        assert_eq!(Sats::new(999).to_string(), "999 sats");
        assert_eq!(Sats::new(2_100).to_string(), "2,100 sats");
        assert_eq!(Sats::new(1_234_567).to_string(), "1,234,567 sats");
    }
}
