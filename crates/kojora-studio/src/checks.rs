//! Worker-owned, bounded, durable, shared queue for trusted local checks.
//! Provider agents never choose raw shell commands or set their own limits.

use kojora_runtime::{
    CancellationToken, IsolatedExecutor, LocalExecutor, ProcessLimits, RunOutcome, RunSpec,
};
use serde::{Deserialize, Serialize};
use std::ffi::OsString;
use std::fs::{self, DirBuilder, File, OpenOptions};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

#[cfg(unix)]
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt};

const MAX_JOURNAL_BYTES: u64 = 1024 * 1024;
const MAX_RECORDS: usize = 256;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HookPhase {
    Manual,
    PostEdit,
    PreMerge,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CheckKind {
    RustFmt,
    RustClippy,
    RustTest,
    Ruff,
    Biome,
}

impl CheckKind {
    /// No shell, npx, eval, arbitrary arguments, workspace-provided scripts,
    /// or provider-selected executable paths.
    fn command(self) -> (&'static str, &'static [&'static str]) {
        match self {
            Self::RustFmt => ("cargo", &["fmt", "--all", "--", "--check"]),
            Self::RustClippy => (
                "cargo",
                &[
                    "clippy",
                    "--workspace",
                    "--all-targets",
                    "--",
                    "-D",
                    "warnings",
                ],
            ),
            Self::RustTest => ("cargo", &["test", "--workspace"]),
            Self::Ruff => ("ruff", &["check", "."]),
            Self::Biome => ("biome", &["check", "."]),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CheckStatus {
    Queued,
    Running,
    Succeeded,
    Failed,
    Cancelled,
    Interrupted,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CheckRecord {
    pub id: u64,
    pub task_id: u64,
    pub hook: HookPhase,
    pub kind: CheckKind,
    pub status: CheckStatus,
    pub created_ms: u64,
    pub finished_ms: Option<u64>,
    pub run_dir: Option<PathBuf>,
    pub error: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct CheckJournal {
    next_id: u64,
    records: Vec<CheckRecord>,
}

impl Default for CheckJournal {
    fn default() -> Self {
        Self {
            next_id: 1,
            records: Vec::new(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct CheckPolicy {
    pub enabled: bool,
    pub max_pending: usize,
    pub timeout_secs: u64,
    /// These are per-process ceilings, not aggregate RSS/CPU budgets.
    pub limits: ProcessLimits,
    /// Post-edit hooks are opt-in, configured by the trusted Worker operator.
    pub post_edit: Vec<CheckKind>,
    pub pre_merge: Vec<CheckKind>,
}

impl Default for CheckPolicy {
    fn default() -> Self {
        Self {
            enabled: false,
            max_pending: 16,
            timeout_secs: 300,
            limits: ProcessLimits::default(),
            post_edit: Vec::new(),
            pre_merge: Vec::new(),
        }
    }
}

impl CheckPolicy {
    pub fn validate(&self) -> io::Result<()> {
        if self.max_pending == 0
            || self.max_pending > 64
            || self.timeout_secs == 0
            || self.timeout_secs > 3600
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "check queue needs max_pending 1..64 and timeout 1..3600 seconds",
            ));
        }
        self.limits.validate()?;
        if !self.enabled && (!self.post_edit.is_empty() || !self.pre_merge.is_empty()) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "hook recipes require explicit check enablement",
            ));
        }
        if self.enabled
            && (self.limits.address_space_mib.is_none() || self.limits.cpu_seconds.is_none())
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "enabled checks require explicit virtual-memory and CPU-time ceilings",
            ));
        }
        Ok(())
    }

    fn permits(&self, hook: HookPhase, kind: CheckKind) -> bool {
        self.enabled
            && match hook {
                HookPhase::Manual => true,
                HookPhase::PostEdit => self.post_edit.contains(&kind),
                HookPhase::PreMerge => self.pre_merge.contains(&kind),
            }
    }
}

pub type CheckAdmission =
    dyn Fn() -> io::Result<Option<crate::CheckPermit>> + Send + Sync + 'static;

type CheckRunner = dyn Fn(&Path, &Path, CheckKind, &CancellationToken, &CheckPolicy) -> io::Result<RunOutcome>
    + Send
    + Sync
    + 'static;

struct Job {
    id: u64,
    workspace: PathBuf,
    token: CancellationToken,
}

#[derive(Clone)]
pub struct CheckQueue {
    root: PathBuf,
    policy: CheckPolicy,
    activated_ms: u64,
    journal: Arc<Mutex<CheckJournal>>,
    sender: mpsc::Sender<Job>,
}

impl CheckQueue {
    pub fn open(root: impl Into<PathBuf>, policy: CheckPolicy) -> io::Result<Self> {
        Self::open_with_runner(root, policy, Arc::new(execute_builtin))
    }

    /// Coordinate check execution with agent admission: a check holds an
    /// exclusive scheduler slot until its process has fully exited.
    pub fn open_with_admission(
        root: impl Into<PathBuf>,
        policy: CheckPolicy,
        admission: Arc<CheckAdmission>,
    ) -> io::Result<Self> {
        Self::open_with_runner_and_admission(
            root,
            policy,
            Arc::new(execute_builtin),
            Some(admission),
        )
    }

    fn open_with_runner(
        root: impl Into<PathBuf>,
        policy: CheckPolicy,
        runner: Arc<CheckRunner>,
    ) -> io::Result<Self> {
        Self::open_with_runner_and_admission(root, policy, runner, None)
    }

    fn open_with_runner_and_admission(
        root: impl Into<PathBuf>,
        policy: CheckPolicy,
        runner: Arc<CheckRunner>,
        admission: Option<Arc<CheckAdmission>>,
    ) -> io::Result<Self> {
        policy.validate()?;
        let root = root.into();
        if !root.is_absolute() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "check data root must be absolute",
            ));
        }
        // Do not follow an attacker-supplied symlink or change an existing
        // directory's permissions. The parent is the Studio-owned 0700 root.
        let mut builder = DirBuilder::new();
        #[cfg(unix)]
        builder.mode(0o700);
        match builder.create(&root) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(error),
        }
        let metadata = fs::symlink_metadata(&root)?;
        if !metadata.file_type().is_dir() {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "checks directory must be a real private directory",
            ));
        }
        #[cfg(unix)]
        if metadata.uid() != unsafe { libc::geteuid() }
            || metadata.permissions().mode() & 0o077 != 0
        {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "checks directory must belong to the current user (mode 0700)",
            ));
        }
        let path = root.join("checks.json");
        let mut journal: CheckJournal = match fs::symlink_metadata(&path) {
            Ok(metadata) => {
                if !metadata.file_type().is_file() || metadata.len() > MAX_JOURNAL_BYTES {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "checks journal must be a bounded regular file, not a symlink",
                    ));
                }
                #[cfg(unix)]
                if metadata.uid() != unsafe { libc::geteuid() }
                    || metadata.permissions().mode() & 0o077 != 0
                {
                    return Err(io::Error::new(
                        io::ErrorKind::PermissionDenied,
                        "checks journal must belong to the current user (mode 0600)",
                    ));
                }
                let file = File::open(&path)?;
                let mut data = Vec::new();
                file.take(MAX_JOURNAL_BYTES + 1).read_to_end(&mut data)?;
                serde_json::from_slice(&data).map_err(io::Error::other)?
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => CheckJournal::default(),
            Err(error) => return Err(error),
        };
        let mut recovered = false;
        for record in &mut journal.records {
            if matches!(record.status, CheckStatus::Queued | CheckStatus::Running) {
                record.status = CheckStatus::Interrupted;
                record.error = Some("Worker restarted before check finished".into());
                record.finished_ms = Some(now_ms());
                recovered = true;
            }
        }
        if recovered {
            persist(&root, &journal)?;
        }
        let journal = Arc::new(Mutex::new(journal));
        let activated_ms = now_ms();
        let (sender, receiver) = mpsc::channel::<Job>();
        let shared = Arc::clone(&journal);
        let worker_root = root.clone();
        let worker_policy = policy.clone();
        thread::Builder::new()
            .name("kojora-check-queue".to_owned())
            .spawn(move || {
                while let Ok(job) = receiver.recv() {
                    execute_job(
                        &worker_root,
                        &worker_policy,
                        &shared,
                        &*runner,
                        admission.as_deref(),
                        job,
                    );
                }
            })?;
        Ok(Self {
            root,
            policy,
            activated_ms,
            journal,
            sender,
        })
    }

    /// Return a bounded tail of a check log, never the entire compiler output.
    pub fn read_log_tail(&self, check_id: u64, stderr: bool) -> io::Result<String> {
        let records = self.snapshot()?;
        let record = records
            .iter()
            .find(|r| r.id == check_id)
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "check not found"))?;
        let Some(run_dir) = &record.run_dir else {
            return Ok(String::new());
        };
        let prefix = self.root.join("runs").join(format!("check-{check_id}"));
        if !run_dir.starts_with(&prefix) {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "check log location is outside its private run directory",
            ));
        }
        let name = if stderr { "stderr.log" } else { "stdout.log" };
        crate::tail_file(&run_dir.join(name), 32 * 1024)
    }

    pub fn snapshot(&self) -> io::Result<Vec<CheckRecord>> {
        Ok(self.journal.lock().map_err(lock_error)?.records.clone())
    }

    pub fn is_enabled(&self) -> bool {
        self.policy.enabled
    }

    pub fn has_post_edit_hooks(&self) -> bool {
        !self.policy.post_edit.is_empty()
    }

    pub fn pending_count(&self) -> usize {
        self.journal.lock().map_or(0, |journal| {
            journal
                .records
                .iter()
                .filter(|r| matches!(r.status, CheckStatus::Queued | CheckStatus::Running))
                .count()
        })
    }

    /// A request must reference an existing detached worktree belonging to
    /// exactly this task. No arbitrary paths cross the worker IPC boundary.
    pub fn enqueue(
        &self,
        task_id: u64,
        hook: HookPhase,
        kind: CheckKind,
        workspace: &Path,
    ) -> io::Result<u64> {
        if !self.policy.permits(hook, kind) {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "check is not enabled for this hook",
            ));
        }
        let expected = self
            .root
            .parent()
            .ok_or_else(|| io::Error::other("check root has no parent"))?
            .join("worktrees")
            .join(format!("task-{task_id}"));
        let actual = fs::canonicalize(workspace)?;
        if fs::canonicalize(&expected)? != actual || !actual.is_dir() {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "check must use the task's own detached Git worktree",
            ));
        }
        let mut journal = self.journal.lock().map_err(lock_error)?;
        if let Some(existing) = journal.records.iter().find(|record| {
            record.task_id == task_id
                && record.hook == hook
                && record.kind == kind
                && (hook != HookPhase::Manual
                    || matches!(record.status, CheckStatus::Queued | CheckStatus::Running))
        }) {
            return Ok(existing.id);
        }
        if journal
            .records
            .iter()
            .filter(|r| matches!(r.status, CheckStatus::Queued | CheckStatus::Running))
            .count()
            >= self.policy.max_pending
        {
            return Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                "local check queue is full",
            ));
        }
        let mut proposed = journal.clone();
        if proposed.records.len() >= MAX_RECORDS {
            if let Some(index) = proposed
                .records
                .iter()
                .position(|r| !matches!(r.status, CheckStatus::Queued | CheckStatus::Running))
            {
                proposed.records.remove(index);
            } else {
                return Err(io::Error::new(
                    io::ErrorKind::WouldBlock,
                    "check history is full",
                ));
            }
        }
        let id = proposed.next_id;
        proposed.next_id = id
            .checked_add(1)
            .ok_or_else(|| io::Error::other("check ID exhausted"))?;
        proposed.records.push(CheckRecord {
            id,
            task_id,
            hook,
            kind,
            status: CheckStatus::Queued,
            created_ms: now_ms(),
            finished_ms: None,
            run_dir: None,
            error: None,
        });
        persist(&self.root, &proposed)?;
        *journal = proposed;
        if self
            .sender
            .send(Job {
                id,
                workspace: actual,
                token: CancellationToken::default(),
            })
            .is_err()
        {
            let mut candidate = journal.clone();
            if let Some(record) = candidate.records.iter_mut().find(|r| r.id == id) {
                record.status = CheckStatus::Interrupted;
                record.error = Some("check worker is unavailable".to_owned());
                record.finished_ms = Some(now_ms());
            }
            persist(&self.root, &candidate)?;
            *journal = candidate;
            return Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "check worker stopped",
            ));
        }
        Ok(id)
    }

    /// Consult the durable journal to avoid duplicate post-edit hooks across
    /// UI reconnect and Worker restarts.
    pub fn observe_post_edit(&self, studio: &crate::StudioState) {
        if self.policy.post_edit.is_empty() {
            return;
        }
        for task in &studio.tasks {
            if task.mode != crate::AgentMode::EditWorktree
                || task.status != crate::TaskStatus::Succeeded
            {
                continue;
            }
            let Some(run) = studio.runs.iter().find(|r| r.task_id == task.id) else {
                continue;
            };
            // Enabling hooks never backfills the whole old Task history.
            if run.finished_ms.unwrap_or(0) < self.activated_ms {
                continue;
            }
            let Some(worktree) = &run.worktree_dir else {
                continue;
            };
            for kind in &self.policy.post_edit {
                if let Err(error) = self.enqueue(task.id, HookPhase::PostEdit, *kind, worktree) {
                    eprintln!("post-edit check for task {}: {error}", task.id);
                }
            }
        }
    }
}

