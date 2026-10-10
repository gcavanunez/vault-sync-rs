use serde_json::{Value, json};
use std::cell::RefCell;
use std::collections::BTreeMap;
use std::ffi::{OsStr, OsString};
use std::fs;
use std::io::Read;
use std::os::unix::fs::{PermissionsExt, symlink};
use std::os::unix::process::ExitStatusExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, mpsc};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

static SERIAL: Mutex<()> = Mutex::new(());
static NEXT_DIR: AtomicU64 = AtomicU64::new(0);
const PROCESS_TIMEOUT: Duration = Duration::from_secs(30);
pub type Environment = BTreeMap<OsString, OsString>;

pub fn binary() -> PathBuf {
    let path = std::env::var_os("VAULT_SYNC_TEST_BINARY")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(env!("CARGO_BIN_EXE_vault-sync")));
    if path.is_absolute() {
        path
    } else {
        std::env::current_dir().unwrap().join(path)
    }
}

pub fn which(name: &str) -> Option<PathBuf> {
    std::env::split_paths(&std::env::var_os("PATH")?)
        .map(|dir| dir.join(name))
        .find(|path| {
            path.is_file()
                && path
                    .metadata()
                    .is_ok_and(|m| m.permissions().mode() & 0o111 != 0)
        })
        .map(|path| fs::canonicalize(path).unwrap())
}

pub struct TempDir(pub PathBuf);

impl TempDir {
    pub fn new(prefix: &str) -> Self {
        loop {
            let time = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let id = NEXT_DIR.fetch_add(1, Ordering::Relaxed);
            let path =
                std::env::temp_dir().join(format!("{prefix}{}-{time}-{id}", std::process::id()));
            match fs::create_dir(&path) {
                Ok(()) => return Self(path),
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(error) => panic!("create temporary directory: {error}"),
            }
        }
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

pub fn quote(text: impl AsRef<OsStr>) -> String {
    format!(
        "'{}'",
        text.as_ref().to_string_lossy().replace('\'', "'\\''")
    )
}

pub fn wait_for(mut predicate: impl FnMut() -> bool, label: &str) {
    wait_for_timeout(&mut predicate, label, Duration::from_secs(10));
}

pub fn wait_for_timeout(mut predicate: impl FnMut() -> bool, label: &str, timeout: Duration) {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if predicate() {
            return;
        }
        thread::sleep(Duration::from_millis(25));
    }
    panic!("Timed out waiting for {label}");
}

#[derive(Clone, Debug)]
pub struct Output {
    pub code: i32,
    pub stdout: String,
    pub stderr: String,
}

type Reader = mpsc::Receiver<std::io::Result<String>>;

struct ProcessInner {
    child: Child,
    status: Option<ExitStatus>,
    stdout: Option<Reader>,
    stderr: Option<Reader>,
    output: Option<Output>,
}

#[derive(Clone)]
pub struct Process(Arc<Mutex<ProcessInner>>);

fn reader(mut pipe: impl Read + Send + 'static) -> Reader {
    let (send, receive) = mpsc::channel();
    thread::spawn(move || {
        let mut bytes = Vec::new();
        let result = pipe
            .read_to_end(&mut bytes)
            .map(|_| String::from_utf8_lossy(&bytes).into_owned());
        let _ = send.send(result);
    });
    receive
}

impl Process {
    pub fn spawn(command: &mut Command) -> Self {
        let mut child = command
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let stdout = reader(child.stdout.take().unwrap());
        let stderr = reader(child.stderr.take().unwrap());
        Self(Arc::new(Mutex::new(ProcessInner {
            child,
            status: None,
            stdout: Some(stdout),
            stderr: Some(stderr),
            output: None,
        })))
    }

    fn inner(&self) -> MutexGuard<'_, ProcessInner> {
        self.0.lock().unwrap_or_else(|error| error.into_inner())
    }

