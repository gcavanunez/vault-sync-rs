use crate::config::Settings;
use crate::git::{self, Git, Repository};
use crate::ipc;
use crate::lock;
use crate::model::{Command, Failure, Kind, Reply, Request, State, Watcher, failure};
use crate::notifications;
use crate::storage;
use crate::sync;
use serde::Serialize;

/// What a control command reports, from the daemon or from a standalone run.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct View {
    pub daemon: bool,
    pub running: bool,
    pub state: State,
    #[serde(flatten)]
    pub live: Option<Live>,
}

/// Fields only a running daemon can report.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Live {
    pub watcher: Watcher,
    pub watcher_error: Option<String>,
    pub next_remote_attempt: Option<String>,
}

/// Asks the daemon when one is listening; otherwise acts directly under the lock.
pub fn control(settings: &Settings, command: Command) -> Result<View, Failure> {
    let repository = git::discover(settings)?;
    let socket = ipc::socket_path(&repository.common);

    match ipc::request(&socket, Request { command }, settings.request_timeout()) {
        Ok(Reply::Ok { status }) => Ok(View {
            daemon: true,
            running: status.running,
            state: status.state,
            live: Some(Live {
                watcher: status.watcher,
                watcher_error: status.watcher_error,
                next_remote_attempt: status.next_remote_attempt,
            }),
        }),
        Ok(Reply::Error { message }) => failure(Kind::Blocked, message),
        Err(error) if error.kind == Kind::Unavailable => standalone(settings, &repository, command),
        Err(error) => Err(error),
    }
}

fn standalone(
    settings: &Settings,
    repository: &Repository,
    command: Command,
) -> Result<View, Failure> {
    let lock_path = repository.metadata.join("lock");

    let paused = match command {
        Command::Status => {
            return Ok(View {
                daemon: false,
                running: lock::held(&lock_path)?,
                state: storage::read_state(&repository.metadata)?,
                live: None,
            });
        }
        Command::Now => None,
        Command::Pause => Some(true),
        Command::Resume => Some(false),
    };

    let lock = lock::acquire(&lock_path)?;
    let before = storage::read_state(&repository.metadata)?;
    let state = match paused {
        Some(paused) => before.with_paused(paused),
        None => sync::sync_once(
            &Git::new(settings, repository, lock.fd()),
            settings,
            &before,
            true,
        )?,
    };
    storage::write_state(&repository.metadata, &state)?;
    notifications::notify(settings, &before, &state);
    drop(lock);

    Ok(View {
        daemon: false,
        running: false,
        state,
        live: None,
    })
}

/// Human-readable form of a view, as printed without `--json`.
pub fn describe(view: &View, command: Command) -> String {
    let yes_no = |value| if value { "yes" } else { "no" };
    let mut lines = vec![
        format!(
            "Daemon: {}",
            if view.daemon { "running" } else { "stopped" }
        ),
        format!("Sync running: {}", yes_no(view.running)),
        format!(
            "Automation: {}",
            if view.state.paused {
                "paused"
            } else {
                "active"
            }
        ),
        view.state.message.clone(),
    ];

    if command == Command::Status {
        let state = &view.state;
        lines.push(format!(
            "Last contact: {}",
            state.last_contact.as_deref().unwrap_or("never")
        ));
        lines.push(format!(
            "Last synced snapshot: {}",
            state
                .last_synced
                .as_ref()
                .map_or("none", |snapshot| snapshot.commit.as_str())
        ));
        lines.push(format!(
            "Last clean completion: {}",
            state.last_clean.as_deref().unwrap_or("never")
        ));

        if let Some(live) = &view.live {
            let watcher = match live.watcher {
                Watcher::Active => "active",
                Watcher::Degraded => "degraded",
            };
            lines.push(match &live.watcher_error {
                Some(error) => format!("Watcher: {watcher} ({error})"),
                None => format!("Watcher: {watcher}"),
            });
            lines.push(format!(
                "Next remote check: {}",
                live.next_remote_attempt.as_deref().unwrap_or("paused")
            ));
        }

        lines.push("Recorded status only; no fetch or fresh working-tree check.".into());
    }

    lines.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn view(live: Option<Live>) -> View {
        View {
            daemon: live.is_some(),
            running: false,
            state: State::default(),
            live,
        }
    }

    #[test]
    fn json_includes_daemon_fields_only_from_a_daemon() {
        let standalone = serde_json::to_value(view(None)).unwrap();
        assert_eq!(standalone["daemon"], false);
        assert!(standalone.get("watcher").is_none());

        let live = serde_json::to_value(view(Some(Live {
            watcher: Watcher::Active,
            watcher_error: None,
            next_remote_attempt: Some("t".into()),
        })))
        .unwrap();
        assert_eq!(live["watcher"], "active");
        assert!(live["watcherError"].is_null());
        assert_eq!(live["nextRemoteAttempt"], "t");
        assert!(
            serde_json::to_string_pretty(&live)
                .unwrap()
                .contains("\"daemon\": true")
        );
    }

    #[test]
    fn status_text_reports_recorded_observations() {
        let text = describe(&view(None), Command::Status);
        assert!(text.starts_with(
            "Daemon: stopped\nSync running: no\nAutomation: active\nNo sync recorded\n"
        ));
        assert!(text.contains("Last synced snapshot: none"));
        assert!(text.ends_with("Recorded status only; no fetch or fresh working-tree check."));
        assert_eq!(describe(&view(None), Command::Now).lines().count(), 4);
    }
}