fn execute_job(
    root: &Path,
    policy: &CheckPolicy,
    shared: &Arc<Mutex<CheckJournal>>,
    runner: &CheckRunner,
    admission: Option<&CheckAdmission>,
    job: Job,
) {
    // Common admission gate: once claimed, no new agent may launch until
    // check completion. While agents are active, keep this check queued.
    let _permit = if let Some(try_claim) = admission {
        loop {
            match try_claim() {
                Ok(Some(permit)) => break Some(permit),
                Ok(None) => thread::sleep(Duration::from_millis(100)),
                Err(error) => {
                    if let Ok(mut state) = shared.lock() {
                        let mut proposed = state.clone();
                        if let Some(record) = proposed.records.iter_mut().find(|r| r.id == job.id) {
                            record.status = CheckStatus::Failed;
                            record.error = Some(format!("check admission failed: {error}"));
                            record.finished_ms = Some(now_ms());
                        }
                        if persist(root, &proposed).is_ok() {
                            *state = proposed;
                        }
                    }
                    return;
                }
            }
        }
    } else {
        None
    };
    let mut guard = match shared.lock() {
        Ok(v) => v,
        Err(_) => return,
    };
    let Some(record) = guard.records.iter().find(|r| r.id == job.id) else {
        return;
    };
    if record.status != CheckStatus::Queued {
        return;
    }
    let kind = record.kind;
    let mut proposed = guard.clone();
    if let Some(record) = proposed.records.iter_mut().find(|r| r.id == job.id) {
        record.status = CheckStatus::Running;
    }
    if persist(root, &proposed).is_err() {
        return;
    }
    *guard = proposed;
    drop(guard);

    let timed_out = Arc::new(AtomicBool::new(false));
    let timed_out_worker = Arc::clone(&timed_out);
    let token = job.token;
    let stopper = token.clone();
    let (done_tx, done_rx) = mpsc::channel::<()>();
    let timeout = Duration::from_secs(policy.timeout_secs);
    let watcher = thread::spawn(move || {
        if matches!(
            done_rx.recv_timeout(timeout),
            Err(mpsc::RecvTimeoutError::Timeout)
        ) {
            timed_out_worker.store(true, Ordering::Release);
            stopper.cancel();
        }
    });
    let log_root = root.join("runs").join(format!("check-{}", job.id));
    let outcome = runner(&job.workspace, &log_root, kind, &token, policy);
    drop(done_tx);
    let _ = watcher.join();

    let (status, error, run_dir) = match outcome {
        Ok(result) => {
            let status = if timed_out.load(Ordering::Acquire) || result.cancelled {
                CheckStatus::Cancelled
            } else if result.success {
                CheckStatus::Succeeded
            } else {
                CheckStatus::Failed
            };
            let reason = if timed_out.load(Ordering::Acquire) {
                Some("check exceeded wall-clock timeout".to_owned())
            } else {
                None
            };
            (status, reason, Some(result.run_dir))
        }
        Err(error) => (CheckStatus::Failed, Some(error.to_string()), None),
    };
    if let Ok(mut state) = shared.lock() {
        let mut proposed = state.clone();
        if let Some(record) = proposed.records.iter_mut().find(|r| r.id == job.id) {
            record.status = status;
            record.error = error;
            record.finished_ms = Some(now_ms());
            record.run_dir = run_dir;
        }
        if let Err(error) = persist(root, &proposed) {
            eprintln!("failed to persist completed check #{}: {error}", job.id);
        } else {
            *state = proposed;
        }
    }
}

