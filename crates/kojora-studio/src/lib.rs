//! Headless local studio control plane: durable bot/task/run records.
//! No UI framework is used here. Execution is single-flight on purpose.

pub mod scheduler;
#[cfg(unix)]
pub mod worker;

use scheduler::{AdmissionPolicy, Scheduler};

use fs2::FileExt;
use kojora_agents::{
    run_agent_cancellable, run_agent_cancellable_limited, AgentRequest, CodexExecAdapter,
    CodexWorkspaceWriteAdapter,
};
use kojora_runtime::{CancellationToken, LocalExecutor, RunOutcome};
use serde::{Deserialize, Serialize};
use std::fs::{self, DirBuilder, File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{SystemTime, UNIX_EPOCH};

#[cfg(unix)]
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};

const SCHEMA_VERSION: u32 = 1;
const MAX_STATE_BYTES: u64 = 4 * 1024 * 1024;
const MAX_PROMPT_BYTES: usize = 32 * 1024;
const MAX_LOG_TAIL_BYTES: usize = 32 * 1024;

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
    pub error: Option<String>,
}

/// A runner can be replaced by a credential-free fake in automated tests.
pub type AgentRunner = dyn Fn(&BotRecord, &str, &Path, AgentMode, &CancellationToken) -> io::Result<RunOutcome>
    + Send
    + Sync
    + 'static;

pub struct Studio {
    root: PathBuf,
    state: Arc<Mutex<StudioState>>,
    scheduler: Arc<Mutex<Scheduler>>,
    last_error: Arc<Mutex<Option<String>>>,
    runner: Arc<AgentRunner>,
    _instance_lock: Arc<File>,
}

impl Studio {
    pub fn open(root: impl Into<PathBuf>) -> io::Result<Self> {
        Self::open_with_runner(
            root,
            Arc::new(|bot, prompt, log_root, mode, cancellation| {
                let request = AgentRequest {
                    prompt,
                    workspace: &bot.workspace,
                    profile_dir: &bot.profile_dir,
                    log_root,
                };
                match mode {
                    AgentMode::Inspect => run_agent_cancellable(
                        &CodexExecAdapter,
                        &LocalExecutor,
                        &request,
                        cancellation,
                    ),
                    AgentMode::EditWorktree => run_agent_cancellable(
                        &CodexWorkspaceWriteAdapter,
                        &LocalExecutor,
                        &request,
                        cancellation,
                    ),
                }
            }),
        )
    }

