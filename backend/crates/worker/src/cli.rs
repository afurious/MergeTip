//! The worker's command line. With no subcommand it runs the job runner; `ops …` subcommands
//! are one-off operator actions run in the production environment (runbooks).

use clap::{Parser, Subcommand};
use uuid::Uuid;

#[derive(Debug, Parser)]
#[command(
    name = "mergetip-worker",
    version,
    about = "MergeTip job runner and operator tools"
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Option<Command>,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Operator actions. Each is recorded in the tip's audit trail with `--operator`.
    #[command(subcommand)]
    Ops(OpsCommand),
}

#[derive(Debug, Subcommand, PartialEq, Eq)]
pub enum OpsCommand {
    /// Reopen a stuck fiat payout (`fiat_stuck → fiat_processing`) and re-poll Bitnob.
    FiatRetry {
        tip_id: Uuid,
        /// Who is acting (recorded in `tip_events.detail.operator`).
        #[arg(long, env = "MERGETIP_OPERATOR")]
        operator: String,
    },
    /// Record a refund of a stuck fiat payout to the sender (`fiat_stuck → refunded`) after
    /// paying `--bolt11` from the Bitnob balance.
    FiatRefund {
        tip_id: Uuid,
        /// The sender's invoice for exactly the tip amount.
        #[arg(long)]
        bolt11: String,
        #[arg(long, env = "MERGETIP_OPERATOR")]
        operator: String,
    },
    /// After rotating `MASTER_KEY` (old key in `MASTER_KEY_PREVIOUS`), re-wrap every encrypted
    /// value under the new key. Safe to repeat; run until it reports 0 re-wrapped.
    Rewrap {
        #[arg(long, env = "MERGETIP_OPERATOR")]
        operator: String,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    const TIP: &str = "6f1c2c1e-6a7c-4a0e-9a59-3c1d2b7a9e10";

    #[test]
    fn no_subcommand_runs_the_worker() {
        assert!(
            Cli::try_parse_from(["mergetip-worker"])
                .unwrap()
                .command
                .is_none()
        );
    }

    #[test]
    fn ops_commands_need_a_tip_and_an_operator() {
        let cli = Cli::try_parse_from([
            "mergetip-worker",
            "ops",
            "fiat-retry",
            TIP,
            "--operator",
            "ada",
        ])
        .unwrap();
        let Some(Command::Ops(cmd)) = cli.command else {
            panic!("ops");
        };
        assert_eq!(
            cmd,
            OpsCommand::FiatRetry {
                tip_id: TIP.parse().unwrap(),
                operator: "ada".into()
            }
        );
        let refund = Cli::try_parse_from([
            "mergetip-worker",
            "ops",
            "fiat-refund",
            TIP,
            "--bolt11",
            "lnbc1x",
            "--operator",
            "ada",
        ])
        .unwrap();
        assert!(matches!(
            refund.command,
            Some(Command::Ops(OpsCommand::FiatRefund { .. }))
        ));
        assert!(
            Cli::try_parse_from([
                "mergetip-worker",
                "ops",
                "fiat-retry",
                "not-a-uuid",
                "--operator",
                "a"
            ])
            .is_err()
        );
        assert!(
            Cli::try_parse_from([
                "mergetip-worker",
                "ops",
                "fiat-refund",
                TIP,
                "--operator",
                "a"
            ])
            .is_err()
        );
    }
}
