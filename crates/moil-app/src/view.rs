//! What the tray shows, as plain data: [`TrayView::new`] turns the agent's
//! [`Snapshot`] and the home's report into the menu, the icon's state and
//! the title beside it. The `tray` module makes it real; everything here is
//! pure.

use std::collections::HashSet;
use std::time::{Duration, Instant, SystemTime};

use moil_agent::report::{HookReport, Problem};
use moil_agent::{
    Connection, JobOutcome, JobReport, JobStatus, ServiceId, ServiceStatus, Snapshot,
};
use moil_proto::{BundleHash, Phase};
use url::Url;

use crate::format;
use crate::reports::HomeReport;

/// The most problems the menu names; `moil status` has them all.
const MAX_PROBLEMS: usize = 8;

/// Everything the tray shows.
#[derive(Debug, Clone, PartialEq)]
pub struct TrayView {
    pub icon: IconState,
    /// Beside the icon: the running job's progress, always as wide.
    pub title: Option<String>,
    pub tooltip: String,
    pub menu: MenuModel,
    /// When the view goes stale without a new snapshot, such as a
    /// reconnection countdown ticking or the last job growing older.
    pub refresh_at: Option<Instant>,
}

/// What the view depends on besides the snapshot.
#[derive(Debug, Clone)]
pub struct ViewContext<'a> {
    pub now: Instant,
    /// The time of day `now` is, for saying how long ago things happened.
    pub clock: SystemTime,
    /// Whether moil opens at login; `None` where that can't be changed.
    pub open_at_login: Option<bool>,
    /// Whether the app keeps a log file to show the owner.
    pub has_log: bool,
    /// The app is shutting the agent down.
    pub quitting: bool,
    /// Reviews the owner put off with "Not now". They stay in the menu but
    /// no longer ask for attention.
    pub deferred: &'a HashSet<(ServiceId, BundleHash)>,
    /// The home's hooks and problems, which the snapshot doesn't have.
    pub home: &'a HomeReport,
}

/// The icon: what the machine is doing, and whether the owner is needed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct IconState {
    pub activity: Activity,
    /// A bundle waits for review, a service needs pairing again, or
    /// something in the home needs fixing.
    pub attention: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Activity {
    Idle,
    /// A job runs. It carries on while paused, so this wins over paused.
    Running,
    Paused,
}

/// The tray menu.
#[derive(Debug, Clone, PartialEq)]
pub struct MenuModel {
    pub entries: Vec<Entry>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Entry {
    /// Text that can't be clicked, such as the status line.
    Text(Label),
    /// Something to do, unless disabled for now.
    Item {
        label: Label,
        action: Action,
        enabled: bool,
    },
    /// A setting that is on or off. Disabled where it can't be changed.
    Check {
        label: Label,
        checked: bool,
        enabled: bool,
        action: Action,
    },
    Submenu {
        label: Label,
        entries: Vec<Entry>,
    },
    Separator,
}

/// A menu label: `text`, then an optional de-emphasized `detail`. Either
/// may be empty.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Label {
    pub text: String,
    pub detail: Option<String>,
}

/// What a menu item does.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Action {
    StopJob,
    SetPaused(bool),
    Pair,
    /// Pair with a service that removed this machine, at its address.
    PairAgain(Url),
    Review {
        service: ServiceId,
        hash: BundleHash,
    },
    Unpair(ServiceId),
    ToggleOpenAtLogin,
    /// Open the home in Finder.
    OpenHome,
    /// Show the log file in Finder.
    ShowLog,
    Quit,
}

impl Label {
    pub fn new(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            detail: None,
        }
    }

    /// A label that is all detail, for secondary information.
    pub fn quiet(detail: impl Into<String>) -> Self {
        Self {
            text: String::new(),
            detail: Some(detail.into()),
        }
    }

    pub fn with_detail(mut self, detail: impl Into<String>) -> Self {
        self.detail = Some(detail.into());
        self
    }
}

impl Entry {
    /// An item that can be clicked.
    pub fn item(label: Label, action: Action) -> Self {
        Self::Item {
            label,
            action,
            enabled: true,
        }
    }
}

impl std::fmt::Display for Label {
    /// The label as one line of text: `text — detail`.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match (&*self.text, &self.detail) {
            (text, None) => f.write_str(text),
            ("", Some(detail)) => f.write_str(detail),
            (text, Some(detail)) => write!(f, "{text} — {detail}"),
        }
    }
}

impl MenuModel {
    /// Whether `other` has the same items doing the same things, so that
    /// updating labels in place turns this menu into `other`. Progress
    /// ticks keep the shape; a new review or service changes it.
    pub fn same_shape(&self, other: &MenuModel) -> bool {
        fn same(a: &[Entry], b: &[Entry]) -> bool {
            a.len() == b.len()
                && a.iter().zip(b).all(|pair| match pair {
                    (Entry::Text(_), Entry::Text(_)) | (Entry::Separator, Entry::Separator) => true,
                    (Entry::Item { action: a, .. }, Entry::Item { action: b, .. })
                    | (Entry::Check { action: a, .. }, Entry::Check { action: b, .. }) => a == b,
                    (Entry::Submenu { entries: a, .. }, Entry::Submenu { entries: b, .. }) => {
                        same(a, b)
                    }
                    _ => false,
                })
        }
        same(&self.entries, &other.entries)
    }