    /// Opens the single-instance state store with per-child OS ceilings.
    /// Worker Daemon calls this; Desktop can continue to use unrestricted
    /// local mode. No GUI dependencies are imported by the service.
    pub fn open_with_limits(
        root: impl Into<PathBuf>,
        limits: kojora_runtime::ProcessLimits,
    ) -> io::Result<Self> {
        limits.validate()?;
        Self::open_with_runner(
            root,
            Arc::new(move |bot, prompt, log_root, mode, cancellation| {
                let request = AgentRequest {
                    prompt,
                    workspace: &bot.workspace,
                    profile_dir: &bot.profile_dir,
                    log_root,
                };
                match mode {
                    AgentMode::Inspect => run_agent_cancellable_limited(
                        &CodexExecAdapter,
                        &LocalExecutor,
                        &request,
                        cancellation,
                        limits,
                    ),
                    AgentMode::EditWorktree => run_agent_cancellable_limited(
                        &CodexWorkspaceWriteAdapter,
                        &LocalExecutor,
                        &request,
                        cancellation,
                        limits,
                    ),
                }
            }),
        )
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
        let lock = Arc::new(lock_options.open(root.join("instance.lock"))?);
        lock.try_lock_exclusive()?;

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
            _instance_lock: lock,
        })
    }

    pub fn snapshot(&self) -> io::Result<StudioState> {
        Ok(self.state.lock().map_err(lock_error)?.clone())
    }

    pub fn data_dir(&self) -> &Path {
        &self.root
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
        });
        commit_state(&self.root, &mut state, candidate)?;
        Ok(id)
    }

    /// Submit one Codex task on a background thread. At most one task runs.
    pub fn submit(&self, bot_id: u64, prompt: &str) -> io::Result<u64> {
        self.submit_with_mode(bot_id, prompt, AgentMode::Inspect)
    }

    /// Opt-in edit runs get a detached Git worktree under Studio's private state root.
    pub fn submit_with_mode(&self, bot_id: u64, prompt: &str, mode: AgentMode) -> io::Result<u64> {
        if prompt.trim().is_empty() || prompt.len() > MAX_PROMPT_BYTES {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "prompt must contain 1-32768 bytes",
            ));
        }
        let prepared = (|| -> io::Result<(u64, u64, BotRecord, CancellationToken)> {
            let mut state = self.state.lock().map_err(lock_error)?;
            let mut candidate = state.clone();
            let bot = candidate
                .bots
                .iter()
                .find(|bot| bot.id == bot_id)
                .cloned()
                .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "bot not found"))?;
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
                let outcome = if cancellation.is_cancelled() {
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
                        )
                        .and_then(|path| {
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
                if let Err(error) = commit_state(&root, &mut state, candidate) {
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
            commit_state(&self.root, &mut state, candidate)?;
            return Err(error);
        }

        Ok(task_id)
    }

    /// Inspect tracked changes and untracked filenames in a completed edit worktree.
    /// Output is bounded in memory; no diff/commit/push side effects.
    pub fn read_worktree_diff(&self, task_id: u64) -> io::Result<String> {
        let state = self.snapshot()?;
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
        let worktree = run.worktree_dir.as_ref().ok_or_else(|| {
            io::Error::new(io::ErrorKind::NotFound, "no edit worktree for this task")
        })?;
        if !worktree.starts_with(self.root.join("worktrees")) {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "invalid worktree path",
            ));
        }
        let mut result =
            run_git_limited(worktree, &["status", "--short", "--untracked-files=normal"])?;
        result.push_str("\n--- Tracked file changes ---\n");
        let remaining = MAX_LOG_TAIL_BYTES.saturating_sub(result.len()).max(1);
        result.push_str(&run_git_limited_with_limit(
            worktree,
            &["diff", "--no-ext-diff", "--no-textconv", "HEAD", "--"],
            remaining,
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
        let known = self.snapshot()?.tasks.iter().any(|task| task.id == task_id);
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
        let mut run_dirs = entries
            .filter_map(Result::ok)
            .filter_map(|entry| {
                entry
                    .file_type()
                    .ok()
                    .filter(|kind| kind.is_dir())
                    .map(|_| entry.path())
            })
            .collect::<Vec<_>>();
        run_dirs.sort_unstable();
        let Some(run_dir) = run_dirs.last() else {
            return Ok(String::new());
        };
        let filename = if stderr { "stderr.log" } else { "stdout.log" };
        tail_file(&run_dir.join(filename), MAX_LOG_TAIL_BYTES)
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

/// Recover compact human-facing assistant responses from Codex JSONL output.
pub fn extract_agent_text(log: &str) -> String {
    let mut messages = Vec::new();
    for line in log.lines() {
        let Ok(value) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        if value.get("type").and_then(|kind| kind.as_str()) != Some("item.completed") {
            continue;
        }
        let Some(item) = value.get("item") else {
            continue;
        };
        if item.get("type").and_then(|kind| kind.as_str()) != Some("agent_message") {
            continue;
        }
        if let Some(message) = item.get("text").and_then(|text| text.as_str()) {
            messages.push(message.to_owned());
        }
    }
    messages.join("\n\n")
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

fn create_detached_worktree(root: &Path, workspace: &Path, task_id: u64) -> io::Result<PathBuf> {
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
    let status = git_command(&workspace)
        .args(["worktree", "add", "--detach"])
        .arg(&destination)
        .arg("HEAD")
        .stdout(Stdio::null())
        .status()?;
    if !status.success() {
        return Err(io::Error::other(
            "Git could not create a detached task worktree",
        ));
    }
    Ok(destination)
}

fn run_git_limited(worktree: &Path, args: &[&str]) -> io::Result<String> {
    run_git_limited_with_limit(worktree, args, MAX_LOG_TAIL_BYTES / 2)
}

fn run_git_limited_with_limit(worktree: &Path, args: &[&str], limit: usize) -> io::Result<String> {
    let mut child = git_command(worktree)
        .args(args)
        .stdout(Stdio::piped())
        .spawn()?;
    let output = (|| -> io::Result<Vec<u8>> {
        let stdout = child
            .stdout
            .as_mut()
            .ok_or_else(|| io::Error::other("no git stdout"))?;
        let mut data = Vec::new();
        stdout.take((limit + 1) as u64).read_to_end(&mut data)?;
        Ok(data)
    })();
    let mut data = match output {
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
    data.truncate(limit);
    let mut result = String::from_utf8_lossy(&data).into_owned();
    if truncated {
        result.push_str("\n[diff output truncated]\n");
    }
    Ok(result)
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

fn persist(root: &Path, state: &StudioState) -> io::Result<()> {
    let bytes = serde_json::to_vec_pretty(state).map_err(io::Error::other)?;
    if bytes.len() as u64 > MAX_STATE_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "studio state exceeds the 4 MiB metadata limit",
        ));
    }
    let tmp = root.join(format!("state-{}-{}.tmp", std::process::id(), unix_ms()?));
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    options.mode(0o600);
    let mut file = options.open(&tmp)?;
    let result = (|| -> io::Result<()> {
        file.write_all(&bytes)?;
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
            error: None,
        });
        persist(&root, &state).unwrap();
        let studio = Studio::open_with_runner(&root, fake_runner()).unwrap();
        assert_eq!(
            studio.snapshot().unwrap().tasks[0].status,
            TaskStatus::Interrupted
        );
        assert_eq!(
            studio.snapshot().unwrap().runs[0].status,
            TaskStatus::Interrupted
        );
        drop(studio);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn parses_codex_messages_without_loading_complete_logs() {
        let log = "{\"type\":\"thread.started\"}\n{\"type\":\"item.completed\",\"item\":{\"type\":\"agent_message\",\"text\":\"Hello\"}}\n{\"type\":\"item.completed\",\"item\":{\"type\":\"command_execution\",\"text\":\"ignore\"}}\n";
        assert_eq!(extract_agent_text(log), "Hello");
    }

    #[test]
    fn edit_mode_never_mutates_original_checkout() {
        let root = temporary_root("worktree");
        let project = root.join("checkout");
        fs::create_dir(&project).unwrap();
        let status = Command::new("git")
            .arg("init")
            .arg(&project)
            .status()
            .unwrap();
        assert!(status.success());
        fs::write(project.join("file.txt"), "initial\n").unwrap();
        let status = git_command(&project)
            .args(["add", "file.txt"])
            .status()
            .unwrap();
        assert!(status.success());
        let status = git_command(&project)
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
            .submit_with_mode(id, "Modify file", AgentMode::EditWorktree)
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
