use crate::model::{Failure, Kind, failure};
use serde::Deserialize;
use serde_json::{Map, Value, json};
use std::env;
use std::ffi::{CStr, OsString};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Settings {
    pub vault_path: String,
    pub device_name: String,
    pub remote: String,
    pub branch: String,
    pub quiet_ms: u64,
    pub checkpoint_ms: u64,
    pub remote_poll_ms: u64,
    pub reconcile_ms: u64,
    pub command_timeout_ms: u64,
    pub request_timeout_ms: u64,
    pub retry_max_ms: u64,
    pub notify_after_failures: u64,
    pub notifications: bool,
    pub path: String,
    pub ssh_command: String,
}

impl Settings {
    pub fn command_timeout(&self) -> Duration {
        Duration::from_millis(self.command_timeout_ms)
    }

    pub fn request_timeout(&self) -> Duration {
        Duration::from_millis(self.request_timeout_ms)
    }

    fn validate(&self) -> Result<(), String> {
        for (name, value) in [
            ("vaultPath", &self.vault_path),
            ("deviceName", &self.device_name),
            ("remote", &self.remote),
            ("branch", &self.branch),
            ("path", &self.path),
            ("sshCommand", &self.ssh_command),
        ] {
            if value.is_empty() {
                return Err(format!("{name} must be a non-empty string"));
            }
        }

        for (name, value) in [
            ("quietMs", self.quiet_ms),
            ("checkpointMs", self.checkpoint_ms),
            ("remotePollMs", self.remote_poll_ms),
            ("reconcileMs", self.reconcile_ms),
            ("commandTimeoutMs", self.command_timeout_ms),
            ("requestTimeoutMs", self.request_timeout_ms),
            ("retryMaxMs", self.retry_max_ms),
            ("notifyAfterFailures", self.notify_after_failures),
        ] {
            if value == 0 {
                return Err(format!("{name} must be a positive integer"));
            }
        }

        Ok(())
    }
}

/// Every setting except `vaultPath`, as written in `config.json`.
pub fn defaults() -> Map<String, Value> {
    let Value::Object(map) = json!({
        "deviceName": hostname(),
        "remote": "origin",
        "branch": "main",
        "quietMs": 10_000,
        "checkpointMs": 60_000,
        "remotePollMs": 60_000,
        "reconcileMs": 300_000,
        "commandTimeoutMs": 300_000,
        "requestTimeoutMs": 900_000,
        "retryMaxMs": 300_000,
        "notifyAfterFailures": 3,
        "notifications": true,
        "path": "/opt/homebrew/bin:/usr/local/bin:/usr/bin:/bin:/usr/sbin:/sbin",
        "sshCommand": "ssh -o BatchMode=yes -o ConnectTimeout=10 -o ServerAliveInterval=15 -o ServerAliveCountMax=2",
    }) else {
        unreachable!("a JSON object literal")
    };
    map
}

/// Parses a config file's text, applying defaults and rejecting unknown keys.
pub fn parse(text: &str) -> Result<Settings, String> {
    let Value::Object(overrides) = serde_json::from_str(text).map_err(|error| error.to_string())?
    else {
        return Err("expected a JSON object".into());
    };
    let mut merged = defaults();
    merged.extend(overrides);
    let settings: Settings =
        serde_json::from_value(Value::Object(merged)).map_err(|error| error.to_string())?;
    settings.validate()?;
    Ok(settings)
}

/// `$VAULT_SYNC_CONFIG`, else `${XDG_CONFIG_HOME:-~/.config}/vault-sync/config.json`.
pub fn config_path() -> Result<PathBuf, Failure> {
    let file = match non_empty_var("VAULT_SYNC_CONFIG") {
        Some(file) => PathBuf::from(file),
        None => config_home()?.join("vault-sync/config.json"),
    };
    absolute(&file)
}

pub fn config_home() -> Result<PathBuf, Failure> {
    match non_empty_var("XDG_CONFIG_HOME") {
        Some(base) => absolute(Path::new(&base)),
        None => Ok(home_dir()?.join(".config")),
    }
}

pub fn load() -> Result<(Settings, PathBuf), Failure> {
    let file = config_path()?;
    let text = fs::read_to_string(&file).map_err(|error| {
        Failure::new(
            Kind::Config,
            format!("Cannot read {}: {error}", file.display()),
        )
    })?;
    let settings = parse(&text).map_err(|error| {
        Failure::new(Kind::Config, format!("Invalid {}: {error}", file.display()))
    })?;

    if !Path::new(&settings.vault_path).is_absolute() {
        return failure(Kind::Config, "vaultPath must be absolute");
    }

    if settings.quiet_ms > settings.checkpoint_ms {
        return failure(Kind::Config, "quietMs must not exceed checkpointMs");
    }

    Ok((settings, file))
}

pub fn home_dir() -> Result<PathBuf, Failure> {
    env::home_dir()
        .filter(|home| !home.as_os_str().is_empty())
        .ok_or_else(|| Failure::new(Kind::Config, "Cannot determine the home directory"))
}

fn non_empty_var(name: &str) -> Option<OsString> {
    env::var_os(name).filter(|value| !value.is_empty())
}

fn absolute(path: &Path) -> Result<PathBuf, Failure> {
    std::path::absolute(path).map_err(|error| Failure::new(Kind::Config, error.to_string()))
}

fn hostname() -> String {
    let mut buffer = [0_u8; 256];
    // SAFETY: gethostname writes at most `len` bytes into the buffer.
    let result = unsafe { libc::gethostname(buffer.as_mut_ptr().cast(), buffer.len()) };

    if result != 0 {
        return String::new();
    }

    CStr::from_bytes_until_nul(&buffer)
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_vault_path_is_required() {
        let settings = parse(r#"{"vaultPath":"/vault","deviceName":"desk"}"#).unwrap();
        assert_eq!(settings.vault_path, "/vault");
        assert_eq!(settings.device_name, "desk");
        assert_eq!(
            (settings.remote.as_str(), settings.branch.as_str()),
            ("origin", "main")
        );
        assert_eq!(settings.quiet_ms, 10_000);
        assert!(settings.notifications);
        assert!(
            parse(r#"{"deviceName":"desk"}"#)
                .unwrap_err()
                .contains("vaultPath")
        );
    }

    #[test]
    fn unknown_keys_and_invalid_values_are_rejected() {
        for text in [
            r#"{"vaultPath":"/v","command":"/bin/sh"}"#,
            r#"{"vaultPath":"/v","quietMs":0}"#,
            r#"{"vaultPath":"/v","quietMs":-5}"#,
            r#"{"vaultPath":"/v","quietMs":1.5}"#,
            r#"{"vaultPath":"/v","notifications":"yes"}"#,
            r#"{"vaultPath":"/v","branch":""}"#,
            r#"["/v"]"#,
            "not json",
        ] {
            assert!(parse(text).is_err(), "{text}");
        }
    }

    #[test]
    fn the_example_config_is_valid() {
        let text = include_str!("../config.example.json");
        let settings = parse(text).unwrap();
        let example: Map<String, Value> = serde_json::from_str(text).unwrap();
        let mut keys: Vec<_> = defaults().keys().cloned().collect();
        keys.push("vaultPath".into());
        keys.sort();
        let mut documented: Vec<_> = example.keys().cloned().collect();
        documented.sort();
        assert_eq!(documented, keys, "config.example.json lists every setting");
        assert_eq!(settings.ssh_command, defaults()["sshCommand"]);
    }
}
