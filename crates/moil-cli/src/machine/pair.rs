//! `moil pair`: pairing with a service new to the home, with one it knows
//! but isn't paired with, or again with a paired one (spec/machine.md
//! §3.2).

use std::process::{Command as Process, Stdio};

use anyhow::{Context as _, bail};
use moil_agent::control::PairingCode;
use moil_agent::display::reveal;
use moil_agent::report::ServiceRef;
use url::Url;

use super::Context;
use super::output::Labels;

pub async fn run(
    ctx: &Context,
    service: &str,
    name: Option<String>,
    no_browser: bool,
    json: bool,
) -> anyhow::Result<()> {
    let target = target(ctx, service)?;
    let paired = ctx
        .change(
            async |client| {
                let pairing = client.pair(&target, name.as_deref()).await?;
                started(&pairing.code, no_browser, json)?;
                Ok(pairing.confirmed().await?)
            },
            async |machine| {
                let pair = async || machine.pair(&target).await;
                let (session, code) = PairingCode::start(&machine, name.clone(), pair).await?;
                started(&code, no_browser, json)?;
                Ok(ServiceRef::from(&session.wait().await?))
            },
        )
        .await?;
    Labels::new()
        .with("Paired", paired.id.to_string())
        .print(json);
    eprintln!(
        "{} can now offer this machine jobs. See its bundles with `moil review {}`.",
        reveal(&paired.name),
        paired.id
    );
    Ok(())
}

/// The base URL to pair with: that of the service `service` names, if the
/// home knows it, else `service` itself.
fn target(ctx: &Context, service: &str) -> anyhow::Result<String> {
    match ctx.reader()?.find_service(service) {
        Ok(known) => Ok(known.base_url.to_string()),
        Err(moil_agent::Error::UnknownService(_)) => {
            // Not a URL, nor a host, which has a dot or a port.
            if !service.contains(['/', '.', ':']) && service.trim() != "localhost" {
                bail!(
                    "the home knows no service {service:?}. To pair with a new service, give \
                     its moil URL, such as https://tide.example.com/moil; `moil services` \
                     lists those the home knows"
                );
            }
            Ok(service.to_owned())
        }
        Err(e) => Err(e.into()),
    }
}

/// The confirmation page, which the agent checked is on the service's
/// origin and passes on as it parsed it, to show and open exactly that.
fn page(checked: &str) -> anyhow::Result<Url> {
    Url::parse(checked).with_context(|| format!("the confirmation page {checked:?} isn't a URL"))
}

/// Shows the code to confirm and where, and opens the page.
fn started(started: &PairingCode, no_browser: bool, json: bool) -> anyhow::Result<()> {
    let page = page(&started.verification_uri_complete)?;
    let service_name = reveal(&started.service_name);
    eprintln!(
        "Pairing with {service_name} at {} as {:?}.",
        started.base_url, started.machine_name
    );
    if started.base_url.scheme() == "http" {
        eprintln!("Warning: this service is reached over plain http.");
    }
    Labels::new()
        .with("Code", started.user_code.as_str())
        .with("URL", page.as_str())
        .print(json);
    if !no_browser && open_browser(&page).is_err() {
        eprintln!("Couldn't open a browser; open the URL above yourself.");
    }
    eprintln!("Check that {service_name} shows the same code, then confirm it there. Waiting…");
    Ok(())
}

fn open_browser(url: &Url) -> std::io::Result<()> {
    let mut command = if cfg!(target_os = "macos") {
        Process::new("open")
    } else if cfg!(windows) {
        let mut command = Process::new("rundll32");
        command.arg("url.dll,FileProtocolHandler");
        command
    } else {
        Process::new("xdg-open")
    };
    command
        .arg(url.as_str())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map(drop)
}