    /// Every entry, submenus' entries right after the submenu, in order.
    pub fn flatten(&self) -> Vec<&Entry> {
        fn walk<'a>(entries: &'a [Entry], out: &mut Vec<&'a Entry>) {
            for entry in entries {
                out.push(entry);
                if let Entry::Submenu { entries, .. } = entry {
                    walk(entries, out);
                }
            }
        }
        let mut out = Vec::new();
        walk(&self.entries, &mut out);
        out
    }

    /// The action of the item labeled `text`.
    #[cfg(test)]
    pub fn action(&self, text: &str) -> Option<&Action> {
        self.flatten().into_iter().find_map(|entry| match entry {
            Entry::Item { label, action, .. } | Entry::Check { label, action, .. }
                if label.to_string() == text =>
            {
                Some(action)
            }
            _ => None,
        })
    }
}

impl TrayView {
    pub fn new(snapshot: &Snapshot, cx: &ViewContext<'_>) -> Self {
        let status = status(snapshot);
        Self {
            icon: IconState::new(snapshot, cx),
            title: snapshot
                .job
                .as_ref()
                .filter(|job| !job.stopping)
                .and_then(progress)
                .map(format::fixed_percent),
            tooltip: format!("moil: {status}"),
            menu: menu(snapshot, cx, status),
            refresh_at: refresh_at(snapshot, cx),
        }
    }
}

impl IconState {
    pub fn new(snapshot: &Snapshot, cx: &ViewContext<'_>) -> Self {
        let activity = match (&snapshot.job, snapshot.paused) {
            (Some(_), _) => Activity::Running,
            (None, true) => Activity::Paused,
            (None, false) => Activity::Idle,
        };
        let review_waiting = snapshot.pending_reviews().any(|(service, bundle)| {
            !cx.deferred
                .contains(&(service.id.clone(), bundle.info.hash))
        });
        let service_lost = snapshot
            .services
            .iter()
            .any(|s| matches!(s.connection, Connection::Unpaired | Connection::Replaced));
        Self {
            activity,
            attention: review_waiting || service_lost || !cx.home.problems.is_empty(),
        }
    }
}

/// The running job's progress in whole percent, once the script reports
/// any.
fn progress(job: &JobStatus) -> Option<u8> {
    match job.phase {
        Phase::Running => job.fraction.and_then(format::percent),
        _ => None,
    }
}

/// A job as the owner knows it: its title, else its bundle's name.
fn job_name(title: Option<&str>, bundle_name: &str) -> String {
    format::one_line(title.unwrap_or(bundle_name), 48)
}

/// What a service is called in the menu, or its ID once it's gone.
fn service_name(snapshot: &Snapshot, id: &ServiceId) -> String {
    let name = snapshot.service(id).map_or(id.as_str(), |s| &s.name);
    format::one_line(name, 32)
}

/// The first line of the menu.
fn status(snapshot: &Snapshot) -> Label {
    match &snapshot.job {
        Some(job) => {
            let what = job_name(job.title.as_deref(), &job.bundle_name);
            let service = service_name(snapshot, &job.service);
            if job.stopping {
                return Label::new(format!("Stopping “{what}” for {service}…"));
            }
            let label = Label::new(format!("Running “{what}” for {service}"));
            match (job.phase, progress(job)) {
                (_, Some(percent)) => label.with_detail(format!("{percent}%")),
                (Phase::Preparing, _) => label.with_detail("preparing"),
                (Phase::Downloading, _) => label.with_detail("downloading inputs"),
                (Phase::Uploading, _) => label.with_detail("uploading results"),
                (Phase::Running, None) => label,
            }
        }
        None if snapshot.paused => Label::new("Paused").with_detail("taking no new jobs"),
        None if snapshot.services.is_empty() => Label::new("Idle"),
        None => Label::new("Idle").with_detail("waiting for jobs"),
    }
}

/// How the job that ran last ended, and when.
fn last_job(snapshot: &Snapshot, report: &JobReport, clock: SystemTime) -> Label {
    let what = job_name(report.title.as_deref(), &report.bundle_name);
    let when = format::ago(report.ended_at, clock);
    match &report.outcome {
        JobOutcome::Succeeded => Label::new(format!("Last: “{what}” finished {when}")),
        JobOutcome::Failed { code, message } => {
            let reason = match format::one_line(message, 64) {
                reason if reason.trim().is_empty() => code.to_string(),
                reason => reason,
            };
            Label::new(format!("Last: “{what}” failed {when}")).with_detail(reason)
        }
        JobOutcome::Cancelled => {
            let service = service_name(snapshot, &report.service);
            Label::new(format!("Last: “{what}” was cancelled by {service} {when}"))
        }
        JobOutcome::Interrupted => Label::new(format!("Last: “{what}” was stopped {when}")),
    }
}

