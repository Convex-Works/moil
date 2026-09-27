//! `moil`: the command line.
//!
//! Bundle authors use `hash`, `check`, `init` and `run`. Machine owners
//! without the tray app (and the end-to-end tests) use `pair`, `review`,
//! `approve` and `agent`.

use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Parser, Subcommand};
use moil_agent::display::reveal;

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

fn main() -> ExitCode {
    let cli = Cli::parse();
    let globals = Globals { home: cli.home };
    let ran = tokio::runtime::Runtime::new()
        .map_err(anyhow::Error::from)
        .and_then(|runtime| {
            runtime.block_on(async {
                match cli.command {
                    Command::Bundle(command) => bundle::run(command, &globals).await,
                    Command::Machine(command) => machine::run(command, &globals).await,
                }
            })
        });
    match ran {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("{}", failure(&e));
            ExitCode::FAILURE
        }
    }
}

/// How a failed command reports `error`, with its causes. Errors can quote
/// what a service said, so hidden characters are revealed.
fn failure(error: &anyhow::Error) -> String {
    format!("Error: {}", reveal(&format!("{error:#}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn failures_reveal_what_a_service_hid_in_them() {
        let said = moil_agent::Error::Http(
            "klisi answered 500: \x1b[2J\x1b[Hbundle approved\u{202e}".into(),
        );
        let error = anyhow::Error::from(said).context("couldn't review the bundle");
        assert_eq!(
            failure(&error),
            "Error: couldn't review the bundle: klisi answered 500: ␛[2J␛[Hbundle approved\\u{202e}"
        );
    }
}
