//! Attempts end to end: real uv, real Python, real HTTP.

mod common;

use std::time::{Duration, Instant};

use common::*;
use moil_proto::{
    ErrorCode, FileInfo, JobEvent, LogLevel, Outcome, Phase, UploadMethod, UrlPolicy,
};
use moil_runtime::{Executor, Job, StopReason};
use serde_json::json;

#[tokio::test]
async fn a_successful_attempt_end_to_end() {
    let h = Harness::new().await;
    let mut job = h.job(
        &probe(),
        steps(json!([
            {"input": "text.txt"},
            {"context": true},
            {"emit": {"type": "progress", "fraction": 0.5, "message": "halfway"}},
            {"emit": {"type": "log", "level": "debug", "message": "details"}},
            {"write": "a.txt", "text": "alpha"},
            {"write": "b.json", "text": "{\"b\": 1}"},
            {"emit": {"v": 1, "type": "result", "files": ["a.txt", "b.json"], "meta": {"n": 2}}},
        ])),
    );
    job.inputs.extend([input(&h.server, "text.txt", "hello")]);
    let (a, mut to_a) = output(&h.server, "a.txt");
    to_a.headers
        .insert("Content-Type".into(), "text/plain".into());
    let (b, mut to_b) = output(&h.server, "b.json");
    to_b.method = UploadMethod::Post;
    job.outputs.extend([(a, to_a), (b, to_b)]);
    let work_dir = h.runtime.work_dir(&job.id, job.attempt);

    let run = h.run(job.clone()).await;

    let result = run.result();
    assert_eq!(result.meta, Some(json!({"n": 2})));
    assert_eq!(
        result.files["a.txt"],
        FileInfo {
            size_bytes: 5,
            sha256: sha256(b"alpha")
        }
    );
    assert_eq!(result.files["b.json"].sha256, sha256(b"{\"b\": 1}"));

    let context = run.data("context");
    assert_eq!(
        run.events,
        [
            JobEvent::Phase {
                phase: Phase::Preparing
            },
            JobEvent::Phase {
                phase: Phase::Downloading
            },
            JobEvent::Phase {
                phase: Phase::Running
            },
            JobEvent::Data {
                payload: json!({"input": "hello"})
            },
            JobEvent::Data {
                payload: json!({"context": context})
            },
            JobEvent::Progress {
                fraction: Some(0.5),
                message: Some("halfway".into())
            },
            JobEvent::log(LogLevel::Debug, "details"),
            JobEvent::Phase {
                phase: Phase::Uploading
            },
        ]
    );

    // The script was told where everything is, with absolute paths.
    let work = h
        .jobs_dir()
        .canonicalize()
        .unwrap()
        .join(format!("{}.1", job.id));
    let work = work.to_str().unwrap();
    assert_eq!(context["v"], 1);
    assert_eq!(context["job_id"], job.id.as_str());
    assert_eq!(context["attempt"], 1);
    assert_eq!(context["params"], job.params);
    assert_eq!(context["work_dir"], work);
    assert_eq!(
        context["inputs"],
        json!({"text.txt": format!("{work}/in/text.txt")})
    );
    assert_eq!(context["output_dir"], format!("{work}/out"));
    assert_eq!(context["outputs"], json!(["a.txt", "b.json"]));
    assert_eq!(context["assets_dir"], format!("{work}/assets"));
    assert_eq!(context["assets"], json!({}));

    // Outputs went up with the method, headers and bytes the job asked for.
    let [put] = &h.server.received("/out/a.txt")[..] else {
        panic!()
    };
    assert_eq!(put.method, "PUT");
    assert_eq!(put.body, b"alpha");
    assert_eq!(put.header("content-type"), Some("text/plain"));
    assert_eq!(put.header("content-length"), Some("5"));
    assert_eq!(put.header("transfer-encoding"), None);
    let [post] = &h.server.received("/out/b.json")[..] else {
        panic!()
    };
    assert_eq!(post.method, "POST");
    assert_eq!(post.body, b"{\"b\": 1}");

    // Nothing is left behind.
    assert!(!work_dir.exists());
    assert!(is_empty_dir(&h.jobs_dir().join(".attempts")));
}

#[tokio::test]
async fn kept_working_directories_survive_the_attempt() {
    let h = Harness::with(|c| c.keep_work_dirs = true).await;
    let mut job = h.job(
        &probe(),
        steps(json!([
            {"write": "a.txt", "text": "alpha"},
            {"emit": {"type": "result", "files": ["a.txt"]}},
        ])),
    );
    job.outputs.extend([output(&h.server, "a.txt")]);
    let work_dir = h.runtime.work_dir(&job.id, job.attempt);

    h.run(job).await.result();

    assert_eq!(
        std::fs::read_to_string(work_dir.join("out/a.txt")).unwrap(),
        "alpha"
    );
    assert!(is_empty_dir(&h.jobs_dir().join(".attempts")));
}

/// What the owner keeps of attempts that succeed (spec §8.5).
mod hand_off {
    use std::collections::BTreeSet;
    use std::path::Path;

    use moil_runtime::HandOff;

    use super::*;

