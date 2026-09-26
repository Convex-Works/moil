//! The script's environment is minimal. A test binary of its own, because it
//! changes this process's environment, which is only sound while no other
//! thread runs.

mod common;

use common::*;
use serde_json::json;

#[test]
fn the_script_sees_nothing_of_the_machines_environment_but_the_basics() {
    // SAFETY: this is the binary's only test, and no other thread exists
    // yet: the async runtime starts below.
    unsafe {
        std::env::set_var("MOIL_TEST_SECRET", "hunter2");
        std::env::set_var("AWS_SECRET_ACCESS_KEY", "wJalrXUtnFEMI");
    }
    let parent: Vec<(String, String)> = std::env::vars().collect();

    tokio::runtime::Runtime::new().unwrap().block_on(async {
        let h = Harness::with(|c| c.extra_env.push(("MOIL_EXTRA".into(), "given".into()))).await;
        let run = h
            .run(h.job(
                &probe(),
                steps(json!([{"environ": true}, {"context": true}, {"emit": {"type": "result"}}])),
            ))
            .await;
        run.result();
        let env = run.data("environ");
        let work_dir = run.data("context")["work_dir"].as_str().unwrap().to_owned();

        assert_eq!(env.get("MOIL_TEST_SECRET"), None);
        assert_eq!(env.get("AWS_SECRET_ACCESS_KEY"), None);
        // Nothing else from this process got through either, apart from
        // the basics the spec lists, and a variable macOS itself sets in
        // every process that uses CoreFoundation.
        let passed = ["PATH", "HOME", "LANG", "TMPDIR", "__CF_USER_TEXT_ENCODING"];
        for (name, _) in &parent {
            assert!(
                passed.contains(&name.as_str()) || env.get(name).is_none(),
                "{name} leaked into the script's environment"
            );
        }
        assert_eq!(env["HOME"], std::env::var("HOME").unwrap());
        assert_eq!(env["TMPDIR"], format!("{work_dir}/tmp"));
        assert_eq!(env["PYTHONUNBUFFERED"], "1");
        assert_eq!(env["UV_NO_CONFIG"], "1");
        assert_eq!(env["UV_MANAGED_PYTHON"], "1");
        assert_eq!(env["MOIL_EXTRA"], "given");
        assert!(env["LANG"].as_str().is_some_and(|lang| !lang.is_empty()));
    });
}
