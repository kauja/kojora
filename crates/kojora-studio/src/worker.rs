//! Private, versioned local Unix-domain control channel for Kojora Studio.
//! Each request owns one short-lived connection. No TCP/network listener,
//! bearer tokens, process execution from wire input, or shell evaluation.

#![cfg(unix)]

use crate::checks::{CheckKind, CheckPolicy, CheckQueue, CheckRecord, HookPhase};
use crate::{
    AgentMode, AgentOutput, AgentProvider, Studio, StudioState, TaskStatus,
    WORKTREE_PREVIEW_TIMEOUT,
};
use kojora_runtime::ProcessLimits;
use serde::{de::DeserializeOwned, Deserialize, Serialize};
use std::fs;
use std::io::{self, Read, Write};
use std::os::unix::fs::{FileTypeExt, MetadataExt, PermissionsExt};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

const VERSION: u32 = 3;
// A prompt byte may take up to six bytes when JSON escapes a control character.
const MAX_REQUEST: usize = 6 * crate::MAX_PROMPT_BYTES + 64 * 1024;
const MAX_RESPONSE: usize = 4 * 1024 * 1024 + 4096;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ActiveTaskStatus {
    pub task_id: u64,
    pub cancellation_requested: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkerStatus {
    pub protocol: u32,
    pub pid: u32,
    pub data_dir: PathBuf,
    pub active_task: Option<u64>,
    pub cancellation_requested: bool,
    #[serde(default)]
    pub active_tasks: Vec<ActiveTaskStatus>,
    pub address_space_mib: Option<u64>,
    pub cpu_seconds: Option<u64>,
    pub concurrency: u32,
    pub active_count: u32,
    pub reserved_memory_mib: u64,
    pub checks_pending: u32,
    pub checks_enabled: bool,
    pub last_error: Option<String>,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "op", content = "data", rename_all = "snake_case")]
enum Request {
    Status,
    Snapshot,
    Checks,
    CheckLog {
        check_id: u64,
        stderr: bool,
    },
    QueueCheck {
        task_id: u64,
        hook: HookPhase,
        kind: CheckKind,
    },
    AddBot {
        name: String,
        workspace: PathBuf,
        profile_dir: PathBuf,
        provider: AgentProvider,
    },
    Submit {
        bot_id: u64,
        prompt: String,
        mode: AgentMode,
    },
    Cancel {
        task_id: u64,
    },
    Log {
        task_id: u64,
        stderr: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        provider: Option<AgentProvider>,
    },
    Diff {
        task_id: u64,
    },
    Shutdown,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "result", content = "data", rename_all = "snake_case")]
enum Response {
    Status(WorkerStatus),
    Snapshot(StudioState),
    Checks(Vec<CheckRecord>),
    Id(u64),
    Text(String),
    AgentOutput(AgentOutput),
    Ok,
    Error { kind: String, message: String },
}

fn kind_name(kind: io::ErrorKind) -> &'static str {
    match kind {
        io::ErrorKind::NotFound => "not_found",
        io::ErrorKind::WouldBlock => "busy",
        io::ErrorKind::InvalidInput => "invalid_input",
        io::ErrorKind::PermissionDenied => "permission_denied",
        io::ErrorKind::TimedOut => "timed_out",
        io::ErrorKind::Unsupported => "unsupported",
        _ => "other",
    }
}

fn decode_error(kind: &str, message: String) -> io::Error {
    let kind = match kind {
        "not_found" => io::ErrorKind::NotFound,
        "busy" => io::ErrorKind::WouldBlock,
        "invalid_input" => io::ErrorKind::InvalidInput,
        "permission_denied" => io::ErrorKind::PermissionDenied,
        "timed_out" => io::ErrorKind::TimedOut,
        "unsupported" => io::ErrorKind::Unsupported,
        _ => io::ErrorKind::Other,
    };
    io::Error::new(kind, message)
}

/// A length-prefixed single JSON frame with explicit allocation limits.
fn receive<T: DeserializeOwned>(stream: &mut UnixStream, limit: usize) -> io::Result<T> {
    let mut header = [0u8; 4];
    stream.read_exact(&mut header)?;
    let length = u32::from_be_bytes(header) as usize;
    if length == 0 || length > limit {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "invalid IPC frame length",
        ));
    }
    let mut frame = vec![0u8; length];
    stream.read_exact(&mut frame)?;
    serde_json::from_slice(&frame)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))
}

fn transmit<T: Serialize>(stream: &mut UnixStream, message: &T, max: usize) -> io::Result<()> {
    let bytes = serde_json::to_vec(message).map_err(io::Error::other)?;
    if bytes.is_empty() || bytes.len() > max {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "IPC response exceeds limit",
        ));
    }
    let length = u32::try_from(bytes.len())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "IPC frame too large"))?;
    stream.write_all(&length.to_be_bytes())?;
    stream.write_all(&bytes)?;
    Ok(())
}

fn status(studio: &Studio, limits: ProcessLimits, checks: Option<&CheckQueue>) -> WorkerStatus {
    let active_tasks: Vec<_> = studio
        .active_task_statuses()
        .into_iter()
        .map(|(task_id, cancellation_requested)| ActiveTaskStatus {
            task_id,
            cancellation_requested,
        })
        .collect();
    let first = active_tasks.first();
    WorkerStatus {
        protocol: VERSION,
        pid: std::process::id(),
        data_dir: studio.data_dir().to_owned(),
        active_task: first.map(|task| task.task_id),
        cancellation_requested: first.is_some_and(|task| task.cancellation_requested),
        address_space_mib: limits.address_space_mib,
        cpu_seconds: limits.cpu_seconds,
        concurrency: studio.max_parallel() as u32,
        active_count: active_tasks.len() as u32,
        active_tasks,
        reserved_memory_mib: studio.reserved_memory_mib(),
        checks_pending: checks.map_or(0, |queue| queue.pending_count() as u32),
        checks_enabled: checks.is_some_and(CheckQueue::is_enabled),
        last_error: studio.last_error(),
    }
}

