mod support;

use serde_json::{Value, json};
use std::fs;
use support::{Fixture, executable, process_missing, quote, text, wait_for, write};

#[test]
fn round_trip_preserves_commit_format_body_idempotency_and_disables_autostash() {
    let f = Fixture::new();
    write(f.a.join("new note.md"), "from linux\n");
    f.now(&f.a);
    let sha = f.git(&f.a, &["rev-parse", "HEAD"]);
    f.now(&f.a);
    assert_eq!(f.git(&f.a, &["rev-parse", "HEAD"]), sha);
    let subject = f.git(&f.a, &["log", "-1", "--format=%s"]);
    let timestamp = subject
        .strip_prefix("vault backup: linux ")
        .expect("commit prefix");
    assert_eq!(timestamp.len(), 19);
    for (index, byte) in timestamp.bytes().enumerate() {
        match index {
            4 | 7 => assert_eq!(byte, b'-'),
            10 => assert_eq!(byte, b' '),
            13 | 16 => assert_eq!(byte, b':'),
            _ => assert!(byte.is_ascii_digit()),
        }
    }
    assert!(
        f.git(&f.a, &["log", "-1", "--format=%b"])
            .contains("new note.md")
    );
    f.git(&f.b, &["config", "pull.rebase", "true"]);
    f.git(&f.b, &["config", "merge.autoStash", "true"]);
    f.now(&f.b);
    assert_eq!(text(f.b.join("new note.md")), "from linux\n");
    assert_eq!(f.state(&f.b)["lastSynced"]["commit"], sha);
}

#[test]
fn offline_edits_commit_locally_and_push_after_reconnect() {
    let f = Fixture::new();
    f.git(
        &f.a,
        &[
            "remote",
            "set-url",
            "origin",
            f.root.join("absent.git").to_str().unwrap(),
        ],
    );
    write(f.a.join("offline.md"), "keep this\n");
    f.sync(&f.a, "now", 1);
    assert_eq!(f.git(&f.a, &["show", "HEAD:offline.md"]), "keep this");
    assert!(f.state(&f.a)["lastSynced"].is_null());
    f.git(
        &f.a,
        &["remote", "set-url", "origin", f.remote.to_str().unwrap()],
    );
    f.now(&f.a);
    f.now(&f.b);
    assert_eq!(text(f.b.join("offline.md")), "keep this\n");
}

#[test]
fn divergence_preserves_both_local_notes_and_commits_without_integration_state() {
    let f = Fixture::new();
    write(f.a.join("shared.md"), "linux edit\n");
    write(f.b.join("shared.md"), "mac edit\n");
    f.now(&f.a);
    assert!(f.sync(&f.b, "now", 1).contains("Histories diverged"));
    assert_eq!(text(f.b.join("shared.md")), "mac edit\n");
    assert_eq!(f.git(&f.b, &["show", "HEAD:shared.md"]), "mac edit");
    for name in ["rebase-merge", "rebase-apply", "MERGE_HEAD"] {
        assert!(!f.b.join(".git").join(name).exists());
    }
}

#[test]
fn competing_push_is_diagnosed_as_divergence() {
    let f = Fixture::new();
    let gate = f.gate("push", false);
    write(f.b.join("mac.md"), "mac\n");
    let pending = f.start(&f.b, "now", &[]);
    gate.wait();
    write(f.a.join("linux.md"), "linux\n");
    f.now(&f.a);
    gate.open();
    let result = pending.result();
    assert_eq!(result.code, 1);
    assert!(result.stdout.contains("Histories diverged"));
    assert_eq!(f.git(&f.b, &["show", "HEAD:mac.md"]), "mac");
}

#[test]
fn rewritten_remote_history_blocks_without_pushing_over_it() {
    let f = Fixture::new();
    let rewound = f.git(&f.a, &["rev-parse", "HEAD"]);
    write(f.a.join("shared.md"), "published\n");
    f.now(&f.a);
    f.now(&f.b);
    f.git(&f.a, &["reset", "--hard", &rewound]);
    f.git(&f.a, &["push", "--force", "origin", "main"]);
    let output = f.sync(&f.b, "now", 1);
    let reply: Value = serde_json::from_str(&output).unwrap();
    assert_eq!(reply["state"]["outcome"], "blocked");
    assert!(output.contains("was rewritten"));
    assert_eq!(f.git(&f.remote, &["rev-parse", "main"]), rewound);
    assert_eq!(text(f.b.join("shared.md")), "published\n");
}

