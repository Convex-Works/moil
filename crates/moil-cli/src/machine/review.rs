//! `moil review` and `moil approve`: what the owner reads before approving
//! a bundle (spec §5.5), and approving it.

use std::io::{self, BufRead, IsTerminal, Write};

use anyhow::bail;
use moil_agent::Review;
use moil_agent::display::{self, reveal};
use moil_agent::report::{BundleReview, ListedBundle, PackageReport, PreviousVersion, ServiceRef};
use moil_bundle::{LockedPackage, PackageSource};
use moil_proto::BundleHash;

use super::Context;
use super::output::{Labels, print_json, table};

/// Lists a service's bundles and whether each is approved, then the
/// approved bundles it no longer offers.
pub async fn list(ctx: &Context, service: &str, json: bool) -> anyhow::Result<()> {
    let (service, bundles) = match ctx.agent().await? {
        Some(mut client) => {
            let listed = client.bundles(service).await?;
            (listed.service, listed.bundles)
        }
        None => {
            let machine = ctx.reader()?;
            let found = machine.find_service(service)?;
            let offered = machine.bundles(&found.id).await?;
            let bundles = ListedBundle::listing(&offered, &found.approvals);
            (ServiceRef::from(&found), bundles)
        }
    };
    if json {
        print_json(&bundles);
        return Ok(());
    }
    if bundles.is_empty() {
        eprintln!("{} offers no bundles.", reveal(&service.name));
        return Ok(());
    }
    let rows: Vec<[String; 3]> = bundles
        .iter()
        .map(|b| {
            let status = match (b.approved, b.offered) {
                (true, true) => "approved",
                (true, false) => "approved, no longer offered",
                (false, _) => "not approved",
            };
            [
                b.hash.to_string(),
                status.into(),
                format!("{} {}", reveal(&b.name), reveal(&b.version)),
            ]
        })
        .collect();
    table(&["HASH", "STATUS", "BUNDLE"], &rows);
    eprintln!("Show one in full with `moil review {} HASH`.", service.id);
    Ok(())
}

/// Shows one bundle in full.
pub async fn one(ctx: &Context, service: &str, hash: BundleHash, json: bool) -> anyhow::Result<()> {
    let review = match ctx.agent().await? {
        Some(mut client) => client.review(service, hash).await?,
        None => {
            let machine = ctx.reader()?;
            let found = machine.find_service(service)?;
            machine.review(&found.id, hash).await?
        }
    };
    if json {
        print_json(&report(&review));
    } else {
        write_review(&review, &mut io::stdout().lock())?;
    }
    Ok(())
}

/// Shows the bundle in full, then approves it once the owner says so.
pub async fn approve(
    ctx: &Context,
    service: &str,
    hash: BundleHash,
    yes: bool,
    json: bool,
) -> anyhow::Result<()> {
    ctx.change(
        async |mut client| {
            let review = client.review(service, hash).await?;
            if confirm(&review, service, yes, json)? {
                client.approve(service, hash).await?;
            }
            Ok(())
        },
        async |machine| {
            let found = machine.find_service(service)?;
            let review = machine.review(&found.id, hash).await?;
            if confirm(&review, service, yes, json)? {
                machine.approve(&review)?;
            }
            Ok(())
        },
    )
    .await?;
    Labels::new().with("Approved", hash.to_string()).print(json);
    Ok(())
}

/// Shows `review`, unless the output is JSON, and says whether to approve
/// it: `false` if it already is. Asks only at a terminal, and only without
/// `--yes` (spec/machine.md §3.1).
fn confirm(review: &Review, service: &str, yes: bool, json: bool) -> anyhow::Result<bool> {
    if !json {
        write_review(review, &mut io::stdout().lock())?;
    }
    if review.approved {
        eprintln!("It was already approved.");
        return Ok(false);
    }
    if yes {
        return Ok(true);
    }
    let hash = review.bundle.hash();
    if !io::stdin().is_terminal() {
        bail!(
            "moil asks before approving a bundle, and can't: stdin isn't a terminal. Once you \
             have read the bundle above, approve it with `moil approve {service} {hash} --yes`"
        );
    }
    let manifest = review.bundle.manifest();
    eprint!(
        "Approve {} {} for {}? [y/N] ",
        reveal(&manifest.name),
        reveal(&manifest.version),
        reveal(&review.service.name)
    );
    io::stderr().flush()?;
    let mut answer = String::new();
    io::stdin().lock().read_line(&mut answer)?;
    if !matches!(answer.trim().to_lowercase().as_str(), "y" | "yes") {
        bail!("{hash} was not approved");
    }
    Ok(true)
}