fn execute_builtin(
    workspace: &Path,
    log_root: &Path,
    kind: CheckKind,
    token: &CancellationToken,
    policy: &CheckPolicy,
) -> io::Result<RunOutcome> {
    let (program, args) = kind.command();
    let target_dir = log_root
        .parent()
        .and_then(Path::parent)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "invalid check path"))?
        .join("target");
    fs::create_dir_all(&target_dir)?;
    #[cfg(unix)]
    fs::set_permissions(&target_dir, fs::Permissions::from_mode(0o700))?;
    let environment = ["PATH", "HOME", "CARGO_HOME", "RUSTUP_HOME", "TMPDIR"]
        .into_iter()
        .filter_map(|name| std::env::var_os(name).map(|value| (OsString::from(name), value)))
        .chain([
            (OsString::from("CARGO_BUILD_JOBS"), OsString::from("1")),
            (OsString::from("CARGO_INCREMENTAL"), OsString::from("0")),
            (
                OsString::from("CARGO_TARGET_DIR"),
                target_dir.into_os_string(),
            ),
            (OsString::from("CARGO_TERM_COLOR"), OsString::from("never")),
        ])
        .collect::<Vec<_>>();
    LocalExecutor.run_isolated_limited(
        &RunSpec {
            program: OsString::from(program),
            args: args.iter().map(|arg| OsString::from(*arg)).collect(),
            cwd: workspace.to_owned(),
            log_root: log_root.to_owned(),
        },
        &environment,
        token,
        policy.limits,
    )
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as u64)
}