#[test]
fn edits_during_fetch_defer_fast_forward() {
    let f = Fixture::new();
    write(f.a.join("remote.md"), "remote\n");
    f.now(&f.a);
    let original = f.git(&f.b, &["rev-parse", "HEAD"]);
    let gate = f.gate("fetch", false);
    let pending = f.start(&f.b, "now", &[]);
    gate.wait();
    write(f.b.join("shared.md"), "still typing\n");
    gate.open();
    assert_eq!(pending.result().code, 3);
    assert_eq!(f.git(&f.b, &["rev-parse", "HEAD"]), original);
    assert_eq!(text(f.b.join("shared.md")), "still typing\n");
    assert!(f.state(&f.b)["lastSynced"].is_null());
}

#[test]
fn edits_during_push_are_not_reported_as_clean() {
    let f = Fixture::new();
    write(f.a.join("first.md"), "snapshot\n");
    let gate = f.gate("push", true);
    let pending = f.start(&f.a, "now", &[]);
    gate.wait();
    write(f.a.join("shared.md"), "newer edit\n");
    gate.open();
    assert_eq!(pending.result().code, 3);
    let state = f.state(&f.a);
    assert_eq!(state["outcome"], "pending");
    assert!(!state["lastSynced"].is_null());
    assert!(state["lastClean"].is_null());
}

#[test]
fn sigterm_stops_git_before_releasing_ownership() {
    let f = Fixture::new();
    let gate = f.gate("fetch", false);
    let pending = f.start(&f.a, "now", &[]);
    gate.wait();
    let pid = text(&gate.ready).trim().parse().unwrap();
    pending.stop();
    assert!(process_missing(pid));
    f.now(&f.a);
}

#[test]
fn sigkill_leaves_ownership_with_the_surviving_git_child() {
    let f = Fixture::new();
    let gate = f.gate("fetch", false);
    let pending = f.start(&f.a, "now", &[]);
    gate.wait();
    pending.signal(libc::SIGKILL);
    pending.wait_exit();
    assert_eq!(f.sync(&f.a, "now", 3), "");
    gate.open();
    pending.result();
    f.wait_for_unlock(&f.a);
    f.now(&f.a);
}

#[test]
fn command_timeout_kills_git_and_retains_the_local_checkpoint() {
    let f = Fixture::new();
    f.configure(&f.a, json!({"commandTimeoutMs": 500}));
    let gate = f.gate("fetch", false);
    write(f.a.join("slow.md"), "save locally\n");
    assert!(f.sync(&f.a, "now", 1).contains("exceeded 500ms"));
    let pid = text(&gate.ready).trim().parse().unwrap();
    assert!(process_missing(pid));
    assert_eq!(f.git(&f.a, &["show", "HEAD:slow.md"]), "save locally");
    f.now(&f.a);
}

#[test]
fn timeout_kills_a_hook_descendant_that_ignores_sigterm() {
    let f = Fixture::new();
    f.configure(&f.a, json!({"commandTimeoutMs": 700}));
    write(f.a.join("snapshot.md"), "snapshot\n");
    let pid_file = f.root.join("hook-pid");
    let hook = f.a.join(".git/hooks/pre-push");
    executable(
        &hook,
        &format!(
            "#!/bin/sh\ntrap '' TERM\necho $$ > {}\nwhile :; do sleep 1; done\n",
            quote(&pid_file)
        ),
    );
    f.sync(&f.a, "now", 1);
    let pid = text(&pid_file);
    let result = f.cmd("ps", &["-o", "stat=", "-p", pid.trim()]);
    assert!(result.code != 0 || result.stdout.starts_with('Z'));
    fs::remove_file(hook).unwrap();
    f.now(&f.a);
}

#[test]
fn pause_persists_and_resume_permits_a_checkpoint() {
    let f = Fixture::new();
    let original = f.git(&f.a, &["rev-parse", "HEAD"]);
    f.sync(&f.a, "pause", 0);
    write(f.a.join("paused.md"), "wait\n");
    f.sync(&f.a, "now", 3);
    assert_eq!(f.git(&f.a, &["rev-parse", "HEAD"]), original);
    assert!(f.sync(&f.a, "status", 0).contains("\"paused\": true"));
    f.sync(&f.a, "resume", 0);
    f.now(&f.a);
    assert_eq!(f.git(&f.a, &["show", "HEAD:paused.md"]), "wait");
}