    /// The names of the files in `dir`.
    fn names(dir: &Path) -> BTreeSet<String> {
        std::fs::read_dir(dir)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().into_string().unwrap())
            .collect()
    }

    /// A job that writes three declared outputs and names two in its result.
    fn two_of_three(h: &Harness) -> Job {
        let mut job = h.job(
            &probe(),
            steps(json!([
                {"write": "a.txt", "text": "alpha"},
                {"write": "b.json", "text": "{}"},
                {"write": "c.txt", "text": "not in the result"},
                {"emit": {"type": "result", "files": ["a.txt", "b.json"], "meta": {"n": 2}}},
            ])),
        );
        job.outputs.extend([
            output(&h.server, "a.txt"),
            output(&h.server, "b.json"),
            output(&h.server, "c.txt"),
        ]);
        job
    }

    #[tokio::test]
    async fn a_success_leaves_exactly_the_uploaded_outputs() {
        let h = Harness::new().await;
        let kept = h.home.path().join("kept");
        let mut job = two_of_three(&h);
        let hand_off = HandOff::new(&kept);
        job.hand_off = Some(hand_off.clone());
        let work_dir = h.runtime.work_dir(&job.id, job.attempt);

        let run = h.run(job).await;

        // The service hears what it would have heard without a hand-off.
        let result = run.result();
        assert_eq!(result.meta, Some(json!({"n": 2})));
        assert_eq!(
            result.files.keys().map(|n| n.as_str()).collect::<Vec<_>>(),
            ["a.txt", "b.json"]
        );
        assert_eq!(hand_off.result(), Some(Ok(())));
        assert_eq!(
            names(&kept),
            BTreeSet::from(["a.txt".into(), "b.json".into()])
        );
        assert_eq!(
            std::fs::read_to_string(kept.join("a.txt")).unwrap(),
            "alpha"
        );
        assert_eq!(std::fs::read_to_string(kept.join("b.json")).unwrap(), "{}");
        assert!(!work_dir.exists());
        assert!(is_empty_dir(&h.jobs_dir().join(".attempts")));
    }

    #[tokio::test]
    async fn a_kept_working_directory_keeps_its_outputs_too() {
        let h = Harness::with(|c| c.keep_work_dirs = true).await;
        let kept = h.home.path().join("kept");
        let mut job = two_of_three(&h);
        let hand_off = HandOff::new(&kept);
        job.hand_off = Some(hand_off.clone());
        let work_dir = h.runtime.work_dir(&job.id, job.attempt);

        h.run(job).await.result();

        assert_eq!(hand_off.result(), Some(Ok(())));
        assert_eq!(
            names(&kept),
            BTreeSet::from(["a.txt".into(), "b.json".into()])
        );
        assert_eq!(
            std::fs::read_to_string(kept.join("a.txt")).unwrap(),
            "alpha"
        );
        assert_eq!(
            names(&work_dir.join("out")),
            BTreeSet::from(["a.txt".into(), "b.json".into(), "c.txt".into()])
        );
    }

    #[tokio::test]
    async fn nothing_is_handed_off_unless_the_attempt_succeeds() {
        let h = Harness::new().await;
        let kept = h.home.path().join("kept");
        let mut job = h.job(
            &probe(),
            steps(json!([
                {"write": "a.txt", "text": "alpha"},
                {"emit": {"type": "result", "files": ["a.txt"]}},
                {"exit": 1},
            ])),
        );
        job.outputs.extend([output(&h.server, "a.txt")]);
        let hand_off = HandOff::new(&kept);
        job.hand_off = Some(hand_off.clone());

        assert_eq!(h.run(job).await.error().code, ErrorCode::Exit);
        assert_eq!(hand_off.result(), None);
        assert!(!kept.exists());
    }

    #[tokio::test]
    async fn outputs_that_cant_be_handed_off_leave_the_outcome_alone() {
        let h = Harness::new().await;
        let kept = h.home.path().join("kept");
        // b.json can't be renamed onto a directory that isn't empty, after
        // a.txt was moved.
        std::fs::create_dir_all(kept.join("b.json/in-the-way")).unwrap();
        let mut job = two_of_three(&h);
        let hand_off = HandOff::new(&kept);
        job.hand_off = Some(hand_off.clone());
        let work_dir = h.runtime.work_dir(&job.id, job.attempt);

        let run = h.run(job).await;

        assert_eq!(run.result().meta, Some(json!({"n": 2})));
        let why = hand_off.result().unwrap().unwrap_err();
        assert!(why.starts_with("couldn't move output b.json to "), "{why}");
        // What was moved was taken back.
        assert_eq!(names(&kept), BTreeSet::from(["b.json".into()]));
        assert!(!work_dir.exists());
    }
}

mod outcome {
    use super::*;

    #[tokio::test]
    async fn an_error_event_fails_with_the_scripts_message_and_retryability() {
        let h = Harness::new().await;
        let run = h
            .run(h.job(
                &probe(),
                steps(json!([
                    {"emit": {"type": "error", "message": "CUDA out of memory", "retryable": true}},
                    {"exit": 1},
                ])),
            ))
            .await;
        let error = run.error();
        assert_eq!(error.code, ErrorCode::ScriptError);
        assert_eq!(error.message, "CUDA out of memory");
        assert!(error.retryable);
        assert_eq!(error.exit_code, Some(1));
    }

    #[tokio::test]
    async fn an_error_wins_over_a_result_and_a_clean_exit() {
        let h = Harness::new().await;
        let run = h
            .run(h.job(
                &probe(),
                steps(json!([
                    {"emit": {"type": "result"}},
                    {"emit": {"type": "error", "message": "changed my mind"}},
                ])),
            ))
            .await;
        let error = run.error();
        assert_eq!(
            (error.code, error.retryable),
            (ErrorCode::ScriptError, false)
        );
        assert_eq!(error.exit_code, Some(0));
    }

    #[tokio::test]
    async fn a_nonzero_exit_fails_even_after_a_result() {
        let h = Harness::new().await;
        let run = h
            .run(h.job(
                &probe(),
                steps(json!([{"emit": {"type": "result"}}, {"exit": 3}])),
            ))
            .await;
        let error = run.error();
        assert_eq!(error.code, ErrorCode::Exit);
        assert_eq!(error.exit_code, Some(3));
        assert!(!error.retryable);
        assert_eq!(error.message, "the script exited with status 3");
    }

    #[tokio::test]
    async fn a_script_killed_by_a_signal_fails_with_exit() {
        let h = Harness::new().await;
        let run = h
            .run(h.job(
                &probe(),
                steps(json!([{"emit": {"type": "result"}}, {"kill_self": 9}])),
            ))
            .await;
        let error = run.error();
        assert_eq!(error.code, ErrorCode::Exit);
        assert!(error.message.contains("signal 9"), "{}", error.message);
    }

