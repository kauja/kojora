//! Headless local studio control plane: durable bot/task/run records.
//! No UI framework is used here. Admission and execution are independent of the provider.

pub mod checks;
pub mod scheduler;
#[cfg(unix)]
pub mod worker;

use checks::{CheckKind, PostEditChecks};
use scheduler::{AdmissionPolicy, Scheduler};

use fs2::FileExt;
pub use kojora_agents::AgentProvider;
use kojora_agents::{
    run_agent_cancellable, run_agent_cancellable_limited, AgentAdapter, AgentRequest,
};
use kojora_runtime::{
    quiet_status_cancellable, CancellationToken, LocalExecutor, ProcessLimits, RunOutcome,
};
use serde::{Deserialize, Serialize};
use std::fs::{self, DirBuilder, File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

#[cfg(unix)]
use std::os::fd::OwnedFd;
#[cfg(unix)]
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
#[cfg(unix)]
use std::os::unix::net::UnixStream;
#[cfg(unix)]
use std::os::unix::process::CommandExt;

const SCHEMA_VERSION: u32 = 1;
const MAX_STATE_BYTES: u64 = 4 * 1024 * 1024;
// Space for both status changes, terminal fields, and a short diagnostic.
const RUN_COMPLETION_FIELDS_BYTES: usize = 512;
// Room for the worktree suffix, device and inode numbers, and creation time.
const WORKTREE_IDENTITY_FIELDS_BYTES: usize = 256;
const MAX_PROMPT_BYTES: usize = 32 * 1024;
const MAX_LOG_TAIL_BYTES: usize = 32 * 1024;
const MAX_AGENT_LOG_SCAN_BYTES: usize = 256 * 1024;
const WORKTREE_PREVIEW_TIMEOUT: Duration = Duration::from_secs(8);

/// This state file only holds metadata and user prompts, never credentials.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StudioState {
    pub schema_version: u32,
    pub next_id: u64,
    pub bots: Vec<BotRecord>,
    pub tasks: Vec<TaskRecord>,
    pub runs: Vec<RunRecord>,
}

impl Default for StudioState {
    fn default() -> Self {
        Self {
            schema_version: SCHEMA_VERSION,
            next_id: 1,
            bots: Vec::new(),
            tasks: Vec::new(),
            runs: Vec::new(),
        }
    }
}

impl StudioState {
    fn next_id(&mut self) -> io::Result<u64> {
        let id = self.next_id;
        self.next_id = id
            .checked_add(1)
            .ok_or_else(|| io::Error::other("studio ID space exhausted"))?;
        Ok(id)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BotRecord {
    pub id: u64,
    pub name: String,
    pub workspace: PathBuf,
    pub profile_dir: PathBuf,
    #[serde(default)]
    pub provider: AgentProvider,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum TaskStatus {
    Running,
    Succeeded,
    Failed,
    Interrupted,
    Cancelled,
}

impl TaskStatus {
    pub fn label(self) -> &'static str {
        match self {
            Self::Running => "Running",
            Self::Succeeded => "Succeeded",
            Self::Failed => "Failed",
            Self::Interrupted => "Interrupted",
            Self::Cancelled => "Cancelled",
        }
    }
}

/// Explicit capability boundary for a single task.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum AgentMode {
    #[default]
    Inspect,
    /// User requested changes in a detached Git worktree, never in the main checkout.
    EditWorktree,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskRecord {
    pub id: u64,
    pub bot_id: u64,
    pub run_id: u64,
    pub prompt: String,
    #[serde(default)]
    pub mode: AgentMode,
    pub status: TaskStatus,
    pub created_ms: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunRecord {
    pub id: u64,
    pub task_id: u64,
    pub session_id: u64,
    pub status: TaskStatus,
    pub started_ms: u64,
    pub finished_ms: Option<u64>,
    pub exit_code: Option<i32>,
    pub run_dir: Option<PathBuf>,
    #[serde(default)]
    pub worktree_dir: Option<PathBuf>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub post_edit_checks: Option<PostEditChecks>,
    pub error: Option<String>,
}

#[derive(Debug, Serialize, Deserialize)]
pub enum AgentOutput {
    Message(String),
    Raw(String),
}

impl AgentOutput {
    fn from_log(provider: AgentProvider, log: String) -> Self {
        let message = provider.extract_text(&log);
        if message.is_empty() {
            Self::Raw(preview_tail(log))
        } else {
            Self::Message(preview_tail(message))
        }
    }

    pub fn into_text(self) -> String {
        match self {
            Self::Message(text) | Self::Raw(text) => text,
        }
    }
}

/// A runner can be replaced by a credential-free fake in automated tests.
pub type AgentRunner = dyn Fn(&BotRecord, &str, &Path, AgentMode, &CancellationToken) -> io::Result<RunOutcome>
    + Send
    + Sync
    + 'static;

/// An exclusive check slot shared with agent admission. Dropping this
/// permit always makes the scheduler available again.
pub struct CheckPermit {
    scheduler: Arc<Mutex<Scheduler>>,
}

impl Drop for CheckPermit {
    fn drop(&mut self) {
        if let Ok(mut scheduler) = self.scheduler.lock() {
            scheduler.finish_check();
        }
    }
}

struct InstanceLock(File);

impl Drop for InstanceLock {
    fn drop(&mut self) {
        // A concurrent fork may still hold the open file description until exec.
        let _ = FileExt::unlock(&self.0);
    }
}

pub struct Studio {
    root: PathBuf,
    state: Arc<Mutex<StudioState>>,
    scheduler: Arc<Mutex<Scheduler>>,
    last_error: Arc<Mutex<Option<String>>>,
    runner: Arc<AgentRunner>,
    #[cfg(unix)]
    recovered_incomplete_runs: bool,
    _instance_lock: Arc<InstanceLock>,
}

fn adapter_for(provider: AgentProvider, mode: AgentMode) -> io::Result<&'static dyn AgentAdapter> {
    match mode {
        AgentMode::Inspect => Ok(provider.inspect_adapter()),
        AgentMode::EditWorktree => provider.edit_adapter().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::Unsupported,
                format!("{} does not support edit mode", provider.label()),
            )
        }),
    }
}

fn local_runner(limits: Option<ProcessLimits>) -> Arc<AgentRunner> {
    Arc::new(move |bot, prompt, log_root, mode, cancellation| {
        let adapter = adapter_for(bot.provider, mode)?;
        let request = AgentRequest {
            prompt,
            workspace: &bot.workspace,
            profile_dir: &bot.profile_dir,
            log_root,
        };
        match limits {
            Some(limits) => run_agent_cancellable_limited(
                adapter,
                &LocalExecutor,
                &request,
                cancellation,
                limits,
            ),
            None => run_agent_cancellable(adapter, &LocalExecutor, &request, cancellation),
        }
    })
}

impl Studio {
    pub fn open(root: impl Into<PathBuf>) -> io::Result<Self> {
        Self::open_with_runner(root, local_runner(None))
    }

    /// Open the headless Worker with per-child OS ceilings.
    pub fn open_with_limits(root: impl Into<PathBuf>, limits: ProcessLimits) -> io::Result<Self> {
        Self::open_with_limits_and_policy(root, limits, AdmissionPolicy::single())
    }

