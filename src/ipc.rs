//! Newline-delimited JSON over a private Unix socket: one request, one reply.

use crate::interrupt;
use crate::model::{Failure, Kind, PROTOCOL, Reply, Request, failure};
use sha2::{Digest, Sha256};
use std::fs::{self, DirBuilder};
use std::io::{self, Read, Write};
use std::net::Shutdown;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

const REQUEST_LIMIT: usize = 4096;
const REPLY_LIMIT: usize = 128 * 1024;
const REQUEST_TIMEOUT: Duration = Duration::from_secs(5);

fn uid() -> u32 {
    // SAFETY: getuid cannot fail.
    unsafe { libc::getuid() }
}

/// `/tmp/vault-sync-<uid>/<24 hex chars of sha256(common dir)>.sock`, so every
/// config that targets the same clone shares one daemon.
pub fn socket_path(common: &Path) -> PathBuf {
    let digest = Sha256::digest(common.as_os_str().as_bytes());
    let name: String = digest
        .iter()
        .take(12)
        .map(|byte| format!("{byte:02x}"))
        .collect();
    PathBuf::from(format!("/tmp/vault-sync-{}", uid())).join(format!("{name}.sock"))
}

/// A bound socket. Its file is removed by [`Server::close`]; only the lock
/// owner listens, so a leftover file is always stale.
pub struct Server {
    path: PathBuf,
}

impl Server {
    pub fn close(self) {
        let _ = fs::remove_file(&self.path);
    }
}

/// Serves each connection on its own thread. `handle` may block until queued
/// work finishes; returning `None` closes the connection without a reply.
pub fn listen(
    path: &Path,
    handle: impl Fn(Request) -> Option<Reply> + Send + Sync + 'static,
) -> Result<Server, Failure> {
    let blocked = |error: io::Error| Failure::new(Kind::Blocked, error.to_string());

    if let Some(directory) = path.parent() {
        private_directory(directory).map_err(blocked)?;
    }

    match fs::remove_file(path) {
        Err(error) if error.kind() != io::ErrorKind::NotFound => return Err(blocked(error)),
        _ => {}
    }

    let listener = UnixListener::bind(path)
        .map_err(|error| Failure::new(Kind::Blocked, format!("Cannot listen: {error}")))?;
    fs::set_permissions(path, fs::Permissions::from_mode(0o600)).map_err(blocked)?;
    let handle = Arc::new(handle);

    thread::Builder::new()
        .name("ipc".into())
        .spawn(move || {
            for stream in listener.incoming() {
                match stream {
                    Ok(stream) => {
                        let handle = Arc::clone(&handle);
                        thread::spawn(move || serve(stream, &*handle));
                    }
                    Err(_) => thread::sleep(Duration::from_millis(100)),
                }
            }
        })
        .map_err(blocked)?;

    Ok(Server {
        path: path.to_owned(),
    })
}

fn private_directory(directory: &Path) -> io::Result<()> {
    DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(directory)?;
    let metadata = fs::symlink_metadata(directory)?;

    if !metadata.is_dir() || metadata.uid() != uid() || metadata.mode() & 0o077 != 0 {
        return Err(io::Error::other(
            "Socket directory must be private and owned by the current user",
        ));
    }

    Ok(())
}

fn serve(mut stream: UnixStream, handle: &dyn Fn(Request) -> Option<Reply>) {
    let _ = stream.set_read_timeout(Some(REQUEST_TIMEOUT));
    let Ok(line) = read_line(&mut stream, REQUEST_LIMIT) else {
        return;
    };
    let _ = stream.set_read_timeout(None);

    let reply = match serde_json::from_str::<Request>(line.trim()) {
        Ok(request) => match handle(request) {
            Some(reply) => reply,
            None => return,
        },
        Err(error) => Reply::Error {
            message: error.to_string(),
        },
    };

    if let Ok(mut text) = serde_json::to_string(&reply) {
        text.push('\n');
        let _ = stream.write_all(text.as_bytes());
    }

    let _ = stream.shutdown(Shutdown::Both);
}

enum ReadError {
    Closed,
    TimedOut,
    Oversized,
    Io(io::Error),
}

fn read_line(stream: &mut UnixStream, limit: usize) -> Result<String, ReadError> {
    let mut data = Vec::new();
    let mut buffer = [0_u8; 4096];

    loop {
        match stream.read(&mut buffer) {
            Ok(0) => return Err(ReadError::Closed),
            Ok(read) => {
                data.extend_from_slice(&buffer[..read]);

                if data.len() > limit {
                    return Err(ReadError::Oversized);
                }

                if let Some(end) = data.iter().position(|byte| *byte == b'\n') {
                    data.truncate(end);
                    return Ok(String::from_utf8_lossy(&data).into_owned());
                }
            }
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
                ) =>
            {
                return Err(ReadError::TimedOut);
            }
            Err(error) => return Err(ReadError::Io(error)),
        }
    }
}

