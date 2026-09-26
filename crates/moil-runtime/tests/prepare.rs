//! Preparing bundles: uv environments and assets, with real uv and a real
//! HTTPS server.

mod common;

use common::*;
use moil_bundle::{Bundle, BundleStore};
use moil_proto::{ErrorCode, LogLevel, Phase};
use moil_runtime::{Executor, Runtime};
use serde_json::{Value, json};

/// A manifest for the probe with these assets, each `(name, path on the
/// server, contents)`, served over HTTPS with their true hash and size.
fn with_assets(h: &Harness, assets: &[(&str, &str, &str)]) -> Bundle {
    let mut entries = serde_json::Map::new();
    for (name, path, contents) in assets {
        h.server.serve(path, *contents);
        entries.insert(
            (*name).into(),
            json!({
                "url": h.server.https_url(path),
                "sha256": sha256(contents.as_bytes()),
                "size_bytes": contents.len(),
            }),
        );
    }
    probe_with_manifest(json!({"moil": 1, "name": "probe", "version": "1", "assets": entries}))
}

fn asset_manifest(asset: Value) -> Bundle {
    probe_with_manifest(
        json!({"moil": 1, "name": "probe", "version": "1", "assets": {"model.bin": asset}}),
    )
}

fn leftovers(dir: &std::path::Path) -> Vec<String> {
    std::fs::read_dir(dir)
        .map(|entries| {
            entries
                .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
                .filter(|name| name.starts_with('.'))
                .collect()
        })
        .unwrap_or_default()
}

mod assets {
    use super::*;

    #[tokio::test]
    async fn assets_are_verified_cached_and_linked_under_their_names() {
        let h = Harness::new().await;
        let bundle = with_assets(
            &h,
            &[
                ("models/tiny.bin", "/models/tiny.bin", "tiny model"),
                ("vocab.txt", "/vocab.txt", "a b c"),
            ],
        );
        let run = h
            .run(h.job(
                &bundle,
                steps(json!([
                    {"asset": "models/tiny.bin"},
                    {"context": true},
                    {"emit": {"type": "result"}},
                ])),
            ))
            .await;
        run.result();
        assert_eq!(run.data("asset"), "tiny model");
        let context = run.data("context");
        let work = context["work_dir"].as_str().unwrap();
        assert_eq!(
            context["assets"],
            json!({
                "models/tiny.bin": format!("{work}/assets/models/tiny.bin"),
                "vocab.txt": format!("{work}/assets/vocab.txt"),
            })
        );
        assert!(
            run.logs(LogLevel::Info)
                .contains(&"downloading asset models/tiny.bin".to_owned())
        );

        // Cached by hash, read-only so a script can't change it for others.
        let cached = h.runtime.config().assets_dir.join(sha256(b"tiny model"));
        assert_eq!(std::fs::read_to_string(&cached).unwrap(), "tiny model");
        assert!(std::fs::metadata(&cached).unwrap().permissions().readonly());

        // The next attempt uses the cache.
        let again = h
            .run(h.job(
                &bundle,
                steps(json!([{"asset": "vocab.txt"}, {"emit": {"type": "result"}}])),
            ))
            .await;
        assert_eq!(again.data("asset"), "a b c");
        assert!(
            again
                .logs(LogLevel::Info)
                .iter()
                .all(|log| !log.starts_with("downloading"))
        );
        assert_eq!(h.server.received("/models/tiny.bin").len(), 1);
        assert_eq!(h.server.received("/vocab.txt").len(), 1);
        assert_eq!(
            leftovers(&h.runtime.config().assets_dir),
            Vec::<String>::new()
        );
    }

    #[tokio::test]
    async fn bundles_share_cached_assets() {
        let h = Harness::new().await;
        let first = with_assets(&h, &[("model.bin", "/model.bin", "weights")]);
        let second = with_assets(&h, &[("elsewhere/weights.bin", "/model.bin", "weights")]);
        assert_ne!(first.hash(), second.hash());
        h.runtime.prepare(&first).await.unwrap();
        h.runtime.prepare(&second).await.unwrap();
        assert_eq!(h.server.received("/model.bin").len(), 1);
    }

    #[tokio::test]
    async fn concurrent_prepares_download_an_asset_once() {
        let h = Harness::new().await;
        let bundle = with_assets(&h, &[("model.bin", "/model.bin", "weights")]);
        let (a, b) = tokio::join!(h.runtime.prepare(&bundle), h.runtime.prepare(&bundle));
        a.unwrap();
        b.unwrap();
        assert_eq!(h.server.received("/model.bin").len(), 1);
    }

