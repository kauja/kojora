//! Minimal, synchronous, headless command execution for trusted callers.
//! This is not an agent adapter, daemon, sandbox, or resource controller.

use std::ffi::OsString;
use std::fs::{self, DirBuilder, File, OpenOptions};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

#[cfg(unix)]
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
#[cfg(unix)]
use std::os::unix::process::CommandExt;

/// A thread-safe, cloneable cancellation request. No PID is exposed to the UI.
#[derive(Debug, Default, Clone)]
pub struct CancellationToken(Arc<AtomicBool>);

impl CancellationToken {
    /// A cancellation request is idempotent and safe from another thread.
    pub fn cancel(&self) {
        self.0.store(true, Ordering::Release);
    }

    pub fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::Acquire)
    }

    /// Only the owner coordinating a completed execution may clear this token.
    pub fn reset(&self) {
        self.0.store(false, Ordering::Release);
    }
}

/// Per-process Unix RLIMIT ceilings. These are not host-wide RSS or CPU
/// quotas: RLIMIT_AS caps virtual address space and RLIMIT_CPU caps CPU
/// seconds per process (not summed across descendants).
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct ProcessLimits {
    pub address_space_mib: Option<u64>,
    pub cpu_seconds: Option<u64>,
}

impl ProcessLimits {
    pub fn validate(self) -> io::Result<()> {
        if self.address_space_mib == Some(0) || self.cpu_seconds == Some(0) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "resource limits must be positive",
            ));
        }
        if let Some(mib) = self.address_space_mib {
            let bytes = mib.checked_mul(1024 * 1024).ok_or_else(|| {
                io::Error::new(io::ErrorKind::InvalidInput, "memory limit overflow")
            })?;
            let _: libc::rlim_t = bytes.try_into().map_err(|_| {
                io::Error::new(io::ErrorKind::InvalidInput, "memory limit too large")
            })?;
        }
        if let Some(seconds) = self.cpu_seconds {
            let _: libc::rlim_t = seconds
                .try_into()
                .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "CPU limit too large"))?;
        }
        Ok(())
    }
}

#[derive(Debug, Clone)]
pub struct RunSpec {
    /// Executable chosen explicitly by a trusted caller. Never shell-expanded.
    pub program: OsString,
    pub args: Vec<OsString>,
    pub cwd: PathBuf,
    pub log_root: PathBuf,
}

#[derive(Debug)]
pub struct RunOutcome {
    pub run_dir: PathBuf,
    pub exit_code: Option<i32>,
    pub success: bool,
    /// Set only when the executor received a cancellation request while the child was alive.
    pub cancelled: bool,
    pub duration: Duration,
    pub stdout_bytes: u64,
    pub stderr_bytes: u64,
}

/// Internal Rust extension point; not the stable external plugin ABI.
pub trait Executor {
    fn run(&self, spec: &RunSpec) -> io::Result<RunOutcome>;
}

#[derive(Debug, Default)]
pub struct LocalExecutor;

/// Executor variant that explicitly clears inherited environment variables.
pub trait IsolatedExecutor {
    fn run_isolated(
        &self,
        spec: &RunSpec,
        environment: &[(OsString, OsString)],
    ) -> io::Result<RunOutcome>;

    /// Fail closed if an executor cannot stop its process tree.
    /// Never silently run a non-cancellable agent under a cancellable API.
    fn run_isolated_cancellable(
        &self,
        _spec: &RunSpec,
        _environment: &[(OsString, OsString)],
        _token: &CancellationToken,
    ) -> io::Result<RunOutcome> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "executor does not implement cancellable process execution",
        ))
    }

    /// A worker with explicit resource ceilings must reject executors that
    /// cannot enforce them instead of silently running without limits.
    fn run_isolated_limited(
        &self,
        _spec: &RunSpec,
        _environment: &[(OsString, OsString)],
        _token: &CancellationToken,
        _limits: ProcessLimits,
    ) -> io::Result<RunOutcome> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "executor does not enforce resource limits",
        ))
    }
}