    #[tokio::test]
    async fn exiting_cleanly_without_a_result_is_no_result() {
        let h = Harness::new().await;
        let run = h
            .run(h.job(
                &probe(),
                steps(json!([{"emit": {"type": "progress", "fraction": 1}}])),
            ))
            .await;
        let error = run.error();
        assert_eq!(
            (error.code, error.exit_code),
            (ErrorCode::NoResult, Some(0))
        );
    }

    #[tokio::test]
    async fn a_result_naming_an_undeclared_output_is_output_missing() {
        let h = Harness::new().await;
        let run = h
            .run(h.job(
                &probe(),
                steps(json!([
                    {"write": "secret.txt", "text": "not yours"},
                    {"emit": {"type": "result", "files": ["secret.txt"]}},
                ])),
            ))
            .await;
        let error = run.error();
        assert_eq!(error.code, ErrorCode::OutputMissing);
        assert!(error.message.contains("secret.txt"), "{}", error.message);
        assert!(h.server.received("/out/secret.txt").is_empty());
    }

    #[tokio::test]
    async fn a_result_naming_an_unwritten_output_is_output_missing() {
        let h = Harness::new().await;
        let mut job = h.job(
            &probe(),
            steps(json!([
                {"write": "a.txt", "text": "alpha"},
                {"emit": {"type": "result", "files": ["a.txt", "b.txt"]}},
            ])),
        );
        job.outputs
            .extend([output(&h.server, "a.txt"), output(&h.server, "b.txt")]);
        let run = h.run(job).await;
        let error = run.error();
        assert_eq!(error.code, ErrorCode::OutputMissing);
        assert!(
            error.message.contains("the script didn't write out/b.txt"),
            "{}",
            error.message
        );
        assert!(
            h.server.received("/out/a.txt").is_empty(),
            "nothing is uploaded"
        );
    }

    #[tokio::test]
    async fn outputs_must_be_regular_files_in_out() {
        // As a script extracting an archive from an input could be tricked
        // into leaving them.
        let h = Harness::new().await;
        let elsewhere = tempfile::tempdir().unwrap();
        let secret = elsewhere.path().join("result.txt");
        std::fs::write(&secret, "the owner's secret").unwrap();
        let secret = secret.to_str().unwrap();
        let dir = elsewhere.path().to_str().unwrap();

        for (make, why) in [
            (
                json!({"symlink": "out/result.txt", "to": secret}),
                "out/result.txt is a symbolic link",
            ),
            (
                json!({"symlink": "out", "to": dir}),
                "out/ is a symbolic link",
            ),
            (
                json!({"fifo": "out/result.txt"}),
                "out/result.txt isn't a regular file",
            ),
        ] {
            let mut job = h.job(
                &probe(),
                steps(json!([make, {"emit": {"type": "result", "files": ["result.txt"]}}])),
            );
            job.outputs.extend([output(&h.server, "result.txt")]);
            let run = h.run(job).await;
            let error = run.error();
            assert_eq!(error.code, ErrorCode::OutputMissing, "{make}");
            assert!(error.message.contains(why), "{make}: {}", error.message);
        }
        assert!(h.server.received("/out/result.txt").is_empty());
    }

    #[tokio::test]
    async fn a_rejected_upload_is_a_retryable_output_upload() {
        let h = Harness::new().await;
        h.server.reply("/out/a.txt", Reply::Status(403));
        let mut job = h.job(
            &probe(),
            steps(json!([
                {"write": "a.txt", "text": "alpha"},
                {"emit": {"type": "result", "files": ["a.txt"]}},
            ])),
        );
        job.outputs.extend([output(&h.server, "a.txt")]);
        let run = h.run(job).await;
        let error = run.error();
        assert_eq!(error.code, ErrorCode::OutputUpload);
        assert!(error.retryable);
        assert!(
            error.message.contains("403") && error.message.contains("no luck"),
            "{}",
            error.message
        );
        assert_eq!(run.phases().last(), Some(&Phase::Uploading));
    }

    #[tokio::test]
    async fn running_past_the_time_limit_is_timeout() {
        let h = Harness::new().await;
        let mut job = h.job(&probe(), steps(json!([{"sleep": 60}])));
        job.timeout = Some(Duration::from_millis(1500));
        let started = Instant::now();
        let run = h.run(job).await;
        let error = run.error();
        assert_eq!(error.code, ErrorCode::Timeout);
        assert!(!error.retryable);
        assert_eq!(
            error.message,
            "the attempt ran past its time limit of 1500 ms"
        );
        assert!(started.elapsed() < Duration::from_secs(10));
    }

    #[tokio::test]
    async fn a_timeout_wins_over_the_scripts_error() {
        let h = Harness::new().await;
        let mut job = h.job(
            &probe(),
            steps(json!([
                {"emit": {"type": "error", "message": "about to hang"}},
                {"sleep": 60},
            ])),
        );
        job.timeout = Some(Duration::from_secs(1));
        assert_eq!(h.run(job).await.error().code, ErrorCode::Timeout);
    }

    #[tokio::test]
    async fn a_cancel_during_the_grace_period_wins_over_the_timeout() {
        let h = Harness::new().await;
        let limit = Duration::from_secs(3600);
        let mut job = h.job(
            &probe(),
            steps(json!([
                {"on_sigterm": "terminating", "linger": 3600},
                {"emit": {"type": "data", "payload": {"ready": true}}},
                {"sleep": 3600},
            ])),
        );
        job.timeout = Some(limit);
        let mut running = h.start(job);
        running.data("ready").await;
        // The time limit passes only now, with the script listening for
        // SIGTERM. The paused clock moves only when told to: waiting for
        // the script keeps it from skipping ahead.
        tokio::time::pause();
        tokio::time::advance(limit).await;
        running.log("terminating").await;
        // The script is still cleaning up when the service cancels: rule 1
        // beats rule 3. Then its grace period, half a second here, runs out.
        running.stop(StopReason::Cancelled);
        tokio::time::advance(Duration::from_secs(60)).await;
        assert_eq!(running.finish().await.outcome, Outcome::Cancelled);
    }
}