    #[tokio::test]
    async fn runtimes_sharing_a_cache_can_fetch_the_same_asset_at_once() {
        let h = Harness::new().await;
        let bundle = with_assets(&h, &[("model.bin", "/model.bin", "weights")]);
        // Another runtime, as in another process, with its own home but
        // the same asset cache.
        let other_home = tempfile::tempdir().unwrap();
        let mut config = h.runtime.config().clone();
        config.bundles = BundleStore::new(other_home.path().join("bundles"));
        config.jobs_dir = other_home.path().join("jobs");
        let other = Runtime::try_new(config).unwrap();

        let (a, b) = tokio::join!(h.runtime.prepare(&bundle), other.prepare(&bundle));
        a.unwrap();
        b.unwrap();
        let assets_dir = &h.runtime.config().assets_dir;
        assert_eq!(
            std::fs::read_to_string(assets_dir.join(sha256(b"weights"))).unwrap(),
            "weights"
        );
        assert_eq!(leftovers(assets_dir), Vec::<String>::new());
    }

    #[tokio::test]
    async fn an_asset_that_cant_be_downloaded_is_a_retryable_asset_download() {
        let h = Harness::new().await;
        let bundle = asset_manifest(json!({
            "url": h.server.https_url("/missing.bin"),
            "sha256": sha256(b"x"),
        }));
        let run = h.run(h.job(&bundle, steps(json!([])))).await;
        let error = run.error();
        assert_eq!(error.code, ErrorCode::AssetDownload);
        assert!(error.retryable);
        assert!(
            error.message.contains("model.bin") && error.message.contains("404"),
            "{}",
            error.message
        );
        assert_eq!(run.phases(), [Phase::Preparing]);
        assert!(
            h.runtime
                .config()
                .assets_dir
                .read_dir()
                .unwrap()
                .next()
                .is_none()
        );
    }

    #[tokio::test]
    async fn an_asset_with_the_wrong_hash_is_refused_and_not_cached() {
        let h = Harness::new().await;
        h.server.serve("/model.bin", "tampered");
        let bundle = asset_manifest(json!({
            "url": h.server.https_url("/model.bin"),
            "sha256": sha256(b"original"),
        }));
        let error = h.runtime.prepare(&bundle).await.unwrap_err();
        assert_eq!(error.code, ErrorCode::AssetMismatch);
        assert!(!error.retryable);
        assert!(
            error.message.contains(&sha256(b"tampered")),
            "{}",
            error.message
        );
        let assets_dir = &h.runtime.config().assets_dir;
        assert!(
            assets_dir.read_dir().unwrap().next().is_none(),
            "nothing was cached"
        );
    }

    #[tokio::test]
    async fn an_asset_larger_than_the_manifest_says_is_refused() {
        let h = Harness::new().await;
        let big = "w".repeat(1 << 20);
        h.server.serve("/model.bin", big.clone());
        let bundle = asset_manifest(json!({
            "url": h.server.https_url("/model.bin"),
            "sha256": sha256(big.as_bytes()),
            "size_bytes": 10,
        }));
        let error = h.runtime.prepare(&bundle).await.unwrap_err();
        assert_eq!(error.code, ErrorCode::AssetMismatch);
        assert!(
            error.message.contains("larger than the 10 bytes"),
            "{}",
            error.message
        );
    }

    #[tokio::test]
    async fn assets_need_a_certificate_the_machine_trusts() {
        let h = Harness::with(|c| c.extra_root_certificates.clear()).await;
        let bundle = with_assets(&h, &[("model.bin", "/model.bin", "weights")]);
        let error = h.runtime.prepare(&bundle).await.unwrap_err();
        assert_eq!(error.code, ErrorCode::AssetDownload);
        assert!(h.server.received("/model.bin").is_empty());
    }

