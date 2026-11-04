//! `moil`: the command line (spec/machine.md §3).
//!
//! Machine owners, and the scripts and agents they let drive moil, use the
//! owner's commands: each reaches the agent running on the home, in the
//! app or as `moil agent`, or works on the home itself when none runs.
//! Bundle authors use `init`, `check`, `hash` and `run`.

use std::path::PathBuf;
use std::process::ExitCode;

use clap::{CommandFactory, FromArgMatches, Parser, Subcommand};
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
    /// Where moil keeps its state [default: ~/.moil]
    #[arg(long, global = true, env = "MOIL_HOME", value_name = "DIR")]
    home: Option<PathBuf>,

    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    #[command(flatten)]
    Machine(machine::Command),
    #[command(flatten)]
    Bundle(bundle::Command),
}

/// Options every command can see.
#[derive(Debug, Clone)]
pub struct Globals {
    pub home: Option<PathBuf>,
}

/// The command failed, and said why on stderr (spec/machine.md §3.1).
/// The command line being wrong exits with 2, as clap exits for it.
const FAILED: u8 = 1;
/// The command needs a running agent, and none runs.
const NO_AGENT: u8 = 3;

fn main() -> ExitCode {
    let matches = grouped_help(Cli::command()).get_matches();
    let cli = Cli::from_arg_matches(&matches).unwrap_or_else(|e| e.exit());
    let globals = Globals { home: cli.home };
    let ran = tokio::runtime::Runtime::new()
        .map_err(anyhow::Error::from)
        .and_then(|runtime| {
            runtime.block_on(async {
                match cli.command {
                    Command::Machine(command) => machine::run(command, &globals).await,
                    Command::Bundle(command) => bundle::run(command, &globals).await,
                }
            })
        });
    match ran {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("{}", failure(&e));
            let status = if e.is::<machine::NeedsAgent>() {
                NO_AGENT
            } else {
                FAILED
            };
            ExitCode::from(status)
        }
    }
}

/// How a failed command reports `error`, with its causes. Errors can quote
/// what a service said, so hidden characters are revealed.
fn failure(error: &anyhow::Error) -> String {
    format!("Error: {}", reveal(&format!("{error:#}")))
}

/// `command` with its help listing the owner's commands, then the bundle
/// authors', each group under a heading of its own.
fn grouped_help(command: clap::Command) -> clap::Command {
    let names = |group: clap::Command| -> Vec<String> {
        let names = group.get_subcommands().map(|c| c.get_name().to_owned());
        names.collect()
    };
    let groups = [
        (
            "Your machine",
            names(machine::Command::augment_subcommands(clap::Command::new(
                "moil",
            ))),
        ),
        (
            "Bundle authoring",
            names(bundle::Command::augment_subcommands(clap::Command::new(
                "moil",
            ))),
        ),
    ];
    let styles = command.get_styles();
    let (header, literal) = (styles.get_header(), styles.get_literal());
    let width = groups
        .iter()
        .flat_map(|(_, names)| names.iter().map(String::len))
        .max()
        .unwrap_or(0);
    let mut listing = String::new();
    for (heading, names) in &groups {
        listing.push_str(&format!("{header}{heading}:{header:#}\n"));
        for name in names {
            let about = command
                .find_subcommand(name)
                .and_then(|c| c.get_about())
                .map(|about| about.to_string())
                .unwrap_or_default();
            let padding = " ".repeat(width - name.len());
            listing.push_str(&format!("  {literal}{name}{literal:#}{padding}  {about}\n"));
        }
        listing.push('\n');
    }
    // The template language reads what is in braces as a tag.
    let listing = listing.replace(['{', '}'], "");
    let template = format!(
        "{{about-with-newline}}\n{{usage-heading}} {{usage}}\n\n{listing}\
         {header}Options:{header:#}\n{{options}}\n\n\
         `moil <command> --help` explains a command, with examples, and \
         `moil hooks --help` how to write a hook."
    );
    command.help_template(template)
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use super::*;

    #[test]
    fn failures_reveal_what_a_service_hid_in_them() {
        let said = moil_agent::Error::Http(
            "tide answered 500: \x1b[2J\x1b[Hbundle approved\u{202e}".into(),
        );
        let error = anyhow::Error::from(said).context("couldn't review the bundle");
        assert_eq!(
            failure(&error),
            "Error: couldn't review the bundle: tide answered 500: ␛[2J␛[Hbundle approved\\u{202e}"
        );
    }

    #[test]
    fn the_command_line_is_well_formed() {
        grouped_help(Cli::command()).debug_assert();
    }

    /// What `moil` with `args` prints for help.
    fn help(args: &[&str]) -> String {
        let shown = grouped_help(Cli::command()).try_get_matches_from(args);
        let help = shown.expect_err("help, rather than a command to run");
        assert_eq!(help.kind(), clap::error::ErrorKind::DisplayHelp, "{help}");
        help.render().to_string()
    }

    #[test]
    fn every_command_explains_itself_with_an_example() {
        for command in Cli::command().get_subcommands() {
            let name = command.get_name();
            let help = help(&["moil", name, "--help"]);
            assert!(help.contains("\nExample"), "{name}:\n{help}");
        }
    }

    /// Whoever reads only `moil hooks --help` can write a hook that works:
    /// its example context is one moil writes, and it explains every field.
    #[test]
    fn the_hooks_help_gives_the_whole_contract() {
        let help = help(&["moil", "hooks", "--help"]);
        let start = help.find("\n  {\n").expect("an example context");
        let end = start + help[start..].find("\n  }\n").expect("its end") + 4;
        let example: serde_json::Value = serde_json::from_str(&help[start..end]).unwrap();
        let context: moil_agent::hooks::HookContext =
            serde_json::from_value(example.clone()).unwrap();
        assert_eq!(serde_json::to_value(&context).unwrap(), example);

        // The table after it names each field, then says what it is.
        let explained: BTreeSet<&str> = help[end..]
            .lines()
            .take_while(|line| !line.starts_with("The hook runs"))
            .filter_map(|line| line.strip_prefix("  "))
            .filter(|line| line.starts_with(|c: char| c.is_ascii_lowercase()))
            .flat_map(|line| {
                let names = line.split(|c: char| c.is_ascii_uppercase()).next();
                names.unwrap_or_default().split(',').map(str::trim)
            })
            .collect();
        let fields = example.as_object().unwrap().keys().map(String::as_str);
        assert_eq!(explained, BTreeSet::from_iter(fields), "{help}");
    }

    #[test]
    fn the_help_lists_every_command_in_its_group() {
        let help = grouped_help(Cli::command()).render_help().to_string();
        let (owners, authors) = help
            .split_once("Bundle authoring:")
            .expect("a heading for bundle authoring");
        let owners = owners.split_once("Your machine:").expect("a heading").1;
        for command in Cli::command().get_subcommands() {
            let name = command.get_name();
            let line = format!("\n  {name} ");
            let group = match ["init", "check", "hash", "run"].contains(&name) {
                true => authors,
                false => owners,
            };
            assert!(group.contains(&line), "{name} in\n{help}");
        }
    }
}