    pub fn open_with_limits_and_policy(
        root: impl Into<PathBuf>,
        limits: ProcessLimits,
        policy: AdmissionPolicy,
    ) -> io::Result<Self> {
        limits.validate()?;
        Self::open_with_runner_and_policy(root, local_runner(Some(limits)), policy)
    }

    pub fn open_with_runner(
        root: impl Into<PathBuf>,
        runner: Arc<AgentRunner>,
    ) -> io::Result<Self> {
        Self::open_with_runner_and_policy(root, runner, AdmissionPolicy::single())
    }

    pub fn open_with_runner_and_policy(
        root: impl Into<PathBuf>,
        runner: Arc<AgentRunner>,
        policy: AdmissionPolicy,
    ) -> io::Result<Self> {
        let scheduler = Scheduler::new(policy)?;
        let root = root.into();
        if !root.is_absolute() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "studio data directory must be absolute",
            ));
        }
        ensure_private_directory(&root)?;
        let mut lock_options = OpenOptions::new();
        lock_options.read(true).write(true).create(true);
        #[cfg(unix)]
        lock_options.mode(0o600);
        let lock = lock_options.open(root.join("instance.lock"))?;
        lock.try_lock_exclusive()?;
        let lock = Arc::new(InstanceLock(lock));

        let mut state = read_state(&root)?;
        let mut recovered = false;
        for task in &mut state.tasks {
            if task.status == TaskStatus::Running {
                task.status = TaskStatus::Interrupted;
                recovered = true;
            }
        }
        for run in &mut state.runs {
            if run.status == TaskStatus::Running {
                run.status = TaskStatus::Interrupted;
                run.error = Some("Studio exited before this run was reconciled".to_owned());
                run.finished_ms = Some(unix_ms()?);
                recovered = true;
            }
        }
        if recovered {
            persist(&root, &state)?;
        }
        Ok(Self {
            root,
            state: Arc::new(Mutex::new(state)),
            scheduler: Arc::new(Mutex::new(scheduler)),
            last_error: Arc::new(Mutex::new(None)),
            runner,
            #[cfg(unix)]
            recovered_incomplete_runs: recovered,
            _instance_lock: lock,
        })
    }

    pub fn snapshot(&self) -> io::Result<StudioState> {
        Ok(self.state.lock().map_err(lock_error)?.clone())
    }

    pub fn data_dir(&self) -> &Path {
        &self.root
    }

    pub fn try_acquire_check_slot(&self) -> io::Result<Option<CheckPermit>> {
        let mut scheduler = self.scheduler.lock().map_err(lock_error)?;
        if !scheduler.try_begin_check() {
            return Ok(None);
        }
        Ok(Some(CheckPermit {
            scheduler: Arc::clone(&self.scheduler),
        }))
    }

    pub fn is_active(&self) -> bool {
        self.active_task_count() != 0
    }

    pub fn active_task_count(&self) -> usize {
        self.scheduler.lock().map_or(0, |s| s.active_count())
    }

    pub fn max_parallel(&self) -> usize {
        self.scheduler.lock().map_or(1, |s| s.max_parallel())
    }

    pub fn reserved_memory_mib(&self) -> u64 {
        self.scheduler.lock().map_or(0, |s| s.reserved_mib())
    }

    pub fn active_task_ids(&self) -> Vec<u64> {
        self.scheduler
            .lock()
            .map_or_else(|_| Vec::new(), |s| s.active_ids())
    }

    #[cfg(unix)]
    pub(crate) fn active_task_statuses(&self) -> Vec<(u64, bool)> {
        self.scheduler
            .lock()
            .map_or_else(|_| Vec::new(), |s| s.active_task_statuses())
    }

    /// Compatibility with the single-task GUI; parallel clients use all IDs.
    pub fn active_task_id(&self) -> Option<u64> {
        self.active_task_ids().into_iter().next()
    }

    pub fn cancellation_requested(&self, task_id: u64) -> bool {
        self.scheduler
            .lock()
            .ok()
            .and_then(|s| s.token(task_id))
            .is_some_and(|token| token.is_cancelled())
    }

    pub fn cancel_task(&self, task_id: u64) -> io::Result<()> {
        let token = self
            .scheduler
            .lock()
            .map_err(lock_error)?
            .token(task_id)
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "task is not active"))?;
        token.cancel();
        Ok(())
    }

    pub fn last_error(&self) -> Option<String> {
        self.last_error.lock().ok().and_then(|error| error.clone())
    }

    pub fn add_bot(&self, name: &str, workspace: PathBuf, profile_dir: PathBuf) -> io::Result<u64> {
        self.add_bot_with_provider(name, workspace, profile_dir, AgentProvider::default())
    }

    pub fn add_bot_with_provider(
        &self,
        name: &str,
        workspace: PathBuf,
        profile_dir: PathBuf,
        provider: AgentProvider,
    ) -> io::Result<u64> {
        let name = name.trim();
        if name.is_empty() || name.len() > 100 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "bot name must contain 1-100 characters",
            ));
        }
        if !workspace.is_absolute() || !profile_dir.is_absolute() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "workspace and profile paths must be absolute",
            ));
        }
        let mut state = self.state.lock().map_err(lock_error)?;
        let mut candidate = state.clone();
        let id = candidate.next_id()?;
        candidate.bots.push(BotRecord {
            id,
            name: name.to_owned(),
            workspace,
            profile_dir,
            provider,
        });
        commit_state(&self.root, &mut state, candidate)?;
        Ok(id)
    }

    /// Submit an inspection task on a background thread.
    pub fn submit(&self, bot_id: u64, prompt: &str) -> io::Result<u64> {
        self.submit_with_mode(bot_id, prompt, AgentMode::Inspect)
    }

    /// Opt-in edit runs get a detached Git worktree under Studio's private state root.
    pub fn submit_with_mode(&self, bot_id: u64, prompt: &str, mode: AgentMode) -> io::Result<u64> {
        self.submit_with_post_edit(bot_id, prompt, mode, &[])
    }

    fn submit_with_post_edit(
        &self,
        bot_id: u64,
        prompt: &str,
        mode: AgentMode,
        post_edit_checks: &[CheckKind],
    ) -> io::Result<u64> {
        if prompt.trim().is_empty() || prompt.len() > MAX_PROMPT_BYTES {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "prompt must contain 1-32768 bytes",
            ));
        }
        let mut post_edit_checks = match mode {
            AgentMode::Inspect => None,
            AgentMode::EditWorktree => PostEditChecks::planned(post_edit_checks),
        };
        let prepared = (|| -> io::Result<(u64, u64, BotRecord, CancellationToken)> {
            let mut state = self.state.lock().map_err(lock_error)?;
            let mut candidate = state.clone();
            let bot = candidate
                .bots
                .iter()
                .find(|bot| bot.id == bot_id)
                .cloned()
                .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "bot not found"))?;
            adapter_for(bot.provider, mode)?;
            let task_id = candidate.next_id()?;
            let run_id = candidate.next_id()?;
            let session_id = candidate.next_id()?;
            let now = unix_ms()?;
            candidate.tasks.push(TaskRecord {
                id: task_id,
                bot_id,
                run_id,
                prompt: prompt.to_owned(),
                mode,
                status: TaskStatus::Running,
                created_ms: now,
            });
            candidate.runs.push(RunRecord {
                id: run_id,
                task_id,
                session_id,
                status: TaskStatus::Running,
                started_ms: now,
                finished_ms: None,
                exit_code: None,
                run_dir: None,
                worktree_dir: None,
                post_edit_checks: post_edit_checks.clone(),
                error: None,
            });
            let mut scheduler = self.scheduler.lock().map_err(lock_error)?;
            let token = scheduler.admit(task_id, bot.profile_dir.clone())?;
            if let Err(error) = commit_state(&self.root, &mut state, candidate) {
                scheduler.release(task_id);
                return Err(error);
            }
            Ok((task_id, run_id, bot, token))
        })();

        let (task_id, run_id, bot, cancellation) = prepared?;
        let root = self.root.clone();
        let instance_lock = Arc::clone(&self._instance_lock);
        let shared_state = Arc::clone(&self.state);
        let scheduler = Arc::clone(&self.scheduler);
        let last_error = Arc::clone(&self.last_error);
        let runner = Arc::clone(&self.runner);
        let prompt = prompt.to_owned();
        let spawn = thread::Builder::new()
            .name(format!("kojora-task-{task_id}"))
            .spawn(move || {
                struct ActiveGuard {
                    scheduler: Arc<Mutex<Scheduler>>,
                    task_id: u64,
                }
                impl Drop for ActiveGuard {
                    fn drop(&mut self) {
                        if let Ok(mut scheduler) = self.scheduler.lock() {
                            scheduler.release(self.task_id);
                        }
                    }
                }
                let _guard = ActiveGuard { scheduler, task_id };
                // Release the worker's lock before the guard frees the reservation.
                let _instance_lock = instance_lock;
                let task_log_root = root.join("runs").join(format!("task-{task_id}"));
                let outcome =
                    if cancellation.is_cancelled() {
                        Err(io::Error::new(
                            io::ErrorKind::Interrupted,
                            "task cancelled before launch",
                        ))
                    } else {
                        match mode {
                            AgentMode::Inspect => {
                                runner(&bot, &prompt, &task_log_root, mode, &cancellation)
                            }
                            AgentMode::EditWorktree => create_detached_worktree(
                                &root,
                                &bot.workspace,
                                task_id,
                                &cancellation,
                            )
                            .and_then(|path| {
                                if let Some(checks) = &mut post_edit_checks {
                                    checks.identify_worktree(&path);
                                }
                                {
                                    let mut state = shared_state.lock().map_err(lock_error)?;
                                    let mut candidate = state.clone();
                                    let run = candidate
                                        .runs
                                        .iter_mut()
                                        .find(|run| run.id == run_id)
                                        .ok_or_else(|| {
                                            io::Error::new(io::ErrorKind::NotFound, "run not found")
                                        })?;
                                    run.worktree_dir = Some(path.clone());
                                    run.post_edit_checks = post_edit_checks;
                                    commit_state(&root, &mut state, candidate)?;
                                }
                                if cancellation.is_cancelled() {
                                    return Err(io::Error::new(
                                        io::ErrorKind::Interrupted,
                                        "task cancelled before agent launch",
                                    ));
                                }
                                let mut isolated_bot = bot;
                                isolated_bot.workspace = path;
                                runner(&isolated_bot, &prompt, &task_log_root, mode, &cancellation)
                            }),
                        }
                    };
                let mut state = match shared_state.lock() {
                    Ok(state) => state,
                    Err(_) => return,
                };

                let (status, exit_code, run_dir, error) = match outcome {
                    Ok(outcome) => (
                        if outcome.cancelled {
                            TaskStatus::Cancelled
                        } else if outcome.success {
                            TaskStatus::Succeeded
                        } else {
                            TaskStatus::Failed
                        },
                        outcome.exit_code,
                        Some(outcome.run_dir),
                        None,
                    ),
                    Err(error) => (
                        if cancellation.is_cancelled() && error.kind() == io::ErrorKind::Interrupted
                        {
                            TaskStatus::Cancelled
                        } else {
                            TaskStatus::Failed
                        },
                        None,
                        None,
                        Some(error.to_string()),
                    ),
                };
                let mut candidate = state.clone();
                if let Some(task) = candidate.tasks.iter_mut().find(|task| task.id == task_id) {
                    task.status = status;
                }
                if let Some(run) = candidate.runs.iter_mut().find(|run| run.id == run_id) {
                    run.status = status;
                    run.exit_code = exit_code;
                    run.run_dir = run_dir;
                    run.error = error;
                    run.finished_ms = unix_ms().ok();
                }
                if let Err(error) = commit_finished_state(&root, &mut state, candidate, run_id) {
                    if let Ok(mut value) = last_error.lock() {
                        *value = Some(format!("could not save completed run: {error}"));
                    }
                }
            });
        if let Err(error) = spawn {
            self.scheduler.lock().map_err(lock_error)?.release(task_id);
            let mut state = self.state.lock().map_err(lock_error)?;
            let mut candidate = state.clone();
            if let Some(task) = candidate.tasks.iter_mut().find(|task| task.id == task_id) {
                task.status = TaskStatus::Failed;
            }
            if let Some(run) = candidate.runs.iter_mut().find(|run| run.id == run_id) {
                run.status = TaskStatus::Failed;
                run.error = Some(error.to_string());
                run.finished_ms = unix_ms().ok();
            }
            commit_finished_state(&self.root, &mut state, candidate, run_id)?;
            return Err(error);
        }

        Ok(task_id)
    }

    /// Inspect tracked changes and untracked filenames in a completed edit worktree.
    /// Output is bounded in memory; no diff/commit/push side effects.
    pub fn read_worktree_diff(&self, task_id: u64) -> io::Result<String> {
        let worktree = {
            let state = self.state.lock().map_err(lock_error)?;
            let task = state
                .tasks
                .iter()
                .find(|task| task.id == task_id)
                .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "task not found"))?;
            let run = state
                .runs
                .iter()
                .find(|run| run.id == task.run_id)
                .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "run not found"))?;
            run.worktree_dir.clone().ok_or_else(|| {
                io::Error::new(io::ErrorKind::NotFound, "no edit worktree for this task")
            })?
        };
        if !worktree.starts_with(self.root.join("worktrees")) {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "invalid worktree path",
            ));
        }
        let deadline = Instant::now() + WORKTREE_PREVIEW_TIMEOUT;
        let mut result = run_git_limited_with_limit(
            &worktree,
            &["status", "--short", "--untracked-files=normal"],
            MAX_LOG_TAIL_BYTES / 2,
            deadline,
        )?;
        result.push_str("\n--- Tracked file changes ---\n");
        let remaining = MAX_LOG_TAIL_BYTES.saturating_sub(result.len()).max(1);
        result.push_str(&run_git_limited_with_limit(
            &worktree,
            &["diff", "--no-ext-diff", "--no-textconv", "HEAD", "--"],
            remaining,
            deadline,
        )?);
        if result.len() > MAX_LOG_TAIL_BYTES {
            let mut boundary = MAX_LOG_TAIL_BYTES;
            while !result.is_char_boundary(boundary) {
                boundary -= 1;
            }
            result.truncate(boundary);
        }
        Ok(result)
    }

    /// Read only the last 32 KiB of the stdout/stderr log, never the whole log.
    pub fn read_log_tail(&self, task_id: u64, stderr: bool) -> io::Result<String> {
        self.read_task_log(task_id, stderr, MAX_LOG_TAIL_BYTES)
    }

    /// Scan a wider stdout window for complete provider events before bounding the display text.
    pub fn read_agent_output(
        &self,
        task_id: u64,
        provider: AgentProvider,
    ) -> io::Result<AgentOutput> {
        let stdout = self.read_task_log(task_id, false, MAX_AGENT_LOG_SCAN_BYTES)?;
        Ok(AgentOutput::from_log(provider, stdout))
    }

    fn read_task_log(&self, task_id: u64, stderr: bool, max_bytes: usize) -> io::Result<String> {
        let known = self
            .state
            .lock()
            .map_err(lock_error)?
            .tasks
            .iter()
            .any(|task| task.id == task_id);
        if !known {
            return Err(io::Error::new(io::ErrorKind::NotFound, "task not found"));
        }
        let log_root = self.root.join("runs").join(format!("task-{task_id}"));
        let entries = match fs::read_dir(&log_root) {
            Ok(entries) => entries,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                return Ok(String::new());
            }
            Err(error) => return Err(error),
        };
        let run_dir = entries
            .filter_map(Result::ok)
            .filter_map(|entry| {
                entry
                    .file_type()
                    .ok()
                    .filter(|kind| kind.is_dir())
                    .map(|_| entry.path())
            })
            .max();
        let Some(run_dir) = run_dir else {
            return Ok(String::new());
        };
        let filename = if stderr { "stderr.log" } else { "stdout.log" };
        tail_file(&run_dir.join(filename), max_bytes)
    }
}