impl LocalExecutor {
    fn execute(
        &self,
        spec: &RunSpec,
        environment: Option<&[(OsString, OsString)]>,
        cancellation: Option<&CancellationToken>,
        limits: ProcessLimits,
    ) -> io::Result<RunOutcome> {
        limits.validate()?;
        ensure_private_log_root(&spec.log_root)?;
        let run_dir = create_run_dir(&spec.log_root)?;
        let mut events = create_private_file(&run_dir.join("events.jsonl"))?;
        let stdout_file = create_private_file(&run_dir.join("stdout.log"))?;
        let stderr_file = create_private_file(&run_dir.join("stderr.log"))?;
        let timestamp = unix_ms()?;
        let started = Instant::now();
        let mut command = Command::new(&spec.program);
        command
            .args(&spec.args)
            .current_dir(&spec.cwd)
            .stdin(if environment.is_some() {
                Stdio::null()
            } else {
                Stdio::inherit()
            })
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        #[cfg(unix)]
        if cancellation.is_some() {
            // A new process group lets cancellation reach descendants too.
            command.process_group(0);
        }
        #[cfg(unix)]
        if limits.address_space_mib.is_some() || limits.cpu_seconds.is_some() {
            // pre_exec only calls async-signal-safe setrlimit syscall(s).
            // No allocations, mutexes or Rust runtimes are touched in child.
            unsafe {
                command.pre_exec(move || apply_process_limits(limits));
            }
        }
        if let Some(variables) = environment {
            command.env_clear();
            command.envs(variables.iter().cloned());
        }
        let mut child = match command.spawn() {
            Ok(child) => child,
            Err(error) => {
                writeln!(
                    events,
                    "{{\"schema_version\":1,\"event\":\"spawn_failed\",\"time_unix_ms\":{timestamp}}}"
                )?;
                events.sync_all()?;
                return Err(error);
            }
        };

        let pid = child.id();
        if let Err(error) = (|| -> io::Result<()> {
            writeln!(
                events,
                "{{\"schema_version\":1,\"event\":\"started\",\"pid\":{pid},\"time_unix_ms\":{timestamp}}}"
            )?;
            events.sync_all()
        })() {
            let _ = child.kill();
            let _ = child.wait();
            return Err(error);
        }

        let stdout = child.stdout.take().expect("piped stdout must exist");
        let stderr = child.stderr.take().expect("piped stderr must exist");
        let stdout_thread = thread::spawn(move || drain_to_log(stdout, stdout_file));
        let stderr_thread = thread::spawn(move || drain_to_log(stderr, stderr_file));

        let (status, cancelled) = match wait_child(&mut child, cancellation) {
            Ok(result) => result,
            Err(error) => {
                // Never abandon a live child or an undrained pipe on wait errors.
                #[cfg(unix)]
                if cancellation.is_some() {
                    let _ = signal_group(child.id(), libc::SIGKILL);
                }
                let _ = child.kill();
                let _ = child.wait();
                let _ = join_drain(stdout_thread);
                let _ = join_drain(stderr_thread);
                return Err(error);
            }
        };
        let stdout_result = join_drain(stdout_thread);
        let stderr_result = join_drain(stderr_thread);
        let duration = started.elapsed();
        let exit_code = status
            .code()
            .map_or_else(|| "null".to_owned(), |code| code.to_string());
        writeln!(
            events,
            "{{\"schema_version\":1,\"event\":\"finished\",\"exit_code\":{exit_code},\"success\":{},\"cancelled\":{cancelled},\"duration_ms\":{}}}",
            status.success() && !cancelled,
            duration.as_millis()
        )?;
        events.sync_all()?;

        Ok(RunOutcome {
            run_dir,
            exit_code: status.code(),
            success: status.success() && !cancelled,
            cancelled,
            duration,
            stdout_bytes: stdout_result?,
            stderr_bytes: stderr_result?,
        })
    }
}

impl Executor for LocalExecutor {
    fn run(&self, spec: &RunSpec) -> io::Result<RunOutcome> {
        self.execute(spec, None, None, ProcessLimits::default())
    }
}