mod stopping {
    use super::*;

    #[tokio::test]
    async fn a_cancelled_attempt_is_cancelled() {
        let h = Harness::new().await;
        let job = h.job(
            &probe(),
            steps(json!([{"emit": {"type": "data", "payload": {"ready": true}}}, {"sleep": 60}])),
        );
        let work_dir = h.runtime.work_dir(&job.id, job.attempt);
        let mut running = h.start(job);
        running.data("ready").await;
        running.stop(StopReason::Cancelled);
        let run = running.finish().await;
        assert_eq!(run.outcome, Outcome::Cancelled);
        assert!(!work_dir.exists());
    }

    #[tokio::test]
    async fn an_interrupted_attempt_fails_retryably() {
        let h = Harness::new().await;
        let job = h.job(
            &probe(),
            steps(json!([
                {"stderr": "working hard"},
                {"emit": {"type": "data", "payload": {"ready": true}}},
                {"sleep": 60},
            ])),
        );
        let mut running = h.start(job);
        running.data("ready").await;
        running.stop(StopReason::Interrupted);
        let run = running.finish().await;
        let error = run.error();
        assert_eq!(error.code, ErrorCode::Interrupted);
        assert!(error.retryable);
        assert_eq!(error.stderr_tail.as_deref(), Some("working hard"));
    }

    #[tokio::test]
    async fn a_script_gets_to_clean_up_before_it_is_killed() {
        // Only waited for in full if the script doesn't exit.
        let h = Harness::with(|c| c.kill_grace = Duration::from_secs(60)).await;
        let job = h.job(
            &probe(),
            steps(json!([
                {"on_sigterm": "cleaning up"},
                {"emit": {"type": "data", "payload": {"ready": true}}},
                {"sleep": 60},
            ])),
        );
        let mut running = h.start(job);
        running.data("ready").await;
        running.stop(StopReason::Cancelled);
        let run = running.finish().await;
        assert_eq!(run.outcome, Outcome::Cancelled);
        assert!(run.logs(LogLevel::Info).contains(&"cleaning up".to_owned()));
    }

    #[tokio::test]
    async fn stopping_kills_the_whole_process_group() {
        let h = Harness::new().await;
        let job = h.job(&probe(), steps(json!([{"spawn": true}, {"sleep": 60}])));
        let mut running = h.start(job);
        let stubborn = running.data("pid").await.as_i64().unwrap() as i32;
        assert!(process_exists(stubborn));
        running.stop(StopReason::Cancelled);
        assert_eq!(running.finish().await.outcome, Outcome::Cancelled);
        assert!(
            gone(stubborn).await,
            "a grandchild ignoring SIGTERM survived"
        );
    }

    #[tokio::test]
    async fn nothing_the_script_started_outlives_it() {
        let h = Harness::new().await;
        let run = h
            .run(h.job(
                &probe(),
                steps(json!([{"spawn": true}, {"emit": {"type": "result"}}])),
            ))
            .await;
        run.result();
        let stubborn = run.data("pid").as_i64().unwrap() as i32;
        assert!(
            gone(stubborn).await,
            "a leftover process survived the attempt"
        );
    }

    #[tokio::test]
    async fn a_stop_while_downloading_ends_the_attempt_promptly() {
        let h = Harness::new().await;
        h.server.reply("/in/huge.bin", Reply::Hang);
        let mut job = h.job(&probe(), steps(json!([])));
        job.inputs.insert(
            "huge.bin".parse().unwrap(),
            moil_proto::Download {
                url: h.server.url("/in/huge.bin"),
                headers: Default::default(),
            },
        );
        let work_dir = h.runtime.work_dir(&job.id, job.attempt);
        let mut running = h.start(job);
        running.phase(Phase::Downloading).await;
        running.stop(StopReason::Interrupted);
        let run = running.finish().await;
        let error = run.error();
        assert_eq!(error.code, ErrorCode::Interrupted);
        assert_eq!((error.exit_code, &error.stderr_tail), (None, &None));
        assert_eq!(run.phases().last(), Some(&Phase::Downloading));
        assert!(!work_dir.exists());
    }

    /// Stops `running` and waits for its outcome, failing if that takes
    /// more than a few seconds.
    async fn stop_promptly(running: Running, reason: StopReason) -> Run {
        running.stop(reason);
        tokio::time::timeout(Duration::from_secs(5), running.finish())
            .await
            .expect("the attempt didn't return promptly after being stopped")
    }

    #[tokio::test]
    async fn a_stop_while_uploading_ends_the_attempt_promptly() {
        let h = Harness::new().await;
        h.server.reply("/out/a.txt", Reply::Hang);
        let mut job = h.job(
            &probe(),
            steps(json!([
                {"write": "a.txt", "text": "alpha"},
                {"emit": {"type": "result", "files": ["a.txt"]}},
            ])),
        );
        job.outputs.extend([output(&h.server, "a.txt")]);
        let mut running = h.start(job);
        running.phase(Phase::Uploading).await;
        let run = stop_promptly(running, StopReason::Cancelled).await;
        assert_eq!(run.outcome, Outcome::Cancelled);
    }

    #[tokio::test]
    async fn a_stop_while_fetching_an_asset_ends_the_attempt_promptly() {
        let h = Harness::new().await;
        h.server.reply("/model.bin", Reply::Hang);
        let bundle = probe_with_manifest(json!({
            "moil": 1, "name": "probe", "version": "1",
            "assets": {"model.bin": {"url": h.server.https_url("/model.bin"), "sha256": sha256(b"x")}},
        }));
        let mut running = h.start(h.job(&bundle, steps(json!([]))));
        running.log("downloading asset model.bin").await;
        let run = stop_promptly(running, StopReason::Interrupted).await;
        assert_eq!(run.error().code, ErrorCode::Interrupted);
        let assets_dir = &h.runtime.config().assets_dir;
        assert!(is_empty_dir(assets_dir), "the partial download was removed");
    }