pub fn default_data_dir() -> io::Result<PathBuf> {
    if let Some(dir) = std::env::var_os("XDG_STATE_HOME") {
        let dir = PathBuf::from(dir);
        if dir.is_absolute() {
            return Ok(dir.join("kojora"));
        }
    }
    let home = std::env::var_os("HOME").ok_or_else(|| {
        io::Error::new(io::ErrorKind::NotFound, "HOME is not set; pass --data-dir")
    })?;
    let home = PathBuf::from(home);
    if cfg!(target_os = "macos") {
        Ok(home
            .join("Library")
            .join("Application Support")
            .join("Kojora"))
    } else {
        Ok(home.join(".local").join("state").join("kojora"))
    }
}

/// Recover assistant responses from an existing Codex JSONL log.
pub fn extract_agent_text(log: &str) -> String {
    AgentProvider::Codex.extract_text(log)
}

/// Run Git only for local trusted repositories. Never evaluate shell input.
fn git_command(workspace: &Path) -> Command {
    let mut command = Command::new("git");
    command
        .arg("-c")
        .arg("core.hooksPath=/dev/null")
        .arg("-C")
        .arg(workspace)
        .stdin(Stdio::null())
        .stderr(Stdio::null());
    command
}

fn create_detached_worktree(
    root: &Path,
    workspace: &Path,
    task_id: u64,
    cancellation: &CancellationToken,
) -> io::Result<PathBuf> {
    let workspace = fs::canonicalize(workspace)?;
    let output = git_command(&workspace)
        .args(["rev-parse", "--show-toplevel"])
        .output()?;
    if !output.status.success() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "workspace is not a Git checkout",
        ));
    }
    let git_root = PathBuf::from(String::from_utf8_lossy(&output.stdout).trim());
    if fs::canonicalize(&git_root)? != workspace {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "Bot workspace must be the root of its Git checkout",
        ));
    }
    if root.starts_with(&workspace) || workspace.starts_with(root) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "Studio state and Git checkout must be separate",
        ));
    }
    let directory = root.join("worktrees");
    ensure_private_directory(&directory)?;
    let destination = directory.join(format!("task-{task_id}"));
    if destination.exists() {
        return Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            "task worktree already exists",
        ));
    }
    let mut command = git_command(&workspace);
    command
        .args(["worktree", "add", "--detach"])
        .arg(&destination)
        .arg("HEAD");
    let status = quiet_status_cancellable(&mut command, cancellation)?;
    if !status.success() {
        return Err(io::Error::other(
            "Git could not create a detached task worktree",
        ));
    }
    Ok(destination)
}