fn dispatch(
    studio: &Studio,
    limits: ProcessLimits,
    checks: &CheckQueue,
    request: Request,
) -> io::Result<(Response, bool)> {
    let (response, shutdown) = match request {
        Request::Status => (
            Response::Status(status(studio, limits, Some(checks))),
            false,
        ),
        Request::Snapshot => (Response::Snapshot(studio.snapshot()?), false),
        Request::Checks => (Response::Checks(checks.snapshot()?), false),
        Request::CheckLog { check_id, stderr } => (
            Response::Text(checks.read_log_tail(check_id, stderr)?),
            false,
        ),
        Request::QueueCheck {
            task_id,
            hook,
            kind,
        } => {
            let state = studio.snapshot()?;
            let task = state
                .tasks
                .iter()
                .find(|t| t.id == task_id)
                .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "task does not exist"))?;
            if task.mode != AgentMode::EditWorktree || task.status == TaskStatus::Running {
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "checks require a completed editable task worktree",
                ));
            }
            let run = state
                .runs
                .iter()
                .find(|r| r.task_id == task_id)
                .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "run missing"))?;
            let workspace = run
                .worktree_dir
                .as_ref()
                .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "task has no worktree"))?;
            (
                Response::Id(checks.enqueue(task_id, hook, kind, workspace)?),
                false,
            )
        }
        Request::AddBot {
            name,
            workspace,
            profile_dir,
            provider,
        } => (
            Response::Id(studio.add_bot_with_provider(&name, workspace, profile_dir, provider)?),
            false,
        ),
        Request::Submit {
            bot_id,
            prompt,
            mode,
        } => (
            Response::Id(studio.submit_with_post_edit(
                bot_id,
                &prompt,
                mode,
                checks.post_edit_kinds(),
            )?),
            false,
        ),
        Request::Cancel { task_id } => {
            studio.cancel_task(task_id)?;
            (Response::Ok, false)
        }
        Request::Log {
            task_id,
            stderr: false,
            provider: Some(provider),
        } => (
            Response::AgentOutput(studio.read_agent_output(task_id, provider)?),
            false,
        ),
        Request::Log {
            task_id, stderr, ..
        } => (
            Response::Text(studio.read_log_tail(task_id, stderr)?),
            false,
        ),
        Request::Diff { task_id } => (Response::Text(studio.read_worktree_diff(task_id)?), false),
        Request::Shutdown => {
            if studio.is_active() {
                return Err(io::Error::new(
                    io::ErrorKind::WouldBlock,
                    "worker has active tasks; cancel or wait for completion",
                ));
            }
            if checks.has_post_edit_hooks() {
                checks.try_observe_post_edit(&studio.snapshot()?)?;
            }
            if checks.pending_count() != 0 {
                return Err(io::Error::new(
                    io::ErrorKind::WouldBlock,
                    "worker has pending checks; wait for completion",
                ));
            }
            (Response::Ok, true)
        }
    };
    Ok((response, shutdown))
}

fn serve_one(
    mut stream: UnixStream,
    studio: &Arc<Studio>,
    limits: ProcessLimits,
    checks: &CheckQueue,
    preview: &mut Option<thread::JoinHandle<()>>,
) -> io::Result<bool> {
    stream.set_read_timeout(Some(Duration::from_secs(3)))?;
    stream.set_write_timeout(Some(Duration::from_secs(3)))?;
    let request: Request = receive(&mut stream, MAX_REQUEST)?;

    if preview
        .as_ref()
        .is_some_and(thread::JoinHandle::is_finished)
    {
        if let Some(finished) = preview.take() {
            let _ = finished.join();
        }
    }
    if preview.is_some() && matches!(request, Request::Diff { .. } | Request::Shutdown) {
        return reply(
            stream,
            Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                "worker is preparing a worktree diff; wait for it to finish",
            )),
        );
    }
    if matches!(request, Request::Diff { .. }) {
        let studio = Arc::clone(studio);
        let checks = checks.clone();
        *preview = Some(
            thread::Builder::new()
                .name("kojora-worker-diff".into())
                .spawn(move || {
                    if let Err(error) = reply(stream, dispatch(&studio, limits, &checks, request)) {
                        eprintln!("worker diff client error: {error}");
                    }
                })?,
        );
        return Ok(false);
    }

    reply(stream, dispatch(studio, limits, checks, request))
}

fn reply(mut stream: UnixStream, result: io::Result<(Response, bool)>) -> io::Result<bool> {
    let (response, shutdown) = match result {
        Ok(result) => result,
        Err(error) => (
            Response::Error {
                kind: kind_name(error.kind()).to_owned(),
                message: error.to_string(),
            },
            false,
        ),
    };
    transmit(&mut stream, &response, MAX_RESPONSE)?;
    Ok(shutdown)
}

struct SocketGuard {
    path: PathBuf,
    metadata: fs::Metadata,
}

impl Drop for SocketGuard {
    fn drop(&mut self) {
        if fs::symlink_metadata(&self.path)
            .is_ok_and(|current| same_socket(&self.metadata, &current))
        {
            let _ = fs::remove_file(&self.path);
        }
    }
}

/// Serves the studio on a local socket within its private data directory.
/// The daemon is the only process that may hold the state-file lock.
pub fn serve(studio: Studio, limits: ProcessLimits) -> io::Result<()> {
    serve_with_checks(studio, limits, CheckPolicy::default())
}