fn menu(snapshot: &Snapshot, cx: &ViewContext<'_>, status: Label) -> MenuModel {
    use Entry::*;
    if cx.quitting {
        let what = if snapshot.job.is_some() {
            "Stopping the job and quitting…"
        } else {
            "Quitting…"
        };
        return MenuModel {
            entries: vec![Text(Label::new(what))],
        };
    }

    let mut entries = vec![Text(status)];
    if let Some(job) = &snapshot.job {
        let mut detail = format::one_line(&job.bundle_name, 32);
        if let Some(message) = &job.message {
            detail = format!("{detail}: {}", format::one_line(message, 48));
        }
        entries.push(Text(Label::quiet(detail)));
        if snapshot.paused {
            entries.push(Text(Label::quiet("Paused: no new jobs after this one")));
        }
        entries.push(Item {
            label: Label::new("Stop Current Job"),
            action: Action::StopJob,
            enabled: !job.stopping,
        });
    }
    if let Some(report) = &snapshot.last_job {
        entries.push(Text(last_job(snapshot, report, cx.clock)));
    }
    let problems = &cx.home.problems;
    if !problems.is_empty() {
        entries.push(Submenu {
            label: Label::new(format!(
                "{} in moil's home",
                format::count(problems.len(), "problem")
            )),
            entries: problems_menu(problems),
        });
    }

    entries.push(Separator);
    if snapshot.services.is_empty() {
        entries.push(Text(Label::quiet("Not paired with any service")));
    }
    for (i, service) in snapshot.services.iter().enumerate() {
        if i > 0 {
            entries.push(Separator);
        }
        entries.push(Submenu {
            label: Label::new(format::one_line(&service.name, 32))
                .with_detail(host(&service.base_url)),
            entries: service_menu(service, cx.home.hooks(&service.id), cx.now),
        });
        entries.push(Text(Label::quiet(connection(&service.connection, cx.now))));
        for bundle in service.bundles.iter().filter(|b| !b.approved) {
            entries.push(Entry::item(
                Label::new(format!(
                    "Review {} {}…",
                    format::one_line(&bundle.info.name, 32),
                    format::one_line(&bundle.info.version, 16)
                )),
                Action::Review {
                    service: service.id.clone(),
                    hash: bundle.info.hash,
                },
            ));
        }
        if service.connection == Connection::Unpaired {
            entries.push(Entry::item(
                Label::new(format!(
                    "Pair with {} Again…",
                    format::one_line(&service.name, 32)
                )),
                Action::PairAgain(service.base_url.clone()),
            ));
        }
    }

    entries.push(Separator);
    entries.push(Entry::item(
        Label::new(if snapshot.paused { "Resume" } else { "Pause" }),
        Action::SetPaused(!snapshot.paused),
    ));
    entries.push(Entry::item(
        Label::new("Pair with a Service…"),
        Action::Pair,
    ));
    entries.push(Separator);
    entries.push(Check {
        label: Label::new("Open at Login"),
        checked: cx.open_at_login == Some(true),
        enabled: cx.open_at_login.is_some(),
        action: Action::ToggleOpenAtLogin,
    });
    entries.push(Entry::item(
        Label::new("Open moil Folder"),
        Action::OpenHome,
    ));
    if cx.has_log {
        entries.push(Entry::item(
            Label::new("Show Log in Finder"),
            Action::ShowLog,
        ));
    }
    entries.push(Entry::item(Label::new("Quit moil"), Action::Quit));
    MenuModel { entries }
}

/// What's wrong with the home, in brief: the files concerned, and where to
/// read what to do about them.
fn problems_menu(problems: &[Problem]) -> Vec<Entry> {
    use Entry::*;
    let mut entries: Vec<Entry> = problems
        .iter()
        .take(MAX_PROBLEMS)
        .map(|p| Text(Label::new(format::one_line(&p.path.to_string_lossy(), 80))))
        .collect();
    if problems.len() > MAX_PROBLEMS {
        let more = problems.len() - MAX_PROBLEMS;
        entries.push(Text(Label::quiet(format!("and {more} more"))));
    }
    entries.push(Separator);
    entries.push(Text(Label::quiet("Run “moil status” for details")));
    entries.push(Entry::item(
        Label::new("Open moil Folder"),
        Action::OpenHome,
    ));
    entries
}

/// A service's submenu: where it is, its bundles, its hooks, and unpairing.
fn service_menu(service: &ServiceStatus, hooks: &[HookReport], now: Instant) -> Vec<Entry> {
    use Entry::*;
    let mut entries = vec![
        Text(Label::quiet(format::one_line(
            service.base_url.as_str(),
            64,
        ))),
        Text(Label::new(connection(&service.connection, now))),
    ];
    if service.insecure {
        entries.push(Text(Label::quiet("Unencrypted connection (http)")));
    }
    entries.push(Separator);
    if service.bundles.is_empty() {
        entries.push(Text(Label::quiet("Offers no job bundles")));
    }
    for bundle in &service.bundles {
        let name = format!(
            "{} {}",
            format::one_line(&bundle.info.name, 32),
            format::one_line(&bundle.info.version, 16)
        );
        entries.push(Entry::item(
            if bundle.approved {
                Label::new(name).with_detail("approved")
            } else {
                Label::new(format!("Review {name}…"))
            },
            Action::Review {
                service: service.id.clone(),
                hash: bundle.info.hash,
            },
        ));
    }
    if !hooks.is_empty() {
        entries.push(Separator);
    }
    for hook in hooks {
        let label = format!("Hook for {}", format::one_line(&hook.bundle, 32));
        entries.push(Text(if hook.runnable {
            Label::quiet(label)
        } else {
            Label::new(label).with_detail("moil can't run it")
        }));
    }
    entries.push(Separator);
    entries.push(Entry::item(
        Label::new(format!("Unpair {}…", format::one_line(&service.name, 32))),
        Action::Unpair(service.id.clone()),
    ));
    entries
}