impl IsolatedExecutor for LocalExecutor {
    fn run_isolated(
        &self,
        spec: &RunSpec,
        environment: &[(OsString, OsString)],
    ) -> io::Result<RunOutcome> {
        self.execute(spec, Some(environment), None, ProcessLimits::default())
    }

    fn run_isolated_cancellable(
        &self,
        spec: &RunSpec,
        environment: &[(OsString, OsString)],
        token: &CancellationToken,
    ) -> io::Result<RunOutcome> {
        self.execute(
            spec,
            Some(environment),
            Some(token),
            ProcessLimits::default(),
        )
    }

    fn run_isolated_limited(
        &self,
        spec: &RunSpec,
        environment: &[(OsString, OsString)],
        token: &CancellationToken,
        limits: ProcessLimits,
    ) -> io::Result<RunOutcome> {
        self.execute(spec, Some(environment), Some(token), limits)
    }
}

#[cfg(unix)]
fn apply_process_limits(limits: ProcessLimits) -> io::Result<()> {
    if let Some(mib) = limits.address_space_mib {
        let bytes = mib * 1024 * 1024;
        let ceiling = libc::rlimit {
            rlim_cur: bytes as libc::rlim_t,
            rlim_max: bytes as libc::rlim_t,
        };
        // SAFETY: primitive numeric values only; called in pre_exec after fork.
        if unsafe { libc::setrlimit(libc::RLIMIT_AS, &ceiling) } != 0 {
            return Err(io::Error::last_os_error());
        }
    }
    if let Some(seconds) = limits.cpu_seconds {
        let ceiling = libc::rlimit {
            rlim_cur: seconds as libc::rlim_t,
            rlim_max: seconds as libc::rlim_t,
        };
        // SAFETY: primitive numeric values only; called in pre_exec after fork.
        if unsafe { libc::setrlimit(libc::RLIMIT_CPU, &ceiling) } != 0 {
            return Err(io::Error::last_os_error());
        }
    }
    Ok(())
}

/// Wait without blocking cancellation. On Unix, the child is started in its
/// own process group. TERM offers a brief cleanup window; KILL then ensures
/// children that ignore TERM cannot keep stdout/stderr pipes open.
fn wait_child(
    child: &mut Child,
    cancellation: Option<&CancellationToken>,
) -> io::Result<(ExitStatus, bool)> {
    let Some(token) = cancellation else {
        return child.wait().map(|status| (status, false));
    };
    loop {
        if token.is_cancelled() {
            #[cfg(unix)]
            {
                // Do not reap the leader between TERM and KILL: its PID cannot
                // be reused while the zombie remains un-waited, avoiding
                // accidentally signalling a different process group.
                signal_group(child.id(), libc::SIGTERM)?;
                thread::sleep(Duration::from_millis(200));
                signal_group(child.id(), libc::SIGKILL)?;
            }
            #[cfg(not(unix))]
            {
                // Windows currently stops the direct child only. The native
                // Studio build is supported on Unix desktop platforms.
                let _ = child.kill();
            }
            return child.wait().map(|status| (status, true));
        }
        if let Some(status) = child.try_wait()? {
            return Ok((status, false));
        }
        thread::sleep(Duration::from_millis(40));
    }
}

#[cfg(unix)]
fn signal_group(child_pid: u32, signal: libc::c_int) -> io::Result<()> {
    let pid = i32::try_from(child_pid)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "invalid child PID"))?;
    // SAFETY: negative PID targets the dedicated process group created by
    // CommandExt::process_group(0); no shell or arbitrary PID is accepted.
    let result = unsafe { libc::kill(-pid, signal) };
    if result == -1 {
        let error = io::Error::last_os_error();
        if error.raw_os_error() != Some(libc::ESRCH) {
            return Err(error);
        }
    }
    Ok(())
}

