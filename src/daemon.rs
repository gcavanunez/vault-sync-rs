//! The long-running service: a filesystem watcher and a socket feed one
//! serialized worker, and a scheduler sleeps until the next deadline.

use crate::clock;
use crate::config::Settings;
use crate::git::{self, Git, Repository};
use crate::interrupt;
use crate::ipc;
use crate::lock;
use crate::log;
use crate::model::{
    Command, Failure, Kind, Outcome, PROTOCOL, Reply, Request, State, Status, Watcher,
};
use crate::notifications;
use crate::scheduling::Scheduling;
use crate::storage;
use crate::sync;
use notify::{EventKind, RecommendedWatcher, RecursiveMode, Watcher as _};
use std::ffi::OsStr;
use std::os::fd::RawFd;
use std::path::{Component, Path};
use std::sync::mpsc::{self, Receiver, Sender, SyncSender};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::thread;
use std::time::Duration;

const QUEUE_CAPACITY: usize = 128;
/// The scheduler re-reads the wall clock at least this often, which bounds how
/// late an overdue deadline fires after the computer wakes from sleep.
const MAX_WAIT: Duration = Duration::from_secs(5);
const FULL_QUEUE_RETRY: Duration = Duration::from_millis(250);

enum Job {
    Automatic,
    Request(Command, SyncSender<Reply>),
    Stop,
}

enum Exit {
    Interrupted,
    Fatal(Failure),
}

struct Shared {
    state: State,
    timing: Scheduling,
    running: bool,
    queued: bool,
    stopping: bool,
    watcher: Watcher,
    watcher_error: Option<String>,
}

struct Daemon {
    settings: Settings,
    repository: Repository,
    lock_fd: RawFd,
    started: i64,
    shared: Mutex<Shared>,
    /// Signalled when the next deadline may have moved earlier or the worker became free.
    wake: Condvar,
}

impl Daemon {
    fn lock(&self) -> MutexGuard<'_, Shared> {
        self.shared.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn status(&self, shared: &Shared) -> Status {
        Status {
            protocol: PROTOCOL,
            vault: self.repository.root.display().to_string(),
            pid: std::process::id(),
            started_at: clock::iso(self.started),
            running: shared.running,
            watcher: shared.watcher,
            watcher_error: shared.watcher_error.clone(),
            next_remote_attempt: (!shared.state.paused)
                .then(|| clock::iso(shared.timing.next_remote().max(self.started))),
            state: shared.state.clone(),
        }
    }

    fn degrade(&self, error: String) {
        let mut shared = self.lock();
        shared.watcher = Watcher::Degraded;
        shared.watcher_error = Some(error);
    }
}

fn millis(value: u64) -> i64 {
    i64::try_from(value).unwrap_or(i64::MAX)
}

/// Runs until interrupted (returning [`Kind::Interrupted`]) or until state can
/// no longer be persisted. Cleanup order: the worker and its Git children,
/// then the socket, the watcher, and finally the repository lock.
pub fn run(settings: Settings) -> Result<(), Failure> {
    let repository = git::discover(&settings)?;
    let lock = lock::acquire(&repository.metadata.join("lock"))?;
    let state = storage::read_state(&repository.metadata)?;
    let timing = Scheduling::new(
        millis(settings.quiet_ms),
        millis(settings.checkpoint_ms),
        millis(settings.remote_poll_ms),
        millis(settings.reconcile_ms),
        millis(settings.retry_max_ms),
    );
    let socket = ipc::socket_path(&repository.common);
    let daemon = Arc::new(Daemon {
        settings,
        repository,
        lock_fd: lock.fd(),
        started: clock::now_ms(),
        shared: Mutex::new(Shared {
            state,
            timing,
            running: false,
            queued: false,
            stopping: false,
            watcher: Watcher::Active,
            watcher_error: None,
        }),
        wake: Condvar::new(),
    });

    let spawn_failure = |error: std::io::Error| Failure::new(Kind::Blocked, error.to_string());
    let (jobs, queue) = mpsc::sync_channel(QUEUE_CAPACITY);
    let (exit, exits) = mpsc::channel();
    let interrupted = exit.clone();
    let _subscription = interrupt::subscribe(move || {
        let _ = interrupted.send(Exit::Interrupted);
    });

    let worker = {
        let daemon = Arc::clone(&daemon);
        thread::Builder::new()
            .name("worker".into())
            .spawn(move || work(&daemon, &queue, &exit))
            .map_err(spawn_failure)?
    };
    let watcher = watch(&daemon);
    let server = {
        let daemon = Arc::clone(&daemon);
        let jobs = jobs.clone();
        ipc::listen(&socket, move |request| respond(&daemon, &jobs, request))?
    };
    log::info(&format!(
        "Watching {}; socket {}",
        daemon.repository.root.display(),
        socket.display()
    ));
    {
        let daemon = Arc::clone(&daemon);
        let jobs = jobs.clone();
        thread::Builder::new()
            .name("scheduler".into())
            .spawn(move || schedule(&daemon, &jobs))
            .map_err(spawn_failure)?;
    }

    let reason = exits.recv().unwrap_or(Exit::Interrupted);
    daemon.lock().stopping = true;
    daemon.wake.notify_all();
    let _ = jobs.try_send(Job::Stop);
    let _ = worker.join();
    server.close();
    drop(watcher);
    drop(lock);

    match reason {
        Exit::Interrupted => Err(Failure::interrupted()),
        Exit::Fatal(error) => Err(error),
    }
}