fn connection(connection: &Connection, now: Instant) -> String {
    match connection {
        Connection::Connecting => "Connecting…".into(),
        Connection::Connected => "Connected".into(),
        Connection::Disconnected { error, retry_at } => {
            let error = format::one_line(error, 48);
            match retry_at.checked_duration_since(now) {
                Some(wait) if !wait.is_zero() => {
                    format!("Offline: {error}. Retrying in {}", format::wait(wait))
                }
                _ => format!("Offline: {error}. Retrying now"),
            }
        }
        Connection::Unpaired => "Removed this Mac; pair again to use it".into(),
        Connection::Replaced => "Taken over by another connection using this Mac's token".into(),
    }
}

/// When a countdown to the next reconnection shows a different number, or
/// the last job's age may read differently.
fn refresh_at(snapshot: &Snapshot, cx: &ViewContext<'_>) -> Option<Instant> {
    let countdowns = snapshot
        .services
        .iter()
        .filter_map(|s| match s.connection {
            Connection::Disconnected { retry_at, .. } => retry_at.checked_duration_since(cx.now),
            _ => None,
        })
        .filter(|wait| !wait.is_zero())
        .map(|wait| {
            let tick = wait.subsec_nanos();
            if tick == 0 {
                Duration::from_secs(1)
            } else {
                Duration::from_nanos(tick.into())
            }
        });
    let last_job = snapshot
        .last_job
        .as_ref()
        .map(|report| format::ago_changes_in(report.ended_at, cx.clock));
    countdowns.chain(last_job).min().map(|wait| cx.now + wait)
}

