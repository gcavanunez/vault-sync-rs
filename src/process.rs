use crate::interrupt;
use crate::model::{Failure, Kind};
use std::ffi::OsString;
use std::io::{self, Read};
use std::os::fd::RawFd;
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::thread;
use std::time::{Duration, Instant};

const OUTPUT_LIMIT: usize = 4 * 1024 * 1024;
const TERMINATION_GRACE: Duration = Duration::from_secs(2);
/// Children receive the repository lock at this descriptor.
const LOCK_FD: RawFd = 3;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Output {
    pub code: i32,
    pub stdout: String,
    pub stderr: String,
}

pub struct Options<'a> {
    pub cwd: &'a Path,
    pub env: &'a [(OsString, OsString)],
    pub timeout: Duration,
    pub lock_fd: Option<RawFd>,
}

#[derive(Default)]
struct Captured {
    bytes: Vec<u8>,
    overflow: bool,
}

enum Event {
    Stdout(Captured),
    Stderr(Captured),
    Exited(i32),
    Interrupted,
}

#[derive(Default)]
struct Progress {
    stdout: Option<Captured>,
    stderr: Option<Captured>,
    code: Option<i32>,
}

impl Progress {
    fn record(&mut self, event: Event) {
        match event {
            Event::Stdout(captured) => self.stdout = Some(captured),
            Event::Stderr(captured) => self.stderr = Some(captured),
            Event::Exited(code) => self.code = Some(code),
            Event::Interrupted => {}
        }
    }

    /// The child has exited and both pipes reached end of file.
    fn closed(&self) -> bool {
        self.code.is_some() && self.stdout.is_some() && self.stderr.is_some()
    }
}

enum Ending {
    Closed,
    TimedOut,
    Interrupted,
}

/// Runs a command in its own session and process group. On timeout or
/// interrupt the whole group gets SIGTERM, then SIGKILL after a grace period,
/// and the child is always reaped before this returns. The repository lock, if
/// given, is inherited as descriptor 3 so a surviving child keeps ownership.
pub fn run(args: &[String], options: &Options<'_>) -> Result<Output, Failure> {
    let Some((program, rest)) = args.split_first() else {
        return Err(Failure::new(Kind::Process, "Missing executable"));
    };

    if interrupt::triggered() {
        return Err(Failure::interrupted());
    }

    let mut command = Command::new(program);
    command
        .args(rest)
        .current_dir(options.cwd)
        .env_clear()
        .envs(options.env.iter().map(|(key, value)| (key, value)))
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let lock_fd = options.lock_fd;
    // SAFETY: the closure runs between fork and exec and only makes
    // async-signal-safe calls (setsid, dup2, fcntl).
    unsafe {
        command.pre_exec(move || prepare_child(lock_fd));
    }

    let mut child = command
        .spawn()
        .map_err(|error| Failure::new(Kind::Process, format!("{program}: {error}")))?;
    let group = child.id() as libc::pid_t;
    let (sender, events) = mpsc::channel();
    capture(child.stdout.take(), sender.clone(), Event::Stdout);
    capture(child.stderr.take(), sender.clone(), Event::Stderr);
    let exited = sender.clone();
    thread::spawn(move || {
        let code = child.wait().map_or(1, |status| status.code().unwrap_or(1));
        let _ = exited.send(Event::Exited(code));
    });
    let _subscription = interrupt::subscribe(move || {
        let _ = sender.send(Event::Interrupted);
    });

    let mut progress = Progress::default();
    let deadline = Instant::now() + options.timeout;
    let ending = loop {
        if progress.closed() {
            break Ending::Closed;
        }

        match events.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
            Ok(Event::Interrupted) => break Ending::Interrupted,
            Ok(event) => progress.record(event),
            Err(RecvTimeoutError::Timeout) => break Ending::TimedOut,
            Err(RecvTimeoutError::Disconnected) => break Ending::Closed,
        }
    };

    // Also reaches descendants, such as hooks, that outlive a finished child.
    signal_group(group, libc::SIGTERM);

    if !progress.closed() {
        drain(
            &events,
            &mut progress,
            Some(Instant::now() + TERMINATION_GRACE),
        );
    }

    signal_group(group, libc::SIGKILL);

    if progress.code.is_none() {
        drain(&events, &mut progress, None);
    }

    match ending {
        Ending::Interrupted => Err(Failure::interrupted()),
        Ending::TimedOut => Err(Failure::new(
            Kind::Timeout,
            format!("{program} exceeded {}ms", options.timeout.as_millis()),
        )),
        Ending::Closed => {
            let stdout = progress.stdout.unwrap_or_default();
            let stderr = progress.stderr.unwrap_or_default();

            if stdout.overflow || stderr.overflow {
                return Err(Failure::new(Kind::Process, "Command output exceeded 4 MiB"));
            }

            Ok(Output {
                code: progress.code.unwrap_or(1),
                stdout: String::from_utf8_lossy(&stdout.bytes).into_owned(),
                stderr: String::from_utf8_lossy(&stderr.bytes).into_owned(),
            })
        }
    }
}