fn run_git_limited_with_limit(
    worktree: &Path,
    args: &[&str],
    limit: usize,
    deadline: Instant,
) -> io::Result<String> {
    let mut command = git_command(worktree);
    command.args(args);
    let mut data = capture_preview(command, limit, deadline)?;
    let truncated = data.len() > limit;
    data.truncate(limit);
    let mut result = String::from_utf8_lossy(&data).into_owned();
    if truncated {
        result.push_str("\n[diff output truncated]\n");
    }
    Ok(result)
}

#[cfg(unix)]
fn preview_time_left(deadline: Instant) -> io::Result<Duration> {
    deadline
        .checked_duration_since(Instant::now())
        .filter(|remaining| !remaining.is_zero())
        .ok_or_else(|| io::Error::new(io::ErrorKind::TimedOut, "Git worktree preview timed out"))
}

#[cfg(unix)]
fn capture_preview(mut command: Command, limit: usize, deadline: Instant) -> io::Result<Vec<u8>> {
    preview_time_left(deadline)?;
    // A socket lets the reader enforce a wall-clock deadline even when Git
    // or one of its helpers still has stdout open without producing output.
    let (mut reader, writer) = UnixStream::pair()?;
    let mut child = command
        .stdout(Stdio::from(OwnedFd::from(writer)))
        .process_group(0)
        .spawn()?;
    drop(command);

    let mut reaped = false;
    let output = (|| -> io::Result<Vec<u8>> {
        let mut data = Vec::new();
        let mut buffer = [0; 8192];
        let capacity = limit + 1;
        while data.len() < capacity {
            reader.set_read_timeout(Some(preview_time_left(deadline)?))?;
            let count = (capacity - data.len()).min(buffer.len());
            match reader.read(&mut buffer[..count]) {
                Ok(0) => break,
                Ok(n) => data.extend_from_slice(&buffer[..n]),
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                Err(error)
                    if matches!(
                        error.kind(),
                        io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
                    ) =>
                {
                    return Err(io::Error::new(
                        io::ErrorKind::TimedOut,
                        "Git worktree preview timed out",
                    ));
                }
                Err(error) => return Err(error),
            }
        }
        if data.len() <= limit {
            loop {
                let remaining = preview_time_left(deadline)?;
                if let Some(status) = child.try_wait()? {
                    reaped = true;
                    return if status.success() {
                        Ok(data)
                    } else {
                        Err(io::Error::other("Git diff/status command failed"))
                    };
                }
                thread::sleep(remaining.min(Duration::from_millis(10)));
            }
        }
        Ok(data)
    })();

    // Kill only while we still own an unreaped child. Its process group
    // includes helpers that may keep the preview output descriptor open.
    if !reaped {
        use rustix::process::{kill_process_group, Pid, Signal};
        let _ = kill_process_group(Pid::from_child(&child), Signal::KILL);
        let _ = child.kill();
        let _ = child.wait();
    }
    output
}

