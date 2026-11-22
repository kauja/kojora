//! Provider-neutral contract for one local, non-interactive coding-agent run.
//!
//! This bootstrap adapter is NOT a persistent session manager or a sandbox.
//! Its public Rust traits are still experimental (not the future plugin ABI).

use kojora_runtime::{CancellationToken, IsolatedExecutor, RunOutcome, RunSpec};
use std::env;
use std::ffi::OsString;
use std::fs::{self, File, OpenOptions};
use std::io;
use std::path::{Path, PathBuf};

#[cfg(unix)]
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};

/// The capabilities of the *adapter*, not the enclosing runtime.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AgentCapabilities {
    pub jsonl_events: bool,
    pub resumable: bool,
    pub read_only: bool,
}

/// Non-interactive provider command construction. No shell evaluation.
pub trait AgentAdapter {
    fn provider(&self) -> &'static str;
    fn executable(&self) -> OsString;
    fn arguments(&self, prompt: &str) -> Vec<OsString>;
    fn profile_variable(&self) -> &'static str;
    fn capabilities(&self) -> AgentCapabilities;
}

/// One-shot Codex CLI agent in explicitly read-only mode.
#[derive(Debug, Default)]
pub struct CodexExecAdapter;

impl AgentAdapter for CodexExecAdapter {
    fn provider(&self) -> &'static str {
        "codex"
    }

    fn executable(&self) -> OsString {
        OsString::from("codex")
    }

    fn arguments(&self, prompt: &str) -> Vec<OsString> {
        [
            "exec",
            "--json",
            "--sandbox",
            "read-only",
            "--ignore-user-config",
            "--",
            prompt,
        ]
        .into_iter()
        .map(OsString::from)
        .collect()
    }

    fn profile_variable(&self) -> &'static str {
        "CODEX_HOME"
    }

    fn capabilities(&self) -> AgentCapabilities {
        AgentCapabilities {
            jsonl_events: true,
            resumable: false,
            read_only: true,
        }
    }
}

/// Explicitly opt-in to Codex workspace-write mode. Only use this with an
/// isolated Git worktree; the adapter itself does not enforce that boundary.
#[derive(Debug, Default)]
pub struct CodexWorkspaceWriteAdapter;

impl AgentAdapter for CodexWorkspaceWriteAdapter {
    fn provider(&self) -> &'static str {
        "codex"
    }

    fn executable(&self) -> OsString {
        OsString::from("codex")
    }

    fn arguments(&self, prompt: &str) -> Vec<OsString> {
        [
            "exec",
            "--json",
            "--sandbox",
            "workspace-write",
            "--ignore-user-config",
            "--",
            prompt,
        ]
        .into_iter()
        .map(OsString::from)
        .collect()
    }

    fn profile_variable(&self) -> &'static str {
        "CODEX_HOME"
    }

    fn capabilities(&self) -> AgentCapabilities {
        AgentCapabilities {
            jsonl_events: true,
            resumable: false,
            read_only: false,
        }
    }
}

/// A single launch. Authentication already lives in profile_dir, not in Kojora.
pub struct AgentRequest<'a> {
    pub prompt: &'a str,
    pub workspace: &'a Path,
    pub profile_dir: &'a Path,
    pub log_root: &'a Path,
}

/// Runs an adapter with an environment cleared by the trusted executor.
/// One profile may be used by only one local Kojora run at a time.
pub fn run_agent<A: AgentAdapter, E: IsolatedExecutor>(
    adapter: &A,
    executor: &E,
    request: &AgentRequest<'_>,
) -> io::Result<RunOutcome> {
    run_agent_inner(adapter, executor, request, None)
}

/// A cancellation-aware agent run. The profile lease remains held until
/// the subprocess has exited and all output has been drained.
pub fn run_agent_cancellable<A: AgentAdapter, E: IsolatedExecutor>(
    adapter: &A,
    executor: &E,
    request: &AgentRequest<'_>,
    token: &CancellationToken,
) -> io::Result<RunOutcome> {
    run_agent_inner(adapter, executor, request, Some(token))
}

