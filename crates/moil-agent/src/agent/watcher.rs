//! Changes made to the home from outside (spec/machine.md §1.3): what
//! moil reads from the home, what a running agent makes of files it can't
//! use, and the task that reads the home again whenever it changes.
//!
//! The task polls. Every [`AgentOptions::home_poll`] it takes a
//! [`fingerprint`] of the files moil reads (their names, sizes, times,
//! inodes and modes, not their contents) and reads the home again only
//! when that changed. Polling, rather than listening for file-system
//! events, because it needs no platform code or dependency; because it
//! sees every way a file can change the same way, whether an editor saves
//! by renaming, git replaces a checkout, or a synced or network folder
//! loses events; and because a home is a few dozen files, so a fingerprint
//! costs a few dozen `stat`s. Events would still need what polling does
//! anyway: a fresh reading to learn what changed, and a wait for a burst
//! of changes, such as a checkout, to end.
//!
//! A change is read once the fingerprint has held still for one poll, so
//! that a checkout in progress isn't read half done, or once it has kept
//! changing for [`MAX_UNSETTLED_POLLS`] polls, so that a file that changes
//! all the time can't hold the others up: within three polls of the
//! change either way.
//!
//! Everything the agent learns from the home comes through here, the
//! changes it makes itself included: the handle asks for a
//! [`Refresh`] once it has written them. Readings reach the coordinator
//! one at a time, in the order they were taken, and it applies each by
//! comparing it with what it goes by, so a change is applied once however
//! many readings show it, and moil's own writes don't loop.
//!
//! [`AgentOptions::home_poll`]: super::AgentOptions::home_poll

use std::collections::hash_map::DefaultHasher;
use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsString;
use std::fs;
use std::hash::{Hash, Hasher};
use std::io;
use std::os::unix::fs::MetadataExt as _;
use std::path::Path;
use std::time::Duration;

use moil_bundle::Bundle;
use tokio::sync::{mpsc, oneshot};
use tokio::time::MissedTickBehavior;
use tokio_util::sync::CancellationToken;

use super::coordinator::Input;
use crate::folders::{self, Unusable};
use crate::hooks;
use crate::machine::SettingsChange;
use crate::report::{HookListing, HookReport, Problem};
use crate::{Home, Machine, Service, ServiceId};

/// How many polls in a row may find the home changing before it is read
/// all the same.
const MAX_UNSETTLED_POLLS: u32 = 3;

/// How deep [`fingerprint`] looks under `services/`, down to
/// `<id>/bundles/<name>/<hash>/<file>`.
const MAX_DEPTH: usize = 4;

/// The home as moil goes by it (spec/machine.md §1): its services, its
/// hooks, and what's wrong with it. A running agent keeps what it read
/// before from files it can't use (§1.3).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct HomeState {
    /// Every service the home knows, by ID.
    pub services: BTreeMap<ServiceId, Service>,
    /// Every hook, by service and then bundle.
    pub hooks: Vec<HookListing>,
    /// What's wrong with the home, for the owner to fix.
    pub problems: Vec<Problem>,
    /// The service folders that have a `pairing.json`, whether moil can use
    /// it or not.
    pub(crate) pairing_files: BTreeSet<ServiceId>,
}

impl HomeState {
    /// The hooks of service `id`, by bundle.
    pub fn service_hooks(&self, id: &ServiceId) -> Vec<HookReport> {
        self.hooks
            .iter()
            .filter(|listing| listing.service == *id)
            .map(|listing| listing.hook.clone())
            .collect()
    }
}

/// What the home says, read in one go by [`read_home`].
#[derive(Debug)]
pub(crate) struct Reading {
    /// The home as its files say now, without what can't be used.
    pub home: HomeState,
    /// How `settings.json` changed since moil last read or wrote it, if it
    /// did and moil can use it.
    pub settings: Option<SettingsChange>,
    /// Verified copies of the bundles that each service's approvals that
    /// count name.
    pub bundles: BTreeMap<ServiceId, Vec<Bundle>>,
    /// What's there, but moil can't use.
    pub unusable: Vec<Unusable>,
}

