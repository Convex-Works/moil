//! The moil tray app: pairs this machine with services, shows the owner
//! every job bundle to approve, and shows what is running. macOS first.
//!
//! `moil-app --demo` runs the same UI on a simulated agent.

use std::process::ExitCode;
use std::sync::Arc;

mod app;
mod backend;
mod demo;
mod format;
#[cfg(test)]
mod gallery;
mod icon;
mod ipc;
mod links;
mod log;
mod login;
mod page;
mod pair;
mod platform;
mod review;
mod tray;
mod unpair;
mod view;

use backend::Backend;
use login::{DemoLoginItem, LoginItem, SystemLoginItem};

const USAGE: &str = "\
usage: moil-app [--demo] [moil://…]

  --demo    run on a simulated agent, touching nothing on this machine
  moil://…  open a moil link, such as moil://pair?url=https://…";

/// What the command line asks for.
#[derive(Debug, Default, PartialEq, Eq)]
struct Options {
    demo: bool,
    help: bool,
    /// `moil://` links to open once the app runs. On macOS links arrive as
    /// events instead; elsewhere they arrive as arguments.
    links: Vec<String>,
}

impl Options {
    fn parse(args: impl IntoIterator<Item = String>) -> Result<Self, String> {
        let mut options = Self::default();
        for arg in args {
            match arg.as_str() {
                "--demo" => options.demo = true,
                "--help" | "-h" => options.help = true,
                // Older macOS versions pass a process serial number.
                psn if psn.starts_with("-psn_") => {}
                link if link.starts_with("moil:") => options.links.push(arg),
                other => return Err(format!("unexpected argument {other:?}")),
            }
        }
        Ok(options)
    }
}

fn main() -> ExitCode {
    let options = match Options::parse(std::env::args().skip(1)) {
        Ok(options) => options,
        Err(e) => {
            eprintln!("moil-app: {e}\n\n{USAGE}");
            return ExitCode::from(2);
        }
    };
    if options.help {
        println!("{USAGE}");
        return ExitCode::SUCCESS;
    }
    let log_dir = if options.demo {
        Some(log::demo_dir())
    } else {
        log::default_dir()
    };
    let log_file = log::init(log_dir.as_deref());

    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .thread_name("moil-agent")
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(e) => {
            eprintln!("moil-app: couldn't start: {e}");
            return ExitCode::FAILURE;
        }
    };
    let (backend, login): (Result<Arc<dyn Backend>, String>, Box<dyn LoginItem>) = {
        let _entered = runtime.enter();
        if options.demo {
            tracing::info!("running the demo: nothing here touches this machine's moil");
            let demo: Arc<dyn Backend> = Arc::new(demo::Demo::start());
            (Ok(demo), Box::new(DemoLoginItem::default()))
        } else {
            (backend::start_agent(), Box::new(SystemLoginItem::new()))
        }
    };
    app::run(runtime, backend, login, options.links, log_file)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(args: &[&str]) -> Result<Options, String> {
        Options::parse(args.iter().map(|s| s.to_string()))
    }

    #[test]
    fn the_command_line_takes_the_demo_flag_and_links() {
        assert_eq!(
            parse(&["--demo", "moil://pair?url=https://klisi.example.com/moil"]),
            Ok(Options {
                demo: true,
                help: false,
                links: vec!["moil://pair?url=https://klisi.example.com/moil".into()],
            })
        );
        assert_eq!(parse(&["-psn_0_12345"]), Ok(Options::default()));
        assert!(parse(&["--help"]).unwrap().help);
        assert_eq!(
            parse(&["--dmeo"]),
            Err("unexpected argument \"--dmeo\"".into())
        );
        assert!(parse(&["https://klisi.example.com"]).is_err());
    }
}