/// Fixed-size buffers keep log capture independent of total output volume.
/// A failed log write must not stop draining a pipe while the child is running.
fn drain_to_log(mut pipe: impl Read, mut log: File) -> io::Result<u64> {
    let mut buffer = [0_u8; 16 * 1024];
    let mut total = 0_u64;
    let mut log_error = None;
    loop {
        let count = match pipe.read(&mut buffer) {
            Ok(0) => break,
            Ok(count) => count,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(error),
        };
        total += count as u64;
        if log_error.is_none() {
            if let Err(error) = log.write_all(&buffer[..count]) {
                log_error = Some(error);
            }
        }
    }
    match log_error {
        Some(error) => Err(error),
        None => Ok(total),
    }
}

fn join_drain(handle: thread::JoinHandle<io::Result<u64>>) -> io::Result<u64> {
    handle
        .join()
        .map_err(|_| io::Error::other("log drain thread panicked"))?
}

fn unix_ms() -> io::Result<u128> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|value| value.as_millis())
        .map_err(|_| io::Error::other("system clock is before the Unix epoch"))
}

fn create_run_dir(root: &Path) -> io::Result<PathBuf> {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|_| io::Error::other("system clock is before the Unix epoch"))?
        .as_nanos();
    for number in 0..16 {
        let dir = root.join(format!("{now}-{}-{number}", std::process::id()));
        let mut builder = DirBuilder::new();
        #[cfg(unix)]
        builder.mode(0o700);
        match builder.create(&dir) {
            Ok(()) => return Ok(dir),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error),
        }
    }
    Err(io::Error::new(
        io::ErrorKind::AlreadyExists,
        "could not allocate a unique run directory",
    ))
}

fn ensure_private_log_root(root: &Path) -> io::Result<()> {
    let mut builder = DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    builder.mode(0o700);
    builder.create(root)?;

    #[cfg(unix)]
    {
        let metadata = fs::symlink_metadata(root)?;
        if !metadata.is_dir() || metadata.permissions().mode() & 0o077 != 0 {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "run log root must be a private directory (mode 0700)",
            ));
        }
    }
    Ok(())
}