/// Queues automatic passes when a deadline arrives and the worker is free.
fn schedule(daemon: &Daemon, jobs: &SyncSender<Job>) {
    let mut shared = daemon.lock();

    loop {
        if shared.stopping {
            return;
        }

        let wait = if shared.state.paused || shared.running || shared.queued {
            None
        } else {
            let now = clock::now_ms();
            let due = shared.timing.next_due();

            if now >= due {
                shared.queued = jobs.try_send(Job::Automatic).is_ok();
                if shared.queued {
                    continue;
                }
                Some(FULL_QUEUE_RETRY)
            } else {
                Some(
                    Duration::from_millis(u64::try_from(due - now).unwrap_or(u64::MAX))
                        .min(MAX_WAIT),
                )
            }
        };

        shared = match wait {
            None => daemon
                .wake
                .wait(shared)
                .unwrap_or_else(PoisonError::into_inner),
            Some(wait) => {
                daemon
                    .wake
                    .wait_timeout(shared, wait)
                    .unwrap_or_else(PoisonError::into_inner)
                    .0
            }
        };
    }
}

/// The only place Git runs: one job at a time, in arrival order.
fn work(daemon: &Daemon, queue: &Receiver<Job>, exit: &Sender<Exit>) {
    for job in queue {
        if interrupt::triggered() {
            return;
        }

        let (command, reply) = match job {
            Job::Stop => return,
            Job::Automatic => (None, None),
            Job::Request(command, reply) => (Some(command), Some(reply)),
        };

        {
            let mut shared = daemon.lock();
            if command.is_none() {
                shared.queued = false;
            }
            shared.running = true;
        }

        let result = match command {
            Some(Command::Pause) => toggle(daemon, true),
            Some(Command::Resume) => toggle(daemon, false),
            Some(Command::Now) => pass(daemon, true),
            Some(Command::Status) => Ok(()),
            None => pass(daemon, false),
        };

        let status = {
            let mut shared = daemon.lock();
            shared.running = false;
            daemon.status(&shared)
        };
        daemon.wake.notify_all();

        match result {
            Ok(()) => {
                if let Some(reply) = reply {
                    let _ = reply.send(Reply::Ok {
                        status: Box::new(status),
                    });
                }
            }
            Err(error) if error.kind == Kind::Interrupted => return,
            // Serving requests after synchronization has silently stopped would
            // mislead clients, so a state that cannot be saved stops the daemon.
            Err(error) => {
                if let Some(reply) = reply {
                    let _ = reply.send(Reply::Error {
                        message: error.message.clone(),
                    });
                }
                let _ = exit.send(Exit::Fatal(error));
                return;
            }
        }
    }
}

fn toggle(daemon: &Daemon, paused: bool) -> Result<(), Failure> {
    let next = daemon.lock().state.with_paused(paused);
    storage::write_state(&daemon.repository.metadata, &next)?;
    let mut shared = daemon.lock();
    shared.state = next;

    if !paused {
        shared.timing.changed(0);
    }

    Ok(())
}