struct ObserverGuard {
    running: Arc<AtomicBool>,
    handle: Option<thread::JoinHandle<()>>,
}

impl Drop for ObserverGuard {
    fn drop(&mut self) {
        self.running.store(false, Ordering::Release);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

pub fn serve_with_checks(
    studio: Studio,
    limits: ProcessLimits,
    policy: CheckPolicy,
) -> io::Result<()> {
    limits.validate()?;
    let socket = studio.data_dir().join("worker.sock");
    // Studio already holds the exclusive lock for this private directory.
    remove_stale_socket(&socket)?;
    let listener = UnixListener::bind(&socket)?;
    let _guard = SocketGuard {
        path: socket.clone(),
        metadata: fs::symlink_metadata(&socket)?,
    };
    fs::set_permissions(&socket, fs::Permissions::from_mode(0o600))?;
    let studio = Arc::new(studio);
    let for_checks = Arc::clone(&studio);
    let checks = CheckQueue::open_for_worker(
        studio.data_dir().join("checks"),
        policy,
        Arc::new(move || for_checks.try_acquire_check_slot()),
        &studio.snapshot()?,
        studio.recovered_incomplete_runs,
    )?;
    let running = Arc::new(AtomicBool::new(true));
    let _observer = if checks.has_post_edit_hooks() {
        let check_copy = checks.clone();
        let studio_copy = Arc::clone(&studio);
        let active = Arc::clone(&running);
        let handle = thread::Builder::new()
            .name("kojora-post-edit-hooks".to_owned())
            .spawn(move || {
                while active.load(Ordering::Acquire) {
                    if let Ok(snapshot) = studio_copy.snapshot() {
                        check_copy.observe_post_edit(&snapshot);
                    }
                    thread::sleep(Duration::from_millis(250));
                }
            })?;
        ObserverGuard {
            running,
            handle: Some(handle),
        }
    } else {
        ObserverGuard {
            running,
            handle: None,
        }
    };
    eprintln!(
        "Kojora worker PID {} listening at {}",
        std::process::id(),
        socket.display()
    );
    let mut preview = None;
    for accepted in listener.incoming() {
        match accepted {
            Ok(stream) => match serve_one(stream, &studio, limits, &checks, &mut preview) {
                Ok(true) => return Ok(()),
                Ok(false) => {}
                Err(error) => eprintln!("worker client error: {error}"),
            },
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(error),
        }
    }
    Ok(())
}

fn remove_stale_socket(socket: &Path) -> io::Result<()> {
    let metadata = match verify_private_socket(socket) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error),
    };
    match UnixStream::connect(socket) {
        Ok(_) => {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                "an active listener is using worker.sock",
            ))
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) if error.kind() == io::ErrorKind::ConnectionRefused => {}
        Err(error) => return Err(error),
    }
    let current = verify_private_socket(socket)?;
    if !same_socket(&metadata, &current) {
        return Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            "worker.sock changed while checking for a stale listener",
        ));
    }
    fs::remove_file(socket)
}

fn same_socket(original: &fs::Metadata, current: &fs::Metadata) -> bool {
    current.file_type().is_socket()
        && current.dev() == original.dev()
        && current.ino() == original.ino()
}

/// Only trust a socket file owned by this Unix user in an owner-private
/// directory, not a symlink or a world-accessible user-supplied endpoint.
fn verify_private_socket(socket: &Path) -> io::Result<fs::Metadata> {
    if !socket.is_absolute() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "worker socket path must be absolute",
        ));
    }
    let parent = socket.parent().ok_or_else(|| {
        io::Error::new(io::ErrorKind::InvalidInput, "worker socket has no parent")
    })?;
    let directory = fs::symlink_metadata(parent)?;
    let metadata = fs::symlink_metadata(socket)?;
    let uid = rustix::process::geteuid().as_raw();
    if !directory.is_dir()
        || directory.file_type().is_symlink()
        || directory.uid() != uid
        || directory.permissions().mode() & 0o077 != 0
        || !metadata.file_type().is_socket()
        || metadata.uid() != uid
        || metadata.permissions().mode() & 0o077 != 0
    {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "worker socket and parent directory must be private and owned by the current user",
        ));
    }
    Ok(metadata)
}

#[derive(Debug, Clone)]
pub struct WorkerClient {
    socket: PathBuf,
    data_dir: PathBuf,
}

