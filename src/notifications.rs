use crate::config::Settings;
use crate::git;
use crate::log;
use crate::model::{Outcome, State};
use crate::process::{self, Options};
use std::path::Path;
use std::time::Duration;

/// Whether a state change deserves a desktop notification: a new blocker, or
/// the network failure that reaches the configured threshold.
pub fn worth_notifying(settings: &Settings, before: &State, after: &State) -> bool {
    let blocker = matches!(after.outcome, Outcome::Blocked | Outcome::Diverged);
    let changed = before.outcome != after.outcome || before.message != after.message;
    let threshold = after.outcome == Outcome::Offline
        && after.failures > before.failures
        && u64::try_from(after.failures) == Ok(settings.notify_after_failures);
    settings.notifications && ((blocker && changed) || threshold)
}

/// Best effort: delivery failures are logged, never returned.
pub fn notify(settings: &Settings, before: &State, after: &State) {
    if !worth_notifying(settings, before, after) {
        return;
    }

    let args: Vec<String> = if cfg!(target_os = "macos") {
        vec![
            "osascript".into(),
            "-e".into(),
            "on run argv\ndisplay notification (item 1 of argv) with title \"Vault sync\"\nend run"
                .into(),
            after.message.clone(),
        ]
    } else {
        vec![
            "notify-send".into(),
            "Vault sync".into(),
            after.message.clone(),
        ]
    };

    let result = process::run(
        &args,
        &Options {
            cwd: Path::new(&settings.vault_path),
            env: &git::environment(settings),
            timeout: Duration::from_secs(5),
            lock_fd: None,
        },
    );

    match result {
        Ok(output) if output.code == 0 => {}
        Ok(output) => log::warn("Notification delivery failed", output.stderr.trim()),
        Err(error) => log::warn("Notification delivery failed", &error.message),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config;

    fn settings() -> Settings {
        config::parse(r#"{"vaultPath":"/v","notifyAfterFailures":3}"#).unwrap()
    }

    fn state(outcome: Outcome, failures: i64, message: &str) -> State {
        State {
            outcome,
            failures,
            message: message.into(),
            ..State::default()
        }
    }

    #[test]
    fn new_blockers_notify_once() {
        let diverged = state(Outcome::Diverged, 1, "Histories diverged");
        assert!(worth_notifying(
            &settings(),
            &state(Outcome::Synced, 0, "ok"),
            &diverged
        ));
        assert!(!worth_notifying(&settings(), &diverged, &diverged));
    }

    #[test]
    fn network_failures_notify_at_the_threshold_only() {
        let offline = |failures| state(Outcome::Offline, failures, "offline");
        assert!(!worth_notifying(&settings(), &offline(1), &offline(2)));
        assert!(worth_notifying(&settings(), &offline(2), &offline(3)));
        assert!(!worth_notifying(&settings(), &offline(3), &offline(4)));
    }

    #[test]
    fn disabled_notifications_never_notify() {
        let mut quiet = settings();
        quiet.notifications = false;
        assert!(!worth_notifying(
            &quiet,
            &state(Outcome::Synced, 0, "ok"),
            &state(Outcome::Blocked, 1, "no")
        ));
    }
}