/// Sends one request to a running daemon. A missing or refused socket is
/// [`Kind::Unavailable`], which callers treat as "no daemon".
pub fn request(path: &Path, request: Request, timeout: Duration) -> Result<Reply, Failure> {
    let process = |message: String| {
        if interrupt::triggered() {
            Failure::interrupted()
        } else {
            Failure::new(Kind::Process, message)
        }
    };

    let mut stream = UnixStream::connect(path).map_err(|error| match error.kind() {
        io::ErrorKind::NotFound | io::ErrorKind::ConnectionRefused => {
            Failure::new(Kind::Unavailable, "Daemon is not running")
        }
        _ => process(error.to_string()),
    })?;

    let interrupted = stream
        .try_clone()
        .map_err(|error| process(error.to_string()))?;
    let _subscription = interrupt::subscribe(move || {
        let _ = interrupted.shutdown(Shutdown::Both);
    });

    let mut text = serde_json::to_string(&request).map_err(|error| process(error.to_string()))?;
    text.push('\n');
    stream
        .set_read_timeout(Some(timeout))
        .and_then(|()| stream.write_all(text.as_bytes()))
        .map_err(|error| process(error.to_string()))?;

    let line = read_line(&mut stream, REPLY_LIMIT).map_err(|error| match error {
        ReadError::TimedOut => Failure::new(
            Kind::Timeout,
            "Daemon request timed out; queued work may still complete",
        ),
        ReadError::Closed => process("Daemon closed without a reply".into()),
        ReadError::Oversized => process("Oversized daemon reply".into()),
        ReadError::Io(error) => process(error.to_string()),
    })?;

    let reply: Reply = serde_json::from_str(line.trim())
        .map_err(|error| Failure::new(Kind::Process, format!("Invalid daemon reply: {error}")))?;

    if let Reply::Ok { status } = &reply
        && status.protocol != PROTOCOL
    {
        return failure(
            Kind::Process,
            format!(
                "Invalid daemon reply: unsupported protocol {}",
                status.protocol
            ),
        );
    }

    Ok(reply)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Command, State, Status, Watcher};

    #[test]
    fn socket_names_are_short_and_stable_per_clone() {
        let path = socket_path(Path::new("/home/me/vault/.git"));
        let name = path.file_name().unwrap().to_str().unwrap();
        assert_eq!(
            path.parent().unwrap(),
            Path::new(&format!("/tmp/vault-sync-{}", uid()))
        );
        assert_eq!(name.len(), 24 + ".sock".len());
        assert!(
            name.trim_end_matches(".sock")
                .bytes()
                .all(|byte| byte.is_ascii_hexdigit())
        );
        assert_eq!(path, socket_path(Path::new("/home/me/vault/.git")));
        assert_ne!(path, socket_path(Path::new("/home/me/other/.git")));
    }

    #[test]
    fn requests_round_trip_and_a_missing_daemon_is_unavailable() {
        let path = socket_path(Path::new(&format!("/test/ipc/{}", std::process::id())));
        let missing = request(
            &path,
            Request {
                command: Command::Status,
            },
            Duration::from_secs(1),
        );
        assert_eq!(missing.unwrap_err().kind, Kind::Unavailable);

        let status = Status {
            protocol: PROTOCOL,
            vault: "/v".into(),
            pid: 1,
            started_at: "t".into(),
            running: false,
            watcher: Watcher::Active,
            watcher_error: None,
            next_remote_attempt: None,
            state: State::default(),
        };
        let expected = status.clone();
        let server = listen(&path, move |request| match request.command {
            Command::Status => Some(Reply::Ok {
                status: Box::new(status.clone()),
            }),
            _ => None,
        })
        .unwrap();

        let reply = request(
            &path,
            Request {
                command: Command::Status,
            },
            Duration::from_secs(5),
        )
        .unwrap();
        assert_eq!(
            reply,
            Reply::Ok {
                status: Box::new(expected)
            }
        );
        let dropped = request(
            &path,
            Request {
                command: Command::Now,
            },
            Duration::from_secs(5),
        );
        assert_eq!(
            dropped.unwrap_err().message,
            "Daemon closed without a reply"
        );
        server.close();
        assert!(!path.exists());
    }
}
