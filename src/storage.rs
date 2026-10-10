use crate::model::{Failure, Kind, State, failure};
use std::fs::{self, DirBuilder, OpenOptions};
use std::io::{self, Write};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::Path;

const STATE_FILE: &str = "daemon-state.json";

pub fn exists(path: &Path) -> Result<bool, Failure> {
    match fs::metadata(path) {
        Ok(_) => Ok(true),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(error) => failure(Kind::Blocked, error.to_string()),
    }
}

/// Reads persisted state. A missing file starts fresh, paused if a legacy
/// `paused` marker file exists; a malformed file stops the operation.
pub fn read_state(metadata: &Path) -> Result<State, Failure> {
    let path = metadata.join(STATE_FILE);

    if !exists(&path)? {
        return Ok(State {
            paused: exists(&metadata.join("paused"))?,
            ..State::default()
        });
    }

    let text = fs::read_to_string(&path)
        .map_err(|error| Failure::new(Kind::Blocked, error.to_string()))?;
    serde_json::from_str(&text)
        .map_err(|error| Failure::new(Kind::Blocked, format!("Invalid persisted state: {error}")))
}

/// Atomically replaces the state file: write, flush to disk, then rename.
pub fn write_state(metadata: &Path, state: &State) -> Result<(), Failure> {
    let path = metadata.join(STATE_FILE);
    let temporary = metadata.join(format!("{STATE_FILE}.{}.tmp", std::process::id()));

    let write = || -> io::Result<()> {
        DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(metadata)?;
        let mut text = serde_json::to_string_pretty(state)?;
        text.push('\n');
        let mut file = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&temporary)?;
        file.write_all(text.as_bytes())?;
        file.sync_all()?;
        fs::rename(&temporary, &path)
    };

    let result = write();
    let _ = fs::remove_file(&temporary);
    result.map_err(|error| Failure::new(Kind::Blocked, format!("Cannot save sync state: {error}")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Outcome;

    fn scratch(name: &str) -> std::path::PathBuf {
        let dir =
            std::env::temp_dir().join(format!("vault-sync-storage-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        dir
    }

    #[test]
    fn missing_state_starts_fresh_and_imports_the_paused_marker() {
        let dir = scratch("fresh");
        assert_eq!(read_state(&dir).unwrap(), State::default());
        assert!(!dir.exists(), "reading never creates metadata");
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("paused"), "").unwrap();
        assert!(read_state(&dir).unwrap().paused);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn written_state_round_trips_privately_without_leftovers() {
        let dir = scratch("round-trip");
        let state = State {
            outcome: Outcome::Synced,
            failures: 2,
            ..State::default()
        };
        write_state(&dir, &state).unwrap();
        assert_eq!(read_state(&dir).unwrap(), state);
        let text = fs::read_to_string(dir.join(STATE_FILE)).unwrap();
        assert!(text.ends_with("}\n") && text.contains("\"paused\": false"));
        assert_eq!(fs::read_dir(&dir).unwrap().count(), 1);
        use std::os::unix::fs::PermissionsExt;
        let mode = fs::metadata(dir.join(STATE_FILE))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn malformed_state_fails_closed() {
        let dir = scratch("malformed");
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join(STATE_FILE), "{").unwrap();
        let error = read_state(&dir).unwrap_err();
        assert_eq!(error.kind, Kind::Blocked);
        assert!(error.message.starts_with("Invalid persisted state"));
        fs::remove_dir_all(dir).unwrap();
    }
}
