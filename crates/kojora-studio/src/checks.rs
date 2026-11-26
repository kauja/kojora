//! Worker-owned, bounded, durable, shared queue for trusted local checks.
//! Provider agents never choose raw shell commands or set their own limits.

use kojora_runtime::{
    CancellationToken, IsolatedExecutor, LocalExecutor, ProcessLimits, RunOutcome, RunSpec,
};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
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
// Room for a finished timestamp, terminal status, and a short diagnostic.
const CHECK_COMPLETION_FIELDS_BYTES: usize = 128;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HookPhase {
    Manual,
    PostEdit,
    PreMerge,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
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
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    queued_worktrees: BTreeMap<u64, WorktreeIdentity>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    next_post_edit: BTreeMap<CheckKind, u64>,
}

impl Default for CheckJournal {
    fn default() -> Self {
        Self {
            next_id: 1,
            records: Vec::new(),
            queued_worktrees: BTreeMap::new(),
            next_post_edit: BTreeMap::new(),
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
    #[cfg(unix)]
    opened_workspace: File,
    token: CancellationToken,
}

#[derive(PartialEq, Eq, Serialize, Deserialize, Clone, Debug)]
struct WorktreeIdentity {
    path: PathBuf,
    device: u64,
    inode: u64,
    created: SystemTime,
}

impl WorktreeIdentity {
    #[cfg(unix)]
    fn from_metadata(path: PathBuf, metadata: fs::Metadata) -> Option<Self> {
        if !metadata.is_dir() {
            return None;
        }
        Some(Self {
            path,
            device: metadata.dev(),
            inode: metadata.ino(),
            created: metadata.created().ok()?,
        })
    }
}

/// The automatic checks opted in for this run and the checkout they may use.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PostEditChecks {
    kinds: Vec<CheckKind>,
    identity: Option<WorktreeIdentity>,
}

impl PostEditChecks {
    pub(crate) fn planned(kinds: &[CheckKind]) -> Option<Self> {
        (!kinds.is_empty()).then(|| Self {
            kinds: kinds.to_vec(),
            identity: None,
        })
    }

    pub(crate) fn identify_worktree(&mut self, path: &Path) {
        #[cfg(unix)]
        {
            self.identity = (|| {
                let path = fs::canonicalize(path).ok()?;
                let metadata = fs::symlink_metadata(&path).ok()?;
                WorktreeIdentity::from_metadata(path, metadata)
            })();
        }
        #[cfg(not(unix))]
        let _ = path;
    }
}

impl Job {
    fn open(id: u64, workspace: PathBuf) -> io::Result<Self> {
        let job = Self {
            id,
            #[cfg(unix)]
            opened_workspace: File::open(&workspace)?,
            workspace,
            token: CancellationToken::default(),
        };
        job.verify_workspace()?;
        Ok(job)
    }

    fn identity(&self) -> Option<WorktreeIdentity> {
        #[cfg(unix)]
        {
            WorktreeIdentity::from_metadata(
                self.workspace.clone(),
                self.opened_workspace.metadata().ok()?,
            )
        }
        #[cfg(not(unix))]
        None
    }

    fn verify_workspace(&self) -> io::Result<()> {
        let metadata = fs::symlink_metadata(&self.workspace).map_err(|error| {
            io::Error::new(
                error.kind(),
                format!("check worktree is unavailable: {error}"),
            )
        })?;
        if !metadata.file_type().is_dir() {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "check worktree is no longer a real directory",
            ));
        }
        #[cfg(unix)]
        {
            let opened = self.opened_workspace.metadata()?;
            if metadata.dev() != opened.dev() || metadata.ino() != opened.ino() {
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "check worktree was replaced after queueing",
                ));
            }
        }
        Ok(())
    }
}

fn validate_worktree(root: &Path, task_id: u64, workspace: &Path) -> io::Result<PathBuf> {
    let worktrees = root
        .parent()
        .ok_or_else(|| io::Error::other("check root has no parent"))?
        .join("worktrees");
    let expected = worktrees.join(format!("task-{task_id}"));
    let actual = fs::canonicalize(workspace)?;
    if !fs::symlink_metadata(&worktrees)?.file_type().is_dir()
        || !fs::symlink_metadata(&expected)?.file_type().is_dir()
        || fs::canonicalize(&expected)? != actual
    {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "check must use the task's own detached Git worktree",
        ));
    }
    Ok(actual)
}

