//! The owner's commands (spec/machine.md §3.2).
//!
//! Each works the same whether or not an agent runs on the home: when one
//! does, in the app or as `moil agent`, the command asks it through the
//! control socket, and what it changes takes effect at once; otherwise the
//! command works on the home itself. Only `stop`, `quit`, `login-item` and
//! `status --watch` need a running agent.
//!
//! Results go to stdout, as `Label: value` lines or columns, or as JSON
//! with `--json`; hints, prompts and progress go to stderr (§3.1).

use std::fmt;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use clap::{Subcommand, ValueEnum};
use moil_agent::control::{Client, ClientError};
use moil_agent::report::{Host, Status};
use moil_agent::{Home, Machine, MachineConfig, ReadOnlyMachine, default_secrets, status};
use moil_proto::BundleHash;

use crate::Globals;
use output::Labels;

mod agent;
mod help;
mod logs;
mod output;
mod pair;
mod review;
mod show;

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Show what moil is doing, and anything wrong with its home
    ///
    /// Shows whether an agent runs on the home, and in what: the moil app,
    /// or `moil agent`. Then whether the machine is paused; the running job
    /// and its progress, and the last job since the agent started; each
    /// service the home knows, with its connection, the bundles approved
    /// for it and those waiting for your review, and its hooks; and
    /// anything moil found wrong with the home, with what to do about it.
    ///
    /// Without an agent, it shows what the home's files say: no job, and no
    /// connections.
    #[command(after_help = help::STATUS)]
    Status {
        /// Print the status again whenever it changes, until interrupted.
        /// Needs a running agent.
        #[arg(long)]
        watch: bool,
        #[command(flatten)]
        format: Format,
    },
    /// Stop taking new jobs. A running job carries on
    ///
    /// Prints `Paused: true`. moil keeps it in settings.json, so the
    /// machine stays paused when moil starts again, until `moil resume`.
    #[command(after_help = help::PAUSE)]
    Pause {
        #[command(flatten)]
        format: Format,
    },
    /// Take new jobs again
    ///
    /// Prints `Paused: false`.
    #[command(after_help = help::RESUME)]
    Resume {
        #[command(flatten)]
        format: Format,
    },
    /// Stop the running job. Its service may give it to another machine
    ///
    /// Prints `Stopping:` with the job's ID and attempt, or nothing if no
    /// job runs. Needs a running agent: without one, exits with status 3.
    #[command(after_help = help::STOP)]
    Stop {
        #[command(flatten)]
        format: Format,
    },
    /// Pair this machine with a service, or pair again
    ///
    /// SERVICE is the service's moil URL, or the ID, host or name of a
    /// service the home knows: one this machine paired with before, or one
    /// whose folder came into services/ from elsewhere, such as a git
    /// clone. Pairing with a service the home knows keeps its approvals
    /// and hooks.
    ///
    /// Prints the code and the confirmation page's URL (`Code:` and
    /// `URL:`), opens the page, and waits until you confirm the same code
    /// there. Then prints `Paired:` with the service's ID. With --json,
    /// prints a line of JSON for each.
    #[command(after_help = help::PAIR)]
    Pair {
        /// The service's moil URL, or the ID, host or name of a service
        /// the home knows.
        service: String,
        /// What services should call this machine, from now on and for
        /// every service. Defaults to the computer's name.
        #[arg(long)]
        name: Option<String>,
        /// Don't open the confirmation page in a browser.
        #[arg(long)]
        no_browser: bool,
        #[command(flatten)]
        format: Format,
    },
    /// Tell a service to forget this machine, and delete the pairing here
    ///
    /// The service's folder stays, with its approvals and hooks, so that
    /// pairing again brings them back. Delete the folder to forget those
    /// too. Prints `Unpaired:` with the service's ID.
    #[command(after_help = help::UNPAIR)]
    Unpair {
        /// The service's ID, host, URL or name.
        service: String,
        #[command(flatten)]
        format: Format,
    },
    /// List the services the home knows, paired or not
    ///
    /// Shows each service's ID, base URL, whether this machine is paired
    /// with it, how many approvals count, and its name. With --json, each
    /// service in full, as `moil status --json` lists them.
    #[command(after_help = help::SERVICES)]
    Services {
        #[command(flatten)]
        format: Format,
    },
    /// List a service's bundles, or show one in full
    ///
    /// Without HASH, lists the bundles the service offers and whether you
    /// approved each, then the approved bundles it no longer offers.
    ///
    /// With HASH, shows that bundle as you should read it before approving
    /// it: the service; the three files in full; the packages it installs;
    /// its hash and how to check it; and what changed since the version of
    /// it you approved before. Everything the service controls is shown
    /// with hidden characters revealed.
    #[command(after_help = help::REVIEW)]
    Review {
        /// The service's ID, host, URL or name.
        service: String,
        /// The bundle to show, by its full hash.
        hash: Option<BundleHash>,
        #[command(flatten)]
        format: Format,
    },
    /// Show a bundle in full, then approve it for a service
    ///
    /// Shows the bundle as `moil review SERVICE HASH` does, then asks you
    /// to confirm. Once approved, the service may send this machine jobs
    /// of exactly this bundle, whose code then runs with your permissions.
    /// Prints `Approved:` with the hash.
    ///
    /// moil asks only at a terminal. When stdin isn't one, and always with
    /// --json, it approves only with --yes.
    #[command(after_help = help::APPROVE)]
    Approve {
        /// The service's ID, host, URL or name.
        service: String,
        /// The bundle to approve, by its full hash.
        hash: BundleHash,
        /// Approve without asking.
        #[arg(long)]
        yes: bool,
        /// Print only `{"approved": HASH}`, as JSON. Needs --yes.
        #[arg(long, requires = "yes")]
        json: bool,
    },
    /// Withdraw a bundle's approval
    ///
    /// Stops a running attempt of that bundle for that service, which the
    /// service may retry on another machine, and removes the bundle's
    /// files from the service's folder. It also removes an approval that
    /// doesn't count, such as one whose files changed. Prints `Revoked:`
    /// with the hash.
    #[command(after_help = help::REVOKE)]
    Revoke {
        /// The service's ID, host, URL or name.
        service: String,
        /// The approved bundle, by its full hash.
        hash: BundleHash,
        #[command(flatten)]
        format: Format,
    },
    /// List the hooks, and whether moil can run each
    #[command(long_about = help::HOOKS_ABOUT, after_help = help::HOOKS)]
    Hooks {
        #[command(flatten)]
        format: Format,
    },
    /// Print the agent's log, or a hook's
    ///
    /// Without SERVICE and BUNDLE, prints the agent's log, logs/moil.log in
    /// the home, which the app and `moil agent` both write. With them,
    /// prints what that service's hook for that bundle printed, between
    /// the lines moil writes as each run starts and ends. Hidden characters
    /// are revealed.
    ///
    /// Once a log passes its size limit, moil renames it with .1 added,
    /// and starts a new one; -f follows the new one.
    #[command(after_help = help::LOGS)]
    Logs {
        /// The service whose hook's log to print: its ID, host, URL or name.
        #[arg(requires = "bundle")]
        service: Option<String>,
        /// The bundle the hook is for.
        #[arg(value_parser = bundle_name)]
        bundle: Option<String>,
        /// Keep printing what is added to the log until interrupted.
        #[arg(short, long)]
        follow: bool,
        /// Print only the last LINES lines [default: all of them, or 10
        /// with -f]
        #[arg(short = 'n', long, value_name = "LINES")]
        lines: Option<usize>,
    },
    /// Print where moil keeps its state
    ///
    /// Prints `Home: <path>`: ~/.moil, or the directory --home or
    /// $MOIL_HOME names. The README.md in it says what the rest is.
    #[command(after_help = help::HOME)]
    Home {
        #[command(flatten)]
        format: Format,
    },
    /// Show whether the moil app opens at login, or turn that on or off
    ///
    /// Prints `Open at login: true` or `false`. Needs the app to be
    /// running: without an agent, exits with status 3, and with `moil
    /// agent`, fails.
    #[command(after_help = help::LOGIN_ITEM)]
    LoginItem {
        /// Turn it on or off.
        #[arg(value_enum)]
        state: Option<Switch>,
        #[command(flatten)]
        format: Format,
    },
    /// Stop the agent: the app quits, or `moil agent` exits
    ///
    /// The running job is interrupted and reported to its service, which
    /// may retry it on another machine. Returns once the agent has
    /// stopped. Needs a running agent: without one, exits with status 3.
    #[command(after_help = help::QUIT)]
    Quit {
        #[command(flatten)]
        format: Format,
    },
    /// Run the agent in this terminal until interrupted
    ///
    /// Takes jobs from every paired service, as the app does, until
    /// interrupted (Ctrl-C) or asked to quit with `moil quit`; then the
    /// running job is interrupted and reported. The other commands reach
    /// it while it runs. It logs to stderr and to logs/moil.log in the
    /// home.
    ///
    /// Only one agent runs on a home at a time: quit the app first.
    #[command(after_help = help::AGENT)]
    Agent,
}

