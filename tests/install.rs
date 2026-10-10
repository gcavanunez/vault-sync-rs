#[cfg(any(target_os = "linux", target_os = "macos"))]
#[allow(dead_code)]
mod support;

#[cfg(any(target_os = "linux", target_os = "macos"))]
use std::fs;
#[cfg(any(target_os = "linux", target_os = "macos"))]
use std::process::Command;
#[cfg(any(target_os = "linux", target_os = "macos"))]
use support::{TempDir, binary, run, text, which, write};

#[test]
fn compiled_installer_preserves_config_and_refuses_different_service_definitions() {
    if !cfg!(any(target_os = "linux", target_os = "macos")) {
        return;
    }
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    check_install();
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn check_install() {
    let root = TempDir::new("vault-install-");
    let home = root.0.join("home & spaces");
    fs::create_dir(&home).unwrap();
    let config = home.join(".config/vault-sync/config.json");
    let install = || {
        run(Command::new("sh")
            .arg("install.sh")
            .arg(binary())
            .current_dir(env!("CARGO_MANIFEST_DIR"))
            .env("HOME", &home)
            .env("XDG_CONFIG_HOME", home.join(".config"))
            .env("VAULT_SYNC_CONFIG", &config))
    };
    let first = install();
    assert_eq!(first.code, 0, "{}{}", first.stdout, first.stderr);
    write(&config, "{\"deviceName\":\"custom\"}\n");
    let second = install();
    assert_eq!(second.code, 0, "{}{}", second.stdout, second.stderr);
    assert_eq!(text(&config), "{\"deviceName\":\"custom\"}\n");
    let definition = if cfg!(target_os = "macos") {
        home.join("Library/LaunchAgents/com.gcavanunez.vault-sync.plist")
    } else {
        home.join(".config/systemd/user/vault-sync.service")
    };
    assert!(text(&definition).contains("daemon"));
    if cfg!(target_os = "linux")
        && let Some(analyze) = which("systemd-analyze")
    {
        let check = run(Command::new(analyze)
            .args(["--user", "verify"])
            .arg(&definition));
        assert_eq!(check.code, 0, "{}", check.stderr);
    }
    if cfg!(target_os = "macos") {
        let check = run(Command::new("plutil").arg("-lint").arg(&definition));
        assert_eq!(check.code, 0);
    }
    write(&definition, "custom service\n");
    assert_ne!(install().code, 0);
    assert_eq!(text(&definition), "custom service\n");
}
