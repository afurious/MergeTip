//! The worker's command line. With no subcommand it runs the job runner; `ops …` subcommands
//! are one-off operator actions run in the production environment (runbooks).

use clap::{Parser, Subcommand};

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
    /// Operator actions. Each logs who acted (`--operator`).
    #[command(subcommand)]
    Ops(OpsCommand),
}

#[derive(Debug, Subcommand, PartialEq, Eq)]
pub enum OpsCommand {
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
    fn ops_commands_need_an_operator() {
        let cli =
            Cli::try_parse_from(["mergetip-worker", "ops", "rewrap", "--operator", "ada"]).unwrap();
        let Some(Command::Ops(cmd)) = cli.command else {
            panic!("ops");
        };
        assert_eq!(
            cmd,
            OpsCommand::Rewrap {
                operator: "ada".into()
            }
        );
        assert!(Cli::try_parse_from(["mergetip-worker", "ops", "rewrap"]).is_err());
    }
}
