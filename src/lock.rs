use crate::model::{Failure, Kind, failure};
use std::fs::{DirBuilder, File, OpenOptions, TryLockError};
use std::io;
use std::os::fd::{AsRawFd, RawFd};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::Path;

/// An OS `flock` held for as long as this value lives. Dropping it closes the
/// descriptor instead of unlocking, so Git children that inherited the
/// descriptor keep ownership after an abrupt parent death. Never delete the
/// lock file: its inode identifies shared ownership.
#[derive(Debug)]
pub struct Lock {
    file: File,
}

impl Lock {
    pub fn fd(&self) -> RawFd {
        self.file.as_raw_fd()
    }
}

pub fn acquire(path: &Path) -> Result<Lock, Failure> {
    if !cfg!(any(target_os = "linux", target_os = "macos")) {
        return failure(Kind::Config, "Only Linux and macOS are supported");
    }

    let cannot =
        |error: io::Error| Failure::new(Kind::Blocked, format!("Cannot acquire OS lock: {error}"));

    if let Some(parent) = path.parent() {
        DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(parent)
            .map_err(cannot)?;
    }

    let file = OpenOptions::new()
        .read(true)
        .append(true)
        .create(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)
        .map_err(cannot)?;

    match file.try_lock() {
        Ok(()) => Ok(Lock { file }),
        Err(TryLockError::WouldBlock) => failure(
            Kind::Busy,
            "Vault is locked by another daemon or a surviving Git child",
        ),
        Err(TryLockError::Error(error)) => Err(cannot(error)),
    }
}

/// Reports whether another process owns the lock, without creating anything.
pub fn held(path: &Path) -> Result<bool, Failure> {
    let file = match File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return failure(Kind::Blocked, error.to_string()),
    };

    match file.try_lock() {
        Ok(()) => Ok(false),
        Err(TryLockError::WouldBlock) => Ok(true),
        Err(TryLockError::Error(error)) => failure(Kind::Blocked, error.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn scratch(name: &str) -> std::path::PathBuf {
        let dir =
            std::env::temp_dir().join(format!("vault-sync-lock-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn a_second_owner_is_busy_until_the_first_is_dropped() {
        let dir = scratch("busy");
        let path = dir.join("nested/lock");
        assert!(!held(&path).unwrap());
        let first = acquire(&path).unwrap();
        assert!(held(&path).unwrap());
        assert_eq!(acquire(&path).unwrap_err().kind, Kind::Busy);
        drop(first);
        assert!(!held(&path).unwrap());
        drop(acquire(&path).unwrap());
        assert!(path.exists(), "the lock file is never removed");
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn a_symlinked_lock_is_refused() {
        let dir = scratch("symlink");
        let target = dir.join("target");
        fs::write(&target, "unchanged").unwrap();
        let path = dir.join("lock");
        std::os::unix::fs::symlink(&target, &path).unwrap();
        assert_eq!(acquire(&path).unwrap_err().kind, Kind::Blocked);
        assert_eq!(fs::read_to_string(&target).unwrap(), "unchanged");
        fs::remove_dir_all(dir).unwrap();
    }
}
