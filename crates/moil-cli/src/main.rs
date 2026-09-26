//! `moil`: the command line.
//!
//! Bundle authors use `hash`, `check`, `init` and `run`. Machine owners
//! without the tray app (and the end-to-end tests) use `pair`, `review`,
//! `approve` and `agent`.

use std::path::PathBuf;

use clap::{Parser, Subcommand};

mod bundle;
mod machine;

#[derive(Debug, Parser)]
#[command(
    name = "moil",
    version,
    about = "Lend your machine to the services you use, one approved job at a time."
)]
struct Cli {
    /// Where moil keeps its state.
    #[arg(long, global = true, env = "MOIL_HOME")]
    home: Option<PathBuf>,

    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    #[command(flatten)]
    Bundle(bundle::Command),
    #[command(flatten)]
    Machine(machine::Command),
}

/// Options every command can see.
#[derive(Debug, Clone)]
pub struct Globals {
    pub home: Option<PathBuf>,
}

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    let globals = Globals { home: cli.home };
    let runtime = tokio::runtime::Runtime::new()?;
    runtime.block_on(async {
        match cli.command {
            Command::Bundle(command) => bundle::run(command, &globals).await,
            Command::Machine(command) => machine::run(command, &globals).await,
        }
    })
}