fn restore_queued_job(
    root: &Path,
    record: &CheckRecord,
    identity: &WorktreeIdentity,
    state: Option<&crate::StudioState>,
) -> Option<Job> {
    let state = state?;
    let task = state.tasks.iter().find(|task| {
        task.id == record.task_id
            && task.mode == crate::AgentMode::EditWorktree
            && task.status != crate::TaskStatus::Running
    })?;
    let run = state
        .runs
        .iter()
        .find(|run| run.id == task.run_id && run.task_id == task.id)?;
    let workspace = validate_worktree(root, task.id, run.worktree_dir.as_deref()?).ok()?;
    let job = Job::open(record.id, workspace).ok()?;
    (job.identity().as_ref()? == identity).then_some(job)
}

#[derive(Clone)]
pub struct CheckQueue {
    root: PathBuf,
    policy: CheckPolicy,
    activated_ms: u64,
    journal: Arc<Mutex<CheckJournal>>,
    observer: Arc<Mutex<()>>,
    started_at_task: Option<u64>,
    sender: mpsc::Sender<Job>,
}

struct WorkerStartup<'a> {
    state: &'a crate::StudioState,
    recovered_incomplete_runs: bool,
}

#[derive(Clone, Copy)]
struct PostEditObservation<'a> {
    next_task: u64,
    identity: Option<&'a WorktreeIdentity>,
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
            None,
        )
    }

    #[cfg(unix)]
    pub(crate) fn open_for_worker(
        root: impl Into<PathBuf>,
        policy: CheckPolicy,
        admission: Arc<CheckAdmission>,
        state: &crate::StudioState,
        recovered_incomplete_runs: bool,
    ) -> io::Result<Self> {
        Self::open_with_runner_and_admission(
            root,
            policy,
            Arc::new(execute_builtin),
            Some(admission),
            Some(WorkerStartup {
                state,
                recovered_incomplete_runs,
            }),
        )
    }

    fn open_with_runner(
        root: impl Into<PathBuf>,
        policy: CheckPolicy,
        runner: Arc<CheckRunner>,
    ) -> io::Result<Self> {
        Self::open_with_runner_and_admission(root, policy, runner, None, None)
    }

    fn open_with_runner_and_admission(
        root: impl Into<PathBuf>,
        policy: CheckPolicy,
        runner: Arc<CheckRunner>,
        admission: Option<Arc<CheckAdmission>>,
        startup: Option<WorkerStartup<'_>>,
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
        let owner = rustix::process::geteuid().as_raw();
        #[cfg(unix)]
        if metadata.uid() != owner || metadata.permissions().mode() & 0o077 != 0 {
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
                if metadata.uid() != owner || metadata.permissions().mode() & 0o077 != 0 {
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
        // A previously started check could still be alive after a Worker crash.
        let recovery_state = startup
            .as_ref()
            .filter(|startup| {
                !startup.recovered_incomplete_runs
                    && !journal
                        .records
                        .iter()
                        .any(|record| record.status == CheckStatus::Running)
            })
            .map(|startup| startup.state);
        let mut identities = std::mem::take(&mut journal.queued_worktrees);
        let mut recovered = !identities.is_empty();
        let mut ready = Vec::new();
        for record in &mut journal.records {
            if record.status == CheckStatus::Queued
                && policy.permits(record.hook, record.kind)
                && ready.len() < policy.max_pending
            {
                if let Some(identity) = identities.remove(&record.id) {
                    if let Some(job) = restore_queued_job(&root, record, &identity, recovery_state)
                    {
                        journal.queued_worktrees.insert(record.id, identity);
                        ready.push(job);
                        continue;
                    }
                }
            }
            let reason = match record.status {
                CheckStatus::Queued => "Queued check could not be verified after restart",
                CheckStatus::Running => "Worker restarted before check finished",
                _ => continue,
            };
            record.status = CheckStatus::Interrupted;
            record.error = Some(reason.into());
            record.finished_ms = Some(now_ms());
            recovered = true;
        }
        let started_at_task = startup.map(|startup| startup.state.next_id);
        if let Some(next_id) = started_at_task {
            let next_post_edit = policy
                .post_edit
                .iter()
                .copied()
                .map(|kind| {
                    let next = if recovery_state.is_some() {
                        journal
                            .next_post_edit
                            .get(&kind)
                            .copied()
                            .unwrap_or(next_id)
                            .min(next_id)
                    } else {
                        next_id
                    };
                    (kind, next)
                })
                .collect();
            recovered |= journal.next_post_edit != next_post_edit;
            journal.next_post_edit = next_post_edit;
        }
        if recovered {
            persist(&root, &journal)?;
        }
        let journal = Arc::new(Mutex::new(journal));
        let activated_ms = now_ms();
        let (sender, receiver) = mpsc::channel::<Job>();
        for job in ready {
            sender
                .send(job)
                .map_err(|_| io::Error::other("check queue is unavailable"))?;
        }
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
            observer: Arc::new(Mutex::new(())),
            started_at_task,
            sender,
        })
    }

    /// Return a bounded tail of a check log, never the entire compiler output.
    pub fn read_log_tail(&self, check_id: u64, stderr: bool) -> io::Result<String> {
        let run_dir = self
            .journal
            .lock()
            .map_err(lock_error)?
            .records
            .iter()
            .find(|r| r.id == check_id)
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "check not found"))?
            .run_dir
            .clone();
        let Some(run_dir) = run_dir else {
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

    #[cfg(unix)]
    pub(crate) fn post_edit_kinds(&self) -> &[CheckKind] {
        &self.policy.post_edit
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
        self.enqueue_with_observation(task_id, hook, kind, workspace, None)
    }

    fn enqueue_with_observation(
        &self,
        task_id: u64,
        hook: HookPhase,
        kind: CheckKind,
        workspace: &Path,
        observation: Option<PostEditObservation<'_>>,
    ) -> io::Result<u64> {
        if !self.policy.permits(hook, kind) {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "check is not enabled for this hook",
            ));
        }
        let actual = validate_worktree(&self.root, task_id, workspace)?;
        let mut journal = self.journal.lock().map_err(lock_error)?;
        if let Some(existing) = journal.records.iter().find(|record| {
            record.task_id == task_id
                && record.hook == hook
                && record.kind == kind
                && (hook == HookPhase::PostEdit
                    || matches!(record.status, CheckStatus::Queued | CheckStatus::Running))
        }) {
            let id = existing.id;
            if let Some(observation) = observation {
                self.advance_post_edit(&mut journal, kind, observation.next_task)?;
            }
            return Ok(id);
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
        let job = Job::open(id, actual)?;
        if let Some(observation) = observation {
            if let Some(identity) = observation.identity {
                if job.identity().as_ref() != Some(identity) {
                    return Err(io::Error::new(
                        io::ErrorKind::PermissionDenied,
                        "post-edit worktree no longer matches the task checkout",
                    ));
                }
            }
            proposed.next_post_edit.insert(kind, observation.next_task);
        }
        if let Some(identity) = job.identity() {
            proposed.queued_worktrees.insert(id, identity);
        }
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
        if self.sender.send(job).is_err() {
            let mut candidate = journal.clone();
            candidate.queued_worktrees.remove(&id);
            if let Some(record) = candidate.records.iter_mut().find(|r| r.id == id) {
                record.status = CheckStatus::Interrupted;
                record.error = Some("check worker is unavailable".to_owned());
                record.finished_ms = Some(now_ms());
            }
            persist_finished(&self.root, &mut candidate, id)?;
            *journal = candidate;
            return Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "check worker stopped",
            ));
        }
        Ok(id)
    }

    /// Schedule configured hooks for newly completed edit tasks.
    pub fn observe_post_edit(&self, studio: &crate::StudioState) {
        if let Err(error) = self.try_observe_post_edit(studio) {
            if error.kind() != io::ErrorKind::WouldBlock {
                eprintln!("could not observe post-edit checks: {error}");
            }
        }
    }

    /// Report transient queue saturation so shutdown cannot discard an unqueued hook.
    pub(crate) fn try_observe_post_edit(&self, studio: &crate::StudioState) -> io::Result<()> {
        if self.policy.post_edit.is_empty() {
            return Ok(());
        }
        let _observer = self.observer.lock().map_err(lock_error)?;
        let mut cursors = {
            let journal = self.journal.lock().map_err(lock_error)?;
            self.policy
                .post_edit
                .iter()
                .map(|&kind| {
                    (
                        kind,
                        journal
                            .next_post_edit
                            .get(&kind)
                            .copied()
                            .unwrap_or_default(),
                    )
                })
                .collect::<BTreeMap<_, _>>()
        };
        // The Studio appends tasks in ID order. Pending checks require the
        // same exclusive slot that an unfinished edit already holds.
        for task in &studio.tasks {
            if task.mode != crate::AgentMode::EditWorktree
                || cursors.values().all(|&next| next > task.id)
            {
                continue;
            }
            if task.status == crate::TaskStatus::Running {
                break;
            }
            if task.status != crate::TaskStatus::Succeeded {
                continue;
            }
            let Some(run) = studio.runs.iter().find(|run| {
                run.id == task.run_id
                    && run.task_id == task.id
                    && run.status == crate::TaskStatus::Succeeded
            }) else {
                continue;
            };
            let Some(worktree) = &run.worktree_dir else {
                continue;
            };
            let (kinds, identity) = if let Some(started_at_task) = self.started_at_task {
                let Some(checks) = &run.post_edit_checks else {
                    continue;
                };
                if task.id < started_at_task && checks.identity.is_none() {
                    continue;
                }
                (checks.kinds.as_slice(), checks.identity.as_ref())
            } else {
                if run.finished_ms.unwrap_or(0) < self.activated_ms {
                    continue;
                }
                (self.policy.post_edit.as_slice(), None)
            };
            for &kind in &self.policy.post_edit {
                let next = cursors.entry(kind).or_default();
                if *next > task.id || !kinds.contains(&kind) {
                    continue;
                }
                let observation = PostEditObservation {
                    next_task: task.id.saturating_add(1),
                    identity,
                };
                match self.enqueue_with_observation(
                    task.id,
                    HookPhase::PostEdit,
                    kind,
                    worktree,
                    Some(observation),
                ) {
                    Ok(_) => {}
                    Err(error)
                        if matches!(
                            error.kind(),
                            io::ErrorKind::NotFound | io::ErrorKind::PermissionDenied
                        ) =>
                    {
                        eprintln!("post-edit check for task {}: {error}", task.id);
                        let mut journal = self.journal.lock().map_err(lock_error)?;
                        self.advance_post_edit(&mut journal, kind, observation.next_task)?;
                    }
                    Err(error) => return Err(error),
                }
                *next = observation.next_task;
            }
        }
        Ok(())
    }

    fn advance_post_edit(
        &self,
        journal: &mut CheckJournal,
        kind: CheckKind,
        next_task: u64,
    ) -> io::Result<()> {
        if next_task
            > journal
                .next_post_edit
                .get(&kind)
                .copied()
                .unwrap_or_default()
        {
            let mut proposed = journal.clone();
            proposed.next_post_edit.insert(kind, next_task);
            persist(&self.root, &proposed)?;
            *journal = proposed;
        }
        Ok(())
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
                        proposed.queued_worktrees.remove(&job.id);
                        if let Some(record) = proposed.records.iter_mut().find(|r| r.id == job.id) {
                            record.status = CheckStatus::Failed;
                            record.error = Some(format!("check admission failed: {error}"));
                            record.finished_ms = Some(now_ms());
                        }
                        if persist_finished(root, &mut proposed, job.id).is_ok() {
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
    let was_recoverable = proposed.queued_worktrees.remove(&job.id).is_some();
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
    let token = &job.token;
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
    // Make the Running transition durable before allowing replayable checks to execute.
    let outcome = if was_recoverable {
        File::open(root).and_then(|dir| dir.sync_all())
    } else {
        Ok(())
    }
    .and_then(|()| job.verify_workspace())
    .and_then(|()| runner(&job.workspace, &log_root, kind, token, policy));
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
        if let Err(error) = persist_finished(root, &mut proposed, job.id) {
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

fn encode_journal(root: &Path, journal: &CheckJournal) -> io::Result<Vec<u8>> {
    let pending = journal
        .records
        .iter()
        .filter(|record| matches!(record.status, CheckStatus::Queued | CheckStatus::Running));
    let mut budget = MAX_JOURNAL_BYTES as usize;
    if pending.clone().next().is_some() {
        // Include the escaped check root and the generated run-directory suffix.
        let path_bytes = serde_json::to_vec(root).map_err(io::Error::other)?.len() + 96;
        for record in pending {
            // The serialized status grows by one byte when it moves to Running.
            let before_running = usize::from(record.status == CheckStatus::Queued);
            budget =
                budget.saturating_sub(CHECK_COMPLETION_FIELDS_BYTES + path_bytes + before_running);
        }
    }
    let bytes = serde_json::to_vec(journal).map_err(io::Error::other)?;
    if bytes.len() > budget {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "check journal size limit including pending check reservations",
        ));
    }
    Ok(bytes)
}

fn persist_finished(root: &Path, journal: &mut CheckJournal, id: u64) -> io::Result<()> {
    let bytes = match encode_journal(root, journal) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == io::ErrorKind::InvalidData => {
            if let Some(record) = journal.records.iter_mut().find(|record| record.id == id) {
                record.run_dir = None;
                if record.error.is_some() {
                    record.error = Some("Check error omitted because the journal is full".into());
                }
            }
            encode_journal(root, journal)?
        }
        Err(error) => return Err(error),
    };
    write_journal(root, &bytes)
}

