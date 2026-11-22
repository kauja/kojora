//! Private, versioned local Unix-domain control channel for Kojora Studio.
//! Each request owns one short-lived connection. No TCP/network listener,
//! bearer tokens, process execution from wire input, or shell evaluation.

#![cfg(unix)]

use crate::{AgentMode, Studio, StudioState};
use kojora_runtime::ProcessLimits;
use serde::{de::DeserializeOwned, Deserialize, Serialize};
use std::fs;
use std::io::{self, Read, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::time::Duration;

const VERSION: u32 = 1;
const MAX_REQUEST: usize = 64 * 1024;
const MAX_RESPONSE: usize = 4 * 1024 * 1024 + 4096;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkerStatus {
    pub protocol: u32,
    pub pid: u32,
    pub data_dir: PathBuf,
    pub active_task: Option<u64>,
    pub cancellation_requested: bool,
    pub address_space_mib: Option<u64>,
    pub cpu_seconds: Option<u64>,
    pub concurrency: u32,
    pub last_error: Option<String>,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "op", content = "data", rename_all = "snake_case")]
enum Request {
    Status,
    Snapshot,
    AddBot { name: String, workspace: PathBuf, profile_dir: PathBuf },
    Submit { bot_id: u64, prompt: String, mode: AgentMode },
    Cancel { task_id: u64 },
    Log { task_id: u64, stderr: bool },
    Diff { task_id: u64 },
    Shutdown,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "result", content = "data", rename_all = "snake_case")]
enum Response {
    Status(WorkerStatus),
    Snapshot(StudioState),
    Id(u64),
    Text(String),
    Ok,
    Error { kind: String, message: String },
}

fn kind_name(kind: io::ErrorKind) -> &'static str {
    match kind {
        io::ErrorKind::NotFound => "not_found",
        io::ErrorKind::WouldBlock => "busy",
        io::ErrorKind::InvalidInput => "invalid_input",
        io::ErrorKind::PermissionDenied => "permission_denied",
        _ => "other",
    }
}

fn decode_error(kind: &str, message: String) -> io::Error {
    let kind = match kind {
        "not_found" => io::ErrorKind::NotFound,
        "busy" => io::ErrorKind::WouldBlock,
        "invalid_input" => io::ErrorKind::InvalidInput,
        "permission_denied" => io::ErrorKind::PermissionDenied,
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
        return Err(io::Error::new(io::ErrorKind::InvalidData, "invalid IPC frame length"));
    }
    let mut frame = vec![0u8; length];
    stream.read_exact(&mut frame)?;
    serde_json::from_slice(&frame)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))
}

fn transmit<T: Serialize>(stream: &mut UnixStream, message: &T, max: usize) -> io::Result<()> {
    let bytes = serde_json::to_vec(message).map_err(io::Error::other)?;
    if bytes.is_empty() || bytes.len() > max {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "IPC response exceeds limit"));
    }
    let length = u32::try_from(bytes.len())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "IPC frame too large"))?;
    stream.write_all(&length.to_be_bytes())?;
    stream.write_all(&bytes)?;
    Ok(())
}

fn status(studio: &Studio, limits: ProcessLimits) -> WorkerStatus {
    let active_task = studio.active_task_id();
    WorkerStatus {
        protocol: VERSION,
        pid: std::process::id(),
        data_dir: studio.data_dir().to_owned(),
        active_task,
        cancellation_requested: active_task.is_some_and(|id| studio.cancellation_requested(id)),
        address_space_mib: limits.address_space_mib,
        cpu_seconds: limits.cpu_seconds,
        concurrency: 1,
        last_error: studio.last_error(),
    }
}

fn dispatch(studio: &Studio, limits: ProcessLimits, request: Request) -> io::Result<(Response, bool)> {
    let (response, shutdown) = match request {
        Request::Status => (Response::Status(status(studio, limits)), false),
        Request::Snapshot => (Response::Snapshot(studio.snapshot()?), false),
        Request::AddBot { name, workspace, profile_dir } => (
            Response::Id(studio.add_bot(&name, workspace, profile_dir)?), false
        ),
        Request::Submit { bot_id, prompt, mode } => (
            Response::Id(studio.submit_with_mode(bot_id, &prompt, mode)?), false
        ),
        Request::Cancel { task_id } => {
            studio.cancel_task(task_id)?;
            (Response::Ok, false)
        }
        Request::Log { task_id, stderr } => (
            Response::Text(studio.read_log_tail(task_id, stderr)?), false
        ),
        Request::Diff { task_id } => (
            Response::Text(studio.read_worktree_diff(task_id)?), false
        ),
        Request::Shutdown => {
            if studio.is_active() {
                return Err(io::Error::new(io::ErrorKind::WouldBlock,
                    "worker is running an agent; cancel and wait for completion first"));
            }
            (Response::Ok, true)
        }
    };
    Ok((response, shutdown))
}