#[test]
fn configuration_aliases_still_share_a_clone_lock() {
    let f = Fixture::new();
    let gate = f.gate("fetch", false);
    let pending = f.start(&f.a, "now", &[]);
    gate.wait();
    let alternative = f.root.join("other.json");
    assert!(f.sync(&f.a, "status", 0).contains("\"running\": true"));
    write(&alternative, text(f.config(&f.a)));
    let result = f
        .start(
            &f.a,
            "now",
            &[("VAULT_SYNC_CONFIG", alternative.as_os_str())],
        )
        .result();
    assert_eq!(result.code, 3);
    assert!(result.stderr.contains("locked"));
    gate.open();
    assert_eq!(pending.result().code, 0);
}

#[test]
fn subdirectory_configuration_cannot_stage_its_parent_repository() {
    let f = Fixture::new();
    let nested = f.a.join("nested");
    fs::create_dir(&nested).unwrap();
    f.configure(&f.a, json!({"vaultPath": nested}));
    write(f.a.join("pending.md"), "do not stage\n");
    f.sync(&f.a, "now", 2);
    assert_eq!(f.git(&f.a, &["diff", "--cached", "--name-only"]), "");
}

#[test]
fn invalid_branch_names_are_reported_as_configuration_errors() {
    let f = Fixture::new();
    f.configure(&f.a, json!({"branch": "bad..name"}));
    let result = f.start(&f.a, "now", &[]).result();
    assert_eq!(result.code, 2);
    assert!(result.stderr.contains("Invalid branch name: bad..name"));
}

#[test]
fn status_is_offline_and_read_only() {
    let f = Fixture::new();
    f.git(
        &f.a,
        &[
            "remote",
            "set-url",
            "origin",
            f.root.join("absent.git").to_str().unwrap(),
        ],
    );
    assert!(f.sync(&f.a, "status", 0).contains("\"daemon\": false"));
    assert!(!f.a.join(".git/vault-sync").exists());
}

#[test]
fn unfinished_rebase_takes_priority_over_detached_head() {
    let f = Fixture::new();
    write(f.a.join("shared.md"), "linux\n");
    f.now(&f.a);
    write(f.b.join("shared.md"), "mac\n");
    f.commit(&f.b);
    f.git(&f.b, &["fetch", "origin"]);
    assert_ne!(
        f.cmd(
            f.git_binary(),
            &["-C", f.b.to_str().unwrap(), "rebase", "origin/main"]
        )
        .code,
        0
    );
    let output = f.sync(&f.b, "now", 1);
    assert!(output.contains("Manual Git operation"));
    assert!(!output.contains("found detached HEAD"));
}

#[test]
fn hook_rejection_preserves_local_commits_and_is_not_a_push_race() {
    let f = Fixture::new();
    executable(f.remote.join("hooks/pre-receive"), "#!/bin/sh\nexit 1\n");
    write(f.a.join("local.md"), "preserve\n");
    let output = f.sync(&f.a, "now", 1);
    assert!(output.contains("Push failed"));
    assert!(!output.contains("Histories diverged"));
    assert!(f.state(&f.a)["lastSynced"].is_null());
}

#[test]
fn a_new_blocker_notifies_after_prior_network_failures() {
    let f = Fixture::new();
    let notifications = f.root.join("notifications");
    for name in ["osascript", "notify-send"] {
        executable(
            f.bin.join(name),
            &format!("#!/bin/sh\necho notice >> {}\n", quote(&notifications)),
        );
    }
    f.configure(&f.b, json!({"notifications": true}));
    f.git(
        &f.b,
        &[
            "remote",
            "set-url",
            "origin",
            f.root.join("absent.git").to_str().unwrap(),
        ],
    );
    f.sync(&f.b, "now", 1);
    f.sync(&f.b, "now", 1);
    assert!(!notifications.exists());
    f.sync(&f.b, "now", 1);
    assert_eq!(text(&notifications).trim().split('\n').count(), 1);
    f.git(
        &f.b,
        &["remote", "set-url", "origin", f.remote.to_str().unwrap()],
    );
    write(f.a.join("shared.md"), "linux\n");
    f.now(&f.a);
    write(f.b.join("shared.md"), "mac\n");
    f.sync(&f.b, "now", 1);
    f.sync(&f.b, "now", 1);
    assert_eq!(text(&notifications).trim().split('\n').count(), 2);
}

#[test]
fn malformed_persisted_state_fails_closed() {
    let f = Fixture::new();
    fs::create_dir(f.a.join(".git/vault-sync")).unwrap();
    write(f.a.join(".git/vault-sync/daemon-state.json"), "{");
    write(f.a.join("untouched.md"), "keep unstaged\n");
    f.sync(&f.a, "now", 1);
    assert_eq!(f.git(&f.a, &["diff", "--cached", "--name-only"]), "");
}

