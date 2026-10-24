//! Tip attributes that are not part of the state machine.

use std::{fmt, str::FromStr};

use serde::{Deserialize, Serialize};

/// When a tip pays: immediately after verification, or when its PR is merged.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TipKind {
    Now,
    OnMerge,
}

impl TipKind {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Now => "now",
            Self::OnMerge => "on_merge",
        }
    }
}

impl fmt::Display for TipKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("unknown tip kind {0:?}")]
pub struct UnknownTipKind(pub String);

impl FromStr for TipKind {
    type Err = UnknownTipKind;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "now" => Ok(Self::Now),
            "on_merge" => Ok(Self::OnMerge),
            other => Err(UnknownTipKind(other.to_owned())),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kind_round_trips_through_its_string_form() {
        for kind in [TipKind::Now, TipKind::OnMerge] {
            assert_eq!(kind.as_str().parse::<TipKind>(), Ok(kind));
        }
        assert!("later".parse::<TipKind>().is_err());
    }
}
