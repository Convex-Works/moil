//! `moil status`, `moil services` and `moil hooks`: what the home holds and
//! what the agent is doing, as text or as the JSON of spec/machine.md §3.3.

use std::io::{self, Write};
use std::path::Path;

use moil_agent::display::reveal;
use moil_agent::report::{ConnectionReport, HookListing, Host, Problem, ServiceReport, Status};
use moil_agent::rfc3339;

use super::output::{print_json, table, word};

pub fn status(status: &Status, json: bool) -> anyhow::Result<()> {
    if json {
        print_json(status);
    } else {
        write_status(status, &mut io::stdout().lock())?;
        let waiting = status
            .services
            .iter()
            .flat_map(|s| s.offered.iter().flatten().map(move |b| (s, b)))
            .find(|(_, bundle)| !bundle.approved);
        if let Some((service, bundle)) = waiting {
            eprintln!(
                "Read a bundle waiting for your review with `moil review {} {}`, and approve it \
                 with `moil approve {} {}`.",
                service.id, bundle.hash, service.id, bundle.hash
            );
        }
    }
    Ok(())
}

/// The status as `Label: value` lines, with what belongs to the job and to
/// each service indented under it.
fn write_status(status: &Status, out: &mut impl Write) -> io::Result<()> {
    writeln!(out, "Home: {}", status.home.display())?;
    match &status.agent {
        Some(agent) => {
            let host = match agent.host {
                Host::App => "the moil app",
                Host::Agent => "`moil agent`",
            };
            writeln!(out, "Agent: {host} (moil {})", agent.version)?;
        }
        None => writeln!(out, "Agent: none")?,
    }
    writeln!(out, "Paused: {}", status.paused)?;
    if let Some(job) = &status.job {
        writeln!(out, "Job: {} attempt {}", job.job_id, job.attempt)?;
        writeln!(out, "  Service: {}", job.service)?;
        writeln!(out, "  Bundle: {}", reveal(&job.bundle))?;
        if let Some(title) = &job.title {
            writeln!(out, "  Title: {}", reveal(title))?;
        }
        let mut phase = word(&job.phase);
        if let Some(fraction) = job.fraction {
            phase.push_str(&format!(", {:.0}%", fraction * 100.0));
        }
        if let Some(message) = &job.message {
            phase.push_str(&format!(": {}", reveal(message)));
        }
        writeln!(out, "  Phase: {phase}")?;
        writeln!(out, "  Started: {}", rfc3339::format(job.started_at))?;
        if job.stopping {
            writeln!(out, "  Stopping: true")?;
        }
    }
    if let Some(job) = &status.last_job {
        writeln!(out, "Last job: {} attempt {}", job.job_id, job.attempt)?;
        writeln!(out, "  Service: {}", job.service)?;
        writeln!(out, "  Bundle: {}", reveal(&job.bundle))?;
        if let Some(title) = &job.title {
            writeln!(out, "  Title: {}", reveal(title))?;
        }
        match &job.error {
            Some(error) => writeln!(
                out,
                "  Outcome: {} ({}: {})",
                word(&job.outcome),
                error.code,
                reveal(&error.message)
            )?,
            None => writeln!(out, "  Outcome: {}", word(&job.outcome))?,
        }
        writeln!(out, "  Ended: {}", rfc3339::format(job.ended_at))?;
    }
    for service in &status.services {
        write_service(service, out)?;
    }
    for problem in &status.problems {
        write_problem(problem, out)?;
    }
    Ok(())
}

/// A problem, with hidden characters revealed in its path too: it names
/// whatever someone put in the home, such as a file pushed to a service
/// folder's repository.
fn write_problem(problem: &Problem, out: &mut impl Write) -> io::Result<()> {
    writeln!(
        out,
        "Problem: {}: {}",
        reveal(&problem.path.to_string_lossy()),
        reveal(&problem.message)
    )
}