impl WorkerClient {
    pub fn connect(socket: impl Into<PathBuf>) -> io::Result<Self> {
        let socket = socket.into();
        verify_private_socket(&socket)?;
        let mut client = Self {
            socket,
            data_dir: PathBuf::new(),
        };
        let status = client.status()?;
        if status.protocol != VERSION {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                format!("incompatible worker protocol {}", status.protocol),
            ));
        }
        client.data_dir = status.data_dir;
        Ok(client)
    }

    pub fn data_dir(&self) -> &Path {
        &self.data_dir
    }

    fn request(&self, request: Request) -> io::Result<Response> {
        let mut stream = UnixStream::connect(&self.socket)?;
        stream.set_write_timeout(Some(Duration::from_secs(3)))?;
        let reply_timeout = if matches!(request, Request::Diff { .. }) {
            WORKTREE_PREVIEW_TIMEOUT + Duration::from_secs(3)
        } else {
            Duration::from_secs(3)
        };
        stream.set_read_timeout(Some(reply_timeout))?;
        transmit(&mut stream, &request, MAX_REQUEST)?;
        match receive(&mut stream, MAX_RESPONSE)? {
            Response::Error { kind, message } => Err(decode_error(&kind, message)),
            other => Ok(other),
        }
    }

    pub fn status(&self) -> io::Result<WorkerStatus> {
        match self.request(Request::Status)? {
            Response::Status(mut value) => {
                if value.active_tasks.is_empty() {
                    value
                        .active_tasks
                        .extend(value.active_task.map(|task_id| ActiveTaskStatus {
                            task_id,
                            cancellation_requested: value.cancellation_requested,
                        }));
                }
                Ok(value)
            }
            _ => Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "expected worker status",
            )),
        }
    }

    pub fn check_log(&self, check_id: u64, stderr: bool) -> io::Result<String> {
        match self.request(Request::CheckLog { check_id, stderr })? {
            Response::Text(value) => Ok(value),
            _ => Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "expected check log",
            )),
        }
    }

    pub fn checks(&self) -> io::Result<Vec<CheckRecord>> {
        match self.request(Request::Checks)? {
            Response::Checks(value) => Ok(value),
            _ => Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "expected check records",
            )),
        }
    }

    pub fn queue_check(&self, task_id: u64, hook: HookPhase, kind: CheckKind) -> io::Result<u64> {
        match self.request(Request::QueueCheck {
            task_id,
            hook,
            kind,
        })? {
            Response::Id(value) => Ok(value),
            _ => Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "expected check ID",
            )),
        }
    }

    pub fn snapshot(&self) -> io::Result<StudioState> {
        match self.request(Request::Snapshot)? {
            Response::Snapshot(value) => Ok(value),
            _ => Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "expected studio snapshot",
            )),
        }
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
        match self.request(Request::AddBot {
            name: name.to_owned(),
            workspace,
            profile_dir,
            provider,
        })? {
            Response::Id(value) => Ok(value),
            _ => Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "expected bot ID",
            )),
        }
    }

    pub fn submit_with_mode(&self, bot_id: u64, prompt: &str, mode: AgentMode) -> io::Result<u64> {
        match self.request(Request::Submit {
            bot_id,
            prompt: prompt.to_owned(),
            mode,
        })? {
            Response::Id(value) => Ok(value),
            _ => Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "expected task ID",
            )),
        }
    }

    pub fn cancel_task(&self, task_id: u64) -> io::Result<()> {
        match self.request(Request::Cancel { task_id })? {
            Response::Ok => Ok(()),
            _ => Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "expected cancellation acknowledgement",
            )),
        }
    }

    pub fn read_log_tail(&self, task_id: u64, stderr: bool) -> io::Result<String> {
        match self.request(Request::Log {
            task_id,
            stderr,
            provider: None,
        })? {
            Response::Text(value) => Ok(value),
            _ => Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "expected log text",
            )),
        }
    }

    pub fn read_agent_output(
        &self,
        task_id: u64,
        provider: AgentProvider,
    ) -> io::Result<AgentOutput> {
        match self.request(Request::Log {
            task_id,
            stderr: false,
            provider: Some(provider),
        })? {
            Response::AgentOutput(value) => Ok(value),
            // An older v3 worker ignores the optional provider and returns a raw log tail.
            Response::Text(raw) => Ok(AgentOutput::from_log(provider, raw)),
            _ => Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "expected agent output",
            )),
        }
    }

    pub fn read_worktree_diff(&self, task_id: u64) -> io::Result<String> {
        match self.request(Request::Diff { task_id })? {
            Response::Text(value) => Ok(value),
            _ => Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "expected diff text",
            )),
        }
    }

    pub fn shutdown(&self) -> io::Result<()> {
        match self.request(Request::Shutdown)? {
            Response::Ok => Ok(()),
            _ => Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "expected shutdown acknowledgement",
            )),
        }
    }
}

/// One shared GUI interface, optionally backed by a separate long-lived daemon.
/// Local mode retains existing backwards-compatible behavior.
pub enum StudioConnection {
    Local(Studio),
    Worker(WorkerClient),
}

impl StudioConnection {
    pub fn status(&self) -> io::Result<WorkerStatus> {
        match self {
            Self::Local(studio) => Ok(status(studio, ProcessLimits::default(), None)),
            Self::Worker(worker) => worker.status(),
        }
    }

    pub fn open_local(root: impl Into<PathBuf>) -> io::Result<Self> {
        Studio::open(root).map(Self::Local)
    }

    pub fn connect_worker(socket: impl Into<PathBuf>) -> io::Result<Self> {
        WorkerClient::connect(socket).map(Self::Worker)
    }

    pub fn data_dir(&self) -> &Path {
        match self {
            Self::Local(value) => value.data_dir(),
            Self::Worker(value) => value.data_dir(),
        }
    }