/// `moil review SERVICE HASH --json` (spec/machine.md §3.3).
fn report(review: &Review) -> BundleReview {
    let bundle = &review.bundle;
    let files = |bundle: &moil_bundle::Bundle| {
        bundle
            .files()
            .map(|(name, text)| (name.to_owned(), text.to_owned()))
            .collect()
    };
    let previous = review.previous.as_ref().map(|previous| PreviousVersion {
        hash: previous.hash(),
        version: previous.manifest().version.clone(),
        changes: review
            .diff()
            .unwrap_or_default()
            .iter()
            .filter(|diff| !diff.is_unchanged())
            .map(|diff| (diff.name.to_owned(), diff.unified()))
            .collect(),
    });
    BundleReview {
        service: ServiceRef::from(&review.service),
        hash: bundle.hash(),
        approved: review.approved,
        files: files(bundle),
        packages: bundle
            .packages()
            .iter()
            .map(|package| PackageReport {
                name: package.name.clone(),
                version: package.version.clone(),
                source: package.link().unwrap_or_else(|| index(package).to_owned()),
                builds_from_source: package.builds_from_source,
            })
            .collect(),
        previous,
    }
}

/// The index, repository or URL the lockfile names for `package`.
fn index(package: &LockedPackage) -> &str {
    match &package.source {
        PackageSource::Registry(index) | PackageSource::Git(index) | PackageSource::Url(index) => {
            index
        }
    }
}

/// Everything the owner needs to decide (spec §5.5): the service, the three
/// files in full, the packages, the hash and how to check it, and what
/// changed since the version approved before. Everything the service or the
/// bundle controls goes through [`reveal`], so no character in it can
/// rewrite what the owner reads.
fn write_review(review: &Review, out: &mut impl Write) -> io::Result<()> {
    let (bundle, service) = (&review.bundle, &review.service);
    let manifest = bundle.manifest();
    let (name, version) = (reveal(&manifest.name), reveal(&manifest.version));
    let service_name = reveal(&service.name);
    writeln!(
        out,
        "Service: {service_name} ({})",
        reveal(service.base_url.as_str())
    )?;
    if service.is_insecure() {
        writeln!(out, "Warning: this service is reached over plain http.")?;
    }
    writeln!(out, "Bundle: {name} {version}")?;
    if let Some(description) = &manifest.description {
        writeln!(out, "Description: {}", reveal(description))?;
    }
    writeln!(out, "Hash: {}", bundle.hash())?;
    let status = if review.approved {
        "approved"
    } else {
        "not approved"
    };
    writeln!(out, "Status: {status}")?;
    if let Some(memory) = manifest.resources.memory_bytes {
        writeln!(out, "Memory: {}", display::memory(memory))?;
    }
    if let Some(gpu) = manifest.resources.gpu {
        writeln!(out, "GPU: {}", format!("{gpu:?}").to_lowercase())?;
    }
    for (asset_name, asset) in &manifest.assets {
        let size = asset
            .size_bytes
            .map(display::size)
            .unwrap_or_else(|| "size unknown".into());
        writeln!(
            out,
            "Asset: {} ({size}) from {}, sha256 {}",
            reveal(asset_name),
            reveal(&asset.url),
            reveal(&asset.sha256)
        )?;
    }

    writeln!(out)?;
    writeln!(
        out,
        "To check the hash yourself, save the three files below into one directory and run"
    )?;
    writeln!(out, "  {}", Review::VERIFY_COMMAND)?;
    writeln!(out, "It must print {}.", bundle.hash())?;
    for (file, text) in bundle.files() {
        writeln!(out)?;
        writeln!(out, "==> {file} <==")?;
        write!(out, "{}", reveal(text))?;
        if !text.ends_with('\n') {
            writeln!(out)?;
            writeln!(out, "(no newline at the end of {file})")?;
        }
    }

    writeln!(out)?;
    let packages = bundle.packages();
    writeln!(out, "Packages ({}):", packages.len())?;
    for package in packages {
        let source = package
            .link()
            .unwrap_or_else(|| format!("from {}", index(package)));
        let built = if package.builds_from_source {
            "  (built from source here; its build tools aren't pinned)"
        } else {
            ""
        };
        writeln!(
            out,
            "  {} {}  {}{built}",
            reveal(&package.name),
            reveal(package.version.as_deref().unwrap_or("(no version)")),
            reveal(&source)
        )?;
    }

    writeln!(out)?;
    match (&review.previous, review.diff()) {
        (Some(previous), Some(diffs)) => {
            writeln!(
                out,
                "Changes since {} {} ({}):",
                reveal(&previous.manifest().name),
                reveal(&previous.manifest().version),
                previous.hash()
            )?;
            for diff in diffs {
                if diff.is_unchanged() {
                    writeln!(out, "  {} is unchanged", diff.name)?;
                } else {
                    write!(out, "{}", reveal(&diff.unified()))?;
                }
            }
        }
        _ => writeln!(
            out,
            "No earlier version of {name} is approved for {service_name}."
        )?,
    }
    writeln!(out)?;
    writeln!(
        out,
        "Approving lets this code run on this machine with your permissions whenever \
         {service_name} sends it a job. moil has no sandbox yet."
    )
}