fn write_service(service: &ServiceReport, out: &mut impl Write) -> io::Result<()> {
    writeln!(out, "Service: {}", service.id)?;
    writeln!(out, "  Name: {}", reveal(&service.name))?;
    writeln!(out, "  URL: {}", reveal(service.base_url.as_str()))?;
    writeln!(out, "  Paired: {}", service.paired)?;
    if let Some(connection) = &service.connection {
        let connection = match connection {
            ConnectionReport::Connecting => "connecting".into(),
            ConnectionReport::Connected => "connected".into(),
            ConnectionReport::Offline { error, retry_at } => format!(
                "offline ({}), trying again at {}",
                reveal(error),
                rfc3339::format(*retry_at)
            ),
            ConnectionReport::Unpaired => {
                "unpaired: the service removed this machine, or moil has no token for it; \
                 pair again"
                    .into()
            }
            ConnectionReport::Replaced => {
                "replaced: another connection with this machine's token took over".into()
            }
        };
        writeln!(out, "  Connection: {connection}")?;
    }
    for approval in &service.approvals {
        writeln!(
            out,
            "  Approved: {} {} {}",
            reveal(&approval.name),
            reveal(&approval.version),
            approval.hash
        )?;
    }
    for bundle in service.offered.iter().flatten().filter(|b| !b.approved) {
        writeln!(
            out,
            "  Waiting for review: {} {} {}",
            reveal(&bundle.name),
            reveal(&bundle.version),
            bundle.hash
        )?;
    }
    for hook in &service.hooks {
        let runs = if hook.runnable { "runs" } else { "can't run" };
        writeln!(
            out,
            "  Hook: {} ({runs}) {}",
            hook.bundle,
            hook.path.display()
        )?;
    }
    Ok(())
}

pub fn services(services: &[ServiceReport], json: bool) -> anyhow::Result<()> {
    if json {
        print_json(&services);
        return Ok(());
    }
    if services.is_empty() {
        eprintln!("No services yet. Pair with one with `moil pair URL`.");
        return Ok(());
    }
    let rows: Vec<[String; 5]> = services
        .iter()
        .map(|s| {
            [
                s.id.to_string(),
                reveal(s.base_url.as_str()).into_owned(),
                if s.paired { "yes" } else { "no" }.into(),
                s.approvals.len().to_string(),
                reveal(&s.name).into_owned(),
            ]
        })
        .collect();
    table(&["ID", "URL", "PAIRED", "APPROVED", "NAME"], &rows);
    Ok(())
}

/// The hooks `status` shows, under each service, and what's wrong with
/// them.
pub fn hooks(status: &Status, json: bool) -> anyhow::Result<()> {
    let hooks: Vec<HookListing> = status
        .services
        .iter()
        .flat_map(|s| {
            s.hooks.iter().map(move |hook| HookListing {
                service: s.id.clone(),
                hook: hook.clone(),
            })
        })
        .collect();
    if json {
        print_json(&hooks);
        return Ok(());
    }
    let mut out = io::stdout().lock();
    for listing in &hooks {
        let hook = &listing.hook;
        writeln!(out, "Hook: {} {}", listing.service, hook.bundle)?;
        writeln!(out, "  Path: {}", hook.path.display())?;
        writeln!(out, "  Log: {}", hook.log.display())?;
        writeln!(out, "  Runnable: {}", hook.runnable)?;
    }
    let in_hooks = |path: &Path| {
        let hooks = |id: &str| Path::new("services").join(id).join("hooks");
        (status.services.iter()).any(|s| path.starts_with(hooks(s.id.as_str())))
    };
    for problem in status.problems.iter().filter(|p| in_hooks(&p.path)) {
        write_problem(problem, &mut out)?;
    }
    if hooks.is_empty() {
        eprintln!(
            "No hooks yet. A hook is an executable at services/<service>/hooks/<bundle>; \
             `moil hooks --help` explains them."
        );
    }
    Ok(())
}