/// `--json`, for every owner's command but `logs` and `agent`.
#[derive(Debug, Clone, Copy, clap::Args)]
pub struct Format {
    /// Print only JSON on stdout: one value, or one per line for a
    /// command that keeps printing.
    #[arg(long)]
    json: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum Switch {
    On,
    Off,
}

fn bundle_name(name: &str) -> Result<String, String> {
    match moil_proto::ids::is_bundle_name(name) {
        true => Ok(name.to_owned()),
        false => Err("a bundle's name is lowercase letters, digits and hyphens".into()),
    }
}

/// A command that needs a running agent found none: it exits with status
/// 3 (spec/machine.md §3.1).
#[derive(Debug)]
pub struct NeedsAgent {
    home: PathBuf,
    /// Why the command needs one.
    why: &'static str,
    /// What to run it in.
    agents: &'static str,
}

/// Whatever runs an agent will do.
const ANY_AGENT: &str = "Open the moil app or run `moil agent`";

impl fmt::Display for NeedsAgent {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "moil isn't running on {}, {}. {}.",
            self.home.display(),
            self.why,
            self.agents
        )
    }
}

impl std::error::Error for NeedsAgent {}

pub async fn run(command: Command, globals: &Globals) -> anyhow::Result<()> {
    let ctx = Context::new(globals)?;
    match command {
        Command::Status { watch, format } => match watch {
            false => {
                let status = ctx.status().await?;
                show::status(&status, format.json)
            }
            true => watch_status(&ctx, format.json).await,
        },
        Command::Pause { format } => set_paused(&ctx, true, format.json).await,
        Command::Resume { format } => set_paused(&ctx, false, format.json).await,
        Command::Stop { format } => {
            let why = "so no job runs to stop";
            let mut client = ctx.needs_agent(why, ANY_AGENT).await?;
            match client.stop().await? {
                Some(job) => {
                    let stopping = format!("{} attempt {}", job.job_id, job.attempt);
                    Labels::new().with("Stopping", stopping).print(format.json);
                }
                None => {
                    Labels::new().print(format.json);
                    eprintln!("No job is running.");
                }
            }
            Ok(())
        }
        Command::Pair {
            service,
            name,
            no_browser,
            format,
        } => pair::run(&ctx, &service, name, no_browser, format.json).await,
        Command::Unpair { service, format } => {
            let id = ctx
                .change(
                    async |mut client| Ok(client.unpair(&service).await?.id),
                    async |machine| {
                        let found = machine.find_service(&service)?;
                        machine.unpair(&found.id).await?;
                        Ok(found.id)
                    },
                )
                .await?;
            Labels::new()
                .with("Unpaired", id.to_string())
                .print(format.json);
            Ok(())
        }
        Command::Services { format } => {
            let status = ctx.status().await?;
            show::services(&status.services, format.json)
        }
        Command::Review {
            service,
            hash: None,
            format,
        } => review::list(&ctx, &service, format.json).await,
        Command::Review {
            service,
            hash: Some(hash),
            format,
        } => review::one(&ctx, &service, hash, format.json).await,
        Command::Approve {
            service,
            hash,
            yes,
            json,
        } => review::approve(&ctx, &service, hash, yes, json).await,
        Command::Revoke {
            service,
            hash,
            format,
        } => {
            ctx.change(
                async |mut client| Ok(client.revoke(&service, hash).await?),
                async |machine| {
                    let found = machine.find_service(&service)?;
                    machine.revoke(&found.id, hash)?;
                    Ok(())
                },
            )
            .await?;
            Labels::new()
                .with("Revoked", hash.to_string())
                .print(format.json);
            Ok(())
        }
        Command::Hooks { format } => show::hooks(&ctx.status().await?, format.json),
        Command::Logs {
            service,
            bundle,
            follow,
            lines,
        } => {
            let path = match (service, bundle) {
                (Some(service), Some(bundle)) => {
                    let service = ctx.reader()?.find_service(&service)?;
                    ctx.home.hook_log(&service.id, &bundle)
                }
                _ => ctx.home.log_file(),
            };
            logs::print(&path, follow, lines)
        }
        Command::Home { format } => {
            let home = ctx.home.root().display().to_string();
            Labels::new().with("Home", home).print(format.json);
            Ok(())
        }
        Command::LoginItem { state, format } => {
            let why = "and only the moil app, while it runs, can open at login";
            let mut client = ctx.needs_agent(why, "Open the moil app first").await?;
            let on = client.login_item(state.map(|s| s == Switch::On)).await?;
            Labels::new().with("Open at login", on).print(format.json);
            Ok(())
        }
        Command::Quit { format } => {
            let why = "so there is nothing to quit";
            let client = ctx.needs_agent(why, "It has already stopped").await?;
            let host = client.agent().host;
            client.quit().await?;
            Labels::new().print(format.json);
            match host {
                Host::App => eprintln!("The moil app has quit."),
                Host::Agent => eprintln!("`moil agent` has stopped."),
            }
            Ok(())
        }
        Command::Agent => agent::run(&ctx).await,
    }
}