fn lock_error<T>(_: std::sync::PoisonError<T>) -> io::Error {
    io::Error::other("check journal lock is poisoned")
}

fn persist(root: &Path, journal: &CheckJournal) -> io::Result<()> {
    let bytes = serde_json::to_vec(journal).map_err(io::Error::other)?;
    if bytes.len() as u64 > MAX_JOURNAL_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "check journal size limit",
        ));
    }
    let tmp = root.join(format!("checks-{}-{}.tmp", std::process::id(), now_ms()));
    let mut opts = OpenOptions::new();
    opts.write(true).create_new(true);
    #[cfg(unix)]
    opts.mode(0o600);
    let result = (|| -> io::Result<()> {
        let mut file = opts.open(&tmp)?;
        file.write_all(&bytes)?;
        file.sync_all()?;
        fs::rename(&tmp, root.join("checks.json"))?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    fn setup() -> (PathBuf, CheckPolicy, PathBuf) {
        let root = std::env::temp_dir().join(format!(
            "kojora-checks-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let worktree = root.join("worktrees").join("task-1");
        fs::create_dir_all(&worktree).unwrap();
        #[cfg(unix)]
        fs::set_permissions(&root, fs::Permissions::from_mode(0o700)).unwrap();
        let policy = CheckPolicy {
            enabled: true,
            max_pending: 4,
            timeout_secs: 5,
            limits: ProcessLimits {
                address_space_mib: Some(8192),
                cpu_seconds: Some(60),
            },
            post_edit: vec![CheckKind::RustFmt],
            pre_merge: vec![],
        };
        (root, policy, worktree)
    }

    #[cfg(unix)]
    #[test]
    fn rejects_symlinked_check_state_without_modifying_target_permissions() {
        use std::os::unix::fs::symlink;
        let (root, policy, _) = setup();
        let target = root.join("other");
        fs::create_dir(&target).unwrap();
        fs::set_permissions(&target, fs::Permissions::from_mode(0o755)).unwrap();
        let checks_root = root.join("checks");
        symlink(&target, &checks_root).unwrap();
        assert_eq!(
            CheckQueue::open(&checks_root, policy).err().unwrap().kind(),
            io::ErrorKind::PermissionDenied
        );
        assert_eq!(
            fs::metadata(&target).unwrap().permissions().mode() & 0o777,
            0o755
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn rejects_public_check_state_and_untrusted_journal_files() {
        use std::os::unix::fs::symlink;
        let (root, policy, _) = setup();
        let checks_root = root.join("checks");
        fs::create_dir(&checks_root).unwrap();
        fs::set_permissions(&checks_root, fs::Permissions::from_mode(0o755)).unwrap();
        assert_eq!(
            CheckQueue::open(&checks_root, policy.clone())
                .err()
                .unwrap()
                .kind(),
            io::ErrorKind::PermissionDenied
        );
        assert_eq!(
            fs::metadata(&checks_root).unwrap().permissions().mode() & 0o777,
            0o755
        );
        fs::set_permissions(&checks_root, fs::Permissions::from_mode(0o700)).unwrap();
        let other = root.join("other.json");
        fs::write(
            &other,
            serde_json::to_vec(&CheckJournal::default()).unwrap(),
        )
        .unwrap();
        let journal_file = checks_root.join("checks.json");
        symlink(&other, &journal_file).unwrap();
        assert!(CheckQueue::open(&checks_root, policy.clone()).is_err());
        fs::remove_file(&journal_file).unwrap();
        fs::write(&journal_file, fs::read(&other).unwrap()).unwrap();
        fs::set_permissions(&journal_file, fs::Permissions::from_mode(0o644)).unwrap();
        assert_eq!(
            CheckQueue::open(&checks_root, policy.clone())
                .err()
                .unwrap()
                .kind(),
            io::ErrorKind::PermissionDenied
        );
        fs::set_permissions(&journal_file, fs::Permissions::from_mode(0o600)).unwrap();
        let queue = CheckQueue::open(&checks_root, policy).unwrap();
        assert!(queue.snapshot().unwrap().is_empty());
        drop(queue);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn validates_fail_closed_limit_configuration() {
        let mut policy = CheckPolicy {
            enabled: true,
            ..CheckPolicy::default()
        };
        assert!(policy.validate().is_err());
        policy.limits = ProcessLimits {
            address_space_mib: Some(8192),
            cpu_seconds: Some(60),
        };
        assert!(policy.validate().is_ok());
    }

    #[test]
    fn deduplicates_post_edit_checks_and_persists_history() {
        let (root, policy, worktree) = setup();
        let counter = Arc::new(AtomicUsize::new(0));
        let counts = Arc::clone(&counter);
        let runner: Arc<CheckRunner> = Arc::new(move |_, log_root, _, _, _| {
            counts.fetch_add(1, Ordering::Relaxed);
            let dir = log_root.join("result");
            fs::create_dir_all(&dir)?;
            Ok(RunOutcome {
                run_dir: dir,
                exit_code: Some(0),
                success: true,
                cancelled: false,
                duration: Duration::ZERO,
                stdout_bytes: 0,
                stderr_bytes: 0,
            })
        });
        let checks =
            CheckQueue::open_with_runner(root.join("checks"), policy.clone(), runner).unwrap();
        let a = checks
            .enqueue(1, HookPhase::PostEdit, CheckKind::RustFmt, &worktree)
            .unwrap();
        let b = checks
            .enqueue(1, HookPhase::PostEdit, CheckKind::RustFmt, &worktree)
            .unwrap();
        assert_eq!(a, b);
        for _ in 0..200 {
            if checks.snapshot().unwrap()[0].status == CheckStatus::Succeeded {
                break;
            }
            thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(checks.snapshot().unwrap()[0].status, CheckStatus::Succeeded);
        assert_eq!(counter.load(Ordering::Relaxed), 1);
        drop(checks);
        let reopened = CheckQueue::open(root.join("checks"), policy).unwrap();
        assert_eq!(
            reopened.snapshot().unwrap()[0].status,
            CheckStatus::Succeeded
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn check_waits_for_active_agent_before_claiming_shared_slot() {
        use crate::scheduler::AdmissionPolicy;
        use crate::{AgentRunner, Studio};
        let (root, policy, worktree) = setup();
        let release_agent = Arc::new(AtomicBool::new(false));
        let agent_flag = Arc::clone(&release_agent);
        let agent: Arc<AgentRunner> = Arc::new(move |_, _, log_root, _, _| {
            while !agent_flag.load(Ordering::Acquire) {
                thread::sleep(Duration::from_millis(10));
            }
            let dir = log_root.join("fake");
            fs::create_dir_all(&dir)?;
            Ok(RunOutcome {
                run_dir: dir,
                exit_code: Some(0),
                success: true,
                cancelled: false,
                duration: Duration::ZERO,
                stdout_bytes: 0,
                stderr_bytes: 0,
            })
        });
        let studio = Arc::new(
            Studio::open_with_runner_and_policy(&root, agent, AdmissionPolicy::single()).unwrap(),
        );
        let bot = studio
            .add_bot("Tester", root.join("workspace"), root.join("profile"))
            .unwrap();
        let _id = studio.submit(bot, "Long running").unwrap();
        let for_checks = Arc::clone(&studio);
        let check_runner: Arc<CheckRunner> = Arc::new(|_, log_root, _, _, _| {
            let dir = log_root.join("test");
            fs::create_dir_all(&dir)?;
            Ok(RunOutcome {
                run_dir: dir,
                exit_code: Some(0),
                success: true,
                cancelled: false,
                duration: Duration::ZERO,
                stdout_bytes: 0,
                stderr_bytes: 0,
            })
        });
        let checks = CheckQueue::open_with_runner_and_admission(
            root.join("checks"),
            policy,
            check_runner,
            Some(Arc::new(move || for_checks.try_acquire_check_slot())),
        )
        .unwrap();
        checks
            .enqueue(1, HookPhase::PostEdit, CheckKind::RustFmt, &worktree)
            .unwrap();
        thread::sleep(Duration::from_millis(75));
        assert_eq!(checks.snapshot().unwrap()[0].status, CheckStatus::Queued);
        assert_eq!(studio.active_task_count(), 1);
        release_agent.store(true, Ordering::Release);
        for _ in 0..200 {
            if checks.snapshot().unwrap()[0].status == CheckStatus::Succeeded {
                break;
            }
            thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(checks.snapshot().unwrap()[0].status, CheckStatus::Succeeded);
        assert!(!studio.is_active());
        drop(checks);
        drop(studio);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn forbids_checking_arbitrary_paths_and_disabled_recipes() {
        let (root, policy, worktree) = setup();
        let checks = CheckQueue::open(root.join("checks"), policy).unwrap();
        assert!(checks
            .enqueue(2, HookPhase::Manual, CheckKind::RustFmt, &worktree)
            .is_err());
        assert!(checks
            .enqueue(1, HookPhase::PreMerge, CheckKind::RustTest, &worktree)
            .is_err());
        drop(checks);
        fs::remove_dir_all(root).unwrap();
    }
}
