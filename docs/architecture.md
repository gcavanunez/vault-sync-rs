# Architecture

vault-sync is one executable with two roles. As a **daemon** it runs all day
under systemd or launchd, watching the vault and syncing it. As a **command**
(`now`, `status`, `pause`, `resume`) it asks the daemon to act, or acts by
itself when no daemon is running. Either way the work is done by running `git`;
vault-sync never talks to GitHub directly.

The diagrams go from the big picture to the details:

1. [Two computers, one remote](#1-two-computers-one-remote)
2. [Inside the daemon](#2-inside-the-daemon)
3. [What a command does](#3-what-a-command-does)
4. [One sync pass](#4-one-sync-pass)
5. [When passes run](#5-when-passes-run)
6. [Ownership and shutdown](#6-ownership-and-shutdown)
7. [Code layout](#7-code-layout)

## 1. Two computers, one remote

```mermaid
flowchart TB
    subgraph A["Computer A"]
        editorA["Editor (Obsidian)"] -- "saves notes" --> cloneA[("Vault clone")]
        cloneA <-->|"watches, commits,<br/>fast-forwards"| daemonA["vault-sync daemon<br/>(systemd or launchd)"]
        cliA["vault-sync now / status"] -. "Unix socket" .-> daemonA
    end

    subgraph B["Computer B"]
        editorB["Editor (Obsidian)"] -- "saves notes" --> cloneB[("Vault clone")]
        cloneB <-->|"watches, commits,<br/>fast-forwards"| daemonB["vault-sync daemon<br/>(systemd or launchd)"]
        cliB["vault-sync now / status"] -. "Unix socket" .-> daemonB
    end

    daemonA <-->|"git fetch and push<br/>over SSH"| remote[("GitHub repository<br/>one branch")]
    daemonB <-->|"git fetch and push<br/>over SSH"| remote
```

Each computer has its own clone and its own daemon, and the daemons never talk
to each other: the GitHub repository is the only meeting point. A daemon only
moves the branch forward. It commits local edits, fetches, fast-forwards when
the remote is ahead and pushes when it is ahead itself. It never merges,
rebases or force-pushes, so if both computers commit before either has synced,
it stops and reports **diverged** for a person to resolve.

## 2. Inside the daemon

```mermaid
flowchart TB
    fs["Watcher thread<br/>inotify or FSEvents"]
    sock["Socket listener<br/>one thread per client"]
    shared[["Shared state<br/>last result, deadlines,<br/>running, queued, paused"]]
    sched["Scheduler thread<br/>sleeps until the next deadline"]
    queue[/"Job queue<br/>automatic, now, pause, resume"/]
    worker["Worker thread<br/>the only place Git runs"]
    git["git child processes<br/>own process group"]
    meta[("Git metadata<br/>vault-sync/lock<br/>vault-sync/daemon-state.json")]

    fs -- "a note changed" --> shared
    sock -- "status: read directly" --> shared
    shared -. "wakes it early" .-> sched
    sched -- "deadline reached" --> queue
    sock -- "now, pause, resume<br/>(client waits for the reply)" --> queue
    queue -- "one job at a time" --> worker
    worker -- "runs" --> git
    worker -- "saves the result" --> meta
    worker -- "new result,<br/>worker free" --> shared
```

| Thread | Job |
| --- | --- |
| Main | Starts everything, then waits; runs shutdown |
| Worker | Runs every Git command and saves every result, one job at a time |
| Scheduler | Sleeps until the earliest deadline, then queues an automatic pass |
| Watcher | Turns file events into "something changed" |
| Socket listener | Answers `status` and queues the other commands, one thread per client |
| Signals | Turns SIGINT, SIGTERM and SIGHUP into an interrupt (see [section 6](#6-ownership-and-shutdown)) |

- All Git work happens on the worker, so timers, file events and CLI commands
  can never run Git at the same time.
- The scheduler does not poll. It is woken early only when an edit starts a new
  batch or the worker becomes free, and it re-reads the clock at least every
  5 seconds, so deadlines that passed while the computer slept fire soon after
  it wakes.
- `status` is answered straight from shared state, so it responds immediately
  even during a slow fetch.
- File reads and changes inside `.git` are ignored. Otherwise Git's own
  activity during a pass would schedule another pass, forever.

## 3. What a command does

```mermaid
sequenceDiagram
    actor You
    participant CLI as vault-sync now
    participant Daemon as daemon
    participant Lock as repository lock
    participant Git as git

    You->>CLI: vault-sync now
    CLI->>Git: find the repository (rev-parse, remote)
    CLI->>Daemon: connect to the Unix socket
    alt a daemon is listening
        CLI->>Daemon: request "now"
        Note over Daemon: waits behind any active work
        Daemon->>Git: sync pass
        Daemon-->>CLI: status with the new result
    else no daemon (socket missing or refused)
        CLI->>Lock: try to take it
        alt held by another process or a surviving Git child
            Lock-->>CLI: busy, exit 3
        else taken
            CLI->>Git: sync pass
            CLI->>CLI: save daemon-state.json, release the lock
        end
    end
    CLI-->>You: exit 0 synced, 3 pending or paused, 1 failed
```

`status` takes the same path, but a daemon answers it immediately, and without
a daemon it only reads `daemon-state.json` and checks whether the lock is held.
It never takes the lock or writes anything. `pause` and `resume` wait behind
active work, then save the flag.

## 4. One sync pass

```mermaid
flowchart TD
    start(["Pass starts"]) --> isPaused{"Paused?"}
    isPaused -- yes --> paused["paused"]
    isPaused -- no --> ready{"Repository<br/>ready?"}
    ready -- "no: merge, rebase or<br/>conflict in progress,<br/>or off the branch" --> blocked1["blocked"]
    ready -- yes --> stage["git add -A, then commit<br/>if anything changed"]
    stage --> allowed{"Remote attempt<br/>allowed?"}
    allowed -- "no: backing off" --> offline1["offline<br/>(commit kept locally)"]
    allowed -- "yes" --> fetch["fetch into<br/>refs/vault-sync/incoming"]
    fetch -- "error or timeout" --> offline2["offline"]
    fetch -- fetched --> rewritten{"Remote history<br/>rewritten?"}
    rewritten -- yes --> blocked2["blocked"]
    rewritten -- no --> compare{"Local vs<br/>remote?"}
    compare -- "both have<br/>new commits" --> diverged1["diverged"]
    compare -- "remote ahead,<br/>new local edits" --> pending1["pending"]
    compare -- "remote ahead,<br/>clean tree" --> ff["fast-forward"]
    compare -- "equal or<br/>local ahead" --> toPush{"Anything<br/>to push?"}
    ff --> toPush
    toPush -- yes --> push["push the snapshot"]
    toPush -- no --> fresh{"Edited since<br/>the snapshot?"}
    push -- accepted --> fresh
    push -- rejected --> refetch["fetch and<br/>compare again"]
    refetch -- "remote moved" --> diverged2["diverged"]
    refetch -- "still rejected" --> blocked3["blocked"]
    refetch -- "up to date" --> fresh
    fresh -- yes --> pending2["pending"]
    fresh -- no --> synced["synced"]

    classDef good fill:#c8efc4,stroke:#2e7d32,color:#111
    classDef waiting fill:#fdeeb5,stroke:#a77b00,color:#111
    classDef stop fill:#f9cfcf,stroke:#b71c1c,color:#111
    class synced good
    class paused,pending1,pending2,offline1,offline2 waiting
    class blocked1,blocked2,blocked3,diverged1,diverged2 stop
```

Each outcome appears next to the step that produces it, so the same outcome
can appear more than once. The remote is skipped only while backing off after
network failures; `vault-sync now` always tries it.

| Outcome | Meaning | `now` exits | Notifies |
| --- | --- | --- | --- |
| synced | The snapshot is on the remote and nothing newer is pending | 0 | No |
| pending | Synced, but newer edits appeared during the pass (or a fast-forward was deferred because of them) | 3 | No |
| paused | Automation is paused | 3 | No |
| offline | The network failed, or the pass is backing off; local commits are kept | 1 | On the third failure in a row |
| diverged | Both sides have new commits; needs a person | 1 | When it first happens |
| blocked | Anything else that needs a person: a manual Git operation, conflicts, a rewritten remote, a push the remote rejects | 1 | When it first happens |

Every outcome is saved to `daemon-state.json`. A fetch lands in
`refs/vault-sync/incoming` first, and the remote-tracking branch only advances
after the new history is checked to extend the old one; that is how a
force-pushed remote is caught before anything is built on it.

## 5. When passes run

```mermaid
flowchart LR
    save["A note is saved"] --> quiet["10 s after the last save<br/>(quiet period)"]
    save --> checkpoint["60 s after the first save<br/>(continuous editing)"]
    anyPass["Any pass starts"] --> reconcile["5 min later<br/>(catch missed events)"]
    remoteDone["A remote attempt ends"] --> poll["60 s later<br/>(repeated failures back off<br/>to 2 min, 4 min, then 5 min)"]
    quiet & checkpoint & reconcile & poll --> earliest{{"Next pass:<br/>the earliest deadline"}}
    earliest --> free{"Paused, running,<br/>or already queued?"}
    free -- yes --> hold["wait for resume<br/>or for the worker"]
    free -- no --> queueIt["queue an automatic pass"]
    now["vault-sync now"] --> queueNow["queue a pass that<br/>skips the backoff"]
```

Saving keeps pushing the quiet period back, so a burst of edits becomes one
commit; the 60-second checkpoint caps how long continuous typing can postpone
it. Remote polling exists because file events cannot see pushes from the other
computer. While the remote is failing, edits are still committed locally on
schedule; only the network attempts back off. All values are configurable in
`config.json`.

## 6. Ownership and shutdown

Only one vault-sync process may run Git in a clone at a time. That is enforced
by an OS file lock (`flock`) on `vault-sync/lock`, which every Git child
inherits as descriptor 3. The lock is released only when the last process
holding that descriptor exits, so ownership survives a crash of vault-sync
itself.

A normal stop (`systemctl stop`, Ctrl-C):

```mermaid
sequenceDiagram
    participant OS as service manager
    participant Sig as signal thread
    participant Worker as worker
    participant Git as git process group
    participant Main as main thread

    Note over Worker,Git: Git holds the lock through descriptor 3
    OS->>Sig: SIGTERM
    Sig->>Worker: interrupt
    Sig->>Main: interrupt
    Worker->>Git: SIGTERM to the whole group
    alt exits within 2 seconds
        Git-->>Worker: exited
    else still running, for example a hook ignoring SIGTERM
        Worker->>Git: SIGKILL to the whole group
        Git-->>Worker: exited
    end
    Worker->>Worker: reap the child, discard the unfinished pass
    Main->>Main: join the worker, remove the socket, stop the watcher
    Main->>Main: close the lock, exit 130
```

A crash or `kill -9`, where no cleanup runs:

```mermaid
sequenceDiagram
    participant Old as daemon
    participant Git as git child
    participant Lock as lock file
    participant New as restarted daemon

    Old->>Lock: take the lock
    Old->>Git: start, passing the lock as descriptor 3
    Note over Old: killed, no cleanup runs
    Note over Git,Lock: the lock stays held through the inherited descriptor
    New->>Lock: try to take it
    Lock-->>New: busy, exit 3 (the service manager retries)
    Git->>Lock: finishes and exits, the lock is released
    New->>Lock: take the lock on the next start
    New->>New: replace the stale socket and carry on
```

This is why a restarted daemon or a `vault-sync now` can report *busy* for a
moment after a crash: it is waiting for the orphaned Git command, not for a
stale lock file. Never delete the lock file; its inode is what the processes
share.

## 7. Code layout

```mermaid
flowchart TB
    main["Entry point<br/>main.rs"]
    roles["Roles<br/>daemon.rs, control.rs<br/>service.rs"]
    syncing["Synchronization<br/>sync.rs, git.rs<br/>notifications.rs"]
    system["System boundary<br/>process.rs, ipc.rs, lock.rs<br/>storage.rs, config.rs<br/>interrupt.rs, log.rs"]
    pure["Pure logic<br/>model.rs, scheduling.rs<br/>clock.rs"]

    main --> roles --> syncing --> system --> pure
```

Each layer only uses layers below it (sometimes skipping one), never above.
Pure logic has no I/O and carries most of the unit tests; everything that
touches the operating system lives in the system boundary. `tests/sync.rs`
checks the compiled executable from the outside, with real Git repositories.

| Module | Responsibility |
| --- | --- |
| `main.rs` | Parses arguments, prints results, maps failures to exit codes |
| `daemon.rs` | The threads, job queue and watcher from [section 2](#2-inside-the-daemon) |
| `control.rs` | `now`, `status`, `pause`, `resume`: ask the daemon or act standalone |
| `service.rs` | Renders and installs the systemd unit or launchd agent |
| `sync.rs` | One sync pass, as in [section 4](#4-one-sync-pass) |
| `git.rs` | Repository discovery and every Git command the pass needs |
| `notifications.rs` | Decides when a result deserves a desktop notification, and sends it |
| `process.rs` | Runs a child with a timeout, an output cap and process-group cleanup |
| `ipc.rs` | The Unix socket: path, request and reply |
| `lock.rs` | Takes and checks the `flock` |
| `storage.rs` | Reads and atomically writes `daemon-state.json` |
| `config.rs` | Finds, parses and validates `config.json` |
| `interrupt.rs` | Turns signals into an interrupt that blocking work subscribes to |
| `log.rs` | Log lines on stderr for journald or launchd |
| `model.rs` | State, outcomes, failures, and the socket's wire format |
| `scheduling.rs` | Deadlines, quiet period, checkpoint and backoff |
| `clock.rs` | UTC timestamps and the local time in commit subjects |
