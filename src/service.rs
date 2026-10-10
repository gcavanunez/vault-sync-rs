use crate::config;
use crate::model::{Failure, Kind, failure};
use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};

pub const LAUNCHD_LABEL: &str = "com.gcavanunez.vault-sync";

fn xml(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

fn unit_quote(value: &str) -> String {
    format!(
        "\"{}\"",
        value
            .replace('\\', "\\\\")
            .replace('"', "\\\"")
            .replace('%', "%%")
    )
}

pub fn render_systemd(binary: &str, config: &str) -> String {
    format!(
        "[Unit]
Description=Vault Git sync daemon
After=network.target

[Service]
Type=simple
ExecStart={} daemon
Environment={}
Restart=on-failure
RestartSec=10
KillMode=control-group
TimeoutStopSec=15
UMask=0077

[Install]
WantedBy=default.target
",
        unit_quote(binary).replace('$', "$$"),
        unit_quote(&format!("VAULT_SYNC_CONFIG={config}")),
    )
}

pub fn render_launchd(binary: &str, config: &str, home: &str) -> String {
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
<key>Label</key><string>{LAUNCHD_LABEL}</string>
<key>ProgramArguments</key><array><string>{}</string><string>daemon</string></array>
<key>EnvironmentVariables</key><dict><key>VAULT_SYNC_CONFIG</key><string>{}</string></dict>
<key>RunAtLoad</key><true/>
<key>KeepAlive</key><true/>
<key>ThrottleInterval</key><integer>10</integer>
<key>ExitTimeOut</key><integer>15</integer>
<key>StandardOutPath</key><string>{}</string>
<key>StandardErrorPath</key><string>{}</string>
</dict></plist>
"#,
        xml(binary),
        xml(config),
        xml(&format!("{home}/Library/Logs/vault-sync.log")),
        xml(&format!("{home}/Library/Logs/vault-sync.error.log")),
    )
}

fn text(path: &Path) -> Result<&str, Failure> {
    path.to_str().ok_or_else(|| {
        Failure::new(
            Kind::Config,
            format!("Path is not UTF-8: {}", path.display()),
        )
    })
}

/// Writes the host's service definition without starting it. An identical
/// existing definition is accepted; a different one or a symlink is refused.
pub fn install() -> Result<String, Failure> {
    let home = config::home_dir()?;
    let config_file = config::config_path()?;
    let binary =
        std::env::current_exe().map_err(|error| Failure::new(Kind::Config, error.to_string()))?;

    if binary.file_name().and_then(|name| name.to_str()) != Some("vault-sync") {
        return failure(
            Kind::Config,
            "Run service-install using the compiled vault-sync executable",
        );
    }

    let (destination, content): (PathBuf, String) = if cfg!(target_os = "linux") {
        (
            config::config_home()?.join("systemd/user/vault-sync.service"),
            render_systemd(text(&binary)?, text(&config_file)?),
        )
    } else if cfg!(target_os = "macos") {
        (
            home.join(format!("Library/LaunchAgents/{LAUNCHD_LABEL}.plist")),
            render_launchd(text(&binary)?, text(&config_file)?, text(&home)?),
        )
    } else {
        return failure(Kind::Config, "Only Linux and macOS are supported");
    };

    write_definition(&destination, &content, &home)
        .map_err(|error| Failure::new(Kind::Config, error.to_string()))?;

    Ok(format!(
        "Installed {}\nConfiguration: {}\nService activation is a separate step.",
        destination.display(),
        config_file.display()
    ))
}

fn write_definition(destination: &Path, content: &str, home: &Path) -> io::Result<()> {
    if let Some(parent) = destination.parent() {
        fs::create_dir_all(parent)?;
    }

    if cfg!(target_os = "macos") {
        fs::create_dir_all(home.join("Library/Logs"))?;
    }

    match fs::symlink_metadata(destination) {
        Ok(existing) => {
            if existing.file_type().is_symlink() || fs::read_to_string(destination)? != content {
                return Err(io::Error::other(format!(
                    "Refusing to replace a different service definition: {}",
                    destination.display()
                )));
            }
            Ok(())
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(destination)?
            .write_all(content.as_bytes()),
        Err(error) => Err(error),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn launchd_definition_escapes_paths_and_runs_a_persistent_daemon() {
        let home = "/Users/person & spaces";
        let text = render_launchd(
            &format!("{home}/vault-sync"),
            &format!("{home}/config.json"),
            home,
        );
        assert!(text.contains("/Users/person &amp; spaces/vault-sync</string><string>daemon"));
        assert!(text.contains("<key>KeepAlive</key><true/>"));
        assert!(text.contains("<key>VAULT_SYNC_CONFIG</key>"));
        assert!(!text.contains("StartCalendarInterval"));
    }

    #[test]
    fn systemd_quotes_paths_and_stops_the_whole_process_group() {
        let text = render_systemd(
            "/home/person % $/vault-sync",
            "/home/person % $/config.json",
        );
        assert!(text.contains(r#"ExecStart="/home/person %% $$/vault-sync" daemon"#));
        assert!(text.contains(r#"Environment="VAULT_SYNC_CONFIG=/home/person %% $/config.json""#));
        assert!(text.contains("KillMode=control-group"));
        assert!(text.contains("Restart=on-failure"));
    }
}