#[cfg(not(unix))]
fn capture_preview(mut command: Command, limit: usize, _deadline: Instant) -> io::Result<Vec<u8>> {
    let mut child = command.stdout(Stdio::piped()).spawn()?;
    let output = (|| -> io::Result<Vec<u8>> {
        let stdout = child
            .stdout
            .as_mut()
            .ok_or_else(|| io::Error::other("no git stdout"))?;
        let mut data = Vec::new();
        stdout.take((limit + 1) as u64).read_to_end(&mut data)?;
        Ok(data)
    })();
    let data = match output {
        Ok(data) => data,
        Err(error) => {
            let _ = child.kill();
            let _ = child.wait();
            return Err(error);
        }
    };
    let truncated = data.len() > limit;
    if truncated {
        let _ = child.kill();
    }
    let status = child.wait()?;
    if !truncated && !status.success() {
        return Err(io::Error::other("Git diff/status command failed"));
    }
    Ok(data)
}

fn tail_file(path: &Path, max_bytes: usize) -> io::Result<String> {
    let mut file = match File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(String::new()),
        Err(error) => return Err(error),
    };
    let size = file.metadata()?.len();
    let start = size.saturating_sub(max_bytes as u64);
    file.seek(SeekFrom::Start(start))?;
    let mut buffer = Vec::with_capacity((size - start) as usize);
    file.take(max_bytes as u64).read_to_end(&mut buffer)?;
    Ok(String::from_utf8_lossy(&buffer).into_owned())
}

fn preview_tail(mut text: String) -> String {
    if text.len() > MAX_LOG_TAIL_BYTES {
        let notice = "[earlier output omitted]\n";
        let start = text.ceil_char_boundary(text.len() + notice.len() - MAX_LOG_TAIL_BYTES);
        text.replace_range(..start, notice);
    }
    text
}

fn ensure_private_directory(path: &Path) -> io::Result<()> {
    let mut builder = DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    builder.mode(0o700);
    builder.create(path)?;
    let meta = fs::symlink_metadata(path)?;
    if !meta.file_type().is_dir() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "studio data root must not be a symlink",
        ));
    }
    #[cfg(unix)]
    if meta.permissions().mode() & 0o077 != 0 {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "studio data root must be private (mode 0700)",
        ));
    }
    Ok(())
}

fn read_state(root: &Path) -> io::Result<StudioState> {
    let path = root.join("state.json");
    let metadata = match fs::symlink_metadata(&path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return Ok(StudioState::default());
        }
        Err(error) => return Err(error),
    };
    if !metadata.file_type().is_file() || metadata.len() > MAX_STATE_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "invalid studio state file",
        ));
    }
    #[cfg(unix)]
    if metadata.permissions().mode() & 0o077 != 0 {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "studio state file must be private (mode 0600)",
        ));
    }
    let bytes = fs::read(&path)?;
    let state: StudioState = serde_json::from_slice(&bytes)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    if state.schema_version != SCHEMA_VERSION {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "unknown studio schema version",
        ));
    }
    Ok(state)
}

/// Publish a candidate only after its durable replacement succeeds.
fn commit_state(root: &Path, shared: &mut StudioState, candidate: StudioState) -> io::Result<()> {
    persist(root, &candidate)?;
    *shared = candidate;
    Ok(())
}

fn commit_finished_state(
    root: &Path,
    shared: &mut StudioState,
    mut candidate: StudioState,
    run_id: u64,
) -> io::Result<()> {
    let bytes = match encode_state(root, &candidate) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == io::ErrorKind::InvalidData => {
            let run = candidate.runs.iter_mut().find(|run| run.id == run_id);
            if let Some(run) = run {
                run.run_dir = None;
                if run.error.is_some() {
                    run.error = Some("Run error omitted because Studio metadata is full".into());
                }
            }
            encode_state(root, &candidate)?
        }
        Err(error) => return Err(error),
    };
    write_state(root, &bytes)?;
    *shared = candidate;
    Ok(())
}

fn encode_state(root: &Path, state: &StudioState) -> io::Result<Vec<u8>> {
    let running = state
        .runs
        .iter()
        .filter(|run| run.status == TaskStatus::Running);
    let mut budget = MAX_STATE_BYTES as usize;
    if running.clone().next().is_some() {
        // Include the escaped state path plus the suffix of a generated run or worktree directory.
        let path_bytes = serde_json::to_vec(root).map_err(io::Error::other)?.len() + 96;
        let identity_bytes = if running
            .clone()
            .any(|run| run.worktree_dir.is_none() && run.post_edit_checks.is_some())
        {
            // The identity uses the canonical path, which can be longer than
            // the configured state path if an ancestor is a symlink.
            serde_json::to_vec(&fs::canonicalize(root)?)
                .map_err(io::Error::other)?
                .len()
                + WORKTREE_IDENTITY_FIELDS_BYTES
        } else {
            0
        };
        for run in running {
            budget = budget.saturating_sub(RUN_COMPLETION_FIELDS_BYTES + path_bytes);
            if run.worktree_dir.is_none() {
                budget = budget.saturating_sub(path_bytes);
                if run.post_edit_checks.is_some() {
                    budget = budget.saturating_sub(identity_bytes);
                }
            }
        }
    }
    let mut bytes = serde_json::to_vec_pretty(state).map_err(io::Error::other)?;
    if bytes.len() > budget {
        // Preserve existing history, including recovery of pre-existing Running records.
        bytes = serde_json::to_vec(state).map_err(io::Error::other)?;
    }
    if bytes.len() > budget {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "studio state exceeds the 4 MiB metadata limit including active run reservations",
        ));
    }
    Ok(bytes)
}

fn persist(root: &Path, state: &StudioState) -> io::Result<()> {
    write_state(root, &encode_state(root, state)?)
}

