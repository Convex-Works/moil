//! A package index (PEP 503) on a server of its own, serving packages that
//! record whether they were built from source (see `tests/index`), and
//! bundles locked against it by real `uv lock`.

use std::ffi::OsString;
use std::path::Path;

use moil_bundle::Bundle;
use moil_runtime::Uv;

use super::{Reply, Server, sha256, uv, uv_cache_env};

/// What the index offers for a package, always at version 1.0.
#[derive(Debug, Clone, Copy)]
pub enum Offer {
    /// Only an sdist.
    Sdist,
    /// An sdist, and a wheel for a platform no test runs on.
    SdistAndForeignWheel,
}

pub struct Index {
    server: Server,
    /// Where building a package leaves a file named after it.
    built: tempfile::TempDir,
    /// Every file the index serves, by path.
    files: Vec<String>,
}

impl Index {
    /// Serves `packages`, each built afresh.
    pub async fn start(packages: &[(&str, Offer)]) -> Self {
        let server = Server::start().await;
        let built = tempfile::tempdir().unwrap();
        let out = tempfile::tempdir().unwrap();
        let script = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/index/packages.py");
        let status = Uv::new(uv())
            .with_env(uv_cache_env())
            .command()
            .args(["run", "--no-project", "python"])
            .arg(script)
            .arg(out.path())
            .arg(built.path())
            .args(packages.iter().map(|(name, offer)| match offer {
                Offer::Sdist => format!("{name}=sdist"),
                Offer::SdistAndForeignWheel => format!("{name}=sdist+foreign-wheel"),
            }))
            .status()
            .await
            .unwrap();
        assert!(status.success(), "couldn't build the index's packages");

        let mut files = Vec::new();
        for (name, _) in packages {
            let mut links = String::new();
            for entry in std::fs::read_dir(out.path()).unwrap() {
                let file_name = entry.unwrap().file_name().into_string().unwrap();
                if !file_name.starts_with(&format!("{name}-1.0")) {
                    continue;
                }
                let bytes = std::fs::read(out.path().join(&file_name)).unwrap();
                let path = format!("/files/{file_name}");
                let url = server.https_url(&path);
                links.push_str(&format!(
                    "<a href=\"{url}#sha256={}\">{file_name}</a>\n",
                    sha256(&bytes)
                ));
                server.serve(&path, bytes);
                files.push(path);
            }
            server.reply(
                &format!("/simple/{name}/"),
                Reply::Html(format!(
                    "<!DOCTYPE html>\n<html><body>\n{links}</body></html>\n"
                )),
            );
        }
        Self {
            server,
            built,
            files,
        }
    }

    /// The variable that makes uv accept the index's certificate, which a
    /// throwaway CA issued. Trusting that CA instead would mean uv trusting
    /// nothing else.
    pub fn uv_env(&self) -> (OsString, OsString) {
        let host = self.server.https_url("").replace("https://", "");
        ("UV_INSECURE_HOST".into(), host.into())
    }

    /// Whether package `name` was built from its sdist.
    pub fn was_built(&self, name: &str) -> bool {
        self.built.path().join(name).exists()
    }

    /// Makes every download of a package file fail with `503`.
    pub fn go_down(&self) {
        for path in &self.files {
            self.server.reply(path, Reply::Status(503));
        }
    }

    /// A bundle whose script depends on `names` from this index, locked
    /// with `uv lock`. Its result's meta maps each name to its `VALUE`.
    pub async fn bundle(&self, names: &[&str]) -> Bundle {
        let dir = tempfile::tempdir().unwrap();
        let dependencies: Vec<String> = names.iter().map(|n| format!("\"{n}==1.0\"")).collect();
        let script = format!(
            r#"# /// script
# requires-python = ">=3.12"
# dependencies = [{dependencies}]
#
# [[tool.uv.index]]
# url = "{index}"
# ///
"""Reports the VALUE of each package it depends on."""

import importlib
import json

values = {{name: importlib.import_module(name).VALUE for name in {names:?}}}
print(json.dumps({{"type": "result", "meta": values}}))
"#,
            dependencies = dependencies.join(", "),
            index = self.server.https_url("/simple"),
        );
        std::fs::write(dir.path().join("job.py"), &script).unwrap();
        // A cache of its own, so that nothing uv fetches to lock reaches
        // the runtime's cache and hides what the runtime fetches.
        let cache = tempfile::tempdir().unwrap();
        let output = Uv::new(uv())
            .with_env([("UV_CACHE_DIR".into(), cache.path().into()), self.uv_env()])
            .command()
            .args(["lock", "--script", "job.py"])
            .current_dir(dir.path())
            .output()
            .await
            .unwrap();
        assert!(
            output.status.success(),
            "uv couldn't lock the bundle: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let lockfile = std::fs::read_to_string(dir.path().join("job.py.lock")).unwrap();
        let manifest = r#"{"moil": 1, "name": "indexed", "version": "1"}"#;
        Bundle::new(script, lockfile, manifest).unwrap()
    }
}
