# Vault sync

A Rust daemon that syncs a notes vault through GitHub, whether Obsidian is open
or closed. Each computer has its own clone and can sync independently. The tool
lives in this repository; notes stay in the vault.

Installed computers need Git 2.31+ and noninteractive Git authentication.
Supported systems are Linux and macOS, on x64 and arm64.

## How it runs

The daemon watches the vault recursively (inotify on Linux, FSEvents on macOS),
polls the remote, and accepts CLI commands through a private Unix socket. One
worker thread runs every Git command, one job at a time. A scheduler thread
sleeps until the next deadline instead of polling, so an idle daemon uses no
measurable CPU.

[docs/architecture.md](docs/architecture.md) has diagrams of how two computers
share a remote, the daemon's threads, a command's path, one sync pass, when
passes run, and how lock ownership survives crashes.

Default timing:

| Trigger | Behavior |
| --- | --- |
| Saved local edits | Sync after 10 seconds without another filesystem event |
| Continuous edits | Start a checkpoint after at most 60 seconds, once the worker is free |
| Remote changes | Check every 60 seconds after the preceding remote attempt completes |
| Missed watcher events | Reconcile every 5 minutes; remote passes also inspect local changes |
| Failed remote attempts | Back off from 60 seconds to a maximum of 5 minutes |
| `now` | Queue a fresh sync and bypass network backoff |

Local edits still get committed during network backoff. Filesystem events cannot
detect pushes to GitHub, so remote polling is necessary. Deadlines use the wall
clock; ones that became overdue while the computer slept fire within five
seconds of waking. The daemon does not wake the computer. Watcher failures
appear in status; periodic reconciliation continues. File reads and changes
inside `.git` are ignored. Ignored files can trigger a scan, but Git's ignore
rules decide what is committed.

## Sync policy

Each pass checks for unfinished Git operations, checkpoints saved edits, fetches
the configured branch, and compares histories:

| Local vs remote | Action |
| --- | --- |
| Equal | No integration or push needed |
| Local ahead | Push the committed snapshot |
| Local behind | Fast-forward if the working tree is still clean |
| Both ahead and behind | Preserve local commits and report divergence |

There are no automatic rebases, merge commits, stashes, force-pushes, or conflict
winners. A rejected push triggers a fresh fetch and history check. Offline edits
can still be saved as local commits. If the remote branch was force-pushed or
rewound since the last fetch, the pass stops as blocked and nothing is pushed.

Commit subjects retain `vault backup: <device> <YYYY-MM-DD HH:mm:ss>` in local
time, with changed files in the body. All nonignored additions, edits, and
deletions are staged, so use a dedicated vault clone.

Save notes and run `vault-sync now` before leaving a computer and before editing
on the next one. Exit 0 means the checked snapshot synced and no pending local
changes were detected at completion. Edits during transfer produce a pending
result instead. This check cannot include unsaved editor buffers or a remote push
made after the last network exchange.

## Install

