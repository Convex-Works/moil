//! Attempts end to end: real uv, real Python, real HTTP.

mod common;

use std::time::{Duration, Instant};

use common::*;
use moil_proto::{ErrorCode, FileInfo, JobEvent, LogLevel, Outcome, Phase, UploadMethod};
use moil_runtime::StopReason;
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
        assert!(error.message.contains("out/b.txt"), "{}", error.message);
        assert!(
            h.server.received("/out/a.txt").is_empty(),
            "nothing is uploaded"
        );
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
        let h = Harness::with(|c| c.kill_grace = Duration::from_secs(1)).await;
        let mut job = h.job(
            &probe(),
            steps(json!([
                {"on_sigterm": "terminating", "linger": 60},
                {"emit": {"type": "result"}},
                {"sleep": 60},
            ])),
        );
        job.timeout = Some(Duration::from_secs(1));
        let mut running = h.start(job);
        // The time limit passed and the script is in its grace period,
        // cleaning up, when the service cancels: rule 1 beats rule 3.
        running.log("terminating").await;
        running.stop(StopReason::Cancelled);
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
        let h = Harness::new().await;
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