    pub fn signal(&self, signal: i32) {
        let pid = self.inner().child.id();
        // Only signal the child: Git deliberately survives SIGKILL of its parent.
        unsafe {
            libc::kill(pid as libc::pid_t, signal);
        }
    }

    pub fn exited(&self) -> bool {
        let mut inner = self.inner();
        if inner.status.is_none() {
            inner.status = inner.child.try_wait().unwrap();
        }
        inner.status.is_some()
    }

    pub fn wait_exit(&self) {
        wait_for_timeout(|| self.exited(), "child exit", PROCESS_TIMEOUT);
    }

    pub fn result(&self) -> Output {
        self.wait_exit();
        let mut inner = self.inner();
        if let Some(output) = &inner.output {
            return output.clone();
        }
        let deadline = Instant::now() + PROCESS_TIMEOUT;
        let read = |slot: &mut Option<Reader>| {
            slot.take()
                .unwrap()
                .recv_timeout(deadline.saturating_duration_since(Instant::now()))
                .expect("Timed out waiting for child output")
                .expect("read child output")
        };
        let stdout = read(&mut inner.stdout);
        let stderr = read(&mut inner.stderr);
        let status = inner.status.unwrap();
        let output = Output {
            code: status
                .code()
                .unwrap_or_else(|| 128 + status.signal().unwrap_or(0)),
            stdout,
            stderr,
        };
        inner.output = Some(output.clone());
        output
    }

    pub fn stop(&self) {
        let mut inner = self.inner();
        if inner.status.is_none() {
            inner.status = inner.child.try_wait().ok().flatten();
        }
        if inner.status.is_some() {
            return;
        }
        unsafe {
            libc::kill(inner.child.id() as libc::pid_t, libc::SIGTERM);
        }
        let deadline = Instant::now() + Duration::from_secs(8);
        while Instant::now() < deadline {
            if let Ok(Some(status)) = inner.child.try_wait() {
                inner.status = Some(status);
                return;
            }
            thread::sleep(Duration::from_millis(25));
        }
        let _ = inner.child.kill();
        // Reap without introducing an unbounded wait in cleanup.
        let deadline = Instant::now() + Duration::from_secs(2);
        while Instant::now() < deadline {
            if let Ok(Some(status)) = inner.child.try_wait() {
                inner.status = Some(status);
                return;
            }
            thread::sleep(Duration::from_millis(25));
        }
    }
}

pub fn run(command: &mut Command) -> Output {
    let process = Process::spawn(command);
    struct StopOnDrop(Process);
    impl Drop for StopOnDrop {
        fn drop(&mut self) {
            self.0.stop();
        }
    }
    let _stop = StopOnDrop(process.clone());
    process.result()
}

pub fn write(path: impl AsRef<Path>, text: impl AsRef<[u8]>) {
    fs::write(path, text).unwrap();
}

pub fn text(path: impl AsRef<Path>) -> String {
    fs::read_to_string(path).unwrap()
}

pub fn executable(path: impl AsRef<Path>, script: &str) {
    write(&path, script);
    fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
}

pub fn process_missing(pid: i32) -> bool {
    unsafe { libc::kill(pid, 0) != 0 }
}

pub struct Gate {
    pub ready: PathBuf,
    release: PathBuf,
    operation: String,
}

impl Gate {
    pub fn wait(&self) {
        wait_for(
            || self.ready.exists(),
            &format!("Git {} gate", self.operation),
        );
    }
    pub fn open(&self) {
        write(&self.release, "");
    }
}

pub struct Fixture {
    pub root: PathBuf,
    pub env: Environment,
    pub bin: PathBuf,
    pub remote: PathBuf,
    pub a: PathBuf,
    pub b: PathBuf,
    git_binary: PathBuf,
    path: String,
    children: RefCell<Vec<Process>>,
    _temp: TempDir,
    _serial: MutexGuard<'static, ()>,
}

