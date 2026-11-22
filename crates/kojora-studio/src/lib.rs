//! Headless local studio control plane: durable bot/task/run records.
//! No UI framework is used here. Execution is single-flight on purpose.

use fs2::FileExt;
use kojora_agents::{run_agent, AgentRequest, CodexExecAdapter};
use kojora_runtime::{LocalExecutor, RunOutcome};
use serde::{Deserialize, Serialize};
use std::fs::{self, DirBuilder, File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
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
}

impl TaskStatus {
    pub fn label(self) -> &'static str {
        match self {
            Self::Running => "Running",
            Self::Succeeded => "Succeeded",
            Self::Failed => "Failed",
            Self::Interrupted => "Interrupted",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskRecord {
    pub id: u64,
    pub bot_id: u64,
    pub run_id: u64,
    pub prompt: String,
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
    pub error: Option<String>,
}

/// A runner can be replaced by a credential-free fake in automated tests.
pub type AgentRunner =
    dyn Fn(&BotRecord, &str, &Path) -> io::Result<RunOutcome> + Send + Sync + 'static;

pub struct Studio {
    root: PathBuf,
    state: Arc<Mutex<StudioState>>,
    active: Arc<AtomicBool>,
    last_error: Arc<Mutex<Option<String>>>,
    runner: Arc<AgentRunner>,
    _instance_lock: File,
}

impl Studio {
    pub fn open(root: impl Into<PathBuf>) -> io::Result<Self> {
        Self::open_with_runner(
            root,
            Arc::new(|bot, prompt, log_root| {
                run_agent(
                    &CodexExecAdapter,
                    &LocalExecutor,
                    &AgentRequest {
                        prompt,
                        workspace: &bot.workspace,
                        profile_dir: &bot.profile_dir,
                        log_root,
                    },
                )
            }),
        )
    }

    pub fn open_with_runner(
        root: impl Into<PathBuf>,
        runner: Arc<AgentRunner>,
    ) -> io::Result<Self> {
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
            active: Arc::new(AtomicBool::new(false)),
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
        self.active.load(Ordering::Acquire)
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
        let id = state.next_id()?;
        state.bots.push(BotRecord {
            id,
            name: name.to_owned(),
            workspace,
            profile_dir,
        });
        persist(&self.root, &state)?;
        Ok(id)
    }

    /// Submit one Codex task on a background thread. At most one task runs.
    pub fn submit(&self, bot_id: u64, prompt: &str) -> io::Result<u64> {
        if prompt.trim().is_empty() || prompt.len() > MAX_PROMPT_BYTES {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "prompt must contain 1-32768 bytes",
            ));
        }
        if self
            .active
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            return Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                "another agent run is active; this MVP runs one at a time",
            ));
        }

        let prepared = (|| -> io::Result<(u64, u64, BotRecord)> {
            let mut state = self.state.lock().map_err(lock_error)?;
            let bot = state
                .bots
                .iter()
                .find(|bot| bot.id == bot_id)
                .cloned()
                .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "bot not found"))?;
            let task_id = state.next_id()?;
            let run_id = state.next_id()?;
            let session_id = state.next_id()?;
            let now = unix_ms()?;
            state.tasks.push(TaskRecord {
                id: task_id,
                bot_id,
                run_id,
                prompt: prompt.to_owned(),
                status: TaskStatus::Running,
                created_ms: now,
            });
            state.runs.push(RunRecord {
                id: run_id,
                task_id,
                session_id,
                status: TaskStatus::Running,
                started_ms: now,
                finished_ms: None,
                exit_code: None,
                run_dir: None,
                error: None,
            });
            persist(&self.root, &state)?;
            Ok((task_id, run_id, bot))
        })();

        let (task_id, run_id, bot) = match prepared {
            Ok(prepared) => prepared,
            Err(error) => {
                self.active.store(false, Ordering::Release);
                return Err(error);
            }
        };

        let root = self.root.clone();
        let shared_state = Arc::clone(&self.state);
        let active = Arc::clone(&self.active);
        let last_error = Arc::clone(&self.last_error);
        let runner = Arc::clone(&self.runner);
        let prompt = prompt.to_owned();
        let spawn = thread::Builder::new()
            .name(format!("kojora-task-{task_id}"))
            .spawn(move || {
                struct ActiveGuard(Arc<AtomicBool>);
                impl Drop for ActiveGuard {
                    fn drop(&mut self) {
                        self.0.store(false, Ordering::Release);
                    }
                }
                let _guard = ActiveGuard(active);
                let task_log_root = root.join("runs").join(format!("task-{task_id}"));
                let outcome = runner(&bot, &prompt, &task_log_root);
                let mut state = match shared_state.lock() {
                    Ok(state) => state,
                    Err(_) => return,
                };

                let (status, exit_code, run_dir, error) = match outcome {
                    Ok(outcome) => (
                        if outcome.success {
                            TaskStatus::Succeeded
                        } else {
                            TaskStatus::Failed
                        },
                        outcome.exit_code,
                        Some(outcome.run_dir),
                        None,
                    ),
                    Err(error) => (TaskStatus::Failed, None, None, Some(error.to_string())),
                };
                if let Some(task) = state.tasks.iter_mut().find(|task| task.id == task_id) {
                    task.status = status;
                }
                if let Some(run) = state.runs.iter_mut().find(|run| run.id == run_id) {
                    run.status = status;
                    run.exit_code = exit_code;
                    run.run_dir = run_dir;
                    run.error = error;
                    run.finished_ms = unix_ms().ok();
                }
                if let Err(error) = persist(&root, &state) {
                    if let Ok(mut value) = last_error.lock() {
                        *value = Some(format!("could not save completed run: {error}"));
                    }
                }
            });
        if let Err(error) = spawn {
            self.active.store(false, Ordering::Release);
            let mut state = self.state.lock().map_err(lock_error)?;
            if let Some(task) = state.tasks.iter_mut().find(|task| task.id == task_id) {
                task.status = TaskStatus::Failed;
            }
            if let Some(run) = state.runs.iter_mut().find(|run| run.id == run_id) {
                run.status = TaskStatus::Failed;
                run.error = Some(error.to_string());
                run.finished_ms = unix_ms().ok();
            }
            persist(&self.root, &state)?;
            return Err(error);
        }

        Ok(task_id)
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
        #[cfg(unix)]
        File::open(root)?.sync_all()?;
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
        Arc::new(|_bot, prompt, log_root| {
            let path = log_root.join("fake-run");
            fs::create_dir_all(&path)?;
            fs::write(path.join("stdout.log"), format!("received: {prompt}"))?;
            Ok(RunOutcome {
                run_dir: path,
                exit_code: Some(0),
                success: true,
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
        let mut state = StudioState::default();
        state.next_id = 5;
        state.tasks.push(TaskRecord {
            id: 1,
            bot_id: 2,
            run_id: 3,
            prompt: "test".into(),
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