fn run_agent_inner<A: AgentAdapter, E: IsolatedExecutor>(
    adapter: &A,
    executor: &E,
    request: &AgentRequest<'_>,
    cancellation: Option<&CancellationToken>,
) -> io::Result<RunOutcome> {
    if request.prompt.trim().is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "agent prompt must not be blank",
        ));
    }

    if cancellation.is_some_and(CancellationToken::is_cancelled) {
        return Err(io::Error::new(
            io::ErrorKind::Interrupted,
            "task cancelled before launch",
        ));
    }

    let workspace = fs::canonicalize(request.workspace)?;
    if !workspace.is_dir() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "workspace must be a directory",
        ));
    }

    let profile = verify_private_profile(request.profile_dir)?;
    if workspace.starts_with(&profile) || profile.starts_with(&workspace) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "credential profile and agent workspace must not overlap",
        ));
    }

    // Exclusive create provides a simple fail-closed per-profile admission lock.
    // A crash may leave the lock stale; the caller must verify no agent is
    // still running before removing a stale lock.
    let _lease = ProfileLease::acquire(&profile)?;

    // Never inherit OAuth tokens, API keys, cloud credentials, or provider
    // settings from Kojora's parent process.
    let mut child_environment = vec![
        (OsString::from("HOME"), profile.as_os_str().to_os_string()),
        (
            OsString::from("PATH"),
            env::var_os("PATH").unwrap_or_else(|| OsString::from("/usr/bin:/bin")),
        ),
        (
            OsString::from(adapter.profile_variable()),
            profile.as_os_str().to_os_string(),
        ),
    ];
    if let Some(lang) = env::var_os("LANG") {
        child_environment.push((OsString::from("LANG"), lang));
    }

    let spec = RunSpec {
        program: adapter.executable(),
        args: adapter.arguments(request.prompt),
        cwd: workspace,
        log_root: request.log_root.to_owned(),
    };

    if let Some(token) = cancellation {
        if token.is_cancelled() {
            return Err(io::Error::new(
                io::ErrorKind::Interrupted,
                "task cancelled before launch",
            ));
        }
        executor.run_isolated_cancellable(&spec, &child_environment, token)
    } else {
        executor.run_isolated(&spec, &child_environment)
    }
}

fn verify_private_profile(path: &Path) -> io::Result<PathBuf> {
    if !path.is_absolute() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "credential profile path must be absolute",
        ));
    }
    let metadata = fs::symlink_metadata(path)?;
    if !metadata.file_type().is_dir() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "credential profile must be a real directory, not a symlink",
        ));
    }
    #[cfg(unix)]
    if metadata.permissions().mode() & 0o077 != 0 {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "credential profile must be private (mode 0700)",
        ));
    }
    fs::canonicalize(path)
}

struct ProfileLease {
    path: PathBuf,
    _lock: File,
}

impl ProfileLease {
    fn acquire(profile: &Path) -> io::Result<Self> {
        let path = profile.join(".kojora-agent.lock");
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        options.mode(0o600);
        let lock = options.open(&path)?;
        Ok(Self { path, _lock: lock })
    }
}

impl Drop for ProfileLease {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::sync::Mutex;
    use std::time::{Duration, SystemTime, UNIX_EPOCH};

    fn test_profile(name: &str) -> PathBuf {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        let root =
            env::temp_dir().join(format!("kojora-agent-{name}-{now}-{}", std::process::id()));
        fs::create_dir(&root).expect("create private profile directory");
        fs::set_permissions(&root, fs::Permissions::from_mode(0o700))
            .expect("set private profile permissions");
        root
    }

    type RecordedCall = (RunSpec, Vec<(OsString, OsString)>);

    #[derive(Default)]
    struct RecordingExecutor {
        seen: Mutex<Option<RecordedCall>>,
    }

    impl IsolatedExecutor for RecordingExecutor {
        fn run_isolated(
            &self,
            spec: &RunSpec,
            environment: &[(OsString, OsString)],
        ) -> io::Result<RunOutcome> {
            *self.seen.lock().expect("mutex") = Some((spec.clone(), environment.to_vec()));
            Ok(RunOutcome {
                run_dir: spec.log_root.join("fake"),
                exit_code: Some(7),
                success: false,
                cancelled: false,
                duration: Duration::ZERO,
                stdout_bytes: 0,
                stderr_bytes: 0,
            })
        }
    }