    #[tokio::test]
    async fn a_stop_while_another_preparation_holds_the_bundle_is_prompt() {
        let h = Harness::new().await;
        h.server.reply("/model.bin", Reply::Hang);
        let bundle = probe_with_manifest(json!({
            "moil": 1, "name": "probe", "version": "1",
            "assets": {"model.bin": {"url": h.server.https_url("/model.bin"), "sha256": sha256(b"x")}},
        }));
        // The preparation that followed approval, stuck on a slow download.
        let background = {
            let (runtime, bundle) = (h.runtime.clone(), bundle.clone());
            tokio::spawn(async move { runtime.prepare(&bundle).await })
        };
        let mut running = h.start(h.job(&bundle, steps(json!([]))));
        running.phase(Phase::Preparing).await;
        let run = stop_promptly(running, StopReason::Cancelled).await;
        assert_eq!(run.outcome, Outcome::Cancelled);
        background.abort();
    }

    #[tokio::test]
    async fn the_time_limit_covers_downloads_too() {
        let h = Harness::new().await;
        h.server.reply("/in/huge.bin", Reply::Hang);
        let mut job = h.job(&probe(), steps(json!([])));
        job.inputs.insert(
            "huge.bin".parse().unwrap(),
            moil_proto::Download {
                url: h.server.url("/in/huge.bin"),
                headers: Default::default(),
            },
        );
        job.timeout = Some(Duration::from_secs(1));
        assert_eq!(h.run(job).await.error().code, ErrorCode::Timeout);
    }
}

mod stdout {
    use super::*;

    #[tokio::test]
    async fn stray_output_becomes_logs_and_bad_events_become_warnings() {
        let h = Harness::new().await;
        let long = "x".repeat(5000);
        let run = h
            .run(h.job(
                &probe(),
                steps(json!([
                    {"print": "hello there"},
                    {"print": "[1, 2]"},
                    {"print": long},
                    {"raw": [0xff, 0xfe, 0x0a]},
                    {"print": ""},
                    {"emit": {"type": "shout"}},
                    {"emit": {"v": 2, "type": "log", "message": "from the future"}},
                    {"emit": {"type": "progress", "fraction": "half"}},
                    {"emit": {"type": "progress"}},
                    {"emit": {"type": "progress", "fraction": 1.5}},
                    {"emit": {"type": "phase", "phase": "uploading"}},
                    {"emit": {"answer": 42}},
                    {"emit": {"type": "log", "level": "fatal", "message": "x"}},
                    {"emit": {"type": "result", "meta": 1}},
                    {"emit": {"type": "result", "meta": 2}},
                    {"emit": {"type": "log", "message": "still listening"}},
                ])),
            ))
            .await;

        assert_eq!(run.result().meta, Some(json!(1)), "the first result counts");
        assert_eq!(
            run.logs(LogLevel::Info),
            [
                "hello there",
                "[1, 2]",
                &"x".repeat(4096),
                "\u{fffd}\u{fffd}",
                "still listening"
            ]
        );
        let warnings = run.logs(LogLevel::Warn);
        let expected = [
            (6, "unknown event type \"shout\""),
            (7, "event version 2 isn't supported"),
            (8, "invalid progress event"),
            (9, "needs a fraction or a message"),
            (10, "isn't between 0 and 1"),
            (11, "scripts can't send phase events"),
            (12, "a JSON object without a \"type\""),
            (13, "invalid log event"),
            (15, "a second result"),
        ];
        assert_eq!(warnings.len(), expected.len(), "{warnings:#?}");
        for (warning, (line, why)) in warnings.iter().zip(expected) {
            assert!(
                warning.starts_with(&format!("ignored stdout line {line}: "))
                    && warning.contains(why),
                "expected line {line} to be ignored as {why:?}, got {warning:?}"
            );
        }
        assert_eq!(
            run.phases(),
            [
                Phase::Preparing,
                Phase::Downloading,
                Phase::Running,
                Phase::Uploading
            ],
            "the script's phase event wasn't forwarded"
        );
    }

    #[tokio::test]
    async fn a_line_over_one_mebibyte_is_dropped_and_reading_goes_on() {
        let h = Harness::new().await;
        let huge = "y".repeat(1 << 20);
        let run = h
            .run(h.job(
                &probe(),
                steps(json!([
                    {"emit": {"type": "log", "message": huge}},
                    {"emit": {"type": "result", "meta": "after"}},
                ])),
            ))
            .await;
        assert_eq!(run.result().meta, Some(json!("after")));
        assert_eq!(
            run.logs(LogLevel::Warn),
            ["dropped stdout line 1: it is longer than the 1 MiB limit"]
        );
        assert!(run.logs(LogLevel::Info).is_empty());
    }

    #[tokio::test]
    async fn payloads_over_256_kib_are_refused() {
        let h = Harness::new().await;
        let big = "z".repeat(300 << 10);
        let run = h
            .run(h.job(
                &probe(),
                steps(json!([
                    {"emit": {"type": "data", "payload": big}},
                    {"emit": {"type": "data", "payload": "small"}},
                    {"emit": {"type": "result", "meta": big}},
                ])),
            ))
            .await;
        // An oversized data event is dropped; an oversized result can't be
        // passed on, so the attempt fails rather than succeeding without
        // the meta the service expects.
        let data: Vec<_> = run
            .script_events()
            .into_iter()
            .filter(|e| matches!(e, JobEvent::Data { .. }))
            .collect();
        assert_eq!(
            data,
            [JobEvent::Data {
                payload: json!("small")
            }]
        );
        let error = run.error();
        assert_eq!(error.code, ErrorCode::NoResult);
        assert_eq!(
            error.message,
            format!(
                "the script's result was refused: the result's meta is {} bytes, over the 256 KiB limit",
                (300 << 10) + 2
            )
        );
        assert_eq!(run.logs(LogLevel::Warn).len(), 2);
    }