fn persist(root: &Path, journal: &CheckJournal) -> io::Result<()> {
    write_journal(root, &encode_journal(root, journal)?)
}

fn write_journal(root: &Path, bytes: &[u8]) -> io::Result<()> {
    let tmp = root.join(format!("checks-{}-{}.tmp", std::process::id(), now_ms()));
    let mut opts = OpenOptions::new();
    opts.write(true).create_new(true);
    #[cfg(unix)]
    opts.mode(0o600);
    let result = (|| -> io::Result<()> {
        let mut file = opts.open(&tmp)?;
        file.write_all(bytes)?;
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
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "test fixtures and assertions can fail immediately"
)]
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

    fn completed_edit(finished_ms: u64, worktree: PathBuf) -> crate::StudioState {
        crate::StudioState {
            next_id: 5,
            tasks: vec![crate::TaskRecord {
                id: 1,
                bot_id: 2,
                run_id: 3,
                prompt: "Edit".into(),
                mode: crate::AgentMode::EditWorktree,
                status: crate::TaskStatus::Succeeded,
                created_ms: finished_ms,
            }],
            runs: vec![crate::RunRecord {
                id: 3,
                task_id: 1,
                session_id: 4,
                status: crate::TaskStatus::Succeeded,
                started_ms: finished_ms,
                finished_ms: Some(finished_ms),
                exit_code: Some(0),
                run_dir: None,
                worktree_dir: Some(worktree),
                post_edit_checks: None,
                error: None,
            }],
            ..crate::StudioState::default()
        }
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
    fn completed_pre_merge_checks_can_be_queued_again() {
        let (root, mut policy, worktree) = setup();
        policy.pre_merge = vec![CheckKind::RustFmt];
        let runner: Arc<CheckRunner> =
            Arc::new(|_, _, _, _, _| Err(io::Error::other("fixture result")));
        let checks = CheckQueue::open_with_runner(root.join("checks"), policy, runner).unwrap();
        let first = checks
            .enqueue(1, HookPhase::PreMerge, CheckKind::RustFmt, &worktree)
            .unwrap();
        for _ in 0..200 {
            if checks.pending_count() == 0 {
                break;
            }
            thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(checks.pending_count(), 0);
        fs::write(worktree.join("changed.txt"), "new contents").unwrap();
        let second = checks
            .enqueue(1, HookPhase::PreMerge, CheckKind::RustFmt, &worktree)
            .unwrap();
        for _ in 0..200 {
            if checks.pending_count() == 0 {
                break;
            }
            thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(checks.pending_count(), 0);
        assert!(second > first);
        drop(checks);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn observer_reports_queue_saturation_and_retries_the_unqueued_hook() {
        let (root, mut policy, worktree) = setup();
        policy.max_pending = 1;
        policy.post_edit = vec![CheckKind::RustFmt, CheckKind::RustClippy];
        let release = Arc::new(AtomicBool::new(false));
        let gate = Arc::clone(&release);
        let runner: Arc<CheckRunner> = Arc::new(move |_, _, _, token, _| {
            while !gate.load(Ordering::Acquire) && !token.is_cancelled() {
                thread::sleep(Duration::from_millis(10));
            }
            Err(io::Error::other("fake check result"))
        });
        let checks = CheckQueue::open_with_runner(root.join("checks"), policy, runner).unwrap();
        let state = completed_edit(checks.activated_ms, worktree);

        let blocked = checks.try_observe_post_edit(&state);
        release.store(true, Ordering::Release);
        for _ in 0..200 {
            if checks.pending_count() == 0 {
                break;
            }
            thread::sleep(Duration::from_millis(10));
        }
        // The original observation remains blocked after the first job exits.
        assert_eq!(checks.pending_count(), 0);
        assert_eq!(blocked.unwrap_err().kind(), io::ErrorKind::WouldBlock);
        checks.try_observe_post_edit(&state).unwrap();
        for _ in 0..200 {
            if checks.pending_count() == 0 {
                break;
            }
            thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(checks.pending_count(), 0);
        let records = checks.snapshot().unwrap();
        assert_eq!(records.len(), 2);
        assert_eq!(records[0].kind, CheckKind::RustFmt);
        assert_eq!(records[1].kind, CheckKind::RustClippy);
        drop(checks);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn oversized_check_diagnostic_cannot_keep_the_queue_pending() {
        let (root, policy, worktree) = setup();
        let runner: Arc<CheckRunner> =
            Arc::new(|_, _, _, _, _| Err(io::Error::other("x".repeat(MAX_JOURNAL_BYTES as usize))));
        let checks = CheckQueue::open_with_runner(root.join("checks"), policy, runner).unwrap();
        let id = checks
            .enqueue(1, HookPhase::Manual, CheckKind::RustFmt, &worktree)
            .unwrap();
        for _ in 0..200 {
            if checks.pending_count() == 0 {
                break;
            }
            thread::sleep(Duration::from_millis(10));
        }
        let records = checks.snapshot().unwrap();
        drop(checks);
        let bytes = fs::read(root.join("checks/checks.json")).unwrap();
        let persisted: CheckJournal = serde_json::from_slice(&bytes).unwrap();
        fs::remove_dir_all(root).unwrap();

        assert_eq!(records[0].id, id);
        assert_eq!(records[0].status, CheckStatus::Failed);
        assert_eq!(persisted.records[0].status, CheckStatus::Failed);
        assert!(persisted.records[0].finished_ms.is_some());
        assert!(persisted.records[0]
            .error
            .as_deref()
            .unwrap()
            .contains("journal is full"));
    }

    #[test]
    fn near_full_check_journal_refuses_to_enqueue_without_completion_space() {
        let (root, policy, worktree) = setup();
        let runner: Arc<CheckRunner> = Arc::new(|_, _, _, _, _| Err(io::Error::other("fixture")));
        let checks = CheckQueue::open_with_runner(root.join("checks"), policy, runner).unwrap();
        let now = now_ms();
        let old = CheckRecord {
            id: 1,
            task_id: 1,
            hook: HookPhase::Manual,
            kind: CheckKind::RustFmt,
            status: CheckStatus::Failed,
            created_ms: now,
            finished_ms: Some(now),
            run_dir: None,
            error: None,
        };
        let projected = CheckJournal {
            next_id: 3,
            records: vec![
                old.clone(),
                CheckRecord {
                    id: 2,
                    status: CheckStatus::Queued,
                    finished_ms: None,
                    ..old.clone()
                },
            ],
            ..CheckJournal::default()
        };
        let remaining = MAX_JOURNAL_BYTES as usize - serde_json::to_vec(&projected).unwrap().len();
        let mut journal = checks.journal.lock().unwrap();
        journal.next_id = 2;
        journal.records.push(CheckRecord {
            error: Some("x".repeat(remaining - 16)),
            ..old
        });
        persist(&checks.root, &journal).unwrap();
        drop(journal);

        let result = checks.enqueue(1, HookPhase::Manual, CheckKind::RustFmt, &worktree);
        assert_eq!(result.unwrap_err().kind(), io::ErrorKind::InvalidData);
        assert_eq!(checks.snapshot().unwrap().len(), 1);
        assert_eq!(checks.pending_count(), 0);
        drop(checks);
        fs::remove_dir_all(root).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn pending_check_does_not_run_in_a_replacement_worktree() {
        let (root, policy, worktree) = setup();
        let release = Arc::new(AtomicBool::new(false));
        let gate = Arc::clone(&release);
        let runs = Arc::new(AtomicUsize::new(0));
        let counted = Arc::clone(&runs);
        let runner: Arc<CheckRunner> = Arc::new(move |_, _, kind, token, _| {
            counted.fetch_add(1, Ordering::Relaxed);
            if kind == CheckKind::RustFmt {
                while !gate.load(Ordering::Acquire) && !token.is_cancelled() {
                    thread::sleep(Duration::from_millis(5));
                }
            }
            Err(io::Error::other("fake check result"))
        });
        let checks = CheckQueue::open_with_runner(root.join("checks"), policy, runner).unwrap();
        checks
            .enqueue(1, HookPhase::Manual, CheckKind::RustFmt, &worktree)
            .unwrap();
        for _ in 0..200 {
            if runs.load(Ordering::Relaxed) != 0 {
                break;
            }
            thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(runs.load(Ordering::Relaxed), 1);
        let queued = checks
            .enqueue(1, HookPhase::Manual, CheckKind::RustClippy, &worktree)
            .unwrap();
        fs::rename(&worktree, root.join("original-worktree")).unwrap();
        fs::create_dir(&worktree).unwrap();
        release.store(true, Ordering::Release);
        for _ in 0..200 {
            if checks.pending_count() == 0 {
                break;
            }
            thread::sleep(Duration::from_millis(5));
        }
        let records = checks.snapshot().unwrap();
        assert_eq!(checks.pending_count(), 0);
        drop(checks);
        fs::remove_dir_all(root).unwrap();

        assert_eq!(runs.load(Ordering::Relaxed), 1);
        let rejected = records.iter().find(|record| record.id == queued).unwrap();
        assert_eq!(rejected.status, CheckStatus::Failed);
        assert!(rejected.error.as_deref().unwrap().contains("worktree"));
    }

    #[cfg(unix)]
    #[test]
    fn recovery_resumes_only_verified_queued_checks_with_enabled_recipes() {
        let (root, policy, worktree) = setup();
        let checks_root = root.join("checks");
        fs::DirBuilder::new()
            .mode(0o700)
            .create(&checks_root)
            .unwrap();
        let job = Job::open(1, fs::canonicalize(&worktree).unwrap()).unwrap();
        let Some(identity) = job.identity() else {
            fs::remove_dir_all(root).unwrap();
            return;
        };
        let mut replaced_identity = identity.clone();
        replaced_identity.inode ^= 1;
        let queued = CheckRecord {
            id: 1,
            task_id: 1,
            hook: HookPhase::Manual,
            kind: CheckKind::RustFmt,
            status: CheckStatus::Queued,
            created_ms: now_ms(),
            finished_ms: None,
            run_dir: None,
            error: None,
        };
        let journal = CheckJournal {
            next_id: 6,
            records: vec![
                queued.clone(),
                CheckRecord {
                    id: 2,
                    status: CheckStatus::Succeeded,
                    ..queued.clone()
                },
                CheckRecord {
                    id: 3,
                    ..queued.clone()
                },
                CheckRecord {
                    id: 4,
                    ..queued.clone()
                },
                CheckRecord {
                    id: 5,
                    hook: HookPhase::PreMerge,
                    ..queued
                },
            ],
            next_post_edit: BTreeMap::new(),
            queued_worktrees: BTreeMap::from([
                (1, identity.clone()),
                (4, replaced_identity),
                (5, identity),
            ]),
        };
        persist(&checks_root, &journal).unwrap();
        let state = completed_edit(now_ms(), worktree);
        let calls = Arc::new(AtomicUsize::new(0));
        let observed = Arc::clone(&calls);
        let runner: Arc<CheckRunner> = Arc::new(move |_, _, _, _, _| {
            observed.fetch_add(1, Ordering::Relaxed);
            Err(io::Error::other("fixture ran"))
        });
        let queue = CheckQueue::open_with_runner_and_admission(
            &checks_root,
            policy.clone(),
            Arc::clone(&runner),
            None,
            Some(WorkerStartup {
                state: &state,
                recovered_incomplete_runs: false,
            }),
        )
        .unwrap();
        for _ in 0..200 {
            if queue.pending_count() == 0 {
                break;
            }
            thread::sleep(Duration::from_millis(5));
        }
        let records = queue.snapshot().unwrap();
        assert_eq!(calls.load(Ordering::Relaxed), 1);
        assert_eq!(records[0].id, 1);
        assert_eq!(records[0].error.as_deref(), Some("fixture ran"));
        assert_eq!(records[1].status, CheckStatus::Succeeded);
        assert!(records[2..]
            .iter()
            .all(|record| record.status == CheckStatus::Interrupted));
        assert!(queue.journal.lock().unwrap().queued_worktrees.is_empty());
        drop(queue);

        let mut ambiguous = journal;
        ambiguous.records[1].status = CheckStatus::Running;
        persist(&checks_root, &ambiguous).unwrap();
        let restarted = CheckQueue::open_with_runner_and_admission(
            &checks_root,
            policy,
            runner,
            None,
            Some(WorkerStartup {
                state: &state,
                recovered_incomplete_runs: false,
            }),
        )
        .unwrap();
        assert_eq!(restarted.pending_count(), 0);
        assert!(restarted
            .snapshot()
            .unwrap()
            .iter()
            .all(|record| record.status == CheckStatus::Interrupted));
        assert_eq!(calls.load(Ordering::Relaxed), 1);
        drop(restarted);
        fs::remove_dir_all(root).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn restart_observes_an_edit_saved_before_the_hook_was_queued() {
        let (root, policy, worktree) = setup();
        let checks_root = root.join("checks");
        let runner: Arc<CheckRunner> =
            Arc::new(|_, _, _, _, _| Err(io::Error::other("fixture ran")));
        let initial = crate::StudioState::default();
        let open = |state: &crate::StudioState| {
            CheckQueue::open_with_runner_and_admission(
                &checks_root,
                policy.clone(),
                Arc::clone(&runner),
                None,
                Some(WorkerStartup {
                    state,
                    recovered_incomplete_runs: false,
                }),
            )
            .unwrap()
        };
        let first = open(&initial);
        let mut finished = completed_edit(first.activated_ms, worktree.clone());
        let mut checks = PostEditChecks::planned(&policy.post_edit).unwrap();
        checks.identify_worktree(&worktree);
        finished.runs[0].post_edit_checks = Some(checks);
        drop(first);

        let reopened = open(&finished);
        reopened.try_observe_post_edit(&finished).unwrap();
        let records = reopened.snapshot().unwrap();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].task_id, 1);
        assert_eq!(records[0].hook, HookPhase::PostEdit);
        for _ in 0..200 {
            if reopened.pending_count() == 0 {
                break;
            }
            thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(reopened.pending_count(), 0);
        {
            // Simulate history eviction. Deduplication must survive without
            // the old check record in the journal.
            let mut journal = reopened.journal.lock().unwrap();
            assert_eq!(journal.records[0].error.as_deref(), Some("fixture ran"));
            journal.records.clear();
            persist(&checks_root, &journal).unwrap();
        }
        drop(reopened);
        let reopened = open(&finished);
        reopened.try_observe_post_edit(&finished).unwrap();
        assert!(reopened.snapshot().unwrap().is_empty());
        drop(reopened);
        fs::remove_dir_all(root).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn restart_rejects_unobserved_hooks_for_a_replacement_worktree() {
        let (root, policy, worktree) = setup();
        let checks_root = root.join("checks");
        let runner: Arc<CheckRunner> =
            Arc::new(|_, _, _, _, _| panic!("check ran in a replaced checkout"));
        let initial = crate::StudioState::default();
        let open = |state: &crate::StudioState| {
            CheckQueue::open_with_runner_and_admission(
                &checks_root,
                policy.clone(),
                Arc::clone(&runner),
                None,
                Some(WorkerStartup {
                    state,
                    recovered_incomplete_runs: false,
                }),
            )
            .unwrap()
        };
        drop(open(&initial));
        let mut finished = completed_edit(now_ms(), worktree.clone());
        let mut checks = PostEditChecks::planned(&policy.post_edit).unwrap();
        checks.identify_worktree(&worktree);
        finished.runs[0].post_edit_checks = Some(checks);
        fs::rename(&worktree, root.join("original-checkout")).unwrap();
        fs::create_dir(&worktree).unwrap();

        let reopened = open(&finished);
        reopened.try_observe_post_edit(&finished).unwrap();
        assert!(reopened.snapshot().unwrap().is_empty());
        drop(reopened);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn observer_does_not_requeue_a_post_edit_check_after_history_eviction() {
        let (root, policy, worktree) = setup();
        let runner: Arc<CheckRunner> =
            Arc::new(|_, _, _, _, _| Err(io::Error::other("fake check result")));
        let checks = CheckQueue::open_with_runner(root.join("checks"), policy, runner).unwrap();
        let post_edit_record = CheckRecord {
            id: 1,
            task_id: 1,
            hook: HookPhase::PostEdit,
            kind: CheckKind::RustFmt,
            status: CheckStatus::Succeeded,
            created_ms: checks.activated_ms,
            finished_ms: Some(checks.activated_ms),
            run_dir: None,
            error: None,
        };
        {
            let mut journal = checks.journal.lock().unwrap();
            journal.records.push(post_edit_record.clone());
            journal
                .records
                .extend((2..=MAX_RECORDS as u64).map(|id| CheckRecord {
                    id,
                    hook: HookPhase::Manual,
                    ..post_edit_record.clone()
                }));
            journal.next_id = MAX_RECORDS as u64 + 1;
            persist(&checks.root, &journal).unwrap();
        }
        let state = completed_edit(checks.activated_ms, worktree.clone());

        checks.observe_post_edit(&state);
        checks
            .enqueue(1, HookPhase::Manual, CheckKind::RustTest, &worktree)
            .unwrap();
        assert!(checks.snapshot().unwrap().iter().all(|r| r.id != 1));

        checks.observe_post_edit(&state);
        for _ in 0..200 {
            if checks.pending_count() == 0 {
                break;
            }
            thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(checks.pending_count(), 0);
        assert!(checks
            .snapshot()
            .unwrap()
            .iter()
            .all(|r| r.hook == HookPhase::Manual));
        drop(checks);
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
            None,
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