fn prepare_child(lock_fd: Option<RawFd>) -> io::Result<()> {
    // SAFETY: plain syscalls on descriptors owned by this freshly forked child.
    let failed = unsafe {
        libc::setsid() == -1
            || match lock_fd {
                // dup2 onto itself is a no-op that would leave close-on-exec set.
                Some(LOCK_FD) => {
                    let flags = libc::fcntl(LOCK_FD, libc::F_GETFD);
                    flags == -1
                        || libc::fcntl(LOCK_FD, libc::F_SETFD, flags & !libc::FD_CLOEXEC) == -1
                }
                Some(fd) => libc::dup2(fd, LOCK_FD) == -1,
                None => false,
            }
    };

    if failed {
        return Err(io::Error::last_os_error());
    }

    Ok(())
}

/// Waits for events until the child has closed, or until the deadline passes.
/// Without a deadline it waits only for the child to exit.
fn drain(events: &Receiver<Event>, progress: &mut Progress, deadline: Option<Instant>) {
    loop {
        let event = match deadline {
            Some(deadline) => {
                if progress.closed() {
                    return;
                }
                match events.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
                    Ok(event) => event,
                    Err(_) => return,
                }
            }
            None => {
                if progress.code.is_some() {
                    return;
                }
                match events.recv() {
                    Ok(event) => event,
                    Err(_) => return,
                }
            }
        };
        progress.record(event);
    }
}

fn capture(
    pipe: Option<impl Read + Send + 'static>,
    sender: Sender<Event>,
    wrap: fn(Captured) -> Event,
) {
    let Some(mut pipe) = pipe else {
        let _ = sender.send(wrap(Captured::default()));
        return;
    };

    thread::spawn(move || {
        let mut captured = Captured::default();
        let mut buffer = [0_u8; 16 * 1024];

        loop {
            match pipe.read(&mut buffer) {
                Ok(0) => break,
                Ok(read) if captured.bytes.len() + read > OUTPUT_LIMIT => captured.overflow = true,
                Ok(read) => captured.bytes.extend_from_slice(&buffer[..read]),
                Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                Err(_) => break,
            }
        }

        let _ = sender.send(wrap(captured));
    });
}

fn signal_group(group: libc::pid_t, signal: libc::c_int) {
    // SAFETY: kill with a negative pid signals a process group; failure (for
    // example ESRCH once the group is empty) is expected and harmless.
    unsafe {
        libc::kill(-group, signal);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::{self, OpenOptions};
    use std::os::fd::AsRawFd;

    fn sh(script: &str, timeout_ms: u64, lock_fd: Option<RawFd>) -> Result<Output, Failure> {
        let env: Vec<(OsString, OsString)> = std::env::vars_os().collect();
        run(
            &["sh".into(), "-c".into(), script.into()],
            &Options {
                cwd: Path::new("/"),
                env: &env,
                timeout: Duration::from_millis(timeout_ms),
                lock_fd,
            },
        )
    }

    fn scratch(name: &str) -> std::path::PathBuf {
        let dir =
            std::env::temp_dir().join(format!("vault-sync-process-{}-{name}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn captures_exit_code_and_both_streams() {
        let output = sh("echo out; echo err >&2; exit 7", 5_000, None).unwrap();
        assert_eq!(
            output,
            Output {
                code: 7,
                stdout: "out\n".into(),
                stderr: "err\n".into()
            }
        );
    }

    #[test]
    fn missing_executables_are_process_failures() {
        let error = run(
            &["/nonexistent/vault-sync-test".into()],
            &Options {
                cwd: Path::new("/"),
                env: &[],
                timeout: Duration::from_secs(1),
                lock_fd: None,
            },
        )
        .unwrap_err();
        assert_eq!(error.kind, Kind::Process);
    }

    #[test]
    fn timeout_kills_the_group_including_descendants_that_ignore_sigterm() {
        let dir = scratch("timeout");
        let pid_file = dir.join("pid");
        let script = format!(
            "sh -c 'trap \"\" TERM; echo $$ > {}; while :; do sleep 1; done' & wait",
            pid_file.display()
        );
        let started = Instant::now();
        let error = sh(&script, 300, None).unwrap_err();
        assert_eq!(error.kind, Kind::Timeout);
        assert!(
            error.message.ends_with("exceeded 300ms"),
            "{}",
            error.message
        );
        assert!(started.elapsed() < Duration::from_secs(5));
        let pid: libc::pid_t = fs::read_to_string(&pid_file)
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        thread::sleep(Duration::from_millis(100));
        // SAFETY: probing with signal 0 has no effect on the target.
        let alive = unsafe { libc::kill(pid, 0) } == 0;
        let zombie = fs::read_to_string(format!("/proc/{pid}/stat"))
            .map(|stat| stat.contains(") Z "))
            .unwrap_or(false);
        assert!(!alive || zombie, "descendant {pid} survived");
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn output_beyond_the_limit_fails_without_buffering_it() {
        let error = sh("head -c 5000000 /dev/zero", 10_000, None).unwrap_err();
        assert_eq!(error.message, "Command output exceeded 4 MiB");
    }

    #[test]
    fn the_lock_descriptor_is_inherited_as_descriptor_three() {
        let dir = scratch("lock");
        let path = dir.join("lock");
        let file = OpenOptions::new()
            .read(true)
            .append(true)
            .create(true)
            .open(&path)
            .unwrap();
        let output = sh("echo inherited >&3", 5_000, Some(file.as_raw_fd())).unwrap();
        assert_eq!(output.code, 0, "{}", output.stderr);
        assert_eq!(fs::read_to_string(&path).unwrap(), "inherited\n");
        fs::remove_dir_all(dir).unwrap();
    }
}