    /// `[[…[1]…]]`, `depth` arrays deep.
    fn nested(depth: usize) -> serde_json::Value {
        (0..depth).fold(json!(1), |inner, _| json!([inner]))
    }

    #[tokio::test]
    async fn payloads_nested_more_than_64_levels_deep_are_refused() {
        let h = Harness::new().await;
        let run = h
            .run(h.job(
                &probe(),
                steps(json!([
                    {"emit": {"type": "data", "payload": nested(64)}},
                    {"emit": {"type": "data", "payload": nested(65)}},
                    {"emit": {"type": "result", "meta": nested(65)}},
                ])),
            ))
            .await;
        let data: Vec<_> = run
            .script_events()
            .into_iter()
            .filter(|e| matches!(e, JobEvent::Data { .. }))
            .collect();
        assert_eq!(
            data,
            [JobEvent::Data {
                payload: nested(64)
            }]
        );
        assert_eq!(
            run.logs(LogLevel::Warn),
            [
                "ignored stdout line 2: the data payload is nested more than 64 levels deep",
                "ignored stdout line 3: the result's meta is nested more than 64 levels deep",
            ]
        );
        let error = run.error();
        assert_eq!(error.code, ErrorCode::NoResult);
        assert!(
            error.message.contains("nested more than 64"),
            "{}",
            error.message
        );

        let run = h
            .run(h.job(
                &probe(),
                steps(json!([{"emit": {"type": "result", "meta": nested(64)}}])),
            ))
            .await;
        assert_eq!(run.result().meta, Some(nested(64)));
    }

    #[tokio::test]
    async fn a_refused_result_is_still_the_first() {
        let h = Harness::new().await;
        let run = h
            .run(h.job(
                &probe(),
                steps(json!([
                    {"emit": {"type": "result", "meta": "z".repeat(300 << 10)}},
                    {"emit": {"type": "result", "meta": "small"}},
                ])),
            ))
            .await;
        assert_eq!(run.error().code, ErrorCode::NoResult);
        assert!(
            run.logs(LogLevel::Warn)
                .contains(&"ignored stdout line 2: a second result".to_owned())
        );
    }

    #[tokio::test]
    async fn a_failure_quotes_a_long_name_briefly() {
        // A `done` over the 1 MiB limit of a control message would be
        // refused every time it's sent.
        let h = Harness::new().await;
        let name = "n".repeat(900_000);
        let run = h
            .run(h.job(
                &probe(),
                steps(json!([{"emit": {"type": "result", "files": [name]}}])),
            ))
            .await;
        let error = run.error();
        assert_eq!(error.code, ErrorCode::OutputMissing);
        assert!(error.message.len() < 1000, "{} bytes", error.message.len());
    }

    #[tokio::test]
    async fn a_result_after_an_error_is_ignored() {
        let h = Harness::new().await;
        let run = h
            .run(h.job(
                &probe(),
                steps(json!([
                    {"emit": {"type": "error", "message": "first"}},
                    {"emit": {"type": "result"}},
                    {"emit": {"type": "error", "message": "second"}},
                ])),
            ))
            .await;
        assert_eq!(run.error().code, ErrorCode::ScriptError);
        assert_eq!(run.error().message, "first");
        assert_eq!(
            run.logs(LogLevel::Warn),
            [
                "ignored stdout line 2: a result after an error",
                "ignored stdout line 3: a second error"
            ]
        );
    }
}

#[tokio::test]
async fn failures_carry_the_last_lines_of_stderr() {
    let h = Harness::new().await;
    let run = h
        .run(h.job(
            &probe(),
            steps(json!([
                {"stderr": "noise noise noise", "times": 2000},
                {"stderr": "the last words"},
                {"exit": 2},
            ])),
        ))
        .await;
    let error = run.error();
    assert_eq!((error.code, error.exit_code), (ErrorCode::Exit, Some(2)));
    let tail = error.stderr_tail.as_deref().unwrap();
    assert!(tail.len() <= 8 << 10, "{} bytes", tail.len());
    let lines: Vec<_> = tail.lines().collect();
    assert_eq!(lines.last(), Some(&"the last words"));
    assert!(lines.len() > 100);
    assert!(
        lines[..lines.len() - 1]
            .iter()
            .all(|l| *l == "noise noise noise")
    );
}

mod transfers {
    use super::*;

    #[tokio::test]
    async fn a_missing_input_is_a_retryable_input_download() {
        let h = Harness::new().await;
        let mut job = h.job(&probe(), steps(json!([{"emit": {"type": "result"}}])));
        job.inputs.insert(
            "gone.txt".parse().unwrap(),
            moil_proto::Download {
                url: h.server.url("/in/gone.txt"),
                headers: Default::default(),
            },
        );
        let run = h.run(job).await;
        let error = run.error();
        assert_eq!(error.code, ErrorCode::InputDownload);
        assert!(error.retryable);
        assert!(error.message.contains("404"), "{}", error.message);
        assert_eq!(run.phases().last(), Some(&Phase::Downloading));
    }

    #[tokio::test]
    async fn an_unreachable_input_is_input_download() {
        let h = Harness::new().await;
        let closed = {
            let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            listener.local_addr().unwrap()
        };
        let mut job = h.job(&probe(), steps(json!([{"emit": {"type": "result"}}])));
        job.inputs.insert(
            "a.txt".parse().unwrap(),
            moil_proto::Download {
                url: format!("http://{closed}/a.txt"),
                headers: Default::default(),
            },
        );
        let run = h.run(job).await;
        assert_eq!(run.error().code, ErrorCode::InputDownload);
        assert!(
            run.error().message.contains("connect"),
            "{}",
            run.error().message
        );
    }