async fn set_paused(ctx: &Context, paused: bool, json: bool) -> anyhow::Result<()> {
    let paused = ctx
        .change(
            async |mut client| Ok(client.set_paused(paused).await?),
            async |machine| Ok(machine.update_settings(|s| s.paused = paused)?.paused),
        )
        .await?;
    Labels::new().with("Paused", paused).print(json);
    Ok(())
}

/// Prints the status, then again whenever it changes, until the agent
/// stops.
async fn watch_status(ctx: &Context, json: bool) -> anyhow::Result<()> {
    let why = "so nothing changes to watch; `moil status` shows the home as it is";
    let client = ctx.needs_agent(why, ANY_AGENT).await?;
    let mut watch = client.watch().await?;
    let mut first = true;
    while let Some(status) = watch.next().await? {
        if !json && !first {
            println!();
        }
        show::status(&status, json)?;
        first = false;
    }
    eprintln!("The agent has stopped.");
    Ok(())
}

/// How long a command that found the home locked waits for the agent that
/// holds it to answer: it takes the lock before it listens for commands.
const AGENT_STARTING: Duration = Duration::from_secs(5);

/// The home a command works on, and the ways to reach it.
#[derive(Debug)]
pub struct Context {
    /// With an absolute root, as reports show it.
    home: Home,
}