    #[test]
    fn codex_uses_read_only_jsonl_mode() {
        let adapter = CodexExecAdapter;
        assert_eq!(adapter.provider(), "codex");
        assert_eq!(adapter.executable(), OsString::from("codex"));
        assert_eq!(adapter.profile_variable(), "CODEX_HOME");
        assert_eq!(
            adapter.arguments("-prompt with spaces"),
            [
                "exec",
                "--json",
                "--sandbox",
                "read-only",
                "--ignore-user-config",
                "--",
                "-prompt with spaces",
            ]
            .into_iter()
            .map(OsString::from)
            .collect::<Vec<_>>()
        );
        assert_eq!(
            adapter.capabilities(),
            AgentCapabilities {
                jsonl_events: true,
                resumable: false,
                read_only: true
            }
        );
    }

    #[test]
    fn codex_write_mode_is_explicit_and_separate() {
        let adapter = CodexWorkspaceWriteAdapter;
        assert_eq!(
            adapter.arguments("make a patch"),
            [
                "exec",
                "--json",
                "--sandbox",
                "workspace-write",
                "--ignore-user-config",
                "--",
                "make a patch",
            ]
            .into_iter()
            .map(OsString::from)
            .collect::<Vec<_>>()
        );
        assert!(!adapter.capabilities().read_only);
        assert_eq!(adapter.profile_variable(), "CODEX_HOME");
    }

    #[test]
    fn does_not_inherit_secrets_and_keeps_nonzero_result() {
        let profile = test_profile("env");
        let workspace = env::current_dir().unwrap();
        let executor = RecordingExecutor::default();
        let outcome = run_agent(
            &CodexExecAdapter,
            &executor,
            &AgentRequest {
                prompt: "Inspect the Rust workspace",
                workspace: &workspace,
                profile_dir: &profile,
                log_root: &workspace.join(".kojora").join("runs"),
            },
        )
        .unwrap();
        assert!(!outcome.success);
        assert_eq!(outcome.exit_code, Some(7));
        let call = executor.seen.lock().unwrap();
        let (spec, variables) = call.as_ref().expect("one invocation");
        assert_eq!(spec.program, OsString::from("codex"));
        assert_eq!(spec.cwd, workspace);
        assert!(variables
            .iter()
            .any(|(name, value)| name == "CODEX_HOME" && value == profile.as_os_str()));
        assert!(!variables.iter().any(|(name, _)| name == "GITHUB_TOKEN"
            || name == "OPENAI_API_KEY"
            || name == "ANTHROPIC_API_KEY"));
        assert!(!profile.join(".kojora-agent.lock").exists());
        drop(call);
        fs::remove_dir_all(profile).unwrap();
    }

    #[test]
    fn prevents_parallel_same_profile_use() {
        let profile = test_profile("lease");
        let first = ProfileLease::acquire(&profile).expect("first");
        assert_eq!(
            ProfileLease::acquire(&profile).err().unwrap().kind(),
            io::ErrorKind::AlreadyExists
        );
        drop(first);
        assert!(ProfileLease::acquire(&profile).is_ok());
        fs::remove_dir_all(profile).unwrap();
    }

    #[test]
    fn refuses_public_or_symlinked_profile() {
        let profile = test_profile("permissions");
        fs::set_permissions(&profile, fs::Permissions::from_mode(0o755)).unwrap();
        assert_eq!(
            verify_private_profile(&profile).unwrap_err().kind(),
            io::ErrorKind::PermissionDenied
        );
        fs::set_permissions(&profile, fs::Permissions::from_mode(0o700)).unwrap();
        let alias = profile.with_extension("alias");
        std::os::unix::fs::symlink(&profile, &alias).unwrap();
        assert_eq!(
            verify_private_profile(&alias).unwrap_err().kind(),
            io::ErrorKind::InvalidInput
        );
        fs::remove_file(alias).unwrap();
        fs::remove_dir_all(profile).unwrap();
    }

    #[test]
    fn refuses_overlapping_paths() {
        let profile = test_profile("overlap");
        let executor = RecordingExecutor::default();
        let error = run_agent(
            &CodexExecAdapter,
            &executor,
            &AgentRequest {
                prompt: "hello",
                workspace: &profile,
                profile_dir: &profile,
                log_root: &profile.join("runs"),
            },
        )
        .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
        fs::remove_dir_all(profile).unwrap();
    }
}