/// Reads the home of `machine`, while none of moil's own changes to its
/// services is half done: its settings, going by them if they changed
/// since moil last read or wrote them, its services and its hooks. Each
/// thing wrong is one problem, whoever goes by the reading. Blocks, and
/// may take a while: the approved bundles are hashed again.
pub(crate) fn read_home(machine: &Machine) -> Reading {
    let _services = machine.lock_services();
    let home = machine.home();
    let mut problems = Vec::new();
    let settings = machine.reload_settings().unwrap_or_else(|e| {
        problems.push(Home::settings_problem(&e));
        None
    });
    let scan = folders::scan(home).unwrap_or_else(|e| {
        problems.push(Problem {
            path: "services".into(),
            message: format!("moil can't read this folder ({e})."),
        });
        folders::Scan {
            unusable: vec![Unusable::Services],
            ..Default::default()
        }
    });
    problems.extend(scan.problems);
    let hooks = hooks::scan(home);
    problems.extend(hooks.problems);
    Reading {
        home: HomeState {
            services: scan
                .services
                .into_iter()
                .map(|service| (service.id.clone(), service))
                .collect(),
            hooks: hooks.hooks,
            problems,
            pairing_files: scan.pairing_files,
        },
        settings,
        bundles: scan.bundles,
        unusable: scan.unusable,
    }
}

impl Reading {
    /// Goes on with what `before` holds of the files this reading couldn't
    /// use (spec/machine.md §1.3).
    pub(super) fn keep_from(&mut self, before: &HomeState) {
        let home = &mut self.home;
        for unusable in &self.unusable {
            match unusable {
                Unusable::Services => {
                    home.services.clone_from(&before.services);
                    home.pairing_files.clone_from(&before.pairing_files);
                }
                Unusable::ServiceFile(id) => {
                    if let Some(service) = before.services.get(id) {
                        home.services.insert(id.clone(), service.clone());
                    }
                }
                Unusable::PairingFile(id) => {
                    let pairing = before.services.get(id).and_then(|s| s.pairing.as_ref());
                    if let (Some(service), Some(pairing)) = (home.services.get_mut(id), pairing)
                        // Only ever to the base URL it was paired with.
                        && pairing.base_url == service.base_url
                    {
                        service.pairing = Some(pairing.clone());
                    }
                }
            }
        }
    }
}

/// Asks the watcher to read the home at once, for a change the owner made
/// through the agent.
#[derive(Debug)]
pub(super) struct Refresh {
    /// A service the agent disconnected from to unpair it, which it may
    /// connect to again once the home has been read, if it's still paired.
    pub left: Option<ServiceId>,
    /// Told once the agent goes by what was read.
    pub done: oneshot::Sender<()>,
}

/// The task that reads the home whenever it changes, and when asked to.
pub(super) struct Watcher {
    pub machine: Machine,
    pub interval: Duration,
    pub inputs: mpsc::UnboundedSender<Input>,
    pub refreshes: mpsc::UnboundedReceiver<Refresh>,
    pub stop: CancellationToken,
    /// The home's fingerprint when it was last read.
    pub read: u64,
}

impl Watcher {
    /// Watches until stopped, or until the agent has gone.
    pub async fn run(mut self) {
        let mut polls = tokio::time::interval(self.interval);
        polls.set_missed_tick_behavior(MissedTickBehavior::Delay);
        // The fingerprint at the last poll, and how many polls in a row
        // found the home changed since it was last read.
        let mut last = self.read;
        let mut unsettled = 0;
        loop {
            let refresh = tokio::select! {
                () = self.stop.cancelled() => return,
                refresh = self.refreshes.recv() => match refresh {
                    Some(refresh) => Some(refresh),
                    None => return,
                },
                _ = polls.tick() => None,
            };
            if refresh.is_none() {
                let now = self
                    .machine
                    .blocking(|machine| Ok(fingerprint(machine.home())))
                    .await;
                let Ok(now) = now else {
                    return;
                };
                let settled = now == last;
                last = now;
                if now == self.read {
                    unsettled = 0;
                    continue;
                }
                unsettled += 1;
                if !settled && unsettled < MAX_UNSETTLED_POLLS {
                    continue;
                }
            }
            let taken = self.machine.blocking(|machine| {
                // Taken first, so that a change made while reading is
                // noticed at the next poll.
                let fingerprint = fingerprint(machine.home());
                Ok((fingerprint, read_home(machine)))
            });
            let Ok((fingerprint, reading)) = taken.await else {
                return;
            };
            (self.read, last, unsettled) = (fingerprint, fingerprint, 0);
            let (left, done) = match refresh {
                Some(Refresh { left, done }) => (left, Some(done)),
                None => (None, None),
            };
            let input = Input::Home {
                reading: Box::new(reading),
                left,
                done,
            };
            if self.inputs.send(input).is_err() {
                return;
            }
        }
    }
}