fn serve_one(mut stream: UnixStream, studio: &Studio, limits: ProcessLimits) -> io::Result<bool> {
    stream.set_read_timeout(Some(Duration::from_secs(3)))?;
    stream.set_write_timeout(Some(Duration::from_secs(3)))?;
    let request: Request = receive(&mut stream, MAX_REQUEST)?;
    let (response, shutdown) = match dispatch(studio, limits, request) {
        Ok(result) => result,
        Err(error) => (
            Response::Error { kind: kind_name(error.kind()).to_owned(), message: error.to_string() },
            false,
        ),
    };
    transmit(&mut stream, &response, MAX_RESPONSE)?;
    Ok(shutdown)
}

struct SocketGuard(PathBuf);
impl Drop for SocketGuard {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
    }
}

/// Serves the studio on a local socket within its private data directory.
/// The daemon is the only process that may hold the state-file lock.
pub fn serve(studio: Studio, limits: ProcessLimits) -> io::Result<()> {
    limits.validate()?;
    // Studio::open already checked that the root is an absolute, private,
    // real directory. Do not require a canonical spelling: on macOS /var
    // commonly resolves to /private/var.
    let socket = studio.data_dir().join("worker.sock");
    if fs::symlink_metadata(&socket).is_ok() {
        return Err(io::Error::new(io::ErrorKind::AlreadyExists,
            "worker.sock already exists; verify no worker is running before removing a stale socket"));
    }
    let listener = UnixListener::bind(&socket)?;
    let _guard = SocketGuard(socket.clone());
    fs::set_permissions(&socket, fs::Permissions::from_mode(0o600))?;
    eprintln!("Kojora worker PID {} listening at {}", std::process::id(), socket.display());
    for accepted in listener.incoming() {
        match accepted {
            Ok(stream) => match serve_one(stream, &studio, limits) {
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

#[derive(Debug, Clone)]
pub struct WorkerClient {
    socket: PathBuf,
    data_dir: PathBuf,
}

impl WorkerClient {
    pub fn connect(socket: impl Into<PathBuf>) -> io::Result<Self> {
        let mut client = Self { socket: socket.into(), data_dir: PathBuf::new() };
        let status = client.status()?;
        if status.protocol != VERSION {
            return Err(io::Error::new(io::ErrorKind::Unsupported,
                format!("incompatible worker protocol {}", status.protocol)));
        }
        client.data_dir = status.data_dir;
        Ok(client)
    }

    pub fn data_dir(&self) -> &Path {
        &self.data_dir
    }

    fn request(&self, request: Request) -> io::Result<Response> {
        let mut stream = UnixStream::connect(&self.socket)?;
        stream.set_read_timeout(Some(Duration::from_secs(3)))?;
        stream.set_write_timeout(Some(Duration::from_secs(3)))?;
        transmit(&mut stream, &request, MAX_REQUEST)?;
        match receive(&mut stream, MAX_RESPONSE)? {
            Response::Error { kind, message } => Err(decode_error(&kind, message)),
            other => Ok(other),
        }
    }

    pub fn status(&self) -> io::Result<WorkerStatus> {
        match self.request(Request::Status)? {
            Response::Status(value) => Ok(value),
            _ => Err(io::Error::new(io::ErrorKind::InvalidData, "expected worker status")),
        }
    }

    pub fn snapshot(&self) -> io::Result<StudioState> {
        match self.request(Request::Snapshot)? {
            Response::Snapshot(value) => Ok(value),
            _ => Err(io::Error::new(io::ErrorKind::InvalidData, "expected studio snapshot")),
        }
    }

    pub fn add_bot(&self, name: &str, workspace: PathBuf, profile_dir: PathBuf) -> io::Result<u64> {
        match self.request(Request::AddBot { name: name.to_owned(), workspace, profile_dir })? {
            Response::Id(value) => Ok(value),
            _ => Err(io::Error::new(io::ErrorKind::InvalidData, "expected bot ID")),
        }
    }

    pub fn submit_with_mode(&self, bot_id: u64, prompt: &str, mode: AgentMode) -> io::Result<u64> {
        match self.request(Request::Submit { bot_id, prompt: prompt.to_owned(), mode })? {
            Response::Id(value) => Ok(value),
            _ => Err(io::Error::new(io::ErrorKind::InvalidData, "expected task ID")),
        }
    }

    pub fn cancel_task(&self, task_id: u64) -> io::Result<()> {
        match self.request(Request::Cancel { task_id })? {
            Response::Ok => Ok(()),
            _ => Err(io::Error::new(io::ErrorKind::InvalidData, "expected cancellation acknowledgement")),
        }
    }

    pub fn read_log_tail(&self, task_id: u64, stderr: bool) -> io::Result<String> {
        match self.request(Request::Log { task_id, stderr })? {
            Response::Text(value) => Ok(value),
            _ => Err(io::Error::new(io::ErrorKind::InvalidData, "expected log text")),
        }
    }

    pub fn read_worktree_diff(&self, task_id: u64) -> io::Result<String> {
        match self.request(Request::Diff { task_id })? {
            Response::Text(value) => Ok(value),
            _ => Err(io::Error::new(io::ErrorKind::InvalidData, "expected diff text")),
        }
    }

    pub fn shutdown(&self) -> io::Result<()> {
        match self.request(Request::Shutdown)? {
            Response::Ok => Ok(()),
            _ => Err(io::Error::new(io::ErrorKind::InvalidData, "expected shutdown acknowledgement")),
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

    pub fn snapshot(&self) -> io::Result<StudioState> {
        match self {
            Self::Local(value) => value.snapshot(),
            Self::Worker(value) => value.snapshot(),
        }
    }

    pub fn add_bot(&self, name: &str, workspace: PathBuf, profile_dir: PathBuf) -> io::Result<u64> {
        match self {
            Self::Local(value) => value.add_bot(name, workspace, profile_dir),
            Self::Worker(value) => value.add_bot(name, workspace, profile_dir),
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
            Self::Worker(value) => value.status().is_ok_and(|status|
                status.active_task == Some(task_id) && status.cancellation_requested),
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

    pub fn read_worktree_diff(&self, task_id: u64) -> io::Result<String> {
        match self {
            Self::Local(value) => value.read_worktree_diff(task_id),
            Self::Worker(value) => value.read_worktree_diff(task_id),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{AgentRunner, BotRecord, TaskStatus};
    use kojora_runtime::{CancellationToken, RunOutcome};
    use std::sync::Arc;
    use std::thread;

    fn test_root() -> PathBuf {
        let root = std::env::temp_dir().join(format!(
            "kojora-worker-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()
        ));
        fs::create_dir(&root).unwrap();
        fs::set_permissions(&root, fs::Permissions::from_mode(0o700)).unwrap();
        root
    }

    fn fake_runner() -> Arc<AgentRunner> {
        Arc::new(|_bot: &BotRecord, prompt: &str, log_root: &Path, _mode: AgentMode, _token: &CancellationToken| {
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
        })
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
        let bot = client.add_bot("A", root.join("workspace"), root.join("profile")).unwrap();
        let task = client.submit_with_mode(bot, "Describe it", AgentMode::Inspect).unwrap();
        drop(client); // UI can exit without interrupting the running worker.
        let second = WorkerClient::connect(&socket).unwrap();
        for _ in 0..200 {
            if second.snapshot().unwrap().tasks[0].status == TaskStatus::Succeeded {
                break;
            }
            thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(second.snapshot().unwrap().tasks[0].status, TaskStatus::Succeeded);
        assert!(second.read_log_tail(task, false).unwrap().contains("Describe it"));
        assert!(second.cancel_task(task).is_err());
        second.shutdown().unwrap();
        server.join().unwrap().unwrap();
        assert!(!socket.exists());
        fs::remove_dir_all(root).unwrap();
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
        stream.write_all(&(MAX_REQUEST as u32 + 1).to_be_bytes()).unwrap();
        drop(stream);
        WorkerClient::connect(&socket).unwrap().shutdown().unwrap();
        server.join().unwrap().unwrap();
        fs::remove_dir_all(root).unwrap();
    }
}
