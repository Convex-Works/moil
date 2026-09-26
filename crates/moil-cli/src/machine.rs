//! Commands for machine owners.

use clap::Subcommand;

use crate::Globals;

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Take jobs from every paired service until interrupted.
    Agent,
}

pub async fn run(command: Command, _globals: &Globals) -> anyhow::Result<()> {
    match command {
        Command::Agent => anyhow::bail!("not implemented yet"),
    }
}
