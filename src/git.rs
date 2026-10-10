use crate::config::Settings;
use crate::model::{Failure, Kind, failure};
use crate::process::{self, Options, Output};
use crate::storage::exists;
use std::collections::BTreeMap;
use std::env;
use std::ffi::OsString;
use std::fs;
use std::os::fd::RawFd;
use std::path::{Path, PathBuf};

const INCOMING_REF: &str = "refs/vault-sync/incoming";

/// Unfinished operations that need a person, checked in priority order.
const MANUAL_OPERATIONS: [&str; 7] = [
    "rebase-merge",
    "rebase-apply",
    "MERGE_HEAD",
    "CHERRY_PICK_HEAD",
    "REVERT_HEAD",
    "sequencer",
    "BISECT_START",
];

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Repository {
    pub root: PathBuf,
    /// The canonical Git common directory; it names the socket and holds metadata.
    pub common: PathBuf,
    pub metadata: PathBuf,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Counts {
    pub ahead: i64,
    pub behind: i64,
}

/// The inherited environment without repository overrides, plus noninteractive settings.
pub fn environment(settings: &Settings) -> Vec<(OsString, OsString)> {
    let mut variables: BTreeMap<OsString, OsString> = env::vars_os()
        .filter(|(key, _)| {
            ![
                "GIT_DIR",
                "GIT_WORK_TREE",
                "GIT_COMMON_DIR",
                "GIT_INDEX_FILE",
                "GIT_OBJECT_DIRECTORY",
                "GIT_ALTERNATE_OBJECT_DIRECTORIES",
                "GIT_PREFIX",
            ]
            .iter()
            .any(|scoped| key == scoped)
        })
        .collect();

    for (key, value) in [
        ("PATH", settings.path.as_str()),
        ("GIT_TERMINAL_PROMPT", "0"),
        ("GIT_OPTIONAL_LOCKS", "0"),
        ("GIT_MERGE_AUTOEDIT", "no"),
        ("GIT_SSH_COMMAND", settings.ssh_command.as_str()),
    ] {
        variables.insert(key.into(), value.into());
    }

    variables.into_iter().collect()
}

pub fn discover(settings: &Settings) -> Result<Repository, Failure> {
    let vault = Path::new(&settings.vault_path);
    let env = environment(settings);

    let run = |args: &[&str], fallback: &str| -> Result<String, Failure> {
        let command: Vec<String> = ["git", "-C", &settings.vault_path]
            .iter()
            .chain(args)
            .map(|arg| (*arg).to_owned())
            .collect();
        let output = process::run(
            &command,
            &Options {
                cwd: vault,
                env: &env,
                timeout: settings.command_timeout(),
                lock_fd: None,
            },
        )?;

        if output.code != 0 {
            let message = output.stderr.trim();
            return failure(
                Kind::Config,
                if message.is_empty() {
                    fallback
                } else {
                    message
                },
            );
        }

        Ok(output.stdout.trim().to_owned())
    };

    let canonical = |path: &Path| {
        fs::canonicalize(path)
            .map_err(|error| Failure::new(Kind::Config, format!("{}: {error}", path.display())))
    };

    let root = canonical(vault)?;
    let top = run(
        &["rev-parse", "--show-toplevel"],
        "Cannot open vault repository",
    )?;

    if root != Path::new(&top) {
        return failure(Kind::Config, "vaultPath must be the repository root");
    }

    run(
        &[
            "check-ref-format",
            &format!("refs/heads/{}", settings.branch),
        ],
        &format!("Invalid branch name: {}", settings.branch),
    )?;

    if !run(&["remote"], "Cannot open vault repository")?
        .lines()
        .any(|remote| remote == settings.remote)
    {
        return failure(Kind::Config, format!("No remote named {}", settings.remote));
    }

    let common = canonical(&root.join(run(
        &["rev-parse", "--git-common-dir"],
        "Cannot open vault repository",
    )?))?;

    Ok(Repository {
        root,
        metadata: common.join("vault-sync"),
        common,
    })
}

/// Git commands for one clone, run while holding its lock.
pub struct Git<'a> {
    settings: &'a Settings,
    repository: &'a Repository,
    env: Vec<(OsString, OsString)>,
    lock_fd: RawFd,
    remote_ref: String,
}

impl<'a> Git<'a> {
    pub fn new(settings: &'a Settings, repository: &'a Repository, lock_fd: RawFd) -> Self {
        Self {
            settings,
            repository,
            env: environment(settings),
            lock_fd,
            remote_ref: format!("refs/remotes/{}/{}", settings.remote, settings.branch),
        }
    }

    /// Runs Git and returns its result whatever the exit code.
    pub fn output(&self, args: &[&str]) -> Result<Output, Failure> {
        let command: Vec<String> = [
            "git",
            "-c",
            "merge.autoStash=false",
            "-c",
            "rebase.autoStash=false",
        ]
        .iter()
        .chain(args)
        .map(|arg| (*arg).to_owned())
        .collect();
        process::run(
            &command,
            &Options {
                cwd: &self.repository.root,
                env: &self.env,
                timeout: self.settings.command_timeout(),
                lock_fd: Some(self.lock_fd),
            },
        )
    }