impl Fixture {
    pub fn new() -> Self {
        let serial = SERIAL.lock().unwrap_or_else(|error| error.into_inner());
        let temp = TempDir::new("vault-sync-");
        let root = temp.0.clone();
        let mut env: Environment = std::env::vars_os()
            .filter(|(key, _)| {
                let key = key.to_string_lossy();
                !["GIT_", "VAULT_", "XDG_"]
                    .iter()
                    .any(|prefix| key.starts_with(prefix))
            })
            .collect();
        env.insert("HOME".into(), root.as_os_str().into());
        for (key, value) in [
            ("GIT_CONFIG_GLOBAL", "/dev/null"),
            ("GIT_CONFIG_NOSYSTEM", "1"),
            ("GIT_AUTHOR_NAME", "Sync Test"),
            ("GIT_AUTHOR_EMAIL", "sync@example.test"),
            ("GIT_COMMITTER_NAME", "Sync Test"),
            ("GIT_COMMITTER_EMAIL", "sync@example.test"),
        ] {
            env.insert(key.into(), value.into());
        }
        let bin = root.join("bin");
        let f = Self {
            path: format!("{}:/usr/bin:/bin:/usr/local/bin", bin.display()),
            bin,
            remote: root.join("remote.git"),
            a: root.join("linux"),
            b: root.join("mac"),
            root,
            env,
            git_binary: which("git").expect("git on PATH"),
            children: RefCell::new(Vec::new()),
            _temp: temp,
            _serial: serial,
        };
        fs::create_dir(&f.bin).unwrap();
        symlink(&f.git_binary, f.bin.join("git")).unwrap();
        for args in [
            vec![
                OsString::from("init"),
                "--bare".into(),
                "--initial-branch=main".into(),
                f.remote.as_os_str().into(),
            ],
            vec![
                OsString::from("clone"),
                f.remote.as_os_str().into(),
                f.a.as_os_str().into(),
            ],
        ] {
            let output = f.cmd(&f.git_binary, &args);
            assert_eq!(output.code, 0, "{}", output.stderr);
        }
        write(f.a.join("shared.md"), "initial\n");
        f.commit(&f.a);
        f.git(&f.a, &["push", "origin", "main"]);
        let output = f.cmd(
            &f.git_binary,
            &[
                OsString::from("clone"),
                f.remote.as_os_str().into(),
                f.b.as_os_str().into(),
            ],
        );
        assert_eq!(output.code, 0, "{}", output.stderr);
        f.configure(&f.a, json!({}));
        f.configure(&f.b, json!({}));
        f
    }

    pub fn config(&self, clone: &Path) -> PathBuf {
        let mut path = clone.as_os_str().to_os_string();
        path.push(".json");
        path.into()
    }

    pub fn configure(&self, clone: &Path, overrides: Value) {
        let mut settings = json!({
            "vaultPath": clone, "deviceName": if clone == self.a { "linux" } else { "mac" },
            "notifications": false, "path": self.path, "commandTimeoutMs": 10_000,
            "requestTimeoutMs": 15_000, "quietMs": 300, "checkpointMs": 1_200, "remotePollMs": 60_000,
        });
        settings
            .as_object_mut()
            .unwrap()
            .extend(overrides.as_object().unwrap().clone());
        write(self.config(clone), settings.to_string());
    }

    pub fn cmd(&self, program: impl AsRef<OsStr>, args: &[impl AsRef<OsStr>]) -> Output {
        let mut command = Command::new(program);
        command.args(args).env_clear().envs(&self.env);
        let mut output = run(&mut command);
        output.stdout = output.stdout.trim().to_owned();
        output.stderr = output.stderr.trim().to_owned();
        output
    }