/// A digest of the files moil reads from the home that an editor or git
/// may change: `settings.json`, and what's in each service folder but its
/// logs and hidden files. Only names and metadata go into it: whatever
/// changes a file changes its size, times or inode.
pub(super) fn fingerprint(home: &Home) -> u64 {
    let mut digest = DefaultHasher::new();
    entry(&mut digest, &home.settings_file());
    walk(&mut digest, &home.services_dir(), 0);
    digest.finish()
}

/// Adds what's in `dir`, `depth` levels under `services/`, to `digest`.
fn walk(digest: &mut DefaultHasher, dir: &Path, depth: usize) {
    let mut names: Vec<OsString> = match fs::read_dir(dir) {
        Ok(entries) => entries
            .filter_map(|entry| entry.ok().map(|entry| entry.file_name()))
            .collect(),
        Err(e) => {
            e.kind().hash(digest);
            return;
        }
    };
    names.sort();
    for name in names {
        // Such as .git, or moil's own files as it writes them. What the
        // hooks print changes all the time, and nothing moil reads.
        if name.to_string_lossy().starts_with('.') || (depth == 1 && name == "logs") {
            continue;
        }
        name.hash(digest);
        let path = dir.join(&name);
        if entry(digest, &path) && depth < MAX_DEPTH {
            walk(digest, &path, depth + 1);
        }
    }
}

/// Adds what's at `path` to `digest`: a link, and what it leads to.
/// Returns whether it's a directory, or leads to one.
fn entry(digest: &mut DefaultHasher, path: &Path) -> bool {
    let own = fs::symlink_metadata(path);
    metadata(digest, &own);
    match own {
        Ok(own) if own.file_type().is_symlink() => {
            let target = fs::metadata(path);
            metadata(digest, &target);
            target.is_ok_and(|target| target.is_dir())
        }
        own => own.is_ok_and(|own| own.is_dir()),
    }
}

fn metadata(digest: &mut DefaultHasher, metadata: &io::Result<fs::Metadata>) {
    match metadata {
        // A directory's times change as moil writes files in it aside
        // before moving them into place; its entries tell what changed.
        Ok(m) if m.is_dir() => (m.dev(), m.ino(), m.mode()).hash(digest),
        Ok(m) => (
            (m.dev(), m.ino(), m.mode(), m.len()),
            (m.mtime(), m.mtime_nsec(), m.ctime(), m.ctime_nsec()),
        )
            .hash(digest),
        Err(e) => e.kind().hash(digest),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(path: &Path, text: &str) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, text).unwrap();
    }

    #[test]
    fn the_fingerprint_follows_what_moil_reads_and_nothing_else() {
        let dir = tempfile::tempdir().unwrap();
        let home = Home::new(dir.path());
        let service = home.services_dir().join("klisi.example.com");
        write(&service.join("service.json"), "{}");
        let before = fingerprint(&home);
        assert_eq!(fingerprint(&home), before, "it's stable");

        // What an editor, git or the hooks do elsewhere doesn't count...
        write(&service.join("logs/transcribe.log"), "== exited 0\n");
        write(&service.join(".git/index"), "changed");
        write(&dir.path().join(".machine/hooks/1/hook.json"), "{}");
        write(&dir.path().join("README.md"), "changed");
        assert_eq!(fingerprint(&home), before);

        // ...but every file moil reads does, however it changes.
        let hook = service.join("hooks/transcribe");
        let changes: [&dyn Fn(); 5] = [
            &|| write(&home.settings_file(), "{\"paused\": true}"),
            &|| write(&service.join("service.json"), "{ }"),
            &|| write(&service.join("bundles/transcribe/abc/job.py"), "print()"),
            &|| write(&hook, "#!/bin/sh\n"),
            &|| {
                use std::os::unix::fs::PermissionsExt;
                fs::set_permissions(&hook, fs::Permissions::from_mode(0o755)).unwrap();
            },
        ];
        let mut seen = vec![before];
        for change in changes {
            change();
            let now = fingerprint(&home);
            assert!(!seen.contains(&now), "{seen:?} {now}");
            seen.push(now);
        }
    }
}