    /// Runs Git and treats a nonzero exit as blocking.
    pub fn run(&self, args: &[&str]) -> Result<Output, Failure> {
        checked(Kind::Blocked, args, self.output(args)?)
    }

    pub fn dirty(&self) -> Result<bool, Failure> {
        Ok(!self
            .run(&["status", "--porcelain=v1", "--untracked-files=all"])?
            .stdout
            .is_empty())
    }

    pub fn head(&self) -> Result<String, Failure> {
        Ok(self.run(&["rev-parse", "HEAD"])?.stdout.trim().to_owned())
    }

    /// Refuses to proceed during manual operations, with conflicts, or off the branch.
    pub fn ready(&self) -> Result<(), Failure> {
        let mut args = vec!["rev-parse"];
        for name in MANUAL_OPERATIONS {
            args.extend(["--git-path", name]);
        }
        let paths = self.run(&args)?.stdout;
        let paths: Vec<&str> = paths.lines().collect();

        if paths.len() != MANUAL_OPERATIONS.len() {
            return failure(Kind::Blocked, "git rev-parse: unexpected --git-path output");
        }

        for (name, path) in MANUAL_OPERATIONS.iter().zip(paths) {
            if exists(&self.repository.root.join(path))? {
                return failure(
                    Kind::Blocked,
                    format!("Manual Git operation requires attention: {name}"),
                );
            }
        }

        if !self.run(&["ls-files", "--unmerged"])?.stdout.is_empty() {
            return failure(Kind::Blocked, "Unresolved Git files require attention");
        }

        let branch = self.output(&["symbolic-ref", "--quiet", "--short", "HEAD"])?;
        let name = branch.stdout.trim();

        if branch.code != 0 || name != self.settings.branch {
            let found = if name.is_empty() {
                "detached HEAD"
            } else {
                name
            };
            return failure(
                Kind::Blocked,
                format!("Expected {}; found {found}", self.settings.branch),
            );
        }

        self.head().map(drop)
    }

    pub fn counts(&self) -> Result<Counts, Failure> {
        let range = format!("HEAD...{}", self.remote_ref);
        let output = self.run(&["rev-list", "--left-right", "--count", &range])?;
        let numbers: Vec<i64> = output
            .stdout
            .split_whitespace()
            .map(str::parse)
            .collect::<Result<_, _>>()
            .map_err(|error| Failure::new(Kind::Blocked, format!("git rev-list: {error}")))?;

        match numbers[..] {
            [ahead, behind] => Ok(Counts { ahead, behind }),
            _ => failure(Kind::Blocked, "git rev-list: unexpected count output"),
        }
    }

    // An empty refmap stops Git from also force-updating the configured
    // remote-tracking ref, so a rewritten remote is caught before it is trusted.
    pub fn fetch(&self) -> Result<(), Failure> {
        let refspec = format!("+refs/heads/{}:{INCOMING_REF}", self.settings.branch);
        let args = [
            "fetch",
            "--no-tags",
            "--refmap=",
            &self.settings.remote,
            &refspec,
        ];
        checked(Kind::Offline, &args, self.output(&args)?)?;

        if self
            .output(&["rev-parse", "--verify", "--quiet", &self.remote_ref])?
            .code
            == 0
        {
            let ancestry = self.output(&[
                "merge-base",
                "--is-ancestor",
                &self.remote_ref,
                INCOMING_REF,
            ])?;

            if ancestry.code == 1 {
                return failure(
                    Kind::Blocked,
                    format!(
                        "Remote {}/{} was rewritten since the last fetch. Pause sync and reconcile manually.",
                        self.settings.remote, self.settings.branch
                    ),
                );
            }

            if ancestry.code != 0 {
                return failure(
                    Kind::Blocked,
                    format!("git merge-base: {}", ancestry.stderr.trim()),
                );
            }
        }

        self.run(&["update-ref", &self.remote_ref, INCOMING_REF])
            .map(drop)
    }

    /// Fast-forwards to the fetched remote when that is the only safe action.
    pub fn integrate(&self) -> Result<(), Failure> {
        self.ready()?;
        let Counts { ahead, behind } = self.counts()?;

        if ahead > 0 && behind > 0 {
            return failure(
                Kind::Diverged,
                format!(
                    "Histories diverged ({ahead} local, {behind} remote commits). Pause sync and reconcile manually."
                ),
            );
        }

        if behind > 0 {
            if self.dirty()? {
                return failure(
                    Kind::Pending,
                    "New local edits appeared; remote integration deferred",
                );
            }

            self.run(&["merge", "--ff-only", "--no-autostash", &self.remote_ref])?;
        }

        Ok(())
    }
}

fn checked(kind: Kind, args: &[&str], output: Output) -> Result<Output, Failure> {
    if output.code == 0 {
        return Ok(output);
    }

    let detail = match output.stderr.trim() {
        "" => output.stdout.trim(),
        stderr => stderr,
    };
    failure(
        kind,
        format!("git {}: {detail}", args.first().unwrap_or(&"")),
    )
}