    pub fn git(&self, clone: &Path, args: &[&str]) -> String {
        let mut all = vec![OsString::from("-C"), clone.as_os_str().into()];
        all.extend(args.iter().map(OsString::from));
        let output = self.cmd(&self.git_binary, &all);
        assert_eq!(output.code, 0, "git {}: {}", args.join(" "), output.stderr);
        output.stdout
    }

    pub fn commit(&self, clone: &Path) {
        self.git(clone, &["add", "-A"]);
        self.git(clone, &["commit", "-m", "test snapshot"]);
    }

    pub fn start(&self, clone: &Path, command: &str, extra: &[(&str, &OsStr)]) -> Process {
        let mut cmd = Command::new(binary());
        cmd.arg(command);
        if command != "daemon" {
            cmd.arg("--json");
        }
        cmd.env_clear()
            .envs(&self.env)
            .env("VAULT_SYNC_CONFIG", self.config(clone));
        for (key, value) in extra {
            cmd.env(key, value);
        }
        let process = Process::spawn(&mut cmd);
        self.children.borrow_mut().push(process.clone());
        process
    }

    pub fn sync(&self, clone: &Path, command: &str, code: i32) -> String {
        let output = self.start(clone, command, &[]).result();
        assert_eq!(
            output.code, code,
            "Expected exit {code}, got {}: {}\n{}",
            output.code, output.stdout, output.stderr
        );
        output.stdout
    }

    pub fn now(&self, clone: &Path) -> String {
        self.sync(clone, "now", 0)
    }

    /// A Git child orphaned by a killed client keeps the lock until it exits;
    /// anything started earlier is correctly refused as busy.
    pub fn wait_for_unlock(&self, clone: &Path) {
        wait_for(
            || self.sync(clone, "status", 0).contains("\"running\": false"),
            "surviving Git exit",
        );
    }

    pub fn state(&self, clone: &Path) -> Value {
        let state: Value =
            serde_json::from_str(&text(clone.join(".git/vault-sync/daemon-state.json"))).unwrap();
        assert!(state["paused"].is_boolean());
        assert!(
            [
                "idle", "synced", "pending", "paused", "diverged", "offline", "blocked"
            ]
            .contains(&state["outcome"].as_str().unwrap())
        );
        assert!(state["message"].is_string());
        for key in ["lastAttempt", "lastContact", "lastClean"] {
            assert!(state.get(key).is_some_and(|v| v.is_null() || v.is_string()));
        }
        let snapshot = state.get("lastSynced").unwrap();
        assert!(
            snapshot.is_null() || (snapshot["commit"].is_string() && snapshot["time"].is_string())
        );
        assert!(state["failures"].as_i64().is_some());
        state
    }

    pub fn gate(&self, operation: &str, after: bool) -> Gate {
        let ready = self.root.join("ready");
        let release = self.root.join("release");
        let gate = self.root.join("gate");
        write(&gate, "");
        fs::remove_file(self.bin.join("git")).unwrap();
        let before = if after {
            format!("{} \"$@\"; code=$?", quote(&self.git_binary))
        } else {
            "code=0".into()
        };
        let action = if after { "exit $code" } else { "break" };
        executable(
            self.bin.join("git"),
            &format!(
                "#!/bin/sh\nfor arg in \"$@\"; do\n  if [ \"$arg\" = {} ] && [ -e {} ]; then\n    rm {}\n    {}\n    echo $$ > {}\n    while [ ! -e {} ]; do sleep 0.02; done\n    {}\n  fi\ndone\nexec {} \"$@\"\n",
                quote(operation),
                quote(&gate),
                quote(&gate),
                before,
                quote(&ready),
                quote(&release),
                action,
                quote(&self.git_binary),
            ),
        );
        Gate {
            ready,
            release,
            operation: operation.into(),
        }
    }

    pub fn git_binary(&self) -> &Path {
        &self.git_binary
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::write(self.root.join("release"), "");
        for child in self.children.get_mut().iter() {
            child.stop();
        }
        let _ = fs::remove_dir_all(&self.root);
    }
}