fn write_state(root: &Path, bytes: &[u8]) -> io::Result<()> {
    let tmp = root.join(format!("state-{}-{}.tmp", std::process::id(), unix_ms()?));
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    options.mode(0o600);
    let mut file = options.open(&tmp)?;
    let result = (|| -> io::Result<()> {
        file.write_all(bytes)?;
        file.sync_all()?;
        fs::rename(&tmp, root.join("state.json"))?;
        // After rename, the new file is already visible. A directory sync
        // failure must not roll back the shared in-memory metadata.
        #[cfg(unix)]
        if let Err(error) = File::open(root).and_then(|dir| dir.sync_all()) {
            eprintln!("warning: studio state was replaced but directory sync failed: {error}");
        }
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    result
}

fn unix_ms() -> io::Result<u64> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .map_err(io::Error::other)
}

fn lock_error<T>(_: std::sync::PoisonError<T>) -> io::Error {
    io::Error::other("studio state mutex poisoned")
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
    use std::time::Duration;

    fn temporary_root(label: &str) -> PathBuf {
        let path = std::env::temp_dir().join(format!(
            "kojora-studio-{label}-{}-{}",
            std::process::id(),
            unix_ms().unwrap()
        ));
        fs::create_dir(&path).unwrap();
        #[cfg(unix)]
        fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).unwrap();
        path
    }

    fn fake_runner() -> Arc<AgentRunner> {
        Arc::new(|_bot, prompt, log_root, _mode, _cancellation| {
            let path = log_root.join("fake-run");
            fs::create_dir_all(&path)?;
            fs::write(path.join("stdout.log"), format!("received: {prompt}"))?;
            Ok(RunOutcome {
                run_dir: path,
                exit_code: Some(0),
                success: true,
                cancelled: false,
                duration: Duration::ZERO,
                stdout_bytes: prompt.len() as u64,
                stderr_bytes: 0,
            })
        })
    }

    fn history_near_limit(root: &Path, margin_after_submit: usize) -> StudioState {
        let now = unix_ms().unwrap();
        let mut state = StudioState {
            next_id: 5,
            ..StudioState::default()
        };
        state.bots.push(BotRecord {
            id: 1,
            name: "Bot".into(),
            workspace: root.join("project"),
            profile_dir: root.join("profile"),
            provider: AgentProvider::Codex,
        });
        state.tasks.push(TaskRecord {
            id: 2,
            bot_id: 1,
            run_id: 3,
            prompt: "Historical task".into(),
            mode: AgentMode::Inspect,
            status: TaskStatus::Failed,
            created_ms: now,
        });
        state.runs.push(RunRecord {
            id: 3,
            session_id: 4,
            task_id: 2,
            status: TaskStatus::Failed,
            started_ms: now,
            finished_ms: Some(now),
            exit_code: Some(1),
            run_dir: None,
            worktree_dir: None,
            post_edit_checks: None,
            error: None,
        });
        let mut submitted = state.clone();
        submitted.next_id = 8;
        submitted.tasks.push(TaskRecord {
            id: 5,
            bot_id: 1,
            run_id: 6,
            prompt: "Next task".into(),
            mode: AgentMode::Inspect,
            status: TaskStatus::Running,
            created_ms: now,
        });
        submitted.runs.push(RunRecord {
            id: 6,
            session_id: 7,
            task_id: 5,
            status: TaskStatus::Running,
            started_ms: now,
            finished_ms: None,
            exit_code: None,
            run_dir: None,
            worktree_dir: None,
            post_edit_checks: None,
            error: None,
        });
        let remaining = MAX_STATE_BYTES as usize - serde_json::to_vec(&submitted).unwrap().len();
        // Pad the old metadata so a new Running record fits but its full outcome does not.
        state.runs[0].error = Some("x".repeat(remaining - margin_after_submit));
        state
    }

    #[test]
    fn nearly_full_history_refuses_a_task_that_cannot_reserve_completion_space() {
        let root = temporary_root("history-budget");
        persist(&root, &history_near_limit(&root, 16)).unwrap();

        let studio = Studio::open_with_runner(&root, fake_runner()).unwrap();
        let submitted = studio.submit(1, "Next task");
        for _ in 0..200 {
            if !studio.is_active() {
                break;
            }
            thread::sleep(Duration::from_millis(10));
        }
        let persisted = read_state(&root).unwrap();
        drop(studio);
        fs::remove_dir_all(root).unwrap();

        assert_eq!(submitted.unwrap_err().kind(), io::ErrorKind::InvalidData);
        assert_eq!(persisted.tasks.len(), 1);
        assert_eq!(persisted.tasks[0].prompt, "Historical task");
    }

    #[cfg(unix)]
    #[test]
    fn edit_reserves_post_edit_metadata_before_creating_a_worktree() {
        let fixture = temporary_root("edit-history-budget");
        let root = fixture.join("state");
        ensure_private_directory(&root).unwrap();
        let project = fixture.join("project");
        fs::create_dir(&project).unwrap();
        fs::write(project.join("file.txt"), "initial\n").unwrap();
        commit_checkout_fixture(&project);
        let path_bytes = serde_json::to_vec(&root).unwrap().len() + 96;
        let margin = RUN_COMPLETION_FIELDS_BYTES + 2 * path_bytes + 48;
        let mut history = history_near_limit(&root, margin);
        history.bots[0].workspace = project;
        persist(&root, &history).unwrap();

        let studio = Studio::open_with_runner(&root, fake_runner()).unwrap();
        let submitted = studio.submit_with_post_edit(
            1,
            "Next task",
            AgentMode::EditWorktree,
            &[CheckKind::RustFmt],
        );
        for _ in 0..200 {
            if !studio.is_active() {
                break;
            }
            thread::sleep(Duration::from_millis(10));
        }
        assert!(!studio.is_active());
        let persisted = read_state(&root).unwrap();
        let orphaned_checkout = root.join("worktrees/task-5").exists();
        drop(studio);
        fs::remove_dir_all(fixture).unwrap();

        assert!(
            submitted.is_err(),
            "admitted an edit without room for its worktree metadata: {:?}, orphaned checkout: {orphaned_checkout}",
            persisted.runs.last().and_then(|run| run.error.as_deref())
        );
        assert_eq!(submitted.unwrap_err().kind(), io::ErrorKind::InvalidData);
        assert_eq!(persisted.tasks.len(), 1);
        assert!(!orphaned_checkout);
    }

    #[test]
    fn oversized_diagnostic_cannot_prevent_a_finished_task_from_being_saved() {
        let root = temporary_root("history-diagnostic");
        persist(&root, &history_near_limit(&root, 2048)).unwrap();
        let runner: Arc<AgentRunner> =
            Arc::new(|_, _, _, _, _| Err(io::Error::other("large error ".repeat(4096))));

        let studio = Studio::open_with_runner(&root, runner).unwrap();
        let task_id = studio.submit(1, "Next task").unwrap();
        for _ in 0..200 {
            if !studio.is_active() {
                break;
            }
            thread::sleep(Duration::from_millis(10));
        }
        assert!(!studio.is_active());
        assert!(studio.last_error().is_none());
        let state = read_state(&root).unwrap();
        assert_eq!(state.tasks[1].id, task_id);
        assert_eq!(state.tasks[1].status, TaskStatus::Failed);
        assert_eq!(state.runs[1].status, TaskStatus::Failed);
        assert!(state.runs[1]
            .error
            .as_deref()
            .unwrap()
            .contains("metadata is full"));
        drop(studio);
        fs::remove_dir_all(root).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn preview_deadline_stops_git_helpers_even_after_the_leader_exits() {
        let root = temporary_root("preview-deadline");
        let mut command = Command::new("sh");
        command
            .current_dir(&root)
            .args(["-c", "(sleep 0.5; touch leaked) &"]);
        let error =
            capture_preview(command, 32, Instant::now() + Duration::from_millis(100)).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
        thread::sleep(Duration::from_millis(600));
        assert!(!root.join("leaked").exists());
        fs::remove_dir_all(root).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn preview_deadline_applies_after_stdout_is_closed() {
        let mut command = Command::new("sh");
        command.args(["-c", "exec >&-; sleep 2"]);
        let deadline = Instant::now() + Duration::from_millis(100);
        assert_eq!(
            capture_preview(command, 32, deadline).unwrap_err().kind(),
            io::ErrorKind::TimedOut
        );
    }

    #[test]
    fn persists_bots_tasks_and_fake_agent_output() {
        let root = temporary_root("persist");
        let service = Studio::open_with_runner(&root, fake_runner()).unwrap();
        let bot = service
            .add_bot("Test", root.join("workspace"), root.join("profile"))
            .unwrap();
        let task = service.submit(bot, "Inspect this crate").unwrap();
        for _ in 0..100 {
            if !service.is_active() {
                break;
            }
            thread::sleep(Duration::from_millis(10));
        }
        assert!(!service.is_active());
        let state = service.snapshot().unwrap();
        assert_eq!(state.bots.len(), 1);
        assert_eq!(state.tasks.len(), 1);
        assert_eq!(state.tasks[0].status, TaskStatus::Succeeded);
        assert_eq!(state.runs.len(), 1);
        assert_ne!(state.runs[0].session_id, state.runs[0].id);
        assert_eq!(
            service.read_log_tail(task, false).unwrap(),
            "received: Inspect this crate"
        );
        drop(service);
        let reopened = Studio::open_with_runner(&root, fake_runner()).unwrap();
        assert_eq!(
            reopened.snapshot().unwrap().tasks[0].status,
            TaskStatus::Succeeded
        );
        drop(reopened);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn fails_closed_when_two_processes_open_same_state() {
        let root = temporary_root("lock");
        let a = Studio::open_with_runner(&root, fake_runner()).unwrap();
        assert!(Studio::open_with_runner(&root, fake_runner()).is_err());
        drop(a);
        assert!(Studio::open_with_runner(&root, fake_runner()).is_ok());
        fs::remove_dir_all(root).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn studio_releases_its_lock_even_if_a_fork_still_holds_a_file_descriptor() {
        let root = temporary_root("inherited-lock");
        let studio = Studio::open_with_runner(&root, fake_runner()).unwrap();
        // A concurrent fork can hold the same open file description until exec.
        let inherited = studio._instance_lock.0.try_clone().unwrap();
        drop(studio);
        let reopened = Studio::open_with_runner(&root, fake_runner());
        drop(inherited);
        fs::remove_dir_all(root).unwrap();
        assert!(reopened.is_ok(), "reopen failed: {:?}", reopened.err());
    }

    #[test]
    fn recovers_running_tasks_after_restart() {
        let root = temporary_root("recovery");
        let mut state = StudioState {
            next_id: 5,
            ..StudioState::default()
        };
        state.tasks.push(TaskRecord {
            id: 1,
            bot_id: 2,
            run_id: 3,
            prompt: "test".into(),
            mode: AgentMode::Inspect,
            status: TaskStatus::Running,
            created_ms: 1,
        });
        state.runs.push(RunRecord {
            id: 3,
            task_id: 1,
            session_id: 4,
            status: TaskStatus::Running,
            started_ms: 1,
            finished_ms: None,
            exit_code: None,
            run_dir: None,
            worktree_dir: None,
            post_edit_checks: None,
            error: None,
        });
        // Legacy Studio allowed a Running state to fill the entire metadata file.
        let initial_len = serde_json::to_vec_pretty(&state).unwrap().len();
        let prompt_len = state.tasks[0].prompt.len() + (MAX_STATE_BYTES as usize - initial_len);
        state.tasks[0].prompt = "x".repeat(prompt_len);
        write_state(&root, &serde_json::to_vec_pretty(&state).unwrap()).unwrap();
        let studio = Studio::open_with_runner(&root, fake_runner()).unwrap();
        assert_eq!(
            studio.snapshot().unwrap().tasks[0].status,
            TaskStatus::Interrupted
        );
        assert_eq!(
            studio.snapshot().unwrap().runs[0].status,
            TaskStatus::Interrupted
        );
        assert_eq!(studio.snapshot().unwrap().tasks[0].prompt.len(), prompt_len);
        drop(studio);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn parses_codex_messages_without_loading_complete_logs() {
        let log = "{\"type\":\"thread.started\"}\n{\"type\":\"item.completed\",\"item\":{\"type\":\"agent_message\",\"text\":\"Hello\"}}\n{\"type\":\"item.completed\",\"item\":{\"type\":\"command_execution\",\"text\":\"ignore\"}}\n";
        assert_eq!(extract_agent_text(log), "Hello");
    }

    #[test]
    fn reads_bots_saved_before_provider_selection() {
        let bot: BotRecord = serde_json::from_str(
            r#"{"id":1,"name":"Existing","workspace":"/project","profile_dir":"/profile"}"#,
        )
        .unwrap();
        assert_eq!(bot.provider, AgentProvider::Codex);
        assert_eq!(bot.profile_dir, Path::new("/profile"));
    }

    fn commit_checkout_fixture(project: &Path) {
        let status = git_command(project).arg("init").status().unwrap();
        assert!(status.success());
        let status = git_command(project)
            .args(["add", "--all"])
            .status()
            .unwrap();
        assert!(status.success());
        let status = git_command(project)
            .args([
                "-c",
                "user.name=Kojora",
                "-c",
                "user.email=kojora@example.invalid",
            ])
            .args(["commit", "-m", "Initial"])
            .stdout(Stdio::null())
            .status()
            .unwrap();
        assert!(status.success());
    }

    #[test]
    fn edit_mode_never_mutates_original_checkout() {
        let root = temporary_root("worktree");
        let project = root.join("checkout");
        fs::create_dir(&project).unwrap();
        fs::write(project.join("file.txt"), "initial\n").unwrap();
        commit_checkout_fixture(&project);
        let studio_root = root.join("state");
        let state_root = studio_root.clone();
        let fake: Arc<AgentRunner> =
            Arc::new(move |bot, _prompt, log_root, mode, _cancellation| {
                assert_eq!(mode, AgentMode::EditWorktree);
                let persisted = read_state(&state_root)?;
                assert_eq!(
                    persisted.runs[0].worktree_dir.as_ref(),
                    Some(&bot.workspace)
                );
                assert_eq!(persisted.runs[0].status, TaskStatus::Running);
                assert!(persisted.runs[0].post_edit_checks.is_some());
                fs::write(bot.workspace.join("file.txt"), "changed\n")?;
                let logs = log_root.join("fake-run");
                fs::create_dir_all(&logs)?;
                fs::write(logs.join("stdout.log"), b"ok")?;
                Ok(RunOutcome {
                    run_dir: logs,
                    exit_code: Some(0),
                    success: true,
                    cancelled: false,
                    duration: Duration::ZERO,
                    stdout_bytes: 2,
                    stderr_bytes: 0,
                })
            });
        let studio = Studio::open_with_runner(&studio_root, fake).unwrap();
        let id = studio
            .add_bot("Editor", project.clone(), root.join("profile"))
            .unwrap();
        let task = studio
            .submit_with_post_edit(
                id,
                "Modify file",
                AgentMode::EditWorktree,
                &[CheckKind::RustFmt],
            )
            .unwrap();
        for _ in 0..200 {
            if !studio.is_active() {
                break;
            }
            thread::sleep(Duration::from_millis(10));
        }
        assert!(!studio.is_active());
        assert_eq!(
            fs::read_to_string(project.join("file.txt")).unwrap(),
            "initial\n"
        );
        let snapshot = studio.snapshot().unwrap();
        assert_eq!(snapshot.tasks[0].mode, AgentMode::EditWorktree);
        assert_eq!(snapshot.tasks[0].status, TaskStatus::Succeeded);
        let worktree = snapshot.runs[0].worktree_dir.as_ref().unwrap();
        assert_eq!(
            fs::read_to_string(worktree.join("file.txt")).unwrap(),
            "changed\n"
        );
        assert!(studio
            .read_worktree_diff(task)
            .unwrap()
            .contains("+changed"));
        drop(studio);
        let _ = git_command(&project)
            .args(["worktree", "remove", "--force"])
            .arg(worktree)
            .status();
        fs::remove_dir_all(root).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn cancelling_an_edit_stops_git_checkout_before_agent_launch() {
        let root = temporary_root("cancel-worktree");
        let project = root.join("checkout");
        fs::create_dir(&project).unwrap();
        fs::write(project.join("file.txt"), "initial\n").unwrap();
        fs::write(project.join(".gitattributes"), "file.txt filter=slow\n").unwrap();
        commit_checkout_fixture(&project);
        let filter = root.join("filter.sh");
        fs::write(
            &filter,
            "cd \"$(dirname \"$0\")\" || exit 1\n\
             touch ready\n\
             attempts=0\n\
             while [ ! -f release ] && [ \"$attempts\" -lt 100 ]; do\n\
             sleep 0.1\n\
             attempts=$((attempts + 1))\n\
             done\n\
             cat\n",
        )
        .unwrap();
        let command = format!(
            "sh '{}'",
            filter.display().to_string().replace('\'', "'\\''")
        );
        assert!(git_command(&project)
            .args(["config", "filter.slow.smudge", &command])
            .status()
            .unwrap()
            .success());

        let studio_root = root.join("state");
        let studio = Studio::open_with_runner(
            &studio_root,
            Arc::new(|_, _, _, _, _| panic!("agent launched after cancellation")),
        )
        .unwrap();
        let bot = studio
            .add_bot("Editor", project.clone(), root.join("profile"))
            .unwrap();
        let task = studio
            .submit_with_mode(bot, "Modify file", AgentMode::EditWorktree)
            .unwrap();
        let ready = root.join("ready");
        let deadline = Instant::now() + Duration::from_secs(3);
        while !ready.exists() && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(10));
        }
        assert!(ready.exists(), "Git did not launch the checkout filter");
        studio.cancel_task(task).unwrap();
        let deadline = Instant::now() + Duration::from_secs(2);
        while studio.is_active() && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(10));
        }
        let stopped_while_filter_was_blocked = !studio.is_active();
        fs::write(root.join("release"), "").unwrap();
        assert!(stopped_while_filter_was_blocked);

        let persisted = read_state(&studio_root).unwrap();
        assert_eq!(persisted.tasks[0].status, TaskStatus::Cancelled);
        assert_eq!(persisted.runs[0].status, TaskStatus::Cancelled);
        assert!(persisted.runs[0].worktree_dir.is_none());
        assert!(!studio_root
            .join("worktrees")
            .join(format!("task-{task}"))
            .exists());
        let worktrees = git_command(&project)
            .args(["worktree", "list", "--porcelain"])
            .output()
            .unwrap();
        assert!(worktrees.status.success());
        assert_eq!(
            String::from_utf8_lossy(&worktrees.stdout)
                .lines()
                .filter(|line| line.starts_with("worktree "))
                .count(),
            1
        );
        assert_eq!(
            fs::read_to_string(project.join("file.txt")).unwrap(),
            "initial\n"
        );
        drop(studio);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn failed_persist_does_not_poison_shared_state() {
        let root = temporary_root("rollback");
        let mut shared = StudioState::default();
        let mut candidate = shared.clone();
        candidate.bots.push(BotRecord {
            id: 1,
            name: "X".repeat(MAX_STATE_BYTES as usize),
            workspace: root.clone(),
            profile_dir: root.clone(),
            provider: AgentProvider::Codex,
        });
        assert!(commit_state(&root, &mut shared, candidate).is_err());
        assert!(shared.bots.is_empty());
        assert_eq!(shared.next_id, 1);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn cancellation_rejects_wrong_task_and_persists_cancelled_status() {
        let root = temporary_root("cancel");
        let fake: Arc<AgentRunner> = Arc::new(|_bot, _prompt, log_root, _mode, token| {
            for _ in 0..200 {
                if token.is_cancelled() {
                    let path = log_root.join("fake-run");
                    fs::create_dir_all(&path)?;
                    fs::write(path.join("stdout.log"), b"cancelled")?;
                    return Ok(RunOutcome {
                        run_dir: path,
                        exit_code: None,
                        success: false,
                        cancelled: true,
                        duration: Duration::ZERO,
                        stdout_bytes: 9,
                        stderr_bytes: 0,
                    });
                }
                thread::sleep(Duration::from_millis(10));
            }
            Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "cancel not received",
            ))
        });
        let studio = Studio::open_with_runner(&root, fake).unwrap();
        let bot_id = studio
            .add_bot(
                "Cancelled bot",
                root.join("workspace"),
                root.join("profile"),
            )
            .unwrap();
        let task_id = studio.submit(bot_id, "stop me").unwrap();
        assert_eq!(studio.active_task_id(), Some(task_id));
        assert!(studio.cancel_task(task_id + 1).is_err());
        assert!(!studio.cancellation_requested(task_id));
        studio.cancel_task(task_id).unwrap();
        assert!(studio.cancellation_requested(task_id));
        for _ in 0..200 {
            if !studio.is_active() {
                break;
            }
            thread::sleep(Duration::from_millis(10));
        }
        assert!(!studio.is_active());
        assert!(studio.cancel_task(task_id).is_err());
        assert_eq!(
            studio.snapshot().unwrap().tasks[0].status,
            TaskStatus::Cancelled
        );
        assert_eq!(
            studio.snapshot().unwrap().runs[0].status,
            TaskStatus::Cancelled
        );
        drop(studio);
        let reopened = Studio::open_with_runner(&root, fake_runner()).unwrap();
        assert_eq!(
            reopened.snapshot().unwrap().tasks[0].status,
            TaskStatus::Cancelled
        );
        drop(reopened);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn rejects_unsafe_bot_parameters_and_prompts() {
        let root = temporary_root("validation");
        let studio = Studio::open_with_runner(&root, fake_runner()).unwrap();
        assert!(studio.add_bot("", root.clone(), root.clone()).is_err());
        assert!(studio
            .add_bot("Bad", PathBuf::from("relative"), root.clone())
            .is_err());
        assert!(studio.submit(123, "valid").is_err());
        let id = studio.add_bot("Good", root.clone(), root.clone()).unwrap();
        assert!(studio.submit(id, "  ").is_err());
        drop(studio);
        fs::remove_dir_all(root).unwrap();
    }
}