    #[tokio::test]
    async fn assets_follow_redirects_to_other_https_hosts_but_never_to_plain_http() {
        // Like Hugging Face, whose `resolve` URLs answer 302 to a CDN on
        // another host, with a certificate from another CA.
        let cdn = Server::start().await;
        let h = Harness::with(|c| c.extra_root_certificates.push(cdn.ca_pem.clone())).await;
        let on_cdn = |path: &str| cdn.https_url(path).replace("127.0.0.1", "localhost");
        // Several MiB, so it arrives in many chunks, each hashed as it's
        // written to disk.
        let weights: Vec<u8> = (0..4u32 << 20)
            .map(|i| (i.wrapping_mul(2_654_435_761) >> 24) as u8)
            .collect();
        cdn.serve("/cdn/model.safetensors", weights.clone());
        h.server.reply(
            "/org/model/resolve/main/model.safetensors",
            Reply::Redirect(on_cdn("/cdn/model.safetensors")),
        );
        let bundle = asset_manifest(json!({
            "url": h.server.https_url("/org/model/resolve/main/model.safetensors"),
            "sha256": sha256(&weights),
            "size_bytes": weights.len(),
        }));
        h.runtime.prepare(&bundle).await.unwrap();
        let cached = h.runtime.config().assets_dir.join(sha256(&weights));
        assert!(std::fs::read(cached).unwrap() == weights);

        // A redirect from https to http on a host that isn't this machine
        // is refused, like such a URL would be in the first place.
        cdn.serve("/cdn/other.bin", "other");
        let port = cdn.url("").rsplit(':').next().unwrap().to_owned();
        h.server.reply(
            "/org/model/resolve/main/other.bin",
            Reply::Redirect(format!("http://0.0.0.0:{port}/cdn/other.bin")),
        );
        let bundle = asset_manifest(json!({
            "url": h.server.https_url("/org/model/resolve/main/other.bin"),
            "sha256": sha256(b"other"),
        }));
        let error = h.runtime.prepare(&bundle).await.unwrap_err();
        assert_eq!(error.code, ErrorCode::AssetDownload);
        assert!(
            error.message.contains("redirected to a URL that uses http"),
            "{}",
            error.message
        );
        assert!(cdn.received("/cdn/other.bin").is_empty());
    }

    #[tokio::test]
    async fn assets_that_cant_all_be_laid_out_are_refused() {
        let h = Harness::new().await;
        let bundle = with_assets(
            &h,
            &[("whisper", "/a", "a"), ("whisper/model.bin", "/b", "b")],
        );
        let error = h.runtime.prepare(&bundle).await.unwrap_err();
        assert_eq!(error.code, ErrorCode::Environment);
        assert!(
            error
                .message
                .contains("\"whisper\" and \"whisper/model.bin\""),
            "{}",
            error.message
        );
    }
}

mod environment {
    use super::*;

    #[tokio::test]
    async fn dependencies_are_installed_from_the_lockfile() {
        let h = Harness::new().await;
        let run = h
            .run(h.job(&bundle("idna"), json!({"name": "bücher.example"})))
            .await;
        assert_eq!(
            run.result().meta,
            Some(json!({"encoded": "xn--bcher-kva.example"}))
        );
    }

    #[tokio::test]
    async fn dependencies_changed_without_relocking_are_refused() {
        let h = Harness::new().await;
        let probe = probe();
        let script = probe
            .script()
            .replace("# dependencies = []", "# dependencies = [\"idna==3.20\"]");
        let bundle = Bundle::new(script, probe.lockfile(), probe.manifest_text()).unwrap();

        let run = h
            .run(h.job(&bundle, steps(json!([{"emit": {"type": "result"}}]))))
            .await;
        let error = run.error();
        assert_eq!(error.code, ErrorCode::Environment);
        assert!(!error.retryable);
        assert!(
            error.message.contains("uv lock --script job.py"),
            "{}",
            error.message
        );
        assert!(error.stderr_tail.as_deref().unwrap().contains("--locked"));
        assert_eq!(run.phases(), [Phase::Preparing], "the script never ran");
    }

    #[tokio::test]
    async fn a_failing_uv_sync_is_environment_with_its_stderr() {
        let h = Harness::new().await;
        let probe = probe();
        let bundle = Bundle::new(
            probe.script().replace(">=3.12", ">=3.99"),
            probe.lockfile().replace(">=3.12", ">=3.99"),
            probe.manifest_text(),
        )
        .unwrap();
        let error = h.runtime.prepare(&bundle).await.unwrap_err();
        assert_eq!(error.code, ErrorCode::Environment);
        assert!(error.message.contains("3.99"), "{}", error.message);
        assert!(error.stderr_tail.as_deref().unwrap().contains("3.99"));
        assert!(error.exit_code.is_some_and(|code| code != 0));
    }

    #[tokio::test]
    async fn a_machine_without_uv_fails_retryably() {
        let h = Harness::with(|c| c.uv = "/nonexistent/uv".into()).await;
        let error = h.runtime.prepare(&probe()).await.unwrap_err();
        assert_eq!(error.code, ErrorCode::Environment);
        assert!(error.retryable, "another machine may have uv");
        assert!(
            error.message.contains("/nonexistent/uv"),
            "{}",
            error.message
        );
    }

    #[tokio::test]
    async fn prepare_stores_the_bundle() {
        let h = Harness::new().await;
        let bundle = probe();
        h.runtime.prepare(&bundle).await.unwrap();
        assert_eq!(
            h.runtime.config().bundles.get(&bundle.hash()).unwrap(),
            Some(bundle)
        );
    }
}