    #[tokio::test]
    async fn an_input_larger_than_the_machine_accepts_is_refused() {
        let h = Harness::with(|c| c.max_download_bytes = 1000).await;
        let big = vec![b'x'; 2000];
        h.server.serve("/in/said.bin", big.clone());
        h.server.reply("/in/unsaid.bin", Reply::Chunked(big));
        for path in ["/in/said.bin", "/in/unsaid.bin"] {
            let mut job = h.job(&probe(), steps(json!([{"emit": {"type": "result"}}])));
            job.inputs.insert(
                "big.bin".parse().unwrap(),
                moil_proto::Download {
                    url: h.server.url(path),
                    headers: Default::default(),
                },
            );
            let run = h.run(job).await;
            let error = run.error();
            assert_eq!(error.code, ErrorCode::InputDownload, "{path}");
            assert!(!error.retryable, "another machine would refuse it too");
            assert_eq!(
                error.message,
                "couldn't download input big.bin: it is larger than the 1000 bytes this machine accepts"
            );
        }
    }

    #[tokio::test]
    async fn inputs_and_outputs_report_how_far_they_got() {
        let h = Harness::new().await;
        let (down, up) = (Gate::default(), Gate::default());
        h.server.reply(
            "/in/big.in",
            Reply::Gated {
                body: vec![b'i'; 4 << 20],
                at: 1 << 20,
                gate: down.clone(),
            },
        );
        h.server.reply(
            "/out/big.out",
            Reply::Receive {
                at: 1 << 20,
                gate: up.clone(),
            },
        );
        let mut job = h.job(
            &probe(),
            steps(json!([
                {"fill": "big.out", "bytes": 32 << 20},
                {"emit": {"type": "result", "files": ["big.out"]}},
            ])),
        );
        job.inputs.insert(
            "big.in".parse().unwrap(),
            moil_proto::Download {
                url: h.server.url("/in/big.in"),
                headers: Default::default(),
            },
        );
        job.outputs.extend([output(&h.server, "big.out")]);
        let mut running = h.start(job);

        running.phase(Phase::Downloading).await;
        loop {
            let (fraction, message) = running.progress().await;
            if fraction == Some(0.25) {
                assert_eq!(message, "input big.in: 1 MiB of 4 MiB");
                break;
            }
            assert!(fraction.is_some_and(|f| f < 0.25), "{fraction:?}");
        }
        down.open();

        // The server stops reading the upload after a mebibyte.
        running.phase(Phase::Uploading).await;
        let (fraction, message) = running.progress().await;
        assert!(fraction.is_some_and(|f| f > 0.0 && f < 1.0), "{fraction:?}");
        assert!(
            message.starts_with("output big.out: ") && message.ends_with(" of 32 MiB"),
            "{message}"
        );
        up.open();

        let run = running.finish().await;
        assert_eq!(run.result().files["big.out"].size_bytes, 32 << 20);
        assert_eq!(
            run.progress().last(),
            Some(&(Some(1.0), "output big.out: 32 MiB of 32 MiB".to_owned()))
        );
        assert!(
            run.progress()
                .contains(&(Some(1.0), "input big.in: 4 MiB of 4 MiB".to_owned()))
        );
    }

    #[tokio::test]
    async fn inputs_come_over_https_with_their_headers() {
        let h = Harness::new().await;
        h.server.serve("/in/secret.txt", "classified");
        let mut job = h.job(
            &probe(),
            steps(json!([{"input": "secret.txt"}, {"emit": {"type": "result"}}])),
        );
        job.inputs.insert(
            "secret.txt".parse().unwrap(),
            moil_proto::Download {
                url: h.server.https_url("/in/secret.txt"),
                headers: [("Authorization".into(), "Bearer presigned".into())].into(),
            },
        );
        let run = h.run(job).await;
        run.result();
        assert_eq!(run.data("input"), "classified");
        let [request] = &h.server.received("/in/secret.txt")[..] else {
            panic!()
        };
        assert_eq!(request.header("authorization"), Some("Bearer presigned"));
    }

    #[tokio::test]
    async fn http_to_other_hosts_is_refused_before_anything_runs() {
        let h = Harness::new().await;
        // 0.0.0.0 reaches the test server, but it isn't a loopback address.
        let port = h.server.url("").rsplit(':').next().unwrap().to_owned();
        let elsewhere = |path: &str| format!("http://0.0.0.0:{port}{path}");
        h.server.serve("/in/a.txt", "a");

        let mut job = h.job(&probe(), steps(json!([{"emit": {"type": "result"}}])));
        job.inputs.insert(
            "a.txt".parse().unwrap(),
            moil_proto::Download {
                url: elsewhere("/in/a.txt"),
                headers: Default::default(),
            },
        );
        let run = h.run(job).await;
        assert_eq!(run.error().code, ErrorCode::InputDownload);
        assert!(!run.error().retryable, "no other attempt would do better");
        assert!(
            run.error().message.contains("only https is allowed"),
            "{}",
            run.error().message
        );
        assert!(run.phases().is_empty(), "nothing was prepared");

        let mut job = h.job(&probe(), steps(json!([{"emit": {"type": "result"}}])));
        let (name, mut upload) = output(&h.server, "b.txt");
        upload.url = elsewhere("/out/b.txt");
        job.outputs.insert(name, upload);
        let run = h.run(job).await;
        assert_eq!(run.error().code, ErrorCode::OutputUpload);
        assert!(!run.error().retryable);
        assert!(run.phases().is_empty(), "the script didn't run");

        assert!(h.server.received("/in/a.txt").is_empty());
    }

    #[tokio::test]
    async fn allow_insecure_http_lets_any_host_through() {
        let h = Harness::with(|c| c.allow_insecure_http = true).await;
        let port = h.server.url("").rsplit(':').next().unwrap().to_owned();
        let elsewhere = |path: &str| format!("http://0.0.0.0:{port}{path}");
        h.server.serve("/in/a.txt", "a");
        let mut job = h.job(
            &probe(),
            steps(json!([
                {"input": "a.txt"},
                {"write": "b.txt", "text": "b"},
                {"emit": {"type": "result", "files": ["b.txt"]}},
            ])),
        );
        job.inputs.insert(
            "a.txt".parse().unwrap(),
            moil_proto::Download {
                url: elsewhere("/in/a.txt"),
                headers: Default::default(),
            },
        );
        let (name, mut upload) = output(&h.server, "b.txt");
        upload.url = elsewhere("/out/b.txt");
        job.outputs.insert(name, upload);
        h.run(job).await.result();
        assert_eq!(h.server.received("/out/b.txt")[0].body, b"b");
    }