fn create_private_file(path: &Path) -> io::Result<File> {
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    options.mode(0o600);
    options.open(path)
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::ffi::OsStr;

    fn test_root(name: &str) -> PathBuf {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        std::env::temp_dir().join(format!("kojora-test-{name}-{now}-{}", std::process::id()))
    }

    fn request(root: &Path, script: &str) -> RunSpec {
        RunSpec {
            program: OsString::from("sh"),
            args: vec![OsString::from("-c"), OsString::from(script)],
            cwd: std::env::current_dir().expect("cwd"),
            log_root: root.to_owned(),
        }
    }

    #[test]
    fn captures_output_and_exit_status() {
        let root = test_root("output");
        let outcome = LocalExecutor
            .run(&request(
                &root,
                "printf 'hello'; printf 'error' >&2; exit 7",
            ))
            .expect("execution");
        assert_eq!(outcome.exit_code, Some(7));
        assert!(!outcome.success);
        assert_eq!(outcome.stdout_bytes, 5);
        assert_eq!(outcome.stderr_bytes, 5);
        assert_eq!(
            fs::read(outcome.run_dir.join("stdout.log")).unwrap(),
            b"hello"
        );
        assert_eq!(
            fs::read(outcome.run_dir.join("stderr.log")).unwrap(),
            b"error"
        );
        let events = fs::read_to_string(outcome.run_dir.join("events.jsonl")).unwrap();
        assert!(events.contains("\"event\":\"started\""));
        assert!(events.contains("\"event\":\"finished\""));
        assert!(events.contains("\"exit_code\":7"));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn forwards_arguments_as_literals() {
        let root = test_root("literal");
        let mut spec = request(&root, "printf '%s' \"$1\"");
        spec.args.push(OsString::from("ignored-zero"));
        spec.args
            .push(OsStr::new("one; echo INJECTED").to_os_string());
        let outcome = LocalExecutor.run(&spec).expect("execution");
        assert!(outcome.success);
        assert_eq!(
            fs::read(outcome.run_dir.join("stdout.log")).unwrap(),
            b"one; echo INJECTED"
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn rejects_public_log_root() {
        let root = test_root("mode");
        fs::create_dir(&root).unwrap();
        fs::set_permissions(&root, fs::Permissions::from_mode(0o755)).unwrap();
        let error = LocalExecutor.run(&request(&root, "true")).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn isolated_runner_does_not_inherit_environment() {
        let root = test_root("isolated");
        let mut spec = request(
            &root,
            "printf '%s' \"${HOME:-missing}:${KOJORA_TEST_VALUE:-missing}\"",
        );
        spec.program = OsString::from("/bin/sh");
        let outcome = LocalExecutor
            .run_isolated(
                &spec,
                &[(
                    OsString::from("KOJORA_TEST_VALUE"),
                    OsString::from("present"),
                )],
            )
            .expect("isolated execution");
        assert!(outcome.success);
        assert_eq!(
            fs::read(outcome.run_dir.join("stdout.log")).unwrap(),
            b"missing:present"
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn cancellation_terminates_process_group_and_journals_outcome() {
        let root = test_root("cancellation");
        let token = CancellationToken::default();
        let request = request(&root, "trap '' TERM; sleep 15 & wait");
        let copy = token.clone();
        let start = Instant::now();
        let worker =
            thread::spawn(move || LocalExecutor.run_isolated_cancellable(&request, &[], &copy));
        thread::sleep(Duration::from_millis(250));
        token.cancel();
        let outcome = worker.join().unwrap().expect("cancellation result");
        assert!(outcome.cancelled);
        assert!(!outcome.success);
        assert!(start.elapsed() < Duration::from_secs(5));
        let events = fs::read_to_string(outcome.run_dir.join("events.jsonl")).unwrap();
        assert!(events.contains("\"cancelled\":true"));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn finished_process_is_not_marked_cancelled() {
        let root = test_root("not-cancelled");
        let token = CancellationToken::default();
        let outcome = LocalExecutor
            .run_isolated_cancellable(&request(&root, "printf ok"), &[], &token)
            .unwrap();
        assert!(outcome.success);
        assert!(!outcome.cancelled);
        assert_eq!(fs::read(outcome.run_dir.join("stdout.log")).unwrap(), b"ok");
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn resource_limits_validate_without_executing_untrusted_code() {
        assert!(ProcessLimits {
            address_space_mib: Some(0),
            cpu_seconds: None
        }
        .validate()
        .is_err());
        assert!(ProcessLimits {
            address_space_mib: Some(u64::MAX),
            cpu_seconds: None
        }
        .validate()
        .is_err());
        assert!(ProcessLimits {
            address_space_mib: Some(512),
            cpu_seconds: Some(10)
        }
        .validate()
        .is_ok());
    }

    #[test]
    fn limited_executor_still_captures_output() {
        let root = test_root("limit");
        let token = CancellationToken::default();
        let outcome = LocalExecutor
            .run_isolated_limited(
                &request(&root, "printf bounded"),
                &[],
                &token,
                ProcessLimits {
                    address_space_mib: Some(256),
                    cpu_seconds: Some(10),
                },
            )
            .unwrap();
        assert!(outcome.success);
        assert_eq!(
            fs::read(outcome.run_dir.join("stdout.log")).unwrap(),
            b"bounded"
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn missing_command_is_recorded() {
        let root = test_root("missing");
        let spec = RunSpec {
            program: OsString::from("kojora-nonexistent-program-xyz"),
            args: vec![],
            cwd: std::env::current_dir().unwrap(),
            log_root: root.clone(),
        };
        let error = LocalExecutor.run(&spec).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::NotFound);
        let run_dir = fs::read_dir(&root).unwrap().next().unwrap().unwrap().path();
        let events = fs::read_to_string(run_dir.join("events.jsonl")).unwrap();
        assert!(events.contains("\"event\":\"spawn_failed\""));
        fs::remove_dir_all(root).unwrap();
    }
}