#[cfg(test)]
mod tests {
    use std::time::SystemTime;

    use moil_agent::display::is_hidden;
    use moil_agent::report::json_line;
    use moil_agent::{Pairing, Service, ServiceId};
    use moil_bundle::Bundle;

    use super::*;

    /// A bundle, and the service offering it, that hide things everywhere
    /// they can: the screen-clearing escape sequence and the invisible
    /// characters would let a harmless-looking review be drawn over the
    /// real one.
    fn hostile() -> Review {
        let clear = "\x1b[2J\x1b[H";
        let script = format!(
            "# /// script\n# requires-python = \">=3.12\"\n# dependencies = []\n# ///\n\
             import os{clear}\nprint('hi')\u{202e}\r\n"
        );
        let hash = format!("sha256:{}", "a".repeat(64));
        let lockfile = format!(
            "version = 1\nrevision = 3\n\n[[package]]\nname = \"requests\\u001b[2J\"\n\
             version = \"2.32.3\\u001b[8m\"\nsource = {{ registry = \"https://pypi.org/simple\" }}\n\
             wheels = [{{ hash = \"{hash}\" }}]\n"
        );
        let manifest = "{\"moil\": 1, \"name\": \"echo\", \"version\": \"1.0\u{3164}\", \
                        \"description\": \"Harmless.\u{200b}\"}";
        let bundle = Bundle::new(script, lockfile, manifest).unwrap();
        let pairing = Pairing {
            base_url: "https://klisi.example.com/moil".parse().unwrap(),
            machine_id: "m_1".into(),
            paired_at: SystemTime::now(),
        };
        Review {
            service: Service {
                id: ServiceId::new("klisi.example.com").unwrap(),
                base_url: pairing.base_url.clone(),
                name: format!("klisi{clear}\u{2066}"),
                approvals: vec![],
                pairing: Some(pairing),
            },
            previous: Some(bundle.clone()),
            bundle,
            approved: false,
        }
    }

    #[test]
    fn a_review_shows_nothing_hidden() {
        let mut out = Vec::new();
        write_review(&hostile(), &mut out).unwrap();
        let shown = String::from_utf8(out).unwrap();
        let hidden: Vec<char> = shown.chars().filter(|&c| is_hidden(c)).collect();
        assert!(hidden.is_empty(), "{hidden:?} in\n{shown}");
        for revealed in [
            "Service: klisi␛[2J␛[H\\u{2066} (https://klisi.example.com/moil)",
            "Bundle: echo 1.0\\u{3164}",
            "Description: Harmless.\\u{200b}",
            "import os␛[2J␛[H",
            "print('hi')\\u{202e}␍",
            "  requests␛[2J 2.32.3␛[8m  https://pypi.org/project/requests␛[2J/2.32.3␛[8m/",
        ] {
            assert!(shown.contains(revealed), "{revealed:?} in\n{shown}");
        }
    }

    #[test]
    fn a_review_as_json_hides_nothing_and_keeps_every_byte() {
        let review = hostile();
        let line = json_line(&report(&review));
        assert!(line.is_ascii(), "{line}");
        let read: BundleReview = serde_json::from_str(&line).unwrap();
        let script = review.bundle.files().next().unwrap().1;
        assert!(read.files.values().any(|text| text == script));
        assert_eq!(read.packages[0].name, "requests\x1b[2J");
        let previous = read.previous.expect("the version approved before");
        assert!(previous.changes.is_empty(), "the same bundle: no changes");
    }
}