fn pass(daemon: &Daemon, force: bool) -> Result<(), Failure> {
    let (before, network) = {
        let mut shared = daemon.lock();

        if shared.state.paused {
            return Ok(());
        }

        let network = shared.timing.begin(clock::now_ms(), force);
        (shared.state.clone(), network)
    };

    let git = Git::new(&daemon.settings, &daemon.repository, daemon.lock_fd);
    let next = sync::sync_once(&git, &daemon.settings, &before, network)?;
    storage::write_state(&daemon.repository.metadata, &next)?;

    {
        let mut shared = daemon.lock();
        shared.state = next.clone();
        let failures = if next.outcome == Outcome::Offline {
            next.failures
        } else {
            0
        };
        shared.timing.completed(clock::now_ms(), failures, network);
    }

    log::info(&next.message);
    notifications::notify(&daemon.settings, &before, &next);
    Ok(())
}

fn respond(daemon: &Daemon, jobs: &SyncSender<Job>, request: Request) -> Option<Reply> {
    if request.command == Command::Status {
        let shared = daemon.lock();
        return Some(Reply::Ok {
            status: Box::new(daemon.status(&shared)),
        });
    }

    let (reply, response) = mpsc::sync_channel(1);

    if jobs.try_send(Job::Request(request.command, reply)).is_err() {
        return Some(Reply::Error {
            message: "Daemon queue is full; retry later".into(),
        });
    }

    response.recv().ok()
}

/// Watcher failures degrade status; periodic reconciliation still catches edits.
fn watch(daemon: &Arc<Daemon>) -> Option<RecommendedWatcher> {
    let handler = {
        let daemon = Arc::clone(daemon);
        move |event: notify::Result<notify::Event>| match event {
            Ok(event) if relevant(&daemon.repository.root, &event) => {
                let first = daemon.lock().timing.changed(clock::now_ms());
                if first {
                    daemon.wake.notify_all();
                }
            }
            Ok(_) => {}
            Err(error) => daemon.degrade(error.to_string()),
        }
    };

    let started = notify::recommended_watcher(handler).and_then(|mut watcher| {
        watcher.watch(&daemon.repository.root, RecursiveMode::Recursive)?;
        Ok(watcher)
    });

    match started {
        Ok(watcher) => Some(watcher),
        Err(error) => {
            daemon.degrade(error.to_string());
            None
        }
    }
}

/// Reads and opens are ignored: Git reads the whole tree during every pass,
/// and counting those would schedule passes forever. Events inside `.git`
/// come from Git itself.
fn relevant(root: &Path, event: &notify::Event) -> bool {
    !matches!(event.kind, EventKind::Access(_))
        && (event.paths.is_empty() || event.paths.iter().any(|path| !inside_git(root, path)))
}

fn inside_git(root: &Path, path: &Path) -> bool {
    path.strip_prefix(root)
        .ok()
        .and_then(|relative| relative.components().next())
        == Some(Component::Normal(OsStr::new(".git")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use notify::event::{AccessKind, AccessMode, CreateKind, ModifyKind};
    use std::path::PathBuf;

    fn event(kind: EventKind, paths: &[&str]) -> notify::Event {
        notify::Event {
            kind,
            paths: paths.iter().map(PathBuf::from).collect(),
            attrs: Default::default(),
        }
    }

    #[test]
    fn edits_outside_git_metadata_are_relevant() {
        let root = Path::new("/vault");
        let modify = EventKind::Modify(ModifyKind::Any);
        assert!(relevant(root, &event(modify, &["/vault/note.md"])));
        assert!(relevant(root, &event(modify, &["/vault/.github/x"])));
        assert!(relevant(root, &event(modify, &["/vault/a/.git/x"])));
        assert!(relevant(
            root,
            &event(EventKind::Create(CreateKind::Folder), &["/vault/nested"])
        ));
        assert!(relevant(root, &event(EventKind::Other, &[])));
        assert!(!relevant(root, &event(modify, &["/vault/.git/index"])));
        assert!(!relevant(root, &event(modify, &["/vault/.git"])));
    }

    #[test]
    fn reads_never_schedule_a_pass() {
        let root = Path::new("/vault");
        for kind in [
            AccessKind::Open(AccessMode::Read),
            AccessKind::Close(AccessMode::Read),
            AccessKind::Close(AccessMode::Write),
        ] {
            assert!(!relevant(
                root,
                &event(EventKind::Access(kind), &["/vault/note.md"])
            ));
        }
    }
}
