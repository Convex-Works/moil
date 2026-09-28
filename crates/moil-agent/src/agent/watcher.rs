//! Changes made to the home from outside (spec/machine.md §1.3): what the
//! agent reads from the home, what it makes of files it can't use, and the
//! task that reads the home again whenever it changes.
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
use crate::folders::{self, FolderFile};
use crate::hooks::{self, HookScan};
use crate::machine::SettingsChange;
use crate::report::{HookListing, Problem};
use crate::{Error, Home, Machine, Service, ServiceId};

/// How many polls in a row may find the home changing before it is read
/// all the same.
const MAX_UNSETTLED_POLLS: u32 = 3;

/// How deep [`fingerprint`] looks under `services/`, down to
/// `<id>/bundles/<name>/<hash>/<file>`.
const MAX_DEPTH: usize = 4;

/// What the home says, read in one go while none of moil's own changes to
/// its services is half done.
#[derive(Debug)]
pub(super) struct Reading {
    /// How `settings.json` changed since moil last read or wrote it, if it
    /// did, or why it can't be used.
    pub settings: Result<Option<SettingsChange>, Error>,
    /// The service folders, or why `services/` can't be listed.
    pub folders: Result<folders::Scan, Error>,
    pub hooks: HookScan,
}

impl Reading {
    /// Reads the home of `machine`. Blocks, and may take a while: the
    /// approved bundles are hashed again.
    pub fn take(machine: &Machine) -> Self {
        let _services = machine.lock_services();
        let home = machine.home();
        Self {
            settings: machine.reload_settings(),
            folders: folders::scan(home),
            hooks: hooks::scan(home),
        }
    }
}

/// The home as the agent goes by it: what it last read from each file it
/// could use (spec/machine.md §1.3).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct HomeState {
    /// Every service the home knows, by ID.
    pub services: BTreeMap<ServiceId, Service>,
    /// The service folders that have a `pairing.json`, whether moil can use
    /// it or not.
    pub pairing_files: BTreeSet<ServiceId>,
    /// Every hook, by service and then bundle.
    pub hooks: Vec<HookListing>,
    pub problems: Vec<Problem>,
}

/// A [`Reading`], as the agent goes by it.
pub(super) struct Merged {
    pub home: HomeState,
    /// How the settings changed, if they did.
    pub settings: Option<SettingsChange>,
    /// Verified copies of the bundles that each service's approvals that
    /// count name.
    pub bundles: BTreeMap<ServiceId, Vec<Bundle>>,
}

impl HomeState {
    /// What the agent goes by once it has taken `reading`: what it says,
    /// but for the files that can't be used, which keep what was read from
    /// them before, and say so.
    pub(super) fn merge(&self, reading: Reading) -> Merged {
        let mut problems = Vec::new();
        let settings = reading.settings.unwrap_or_else(|e| {
            problems.push(Home::settings_problem(&e, true));
            None
        });
        let mut merged = Merged {
            home: HomeState::default(),
            settings,
            bundles: BTreeMap::new(),
        };
        match reading.folders {
            Ok(scan) => {
                let mut services: BTreeMap<ServiceId, Service> = scan
                    .services
                    .into_iter()
                    .map(|service| (service.id.clone(), service))
                    .collect();
                let mut folder_problems = scan.problems;
                for unusable in &scan.unusable {
                    let before = self.services.get(&unusable.id);
                    let kept = match unusable.file {
                        FolderFile::Service => before.is_some_and(|before| {
                            services.insert(before.id.clone(), before.clone());
                            true
                        }),
                        FolderFile::Pairing => {
                            let pairing = before.and_then(|before| before.pairing.as_ref());
                            match (services.get_mut(&unusable.id), pairing) {
                                // Only ever to the base URL it was paired with.
                                (Some(service), Some(pairing))
                                    if pairing.base_url == service.base_url =>
                                {
                                    service.pairing = Some(pairing.clone());
                                    true
                                }
                                _ => false,
                            }
                        }
                    };
                    if kept {
                        let path = unusable.path();
                        for problem in folder_problems.iter_mut().filter(|p| p.path == path) {
                            *problem = unusable.problem(true);
                        }
                    }
                }
                problems.extend(folder_problems);
                merged.home.services = services;
                merged.home.pairing_files = scan.pairing_files;
                merged.bundles = scan.bundles;
            }
            Err(e) => {
                problems.push(Problem {
                    path: "services".into(),
                    message: format!(
                        "moil can't read this folder ({e}), so it goes on with what it read from \
                         it before."
                    ),
                });
                merged.home.services = self.services.clone();
                merged.home.pairing_files = self.pairing_files.clone();
            }
        }
        problems.extend(reading.hooks.problems);
        merged.home.hooks = reading.hooks.hooks;
        merged.home.problems = problems;
        merged
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
                Ok((fingerprint, Reading::take(machine)))
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