/// The host (and port, if unusual) the owner would recognize.
fn host(url: &Url) -> String {
    match (url.host_str(), url.port()) {
        (Some(host), Some(port)) => format!("{host}:{port}"),
        (Some(host), None) => host.to_owned(),
        _ => url.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use moil_agent::BundleStatus;
    use moil_proto::{BundleInfo, ErrorCode, JobId};

    use super::*;

    fn hash(c: char) -> BundleHash {
        c.to_string().repeat(64).parse().unwrap()
    }

    fn bundle(name: &str, version: &str, h: char, approved: bool) -> BundleStatus {
        BundleStatus {
            info: BundleInfo {
                hash: hash(h),
                name: name.into(),
                version: version.into(),
                description: None,
            },
            approved,
        }
    }

    fn klisi() -> ServiceStatus {
        ServiceStatus {
            id: "klisi.example.com".parse().unwrap(),
            name: "klisi".into(),
            base_url: "https://klisi.example.com/moil".parse().unwrap(),
            connection: Connection::Connected,
            bundles: vec![
                bundle("transcribe", "0.2.0", 'a', true),
                bundle("transcribe", "0.3.0", 'b', false),
            ],
            insecure: false,
        }
    }

    fn lab(now: Instant) -> ServiceStatus {
        ServiceStatus {
            id: "localhost-8787".parse().unwrap(),
            name: "lab".into(),
            base_url: "http://localhost:8787/moil".parse().unwrap(),
            connection: Connection::Disconnected {
                error: "connection refused".into(),
                retry_at: now + Duration::from_millis(11_400),
            },
            bundles: vec![],
            insecure: true,
        }
    }

    fn job(phase: Phase, fraction: Option<f64>) -> JobStatus {
        JobStatus {
            service: "klisi.example.com".parse().unwrap(),
            job_id: JobId::new("j_42").unwrap(),
            attempt: 1,
            title: Some("Weekly sync".into()),
            bundle_name: "transcribe".into(),
            phase,
            fraction,
            message: Some("diarizing".into()),
            started_at: Instant::now(),
            stopping: false,
        }
    }

    /// How "Design review" ended, `ago` before `clock`.
    fn report(outcome: JobOutcome, clock: SystemTime, ago: Duration) -> JobReport {
        JobReport {
            service: "klisi.example.com".parse().unwrap(),
            job_id: JobId::new("j_41").unwrap(),
            attempt: 2,
            title: Some("Design review".into()),
            bundle_name: "transcribe".into(),
            outcome,
            ended_at: clock - ago,
        }
    }

    fn hook(bundle: &str, runnable: bool) -> HookReport {
        let service = "/Users/ada/.moil/services/klisi.example.com";
        HookReport {
            bundle: bundle.into(),
            path: format!("{service}/hooks/{bundle}").into(),
            log: format!("{service}/logs/{bundle}.log").into(),
            runnable,
        }
    }

    fn problem(path: &str) -> Problem {
        Problem {
            path: path.into(),
            message: "moil can't run this hook: it isn't executable. Run chmod +x on it.".into(),
        }
    }

    fn out_of_memory() -> JobOutcome {
        JobOutcome::Failed {
            code: ErrorCode::ScriptError,
            message: "CUDA out of memory".into(),
        }
    }

    struct Fixture {
        now: Instant,
        clock: SystemTime,
        deferred: HashSet<(ServiceId, BundleHash)>,
        open_at_login: Option<bool>,
        has_log: bool,
        quitting: bool,
        home: HomeReport,
    }

    impl Fixture {
        fn new() -> Self {
            Self {
                now: Instant::now(),
                clock: SystemTime::UNIX_EPOCH + Duration::from_secs(1_800_000_000),
                deferred: HashSet::new(),
                open_at_login: Some(true),
                has_log: true,
                quitting: false,
                home: HomeReport::default(),
            }
        }

        fn view(&self, snapshot: &Snapshot) -> TrayView {
            TrayView::new(
                snapshot,
                &ViewContext {
                    now: self.now,
                    clock: self.clock,
                    open_at_login: self.open_at_login,
                    has_log: self.has_log,
                    quitting: self.quitting,
                    deferred: &self.deferred,
                    home: &self.home,
                },
            )
        }
    }

    fn lines(menu: &MenuModel) -> Vec<String> {
        fn walk(entries: &[Entry], depth: usize, out: &mut Vec<String>) {
            for entry in entries {
                let indent = "  ".repeat(depth);
                match entry {
                    Entry::Text(label) => out.push(format!("{indent}{label}")),
                    Entry::Item { label, enabled, .. } => out.push(format!(
                        "{indent}[{label}]{}",
                        if *enabled { "" } else { " (disabled)" }
                    )),
                    Entry::Check {
                        label,
                        checked,
                        enabled,
                        ..
                    } => out.push(format!(
                        "{indent}[{}{label}]{}",
                        if *checked { "✓ " } else { "" },
                        if *enabled { "" } else { " (disabled)" }
                    )),
                    Entry::Submenu { label, entries } => {
                        out.push(format!("{indent}{label} ▸"));
                        walk(entries, depth + 1, out);
                    }
                    Entry::Separator => out.push(format!("{indent}---")),
                }
            }
        }
        let mut out = Vec::new();
        walk(&menu.entries, 0, &mut out);
        out
    }

    #[test]
    fn the_menu_lays_out_status_services_and_controls() {
        let fx = Fixture::new();
        let snapshot = Snapshot {
            paused: false,
            services: vec![klisi(), lab(fx.now)],
            job: Some(job(Phase::Running, Some(0.42))),
            last_job: Some(report(out_of_memory(), fx.clock, Duration::from_secs(300))),
        };
        assert_eq!(
            lines(&fx.view(&snapshot).menu),
            [
                "Running “Weekly sync” for klisi — 42%",
                "transcribe: diarizing",
                "[Stop Current Job]",
                "Last: “Design review” failed 5 minutes ago — CUDA out of memory",
                "---",
                "klisi — klisi.example.com ▸",
                "  https://klisi.example.com/moil",
                "  Connected",
                "  ---",
                "  [transcribe 0.2.0 — approved]",
                "  [Review transcribe 0.3.0…]",
                "  ---",
                "  [Unpair klisi…]",
                "Connected",
                "[Review transcribe 0.3.0…]",
                "---",
                "lab — localhost:8787 ▸",
                "  http://localhost:8787/moil",
                "  Offline: connection refused. Retrying in 12s",
                "  Unencrypted connection (http)",
                "  ---",
                "  Offers no job bundles",
                "  ---",
                "  [Unpair lab…]",
                "Offline: connection refused. Retrying in 12s",
                "---",
                "[Pause]",
                "[Pair with a Service…]",
                "---",
                "[✓ Open at Login]",
                "[Open moil Folder]",
                "[Show Log in Finder]",
                "[Quit moil]",
            ]
        );
    }

    #[test]
    fn hooks_show_under_their_service_and_problems_in_brief() {
        let mut fx = Fixture::new();
        let klisi_id: ServiceId = "klisi.example.com".parse().unwrap();
        fx.home.hooks.insert(
            klisi_id,
            vec![hook("summarize", false), hook("transcribe", true)],
        );
        fx.home.problems = vec![
            problem("services/klisi.example.com/hooks/summarize"),
            problem("services/stray.txt"),
        ];
        let snapshot = Snapshot {
            services: vec![klisi(), lab(fx.now)],
            last_job: Some(report(
                JobOutcome::Succeeded,
                fx.clock,
                Duration::from_secs(300),
            )),
            ..Snapshot::default()
        };
        let lines = lines(&fx.view(&snapshot).menu);
        let first_service = lines.iter().position(|l| l.starts_with("klisi")).unwrap();
        assert_eq!(
            lines[..first_service],
            [
                "Idle — waiting for jobs",
                "Last: “Design review” finished 5 minutes ago",
                "2 problems in moil's home ▸",
                "  services/klisi.example.com/hooks/summarize",
                "  services/stray.txt",
                "  ---",
                "  Run “moil status” for details",
                "  [Open moil Folder]",
                "---",
            ]
        );
        assert_eq!(
            lines[first_service..first_service + 11],
            [
                "klisi — klisi.example.com ▸",
                "  https://klisi.example.com/moil",
                "  Connected",
                "  ---",
                "  [transcribe 0.2.0 — approved]",
                "  [Review transcribe 0.3.0…]",
                "  ---",
                "  Hook for summarize — moil can't run it",
                "  Hook for transcribe",
                "  ---",
                "  [Unpair klisi…]",
            ]
        );
        let lab_menu: Vec<_> = lines
            .iter()
            .skip_while(|l| !l.starts_with("lab"))
            .take_while(|l| !l.contains("Unpair lab"))
            .collect();
        assert!(
            !lab_menu.iter().any(|l| l.contains("Hook")),
            "{lab_menu:#?}"
        );
    }

    #[test]
    fn a_long_list_of_problems_is_cut_short() {
        let mut fx = Fixture::new();
        fx.home.problems = (0..12)
            .map(|i| problem(&format!("services/s{i}/service.json")))
            .collect();
        let lines = lines(&fx.view(&Snapshot::default()).menu);
        assert_eq!(lines[1], "12 problems in moil's home ▸");
        assert_eq!(lines[9], "  services/s7/service.json");
        assert_eq!(lines[10], "  and 4 more");
        assert_eq!(lines[12], "  Run “moil status” for details");
    }

    #[test]
    fn items_do_what_they_say() {
        let fx = Fixture::new();
        let snapshot = Snapshot {
            paused: true,
            services: vec![klisi()],
            job: None,
            last_job: None,
        };
        let menu = fx.view(&snapshot).menu;
        assert_eq!(
            menu.action("Review transcribe 0.3.0…"),
            Some(&Action::Review {
                service: "klisi.example.com".parse().unwrap(),
                hash: hash('b')
            })
        );
        assert_eq!(
            menu.action("transcribe 0.2.0 — approved"),
            Some(&Action::Review {
                service: "klisi.example.com".parse().unwrap(),
                hash: hash('a')
            })
        );
        assert_eq!(menu.action("Resume"), Some(&Action::SetPaused(false)));
        assert_eq!(menu.action("Pause"), None);
        assert_eq!(menu.action("Stop Current Job"), None, "no job, no stop");
        assert_eq!(
            menu.action("Unpair klisi…"),
            Some(&Action::Unpair("klisi.example.com".parse().unwrap()))
        );
        assert_eq!(menu.action("Open moil Folder"), Some(&Action::OpenHome));
        assert_eq!(menu.action("Show Log in Finder"), Some(&Action::ShowLog));
        assert_eq!(menu.action("Quit moil"), Some(&Action::Quit));
    }

    #[test]
    fn the_log_is_offered_only_when_there_is_one() {
        let mut fx = Fixture::new();
        fx.has_log = false;
        let menu = fx.view(&Snapshot::default()).menu;
        assert_eq!(menu.action("Show Log in Finder"), None);
        assert_eq!(menu.action("Quit moil"), Some(&Action::Quit));
    }

    #[test]
    fn the_status_line_follows_the_job() {
        let fx = Fixture::new();
        let status = |paused, job| {
            lines(
                &fx.view(&Snapshot {
                    paused,
                    services: vec![klisi()],
                    job,
                    last_job: None,
                })
                .menu,
            )[0]
            .clone()
        };
        assert_eq!(status(false, None), "Idle — waiting for jobs");
        assert_eq!(status(true, None), "Paused — taking no new jobs");
        let running = |phase, fraction| status(false, Some(job(phase, fraction)));
        assert_eq!(
            running(Phase::Preparing, None),
            "Running “Weekly sync” for klisi — preparing"
        );
        assert_eq!(
            running(Phase::Downloading, None),
            "Running “Weekly sync” for klisi — downloading inputs"
        );
        assert_eq!(
            running(Phase::Running, None),
            "Running “Weekly sync” for klisi"
        );
        assert_eq!(
            running(Phase::Running, Some(0.999)),
            "Running “Weekly sync” for klisi — 99%"
        );
        assert_eq!(
            running(Phase::Uploading, Some(1.0)),
            "Running “Weekly sync” for klisi — uploading results"
        );

        let mut untitled = job(Phase::Running, Some(0.1));
        untitled.title = None;
        assert_eq!(
            status(false, Some(untitled)),
            "Running “transcribe” for klisi — 10%"
        );

        let paused = fx.view(&Snapshot {
            paused: true,
            services: vec![klisi()],
            job: Some(job(Phase::Running, Some(0.5))),
            last_job: None,
        });
        assert!(lines(&paused.menu).contains(&"Paused: no new jobs after this one".to_owned()));
        assert_eq!(
            paused.icon.activity,
            Activity::Running,
            "the job carries on"
        );
    }

    #[test]
    fn the_title_shows_progress_only_while_the_script_runs() {
        let fx = Fixture::new();
        let title = |job| {
            fx.view(&Snapshot {
                paused: false,
                services: vec![],
                job,
                last_job: None,
            })
            .title
        };
        assert_eq!(
            title(Some(job(Phase::Running, Some(0.42)))),
            Some("\u{2007}42%".into()),
            "as wide as 100%"
        );
        assert_eq!(
            title(Some(job(Phase::Running, Some(0.07)))),
            Some("\u{2007}\u{2007}7%".into())
        );
        assert_eq!(title(Some(job(Phase::Running, None))), None);
        assert_eq!(title(Some(job(Phase::Preparing, None))), None);
        assert_eq!(title(None), None);
    }

    #[test]
    fn the_last_job_says_how_it_ended() {
        let fx = Fixture::new();
        let line = |outcome, ago: u64| {
            let snapshot = Snapshot {
                services: vec![klisi()],
                last_job: Some(report(outcome, fx.clock, Duration::from_secs(ago))),
                ..Snapshot::default()
            };
            lines(&fx.view(&snapshot).menu)[1].clone()
        };
        assert_eq!(
            line(out_of_memory(), 5),
            "Last: “Design review” failed just now — CUDA out of memory"
        );
        assert_eq!(
            line(JobOutcome::Succeeded, 2 * 3600),
            "Last: “Design review” finished 2 hours ago"
        );
        assert_eq!(
            line(JobOutcome::Cancelled, 60),
            "Last: “Design review” was cancelled by klisi 1 minute ago"
        );
        assert_eq!(
            line(JobOutcome::Interrupted, 60),
            "Last: “Design review” was stopped 1 minute ago"
        );
        let silent = JobOutcome::Failed {
            code: ErrorCode::NoResult,
            message: " \n".into(),
        };
        assert_eq!(
            line(silent, 5),
            "Last: “Design review” failed just now — no_result",
            "an empty message still says something"
        );

        let untitled = Snapshot {
            last_job: Some(JobReport {
                title: None,
                ..report(JobOutcome::Succeeded, fx.clock, Duration::ZERO)
            }),
            ..Snapshot::default()
        };
        assert_eq!(
            lines(&fx.view(&untitled).menu)[1],
            "Last: “transcribe” finished just now"
        );
        assert!(
            !lines(&fx.view(&Snapshot::default()).menu)
                .iter()
                .any(|l| l.starts_with("Last:")),
            "nothing ran yet"
        );
    }

    #[test]
    fn a_long_failure_message_stays_on_one_short_line() {
        let fx = Fixture::new();
        let rambling = JobOutcome::Failed {
            code: ErrorCode::ScriptError,
            message: format!(
                "Traceback (most recent call last):\n{}",
                "  File x\n".repeat(50)
            ),
        };
        let snapshot = Snapshot {
            last_job: Some(report(rambling, fx.clock, Duration::ZERO)),
            ..Snapshot::default()
        };
        let menu = fx.view(&snapshot).menu;
        let Entry::Text(label) = &menu.entries[1] else {
            panic!("{menu:?}");
        };
        let detail = label.detail.as_deref().unwrap();
        assert!(
            !detail.contains('\n') && detail.chars().count() <= 64,
            "{detail:?}"
        );
    }

    #[test]
    fn a_stopping_job_says_so_and_cant_be_stopped_again() {
        let fx = Fixture::new();
        let running = Snapshot {
            services: vec![klisi()],
            job: Some(job(Phase::Running, Some(0.42))),
            ..Snapshot::default()
        };
        let mut stopping = running.clone();
        stopping.job.as_mut().unwrap().stopping = true;
        let (before, after) = (fx.view(&running), fx.view(&stopping));

        assert_eq!(
            lines(&after.menu)[..3],
            [
                "Stopping “Weekly sync” for klisi…",
                "transcribe: diarizing",
                "[Stop Current Job] (disabled)",
            ]
        );
        assert_eq!(after.title, None, "no progress while it stops");
        assert_eq!(
            after.icon.activity,
            Activity::Running,
            "until it has stopped"
        );
        assert!(
            before.menu.same_shape(&after.menu),
            "the open menu updates in place"
        );
    }

    #[test]
    fn attention_means_the_owner_is_needed() {
        let mut fx = Fixture::new();
        let attention = |fx: &Fixture, services| {
            fx.view(&Snapshot {
                paused: false,
                services,
                job: None,
                last_job: None,
            })
            .icon
            .attention
        };
        assert!(attention(&fx, vec![klisi()]), "a review waits");

        let mut reviewed = klisi();
        reviewed.bundles.retain(|b| b.approved);
        assert!(
            !attention(&fx, vec![reviewed.clone(), lab(fx.now)]),
            "offline isn't the owner's problem"
        );

        for lost in [Connection::Unpaired, Connection::Replaced] {
            let mut service = reviewed.clone();
            service.connection = lost;
            assert!(attention(&fx, vec![service]));
        }

        fx.home.problems = vec![problem("services/stray.txt")];
        assert!(attention(&fx, vec![reviewed.clone()]), "a problem to fix");
        fx.home.problems.clear();

        fx.deferred
            .insert(("klisi.example.com".parse().unwrap(), hash('b')));
        assert!(!attention(&fx, vec![klisi()]), "put off with Not now");
        let menu = fx
            .view(&Snapshot {
                paused: false,
                services: vec![klisi()],
                job: None,
                last_job: None,
            })
            .menu;
        assert!(
            menu.action("Review transcribe 0.3.0…").is_some(),
            "but still in the menu"
        );
    }

    #[test]
    fn activity_prefers_running_then_paused() {
        let fx = Fixture::new();
        let activity = |paused, job| {
            fx.view(&Snapshot {
                paused,
                services: vec![],
                job,
                last_job: None,
            })
            .icon
            .activity
        };
        assert_eq!(activity(false, None), Activity::Idle);
        assert_eq!(activity(true, None), Activity::Paused);
        assert_eq!(
            activity(false, Some(job(Phase::Preparing, None))),
            Activity::Running
        );
    }

    #[test]
    fn an_unpaired_service_offers_pairing_again() {
        let fx = Fixture::new();
        let mut service = klisi();
        service.connection = Connection::Unpaired;
        let menu = fx
            .view(&Snapshot {
                paused: false,
                services: vec![service],
                job: None,
                last_job: None,
            })
            .menu;
        assert!(lines(&menu).contains(&"Removed this Mac; pair again to use it".to_owned()));
        assert_eq!(
            menu.action("Pair with klisi Again…"),
            Some(&Action::PairAgain(
                "https://klisi.example.com/moil".parse().unwrap()
            ))
        );
    }

    #[test]
    fn open_at_login_reflects_what_can_be_changed() {
        let mut fx = Fixture::new();
        let snapshot = Snapshot::default();
        let check = |fx: &Fixture| {
            lines(&fx.view(&snapshot).menu)
                .into_iter()
                .find(|l| l.contains("Open at Login"))
                .unwrap()
        };
        assert_eq!(check(&fx), "[✓ Open at Login]");
        fx.open_at_login = Some(false);
        assert_eq!(check(&fx), "[Open at Login]");
        fx.open_at_login = None;
        assert_eq!(check(&fx), "[Open at Login] (disabled)");
        assert!(
            lines(&fx.view(&snapshot).menu).contains(&"Not paired with any service".to_owned())
        );
    }

    #[test]
    fn quitting_leaves_nothing_to_click() {
        let mut fx = Fixture::new();
        fx.quitting = true;
        let view = fx.view(&Snapshot {
            paused: false,
            services: vec![klisi()],
            job: Some(job(Phase::Running, Some(0.3))),
            last_job: None,
        });
        assert_eq!(lines(&view.menu), ["Stopping the job and quitting…"]);
    }

    #[test]
    fn progress_keeps_the_shape_and_new_reviews_change_it() {
        let fx = Fixture::new();
        let at = |fraction, services| {
            fx.view(&Snapshot {
                paused: false,
                services,
                job: Some(job(Phase::Running, Some(fraction))),
                last_job: None,
            })
            .menu
        };
        let before = at(0.41, vec![klisi()]);
        let after = at(0.42, vec![klisi()]);
        assert_ne!(before, after);
        assert!(before.same_shape(&after));

        let mut more = klisi();
        more.bundles.push(bundle("summarize", "0.1.0", 'c', false));
        assert!(!before.same_shape(&at(0.42, vec![more])));

        let mut approved = klisi();
        approved.bundles[1].approved = true;
        assert!(!before.same_shape(&at(0.42, vec![approved])));
    }

    #[test]
    fn countdowns_refresh_when_their_number_changes() {
        let fx = Fixture::new();
        let view = fx.view(&Snapshot {
            paused: false,
            services: vec![klisi(), lab(fx.now)],
            job: None,
            last_job: None,
        });
        assert_eq!(view.refresh_at, Some(fx.now + Duration::from_millis(400)));
        assert!(
            fx.view(&Snapshot {
                paused: false,
                services: vec![klisi()],
                job: None,
                last_job: None,
            })
            .refresh_at
            .is_none()
        );

        let mut due = lab(fx.now);
        due.connection = Connection::Disconnected {
            error: "timed out".into(),
            retry_at: fx.now,
        };
        let view = fx.view(&Snapshot {
            paused: false,
            services: vec![due],
            job: None,
            last_job: None,
        });
        assert_eq!(view.refresh_at, None);
        assert!(lines(&view.menu).contains(&"Offline: timed out. Retrying now".to_owned()));
    }

    #[test]
    fn the_last_jobs_age_refreshes_on_the_minute() {
        let fx = Fixture::new();
        let ended = |ago| Snapshot {
            last_job: Some(report(JobOutcome::Succeeded, fx.clock, ago)),
            ..Snapshot::default()
        };
        let view = fx.view(&ended(Duration::from_secs(90)));
        assert_eq!(view.refresh_at, Some(fx.now + Duration::from_secs(30)));

        // A reconnection countdown ticks sooner.
        let mut both = ended(Duration::from_secs(90));
        both.services = vec![lab(fx.now)];
        assert_eq!(
            fx.view(&both).refresh_at,
            Some(fx.now + Duration::from_millis(400))
        );
    }

    #[test]
    fn service_text_cannot_disguise_the_menu() {
        let mut fx = Fixture::new();
        // Hooks and folders can come from git.
        fx.home.hooks.insert(
            "klisi.example.com".parse().unwrap(),
            vec![hook("tr\u{202e}anscribe\nOK", true)],
        );
        fx.home.problems = vec![problem("services/\u{2066}evil\u{2069}\nOK")];
        let mut service = klisi();
        service.name = "kli\u{202e}si\nOK".into();
        service.bundles[1].info.name = format!("{}x", "long".repeat(20));
        let menu = fx
            .view(&Snapshot {
                paused: false,
                services: vec![service],
                job: None,
                last_job: None,
            })
            .menu;
        let lines = lines(&menu);
        assert!(
            lines.contains(&"klisi OK — klisi.example.com ▸".to_owned()),
            "{lines:#?}"
        );
        assert!(
            lines.contains(&format!("[Review {}… 0.3.0…]", "long".repeat(8))),
            "{lines:#?}"
        );
        assert!(
            lines.contains(&"  Hook for transcribe OK".to_owned()),
            "{lines:#?}"
        );
        assert!(
            lines.contains(&"  services/evil OK".to_owned()),
            "{lines:#?}"
        );
    }
}