#[test]
fn notification_delivery_failure_preserves_machine_readable_sync_output() {
    let f = Fixture::new();
    for name in ["osascript", "notify-send"] {
        executable(f.bin.join(name), "#!/bin/sh\nexit 1\n");
    }
    f.configure(
        &f.a,
        json!({"notifications": true, "notifyAfterFailures": 1}),
    );
    f.git(
        &f.a,
        &[
            "remote",
            "set-url",
            "origin",
            f.root.join("absent.git").to_str().unwrap(),
        ],
    );
    let output = f.sync(&f.a, "now", 1);
    let reply: Value = serde_json::from_str(&output).unwrap();
    assert_eq!(reply["state"]["outcome"], "offline");
}

#[test]
fn native_watcher_syncs_nested_notes_before_the_remote_polling_deadline() {
    let f = Fixture::new();
    let daemon = f.start(&f.a, "daemon", &[]);
    wait_for(
        || f.a.join(".git/vault-sync/daemon-state.json").exists(),
        "startup sync",
    );
    assert!(
        f.sync(&f.a, "status", 0)
            .contains("\"watcher\": \"active\"")
    );
    fs::create_dir(f.a.join("nested")).unwrap();
    write(f.a.join("nested/note.md"), "watch me\n");
    wait_for(
        || {
            f.git(&f.remote, &["ls-tree", "-r", "--name-only", "main"])
                .contains("nested/note.md")
        },
        "watcher push",
    );
    daemon.stop();
}

#[test]
fn remote_polling_receives_a_handoff_and_pause_survives_restart() {
    let f = Fixture::new();
    f.configure(&f.a, json!({"remotePollMs": 800}));
    let daemon = f.start(&f.a, "daemon", &[]);
    wait_for(
        || f.a.join(".git/vault-sync/daemon-state.json").exists(),
        "startup sync",
    );
    fs::create_dir(f.a.join("nested")).unwrap();
    write(f.a.join("nested/note.md"), "watch me\n");
    wait_for(
        || {
            f.git(&f.remote, &["ls-tree", "-r", "--name-only", "main"])
                .contains("nested/note.md")
        },
        "watcher push",
    );
    f.now(&f.b);
    write(f.b.join("from-mac.md"), "poll me\n");
    f.now(&f.b);
    wait_for(|| f.a.join("from-mac.md").exists(), "remote poll");
    f.sync(&f.a, "pause", 0);
    daemon.stop();
    let restarted = f.start(&f.a, "daemon", &[]);
    wait_for(
        || f.sync(&f.a, "status", 0).contains("\"daemon\": true"),
        "daemon restart",
    );
    write(f.a.join("paused.md"), "wait\n");
    f.sync(&f.a, "now", 3);
    assert!(
        f.git(&f.a, &["status", "--porcelain"])
            .contains("paused.md")
    );
    f.sync(&f.a, "resume", 0);
    f.now(&f.a);
    assert_eq!(f.git(&f.a, &["status", "--porcelain"]), "");
    restarted.stop();
}

#[test]
fn daemon_restart_cannot_bypass_a_surviving_child_lock_and_recovers_its_stale_socket() {
    let f = Fixture::new();
    let gate = f.gate("fetch", false);
    let first = f.start(&f.a, "daemon", &[]);
    gate.wait();
    first.signal(libc::SIGKILL);
    first.wait_exit();
    let blocked = f.start(&f.a, "daemon", &[]).result();
    assert_eq!(blocked.code, 3);
    assert!(blocked.stderr.contains("locked"));
    gate.open();
    first.result();
    f.wait_for_unlock(&f.a);
    let restarted = f.start(&f.a, "daemon", &[]);
    wait_for(
        || f.sync(&f.a, "status", 0).contains("\"daemon\": true"),
        "socket recovery",
    );
    f.now(&f.a);
    restarted.stop();
}

#[test]
fn daemon_serializes_manual_requests_and_pause_waits_for_active_git() {
    let f = Fixture::new();
    let gate = f.gate("fetch", false);
    let daemon = f.start(&f.a, "daemon", &[]);
    gate.wait();
    assert!(f.sync(&f.a, "status", 0).contains("\"running\": true"));
    let second = f.start(&f.a, "now", &[]);
    let pause = f.start(&f.a, "pause", &[]);
    let duplicate = f.start(&f.a, "daemon", &[]);
    assert_eq!(duplicate.result().code, 3);
    assert!(!second.exited());
    assert!(!pause.exited());
    gate.open();
    // Independent clients may arrive in either order; now behind pause defers.
    assert!([0, 3].contains(&second.result().code));
    assert_eq!(pause.result().code, 0);
    assert_eq!(f.state(&f.a)["paused"], true);
    daemon.stop();
}