impl Context {
    fn new(globals: &Globals) -> anyhow::Result<Self> {
        let root = match &globals.home {
            Some(root) => root.clone(),
            None => Home::default_root()?,
        };
        let root = std::path::absolute(&root).unwrap_or(root);
        Ok(Self {
            home: Home::new(root),
        })
    }

    /// The home, and where its tokens are kept.
    fn config(&self) -> Result<MachineConfig, moil_agent::Error> {
        Ok(MachineConfig {
            secrets: default_secrets(&self.home)?,
            home: self.home.clone(),
            app_version: env!("CARGO_PKG_VERSION").to_owned(),
        })
    }

    /// The home, to read, whether or not an agent runs on it.
    fn reader(&self) -> anyhow::Result<ReadOnlyMachine> {
        Ok(Machine::open_read_only(self.config()?)?)
    }

    /// The home, locked: only while no agent runs on it.
    fn owner(&self) -> Result<Machine, moil_agent::Error> {
        Machine::open(self.config()?)
    }

    /// The agent running on the home, if one does.
    async fn agent(&self) -> anyhow::Result<Option<Client>> {
        match Client::connect(&self.home).await {
            Ok(client) => Ok(Some(client)),
            Err(ClientError::NoAgent(_)) => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    /// The agent running on the home, for a command that only it can carry
    /// out, which fails with [`NeedsAgent`] if none runs, saying `why` and
    /// what the owner can run it in.
    async fn needs_agent(&self, why: &'static str, agents: &'static str) -> anyhow::Result<Client> {
        match self.agent().await? {
            Some(client) => Ok(client),
            None => Err(NeedsAgent {
                home: self.home.root().to_owned(),
                why,
                agents,
            }
            .into()),
        }
    }

    /// The agent that holds the home's lock, once it answers.
    async fn starting_agent(&self) -> anyhow::Result<Option<Client>> {
        let give_up_at = Instant::now() + AGENT_STARTING;
        loop {
            if let Some(client) = self.agent().await? {
                return Ok(Some(client));
            }
            if Instant::now() >= give_up_at {
                return Ok(None);
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    /// Makes a change through the agent running on the home, or on the
    /// home itself when none runs. If an agent starts in between and takes
    /// the home first, the change goes through it after all.
    async fn change<T>(
        &self,
        agent: impl AsyncFnOnce(Client) -> anyhow::Result<T>,
        home: impl AsyncFnOnce(Machine) -> anyhow::Result<T>,
    ) -> anyhow::Result<T> {
        if let Some(client) = self.agent().await? {
            return agent(client).await;
        }
        match self.owner() {
            Ok(machine) => home(machine).await,
            Err(moil_agent::Error::AlreadyRunning(root)) => match self.starting_agent().await? {
                Some(client) => agent(client).await,
                None => Err(moil_agent::Error::AlreadyRunning(root).into()),
            },
            Err(e) => Err(e.into()),
        }
    }

    /// The status, from the agent running on the home, or from the home
    /// itself.
    async fn status(&self) -> anyhow::Result<Status> {
        match self.agent().await? {
            Some(mut client) => Ok(client.status().await?),
            None => Ok(status::from_home(&self.reader()?)),
        }
    }
}
