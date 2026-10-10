use serde::{Deserialize, Deserializer, Serialize};
use std::fmt;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Config,
    Process,
    Timeout,
    Offline,
    Diverged,
    Blocked,
    Pending,
    Busy,
    Unavailable,
    /// A shutdown signal stopped the operation; it is never recorded as a sync result.
    Interrupted,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Failure {
    pub kind: Kind,
    pub message: String,
}

impl Failure {
    pub fn new(kind: Kind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
        }
    }

    pub fn interrupted() -> Self {
        Self::new(Kind::Interrupted, "Interrupted")
    }

    pub fn exit_code(&self) -> i32 {
        match self.kind {
            Kind::Config => 2,
            Kind::Busy | Kind::Timeout => 3,
            Kind::Interrupted => 130,
            _ => 1,
        }
    }
}

impl fmt::Display for Failure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for Failure {}

pub fn failure<T>(kind: Kind, message: impl Into<String>) -> Result<T, Failure> {
    Err(Failure::new(kind, message))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Outcome {
    Idle,
    Synced,
    Pending,
    Paused,
    Diverged,
    Offline,
    Blocked,
}

impl Outcome {
    /// How a failed pass is recorded.
    pub fn of(kind: Kind) -> Self {
        match kind {
            Kind::Diverged => Self::Diverged,
            Kind::Pending => Self::Pending,
            Kind::Offline | Kind::Timeout => Self::Offline,
            _ => Self::Blocked,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Snapshot {
    pub commit: String,
    pub time: String,
}

/// Persisted in `daemon-state.json`. Nullable fields must be present, so a
/// truncated file is rejected instead of silently reset.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct State {
    pub paused: bool,
    pub outcome: Outcome,
    pub message: String,
    #[serde(deserialize_with = "present")]
    pub last_attempt: Option<String>,
    #[serde(deserialize_with = "present")]
    pub last_contact: Option<String>,
    #[serde(deserialize_with = "present")]
    pub last_synced: Option<Snapshot>,
    #[serde(deserialize_with = "present")]
    pub last_clean: Option<String>,
    pub failures: i64,
}

// A custom deserializer stops serde from treating a missing Option as null.
fn present<'de, D: Deserializer<'de>, T: Deserialize<'de>>(
    input: D,
) -> Result<Option<T>, D::Error> {
    Option::deserialize(input)
}

impl Default for State {
    fn default() -> Self {
        Self {
            paused: false,
            outcome: Outcome::Idle,
            message: "No sync recorded".into(),
            last_attempt: None,
            last_contact: None,
            last_synced: None,
            last_clean: None,
            failures: 0,
        }
    }
}

impl State {
    pub fn with_paused(&self, paused: bool) -> Self {
        Self {
            paused,
            outcome: if paused {
                Outcome::Paused
            } else {
                Outcome::Idle
            },
            message: if paused {
                "Automation is paused"
            } else {
                "Automation resumed"
            }
            .into(),
            ..self.clone()
        }
    }

    /// Exit status of `now`: 0 for a clean sync, 3 when pending or paused, 1 otherwise.
    pub fn exit_code(&self) -> i32 {
        if self.paused || self.outcome == Outcome::Pending {
            3
        } else if self.outcome == Outcome::Synced {
            0
        } else {
            1
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Watcher {
    Active,
    Degraded,
}

pub const PROTOCOL: u8 = 1;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Status {
    pub protocol: u8,
    pub vault: String,
    pub pid: u32,
    pub started_at: String,
    pub running: bool,
    pub watcher: Watcher,
    #[serde(deserialize_with = "present")]
    pub watcher_error: Option<String>,
    #[serde(deserialize_with = "present")]
    pub next_remote_attempt: Option<String>,
    pub state: State,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Command {
    Status,
    Now,
    Pause,
    Resume,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Request {
    pub command: Command,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "_tag", rename_all = "lowercase")]
pub enum Reply {
    Ok { status: Box<Status> },
    Error { message: String },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn failures_map_to_outcomes_and_exit_codes() {
        assert_eq!(Outcome::of(Kind::Timeout), Outcome::Offline);
        assert_eq!(Outcome::of(Kind::Offline), Outcome::Offline);
        assert_eq!(Outcome::of(Kind::Diverged), Outcome::Diverged);
        assert_eq!(Outcome::of(Kind::Pending), Outcome::Pending);
        assert_eq!(Outcome::of(Kind::Process), Outcome::Blocked);

        for (kind, code) in [
            (Kind::Config, 2),
            (Kind::Busy, 3),
            (Kind::Timeout, 3),
            (Kind::Blocked, 1),
            (Kind::Interrupted, 130),
        ] {
            assert_eq!(Failure::new(kind, "").exit_code(), code);
        }
    }

    #[test]
    fn now_exit_codes_follow_the_recorded_outcome() {
        let state = |outcome, paused| State {
            outcome,
            paused,
            ..State::default()
        };
        assert_eq!(state(Outcome::Synced, false).exit_code(), 0);
        assert_eq!(state(Outcome::Pending, false).exit_code(), 3);
        assert_eq!(state(Outcome::Paused, true).exit_code(), 3);
        assert_eq!(state(Outcome::Offline, false).exit_code(), 1);
    }

    #[test]
    fn state_round_trips_with_camel_case_fields() {
        let state = State {
            last_synced: Some(Snapshot {
                commit: "abc".into(),
                time: "t".into(),
            }),
            ..State::default()
        };
        let text = serde_json::to_string(&state).unwrap();
        assert!(text.contains("\"lastSynced\":{\"commit\":\"abc\",\"time\":\"t\"}"));
        assert!(text.contains("\"lastClean\":null"));
        assert_eq!(serde_json::from_str::<State>(&text).unwrap(), state);
    }

    #[test]
    fn truncated_or_invalid_state_is_rejected() {
        let complete = r#"{"paused":false,"outcome":"idle","message":"m","lastAttempt":null,"lastContact":null,"lastSynced":null,"lastClean":null,"failures":0}"#;
        assert!(serde_json::from_str::<State>(complete).is_ok());
        assert!(
            serde_json::from_str::<State>(&complete.replace(r#""lastClean":null,"#, "")).is_err()
        );
        assert!(serde_json::from_str::<State>(&complete.replace("idle", "sleeping")).is_err());
        assert!(serde_json::from_str::<State>("{").is_err());
    }

    #[test]
    fn replies_use_the_tagged_wire_format() {
        let error = Reply::Error {
            message: "full".into(),
        };
        assert_eq!(
            serde_json::to_string(&error).unwrap(),
            r#"{"_tag":"error","message":"full"}"#
        );
        let request: Request = serde_json::from_str(r#"{"command":"pause"}"#).unwrap();
        assert_eq!(request.command, Command::Pause);
        assert!(serde_json::from_str::<Request>(r#"{"command":"push"}"#).is_err());
    }
}