    #[tokio::test]
    async fn redirects_are_held_to_the_url_policy() {
        let h = Harness::new().await;
        let port = h.server.url("").rsplit(':').next().unwrap().to_owned();
        h.server.serve("/in/real.txt", "real");
        h.server
            .reply("/in/a.txt", Reply::Redirect(h.server.url("/in/real.txt")));
        h.server.reply(
            "/in/b.txt",
            Reply::Redirect(format!("http://0.0.0.0:{port}/in/real.txt")),
        );

        let mut job = h.job(
            &probe(),
            steps(json!([{"input": "a.txt"}, {"emit": {"type": "result"}}])),
        );
        job.inputs.insert(
            "a.txt".parse().unwrap(),
            moil_proto::Download {
                url: h.server.url("/in/a.txt"),
                headers: Default::default(),
            },
        );
        let run = h.run(job).await;
        assert_eq!(run.data("input"), "real", "an allowed redirect is followed");

        let mut job = h.job(&probe(), steps(json!([{"emit": {"type": "result"}}])));
        job.inputs.insert(
            "b.txt".parse().unwrap(),
            moil_proto::Download {
                url: h.server.url("/in/b.txt"),
                headers: Default::default(),
            },
        );
        let run = h.run(job).await;
        assert_eq!(run.error().code, ErrorCode::InputDownload);
        assert!(!run.error().retryable);
        assert!(
            run.error().message.contains("redirected"),
            "{}",
            run.error().message
        );
        assert_eq!(h.server.received("/in/real.txt").len(), 1);
    }

    #[tokio::test]
    async fn a_remote_services_job_cant_reach_this_machine_over_http() {
        // A service that isn't on loopback aims a job's transfers at a
        // service that listens only on the owner's machine.
        let h = Harness::new().await;
        let policy =
            UrlPolicy::for_service(&"https://acme.example.com/moil".parse().unwrap(), false);
        h.server.serve("/local/secret", "the owner's");

        let mut job = h.job(&probe(), steps(json!([{"emit": {"type": "result"}}])));
        job.url_policy = policy;
        job.inputs.insert(
            "a.txt".parse().unwrap(),
            moil_proto::Download {
                url: h.server.url("/local/secret"),
                headers: Default::default(),
            },
        );
        let run = h.run(job).await;
        let error = run.error();
        assert_eq!(error.code, ErrorCode::InputDownload);
        assert!(!error.retryable);
        assert!(
            error
                .message
                .contains("on this machine, for a service that isn't"),
            "{}",
            error.message
        );

        let mut job = h.job(&probe(), steps(json!([{"emit": {"type": "result"}}])));
        job.url_policy = policy;
        let (name, mut upload) = output(&h.server, "b.txt");
        upload.url = h.server.url("/local/admin");
        job.outputs.insert(name, upload);
        let error = h.run(job).await.error().clone();
        assert_eq!(error.code, ErrorCode::OutputUpload);
        assert!(!error.retryable);

        // Nor through a redirect from a URL it may use.
        h.server
            .reply("/in/a.txt", Reply::Redirect(h.server.url("/local/secret")));
        let mut job = h.job(&probe(), steps(json!([{"emit": {"type": "result"}}])));
        job.url_policy = policy;
        job.inputs.insert(
            "a.txt".parse().unwrap(),
            moil_proto::Download {
                url: h.server.https_url("/in/a.txt"),
                headers: Default::default(),
            },
        );
        let run = h.run(job).await;
        let error = run.error();
        assert_eq!(error.code, ErrorCode::InputDownload);
        assert!(!error.retryable);
        assert!(
            error
                .message
                .contains("redirected to a URL that uses http to 127.0.0.1, on this machine"),
            "{}",
            error.message
        );
        assert_eq!(h.server.received("/in/a.txt").len(), 1);
        assert!(h.server.received("/local/secret").is_empty());
        assert!(h.server.received("/local/admin").is_empty());
    }

    #[tokio::test]
    async fn a_failure_quoting_the_service_stays_under_the_message_limit() {
        let h = Harness::new().await;
        let mut job = h.job(&probe(), steps(json!([{"emit": {"type": "result"}}])));
        job.inputs.insert(
            "a.txt".parse().unwrap(),
            moil_proto::Download {
                url: h.server.url("/in/a.txt"),
                headers: [("not a header ".repeat(70_000), "x".into())].into(),
            },
        );
        let error = h.run(job).await.error().clone();
        assert_eq!(error.code, ErrorCode::InputDownload);
        assert!(
            error.message.len() <= 64 << 10,
            "{} bytes",
            error.message.len()
        );
    }

    #[tokio::test]
    async fn an_upload_answered_with_a_redirect_fails() {
        // Following it would turn the upload into a GET, which could
        // succeed without uploading anything.
        let h = Harness::new().await;
        h.server.serve("/elsewhere", "fine");
        h.server
            .reply("/out/a.txt", Reply::SeeOther(h.server.url("/elsewhere")));
        let mut job = h.job(
            &probe(),
            steps(json!([
                {"write": "a.txt", "text": "alpha"},
                {"emit": {"type": "result", "files": ["a.txt"]}},
            ])),
        );
        job.outputs.extend([output(&h.server, "a.txt")]);
        let run = h.run(job).await;
        let error = run.error();
        assert_eq!(error.code, ErrorCode::OutputUpload);
        assert!(error.message.contains("303"), "{}", error.message);
        assert!(h.server.received("/elsewhere").is_empty());
    }
}
