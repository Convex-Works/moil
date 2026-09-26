//! Commands for bundle authors.

use std::path::PathBuf;

use clap::Subcommand;

use crate::Globals;

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Print a bundle's hash.
    Hash {
        /// The bundle's directory.
        #[arg(default_value = ".")]
        dir: PathBuf,
    },
}

pub async fn run(command: Command, _globals: &Globals) -> anyhow::Result<()> {
    match command {
        Command::Hash { dir } => {
            let bundle = moil_bundle::Bundle::from_dir(dir)?;
            println!("{}", bundle.hash());
            Ok(())
        }
    }
}