    pub fn check_log(&self, check_id: u64, stderr: bool) -> io::Result<String> {
        match self {
            Self::Local(_) => Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "check logs require a separate Worker",
            )),
            Self::Worker(value) => value.check_log(check_id, stderr),
        }
    }

    pub fn checks(&self) -> io::Result<Vec<CheckRecord>> {
        match self {
            Self::Local(_) => Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "local checks require a separate Worker",
            )),
            Self::Worker(value) => value.checks(),
        }
    }

    pub fn queue_check(&self, task_id: u64, hook: HookPhase, kind: CheckKind) -> io::Result<u64> {
        match self {
            Self::Local(_) => Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "local checks require a separate Worker",
            )),
            Self::Worker(value) => value.queue_check(task_id, hook, kind),
        }
    }

    pub fn snapshot(&self) -> io::Result<StudioState> {
        match self {
            Self::Local(value) => value.snapshot(),
            Self::Worker(value) => value.snapshot(),
        }
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
        match self {
            Self::Local(value) => {
                value.add_bot_with_provider(name, workspace, profile_dir, provider)
            }
            Self::Worker(value) => {
                value.add_bot_with_provider(name, workspace, profile_dir, provider)
            }
        }
    }

    pub fn submit_with_mode(&self, bot_id: u64, prompt: &str, mode: AgentMode) -> io::Result<u64> {
        match self {
            Self::Local(value) => value.submit_with_mode(bot_id, prompt, mode),
            Self::Worker(value) => value.submit_with_mode(bot_id, prompt, mode),
        }
    }

    pub fn cancel_task(&self, task_id: u64) -> io::Result<()> {
        match self {
            Self::Local(value) => value.cancel_task(task_id),
            Self::Worker(value) => value.cancel_task(task_id),
        }
    }

    pub fn active_task_id(&self) -> Option<u64> {
        match self {
            Self::Local(value) => value.active_task_id(),
            Self::Worker(value) => value.status().ok().and_then(|status| status.active_task),
        }
    }

    pub fn is_active(&self) -> bool {
        self.active_task_id().is_some()
    }

    pub fn cancellation_requested(&self, task_id: u64) -> bool {
        match self {
            Self::Local(value) => value.cancellation_requested(task_id),
            Self::Worker(value) => value.status().is_ok_and(|status| {
                status
                    .active_tasks
                    .iter()
                    .any(|task| task.task_id == task_id && task.cancellation_requested)
            }),
        }
    }

    pub fn last_error(&self) -> Option<String> {
        match self {
            Self::Local(value) => value.last_error(),
            Self::Worker(value) => match value.status() {
                Ok(status) => status.last_error,
                Err(error) => Some(format!("Worker disconnected: {error}")),
            },
        }
    }

    pub fn read_log_tail(&self, task_id: u64, stderr: bool) -> io::Result<String> {
        match self {
            Self::Local(value) => value.read_log_tail(task_id, stderr),
            Self::Worker(value) => value.read_log_tail(task_id, stderr),
        }
    }

    pub fn read_agent_output(
        &self,
        task_id: u64,
        provider: AgentProvider,
    ) -> io::Result<AgentOutput> {
        match self {
            Self::Local(value) => value.read_agent_output(task_id, provider),
            Self::Worker(value) => value.read_agent_output(task_id, provider),
        }
    }

    pub fn read_worktree_diff(&self, task_id: u64) -> io::Result<String> {
        match self {
            Self::Local(value) => value.read_worktree_diff(task_id),
            Self::Worker(value) => value.read_worktree_diff(task_id),
        }
    }
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
    use crate::{AgentRunner, BotRecord, TaskStatus};
    use kojora_runtime::{CancellationToken, RunOutcome};
    use std::sync::Arc;
    use std::thread;

    fn test_root() -> PathBuf {
        // Account for long macOS temporary paths and the Unix socket path limit.
        let root = std::env::temp_dir().join(format!(
            "kjw-{}-{:x}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir(&root).unwrap();
        fs::set_permissions(&root, fs::Permissions::from_mode(0o700)).unwrap();
        root
    }

    fn fake_runner() -> Arc<AgentRunner> {
        Arc::new(
            |_bot: &BotRecord,
             prompt: &str,
             log_root: &Path,
             _mode: AgentMode,
             _token: &CancellationToken| {
                let run_dir = log_root.join("fake-run");
                fs::create_dir_all(&run_dir)?;
                fs::write(run_dir.join("stdout.log"), format!("echo: {prompt}"))?;
                Ok(RunOutcome {
                    run_dir,
                    success: true,
                    cancelled: false,
                    exit_code: Some(0),
                    duration: Duration::ZERO,
                    stdout_bytes: 5,
                    stderr_bytes: 0,
                })
            },
        )
    }

    #[test]
    fn status_reports_cancellation_for_the_selected_parallel_task() {
        let root = test_root();
        let release = Arc::new(AtomicBool::new(false));
        let allow_finish = Arc::clone(&release);
        let fake = fake_runner();
        let runner: Arc<AgentRunner> = Arc::new(move |bot, prompt, log, mode, token| {
            while !allow_finish.load(Ordering::Acquire) {
                thread::sleep(Duration::from_millis(10));
            }
            fake(bot, prompt, log, mode, token)
        });
        let studio = Studio::open_with_runner_and_policy(
            &root,
            runner,
            crate::scheduler::AdmissionPolicy {
                max_parallel: 2,
                memory_mib_per_task: 128,
                total_memory_mib: 256,
            },
        )
        .unwrap();
        let first = studio
            .add_bot("First", root.join("project"), root.join("first"))
            .unwrap();
        let second = studio
            .add_bot("Second", root.join("project"), root.join("second"))
            .unwrap();
        let first = studio.submit(first, "Read from account one").unwrap();
        let second = studio.submit(second, "Read from account two").unwrap();
        studio.cancel_task(second).unwrap();
        let activity = status(&studio, ProcessLimits::default(), None);
        release.store(true, Ordering::Release);
        assert_eq!(activity.concurrency, 2);
        assert_eq!(activity.active_count, 2);
        assert_eq!(activity.active_task, Some(first));
        assert_eq!(
            activity.active_tasks,
            vec![
                ActiveTaskStatus {
                    task_id: first,
                    cancellation_requested: false
                },
                ActiveTaskStatus {
                    task_id: second,
                    cancellation_requested: true
                },
            ]
        );
        for _ in 0..200 {
            if !studio.is_active() {
                break;
            }
            thread::sleep(Duration::from_millis(10));
        }
        assert!(!studio.is_active());
        drop(studio);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn shutdown_observes_newly_completed_edit_tasks() {
        let root = test_root();
        let worktree = root.join("worktrees/task-1");
        fs::create_dir_all(&worktree).unwrap();
        let limits = ProcessLimits {
            address_space_mib: Some(8192),
            cpu_seconds: Some(60),
        };
        let policy = CheckPolicy {
            enabled: true,
            limits,
            post_edit: vec![CheckKind::RustFmt],
            ..CheckPolicy::default()
        };
        let release = Arc::new(AtomicBool::new(false));
        let gate = Arc::clone(&release);
        let checks = CheckQueue::open_with_admission(
            root.join("checks"),
            policy,
            Arc::new(move || {
                if gate.load(Ordering::Acquire) {
                    return Err(io::Error::other("fixture prevents running a real check"));
                }
                Ok(None)
            }),
        )
        .unwrap();
        let finished_ms = crate::unix_ms().unwrap();
        let state = StudioState {
            next_id: 4,
            tasks: vec![crate::TaskRecord {
                id: 1,
                run_id: 2,
                bot_id: 3,
                prompt: "Completed edit".into(),
                mode: AgentMode::EditWorktree,
                status: TaskStatus::Succeeded,
                created_ms: finished_ms,
            }],
            runs: vec![crate::RunRecord {
                id: 2,
                task_id: 1,
                session_id: 3,
                status: TaskStatus::Succeeded,
                started_ms: finished_ms,
                finished_ms: Some(finished_ms),
                exit_code: Some(0),
                run_dir: None,
                worktree_dir: Some(worktree),
                post_edit_checks: None,
                error: None,
            }],
            ..StudioState::default()
        };
        crate::persist(&root, &state).unwrap();
        let studio = Studio::open_with_runner(&root, fake_runner()).unwrap();
        let shutdown = dispatch(&studio, limits, &checks, Request::Shutdown);
        release.store(true, Ordering::Release);
        assert_eq!(shutdown.unwrap_err().kind(), io::ErrorKind::WouldBlock);
        assert_eq!(checks.snapshot().unwrap()[0].hook, HookPhase::PostEdit);
        for _ in 0..200 {
            if checks.pending_count() == 0 {
                break;
            }
            thread::sleep(Duration::from_millis(10));
        }
        assert!(matches!(
            dispatch(&studio, limits, &checks, Request::Shutdown).unwrap(),
            (Response::Ok, true)
        ));
        drop(checks);
        drop(studio);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn daemon_ipc_survives_client_disconnect_and_reuses_state() {
        let root = test_root();
        let studio = Studio::open_with_runner(&root, fake_runner()).unwrap();
        let socket = root.join("worker.sock");
        let server = thread::spawn(move || serve(studio, ProcessLimits::default()));
        for _ in 0..200 {
            if socket.exists() {
                break;
            }
            thread::sleep(Duration::from_millis(10));
        }
        let client = WorkerClient::connect(&socket).expect("connect to daemon");
        assert_eq!(client.status().unwrap().concurrency, 1);
        let bot = client
            .add_bot("A", root.join("workspace"), root.join("profile"))
            .unwrap();
        let claude_profile = root.join("claude-work");
        let claude_bot = client
            .add_bot_with_provider(
                "Claude Work",
                root.join("workspace"),
                claude_profile.clone(),
                AgentProvider::ClaudeCode,
            )
            .unwrap();
        assert_eq!(
            client
                .submit_with_mode(claude_bot, "Try editing", AgentMode::EditWorktree)
                .unwrap_err()
                .kind(),
            io::ErrorKind::Unsupported
        );
        let task = client
            .submit_with_mode(bot, "Describe it", AgentMode::Inspect)
            .unwrap();
        drop(client); // UI can exit without interrupting the running worker.
        let second = WorkerClient::connect(&socket).unwrap();
        let state = second.snapshot().unwrap();
        assert_eq!(state.bots[0].provider, AgentProvider::Codex);
        assert_eq!(state.bots[1].provider, AgentProvider::ClaudeCode);
        assert_eq!(state.bots[1].profile_dir, claude_profile);
        for _ in 0..200 {
            if second.snapshot().unwrap().tasks[0].status == TaskStatus::Succeeded {
                break;
            }
            thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(
            second.snapshot().unwrap().tasks[0].status,
            TaskStatus::Succeeded
        );
        assert!(second
            .read_log_tail(task, false)
            .unwrap()
            .contains("Describe it"));
        assert!(second.cancel_task(task).is_err());
        second.shutdown().unwrap();
        server.join().unwrap().unwrap();
        assert!(!socket.exists());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn worker_accepts_a_full_length_prompt_after_json_escaping() {
        let root = test_root();
        let studio = Studio::open_with_runner(&root, fake_runner()).unwrap();
        let bot = studio
            .add_bot("A", root.join("workspace"), root.join("profile"))
            .unwrap();
        let socket = root.join("worker.sock");
        let server = thread::spawn(move || serve(studio, ProcessLimits::default()));
        let mut connected = None;
        for _ in 0..200 {
            if let Ok(client) = WorkerClient::connect(&socket) {
                connected = Some(client);
                break;
            }
            thread::sleep(Duration::from_millis(10));
        }
        let client = connected.unwrap();
        let prompt = "\u{0001}".repeat(crate::MAX_PROMPT_BYTES);
        let submitted = client.submit_with_mode(bot, &prompt, AgentMode::Inspect);
        for _ in 0..200 {
            if client.status().unwrap().active_count == 0 {
                break;
            }
            thread::sleep(Duration::from_millis(10));
        }
        let state = client.snapshot().unwrap();
        client.shutdown().unwrap();
        server.join().unwrap().unwrap();
        fs::remove_dir_all(root).unwrap();

        let task_id = submitted.unwrap();
        assert_eq!(state.tasks[0].id, task_id);
        assert_eq!(state.tasks[0].prompt, prompt);
        assert_eq!(state.tasks[0].status, TaskStatus::Succeeded);
    }

    #[test]
    fn worker_keeps_the_tail_of_large_assistant_messages() {
        let root = test_root();
        let fake = fake_runner();
        let runner: Arc<AgentRunner> = Arc::new(move |bot, prompt, log, mode, token| {
            let mut outcome = fake(bot, prompt, log, mode, token)?;
            let text = format!("{} end of response", "返".repeat(18_000));
            let event = match bot.provider {
                AgentProvider::Codex => serde_json::json!({
                    "type": "item.completed", "item": {"type": "agent_message", "text": text}
                }),
                AgentProvider::ClaudeCode => serde_json::json!({
                    "type": "assistant", "message": {"content": [{"type": "text", "text": text}]}
                }),
            };
            let json = format!("{event}\n");
            fs::write(outcome.run_dir.join("stdout.log"), &json)?;
            outcome.stdout_bytes = json.len() as u64;
            Ok(outcome)
        });
        let studio = Studio::open_with_runner(&root, runner).unwrap();
        let socket = root.join("worker.sock");
        let server = thread::spawn(move || serve(studio, ProcessLimits::default()));
        let mut connected = None;
        for _ in 0..200 {
            if let Ok(client) = WorkerClient::connect(&socket) {
                connected = Some(client);
                break;
            }
            thread::sleep(Duration::from_millis(10));
        }
        let client = connected.unwrap();
        let mut output = Vec::new();
        for provider in AgentProvider::ALL {
            let bot = client
                .add_bot_with_provider(
                    provider.label(),
                    root.join("project"),
                    root.join("profile"),
                    provider,
                )
                .unwrap();
            let task = client
                .submit_with_mode(bot, "Report", AgentMode::Inspect)
                .unwrap();
            for _ in 0..200 {
                if client.status().unwrap().active_count == 0 {
                    break;
                }
                thread::sleep(Duration::from_millis(10));
            }
            let raw = client.read_log_tail(task, false).unwrap();
            assert!(provider.extract_text(&raw).is_empty());
            output.push((provider, client.read_agent_output(task, provider).unwrap()));
        }
        client.shutdown().unwrap();
        server.join().unwrap().unwrap();
        fs::remove_dir_all(root).unwrap();

        for (provider, output) in output {
            let AgentOutput::Message(text) = output else {
                panic!("{provider:?} returned raw text instead of the assistant message");
            };
            assert!(
                text.ends_with(" end of response"),
                "{provider:?} lost the response tail"
            );
            assert!(text.starts_with("[earlier output omitted]\n"));
            assert!(text.len() <= crate::MAX_LOG_TAIL_BYTES);
        }
    }

    #[test]
    fn slow_worktree_diff_keeps_status_and_cancellation_available() {
        let root = test_root().canonicalize().unwrap();
        let project = root.join("project");
        fs::create_dir(&project).unwrap();
        let git = |args: &[&str]| {
            assert!(crate::git_command(&project)
                .args(args)
                .stdout(std::process::Stdio::null())
                .status()
                .unwrap()
                .success());
        };
        git(&["init"]);
        fs::write(project.join("file.txt"), "before\n").unwrap();
        git(&["add", "file.txt"]);
        git(&[
            "-c",
            "user.name=Kojora",
            "-c",
            "user.email=kojora@example.invalid",
            "commit",
            "-m",
            "Initial",
        ]);
        let monitor = root.join("fsmonitor");
        fs::write(
            &monitor,
            "#!/bin/sh\n: > .monitor-started\ncount=0\nwhile [ ! -f .monitor-release ] && [ \"$count\" -lt 350 ]; do\n  sleep 0.02\n  count=$((count+1))\ndone\nexit 1\n",
        )
        .unwrap();
        fs::set_permissions(&monitor, fs::Permissions::from_mode(0o700)).unwrap();

        let fake = fake_runner();
        let runner: Arc<AgentRunner> = Arc::new(move |bot, prompt, logs, mode, token| {
            if mode == AgentMode::EditWorktree {
                fs::write(bot.workspace.join("file.txt"), "after\n")?;
                return fake(bot, prompt, logs, mode, token);
            }
            for _ in 0..800 {
                if token.is_cancelled() {
                    return Err(io::Error::from(io::ErrorKind::Interrupted));
                }
                thread::sleep(Duration::from_millis(10));
            }
            Err(io::Error::from(io::ErrorKind::TimedOut))
        });
        let state_dir = root.join("state");
        let studio = Studio::open_with_runner(&state_dir, runner).unwrap();
        let bot = studio
            .add_bot("Bot", project.clone(), root.join("profile"))
            .unwrap();
        let edit = studio
            .submit_with_mode(bot, "Edit", AgentMode::EditWorktree)
            .unwrap();
        for _ in 0..200 {
            if !studio.is_active() {
                break;
            }
            thread::sleep(Duration::from_millis(10));
        }
        let state = studio.snapshot().unwrap();
        assert_eq!(state.tasks[0].status, TaskStatus::Succeeded);
        let worktree = state.runs[0].worktree_dir.as_ref().unwrap();
        git(&["config", "core.fsmonitor", monitor.to_str().unwrap()]);

        let socket = state_dir.join("worker.sock");
        let server = thread::spawn(move || serve(studio, ProcessLimits::default()));
        let mut client = None;
        for _ in 0..200 {
            if let Ok(connected) = WorkerClient::connect(&socket) {
                client = Some(connected);
                break;
            }
            thread::sleep(Duration::from_millis(10));
        }
        let client = client.unwrap();
        let inspect = client
            .submit_with_mode(bot, "Inspect", AgentMode::Inspect)
            .unwrap();
        let preview_client = client.clone();
        let diff = thread::spawn(move || preview_client.read_worktree_diff(edit));
        for _ in 0..200 {
            if worktree.join(".monitor-started").exists() {
                break;
            }
            thread::sleep(Duration::from_millis(10));
        }
        assert!(worktree.join(".monitor-started").exists());

        let activity = client.status();
        let (cancel, second_preview) = if activity.is_ok() {
            (client.cancel_task(inspect), client.read_worktree_diff(edit))
        } else {
            (
                Err(io::Error::other("status was blocked")),
                Ok(String::new()),
            )
        };
        fs::write(worktree.join(".monitor-release"), "").unwrap();
        let diff = diff.join().unwrap();
        if cancel.is_err() {
            let _ = client.cancel_task(inspect);
        }
        let stopped = (0..200).any(|_| match client.shutdown() {
            Ok(()) => true,
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                thread::sleep(Duration::from_millis(10));
                false
            }
            Err(error) => panic!("could not stop Worker: {error}"),
        });
        assert!(stopped, "Worker did not stop after the preview finished");
        server.join().unwrap().unwrap();
        fs::remove_dir_all(root).unwrap();

        assert_eq!(activity.unwrap().active_task, Some(inspect));
        cancel.unwrap();
        assert_eq!(
            second_preview.unwrap_err().kind(),
            io::ErrorKind::WouldBlock
        );
        assert!(diff.unwrap().contains("+after"));
    }

    #[test]
    fn rejects_public_or_symlinked_control_sockets() {
        let root = test_root();
        let socket = root.join("worker.sock");
        // A regular file cannot masquerade as the worker endpoint.
        fs::write(&socket, b"fake").unwrap();
        assert_eq!(
            verify_private_socket(&socket).unwrap_err().kind(),
            io::ErrorKind::PermissionDenied
        );
        assert_eq!(
            remove_stale_socket(&socket).unwrap_err().kind(),
            io::ErrorKind::PermissionDenied
        );
        assert_eq!(fs::read(&socket).unwrap(), b"fake");
        fs::remove_file(&socket).unwrap();
        let target = root.join("target");
        fs::write(&target, b"untouched").unwrap();
        std::os::unix::fs::symlink(&target, &socket).unwrap();
        assert_eq!(
            remove_stale_socket(&socket).unwrap_err().kind(),
            io::ErrorKind::PermissionDenied
        );
        assert!(fs::symlink_metadata(&socket).unwrap().is_symlink());
        assert_eq!(fs::read(&target).unwrap(), b"untouched");
        fs::remove_file(&socket).unwrap();
        fs::set_permissions(&root, fs::Permissions::from_mode(0o755)).unwrap();
        assert!(verify_private_socket(&socket).is_err());
        fs::set_permissions(&root, fs::Permissions::from_mode(0o700)).unwrap();
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn worker_refuses_to_replace_a_live_socket() {
        let root = test_root();
        let socket = root.join("worker.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        fs::set_permissions(&socket, fs::Permissions::from_mode(0o600)).unwrap();

        let studio = Studio::open_with_runner(&root, fake_runner()).unwrap();
        let error = serve(studio, ProcessLimits::default()).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::AlreadyExists);
        assert!(socket.exists(), "a live socket must remain in place");
        drop(listener);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn socket_cleanup_preserves_a_file_that_replaced_the_listener_path() {
        let root = test_root();
        let socket = root.join("worker.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        let guard = SocketGuard {
            path: socket.clone(),
            metadata: fs::symlink_metadata(&socket).unwrap(),
        };
        fs::remove_file(&socket).unwrap();
        fs::write(&socket, b"keep me").unwrap();
        drop(guard);
        assert_eq!(fs::read(&socket).unwrap(), b"keep me");
        drop(listener);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn worker_can_restart_with_an_owned_stale_socket() {
        let root = test_root();
        let socket = root.join("worker.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        fs::set_permissions(&socket, fs::Permissions::from_mode(0o600)).unwrap();
        drop(listener);
        let studio = Studio::open_with_runner(&root, fake_runner()).unwrap();
        let restarted = thread::spawn(move || serve(studio, ProcessLimits::default()));
        let mut connected = None;
        for _ in 0..200 {
            if let Ok(client) = WorkerClient::connect(&socket) {
                connected = Some(client);
                break;
            }
            if restarted.is_finished() {
                break;
            }
            thread::sleep(Duration::from_millis(10));
        }
        if let Some(client) = &connected {
            client.shutdown().unwrap();
        } else {
            assert!(restarted.is_finished(), "Worker never became reachable");
        }
        let result = restarted.join().unwrap();
        let socket_removed = !socket.exists();
        fs::remove_dir_all(root).unwrap();

        result.unwrap();
        assert!(connected.is_some());
        assert!(socket_removed);
    }

    #[test]
    fn rejects_oversized_frames() {
        let root = test_root();
        let studio = Studio::open_with_runner(&root, fake_runner()).unwrap();
        let socket = root.join("worker.sock");
        let server = thread::spawn(move || serve(studio, ProcessLimits::default()));
        for _ in 0..200 {
            if socket.exists() {
                break;
            }
            thread::sleep(Duration::from_millis(10));
        }
        let mut stream = UnixStream::connect(&socket).unwrap();
        stream
            .write_all(&(MAX_REQUEST as u32 + 1).to_be_bytes())
            .unwrap();
        drop(stream);
        WorkerClient::connect(&socket).unwrap().shutdown().unwrap();
        server.join().unwrap().unwrap();
        fs::remove_dir_all(root).unwrap();
    }
}