With [mise](https://mise.jdx.dev), from the prebuilt release binaries:

```sh
mise use -g github:gcavanunez/vault-sync-rs
vault-sync --version
```

Releases contain static Linux executables for x64 and arm64, which run on any
distribution, and macOS executables for Apple Silicon and Intel. Each one is
built and tested on its own platform before it is published, and mise checks
its checksum and GitHub build attestation when it downloads it.

Or build from source with Rust 1.89 or newer:

```sh
cargo build --release --locked   # target/release/vault-sync, about 1.2 MB
```

## Set up the service

The service runs the daemon from a stable path, `~/.local/bin/vault-sync`,
rather than from mise's per-version install directory. The installer copies the
executable there, creates `~/.config/vault-sync/config.json` if absent, and
writes a systemd or launchd definition. It does not start the service.

```sh
# After installing with mise:
sh "$(mise where github:gcavanunez/vault-sync-rs)/install.sh"
# From a source checkout, after cargo build:
./install.sh
# Or with any compiled executable:
./install.sh /path/to/vault-sync
```

Existing config is preserved; a different executable or service definition
must be moved aside before replacing it (see [Upgrade](#upgrade)).

Edit the generated config, using [config.example.json](config.example.json) as a
reference. Only `vaultPath` is required. Set a stable `deviceName` on each machine:

```json
{
  "vaultPath": "/home/you/notes",
  "deviceName": "linux-desktop"
}
```

On macOS, use the actual `/Users/...` path and a different device label. The vault
path must be absolute and must name the repository root. The repository needs an
initial commit, the configured remote branch, and a Git author identity. Unknown
keys are rejected.

All timing options are milliseconds. `path` supplies the service's executable
search path; include any credential helpers or hook tools you need. `sshCommand`
defaults to SSH batch mode with connection and keepalive timeouts. A shell-only
agent or version manager may be unavailable to systemd or launchd.

`VAULT_SYNC_CONFIG` selects an alternative JSON file. Otherwise the config lives
under `${XDG_CONFIG_HOME:-~/.config}/vault-sync/config.json`. Service installation
records its absolute path. Restart the daemon after changing configuration.

## Upgrade

Configuration, `daemon-state.json`, the lock file and the socket stay
compatible between versions, so upgrades need no migration. The installer
refuses to overwrite a different executable, so move the old one aside:

```sh
mise upgrade github:gcavanunez/vault-sync-rs   # or rebuild from source
# Linux; on macOS use launchctl bootout/bootstrap as below.
systemctl --user stop vault-sync.service
mv ~/.local/bin/vault-sync ~/.local/bin/vault-sync.old
sh "$(mise where github:gcavanunez/vault-sync-rs)/install.sh"   # or ./install.sh
systemctl --user start vault-sync.service
vault-sync status
```

`install.sh` accepts the existing service definition only if it is identical,
which it is when the binary path and config path are unchanged.

## Cut over from Obsidian Git

Finish syncing with the existing plugin on each computer. Use its **Git:
Pause/Resume automatic routines** command to pause automatic commits, pushes,
pulls, and pull-on-startup. Keep the plugin enabled for its Git UI. Its pause flag
is device-local, so do this separately on both machines.

Test the new executable, then its real service environment:

```sh
vault-sync status
vault-sync now
```

On Linux:

```sh
systemctl --user daemon-reload
systemctl --user start vault-sync.service
journalctl --user -u vault-sync.service -n 50
vault-sync status
systemctl --user enable vault-sync.service
```

On macOS, loading the agent starts it and enables its login service:

```sh
launchctl bootstrap "gui/$(id -u)" "$HOME/Library/LaunchAgents/com.gcavanunez.vault-sync.plist"
launchctl print "gui/$(id -u)/com.gcavanunez.vault-sync"
vault-sync status
```

macOS logs append to `~/Library/Logs/vault-sync.log` and `vault-sync.error.log`.
The user service needs a logged-in session. Test authentication, notification
permissions, sleep/resume, and a handoff in both directions on the actual machines.

Manual Git actions in the plugin remain available. Pause vault-sync and wait for
the command to return before using them. The lock coordinates vault-sync
processes, not editors or other Git tools.

## Commands

| Command | Behavior |
| --- | --- |
| `daemon` | Watch and sync until interrupted; intended for the OS service |
| `now` or `sync` | Queue a fresh pass and wait for its result |
| `status` | Read live daemon state or persisted state without fetching |
| `pause` | Wait for queued work, then persistently pause future automation |
| `resume` | Clear pause; a running daemon schedules another pass |
| `service-install` | Write the host OS service definition without starting it |

Control commands accept `--json`. When the daemon is stopped, `now`, `pause`, and
`resume` operate directly under the same repository lock. They return busy if a
different standalone process or surviving Git child holds it. Status reports
that lock as active. A daemon client's timeout does not cancel already queued
work; inspect status before retrying.

Status includes the last attempted result, remote contact, synced snapshot, and
clean completion. These are recorded observations, not a fresh working-tree or
remote check. It does not establish that the OS service is enabled.

Exit codes are 0 for successful controls or a clean sync, 1 for a sync failure
or a usage error, 2 for invalid application configuration, and 3 for pending
edits, pause, busy ownership, or a client timeout. SIGINT, SIGTERM and SIGHUP
stop any running Git command, wait for it, release ownership, and exit with 130.

## State and process ownership

The Git common directory contains `vault-sync/lock` and `daemon-state.json`.
State is schema-validated and atomically replaced (written, flushed to disk,
then renamed). A malformed state file stops the operation. These files are Git
metadata and are not committed. Fetches land in `refs/vault-sync/incoming` and
only advance the remote-tracking branch after its history is checked.

The OS `flock` remains held for the daemon's lifetime. Each Git command runs in
its own session and process group and inherits the lock as descriptor 3, which
retains ownership even after an abrupt parent death. On a signal or timeout, the
runner sends SIGTERM to the Git process group, escalates to SIGKILL after two
seconds, and reaps the child before releasing its descriptor. Never delete the
lock file, because its inode identifies shared ownership; a symlinked lock file
is refused.

The socket lives in a private `/tmp/vault-sync-<uid>/` directory with a filename
derived from the canonical Git common directory. Only the lock owner replaces
a stale socket. Separate configs targeting the same clone share ownership and
state. After an interrupted Git operation, inspect `git status`; vault-sync does
not delete Git lock files or repair incomplete operations.

A panic aborts the process rather than leaving a daemon that answers status but
no longer syncs; the service manager restarts it.

## Notifications and recovery

New blockers such as divergence notify immediately. Network failures notify
after three consecutive failures by default. Set `notifications` to `false` to
disable delivery. Linux uses `notify-send`; macOS uses `osascript`. Status and
logs remain available if desktop notifications fail.

To resolve divergence, stop editing and pause the affected clone:

```sh
vault-sync pause
cd /path/to/vault
git status
# Commit any further saved edits before integrating.
git fetch origin
git merge --no-autostash --no-edit origin/main
# Resolve conflicts if needed, then git add and git merge --continue.
vault-sync resume
vault-sync now
```

A rewritten remote uses the same steps, but decide which history to keep first:
merging brings back any commits the force-push removed.

To return to plugin automation, pause vault-sync, then stop its service:

```sh
# Linux
systemctl --user disable --now vault-sync.service
# macOS
launchctl bootout "gui/$(id -u)/com.gcavanunez.vault-sync"
```

Check Git status before resuming the plugin's automatic routines.

## Development

```sh
cargo fmt --check
cargo clippy --all-targets --locked -- -D warnings
cargo test --locked
cargo run -- --help
```

CI runs the same checks on Linux (x64 and arm64) and macOS (Apple Silicon and
Intel) for every push and pull request. The suite exercises signals, process
groups, file locks and the native watcher, which differ between the two systems.

The tests use disposable local Git repositories, including real competing pushes,
edits during fetch/push, timeouts, process termination, inherited locks, watcher
delivery, remote polling, and daemon pause/resume. They never use the real vault.
`tests/sync.rs` treats the executable as a black box, so it can check any build:

```sh
VAULT_SYNC_TEST_BINARY=/path/to/vault-sync cargo test --test sync --test install
```

## Releases

Bump `version` in `Cargo.toml`, commit, and push a matching tag:

```sh
git tag v0.4.0
git push origin v0.4.0
```

The release workflow checks that the tag matches the crate version, builds on
each platform, runs the whole test suite against each executable it is about to
ship, attests the archives, and publishes them with a `SHA256SUMS` file. Each
archive contains `bin/vault-sync`, `install.sh`, `config.example.json`, this
README and the license.

## Source layout

The modules form layers; see [Code layout](docs/architecture.md#7-code-layout).

- `src/main.rs`: command-line parsing and exit codes.
- `src/model.rs`, `scheduling.rs`, `clock.rs`: pure state, policy and time formatting.
- `src/control.rs`, `ipc.rs`: daemon client, standalone fallback, socket protocol.
- `src/daemon.rs`: watcher, scheduler and the serialized worker.
- `src/git.rs`, `sync.rs`: Git commands and fast-forward-only synchronization.
- `src/process.rs`, `interrupt.rs`, `lock.rs`, `storage.rs`: process lifecycle, signals, ownership, persistence.
- `src/config.rs`, `notifications.rs`, `service.rs`, `log.rs`: settings, notifications, systemd and launchd definitions, logging.
- `tests/`: black-box tests of the compiled executable and its installer.
