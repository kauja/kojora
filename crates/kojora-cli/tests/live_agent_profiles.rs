#![cfg(unix)]

use kojora_agents::{run_agent_cancellable, AgentProvider, AgentRequest};
use kojora_runtime::{CancellationToken, LocalExecutor};
use std::env;
use std::fs;
use std::io::{self, Read};
use std::os::unix::fs::DirBuilderExt;
use std::path::PathBuf;
use std::process::Command;
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

struct SmokeRoot(PathBuf);

impl Drop for SmokeRoot {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

#[test]
#[ignore = "requires an explicitly selected, signed-in provider profile and live service access"]
fn signed_in_provider_reads_an_isolated_fixture() -> io::Result<()> {
    let profile = env::var_os("KOJORA_LIVE_SMOKE_PROFILE")
        .map(PathBuf::from)
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "Set KOJORA_LIVE_SMOKE_PROFILE to the absolute path of a signed-in test profile",
            )
        })?;
    let provider: AgentProvider = env::var("KOJORA_LIVE_SMOKE_PROVIDER")
        .unwrap_or_else(|_| "codex".into())
        .parse()
        .map_err(io::Error::other)?;

    let unique = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(io::Error::other)?
        .as_nanos();
    let root = env::temp_dir().join(format!("kojora-live-smoke-{}-{unique}", std::process::id()));
    fs::DirBuilder::new().mode(0o700).create(&root)?;
    let fixture = SmokeRoot(root);
    let workspace = fixture.0.join("workspace");
    fs::create_dir(&workspace)?;
    let git = Command::new("git")
        .args(["init", "--quiet", "--template="])
        .current_dir(&workspace)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .status()?;
    if !git.success() {
        return Err(io::Error::other(
            "Could not initialize the smoke-test checkout",
        ));
    }
    let marker = format!("kojora-smoke-{unique}");
    fs::write(workspace.join("smoke-marker.txt"), &marker)?;

    let cancellation = CancellationToken::default();
    let stopper = cancellation.clone();
    let (done, completion) = mpsc::channel::<()>();
    let watchdog = thread::spawn(move || {
        if matches!(
            completion.recv_timeout(Duration::from_secs(90)),
            Err(mpsc::RecvTimeoutError::Timeout)
        ) {
            stopper.cancel();
        }
    });
    let outcome = run_agent_cancellable(
        provider.inspect_adapter(),
        &LocalExecutor,
        &AgentRequest {
            prompt:
                "Read smoke-marker.txt in the current workspace and reply with its contents only.",
            workspace: &workspace,
            profile_dir: &profile,
            log_root: &fixture.0.join("runs"),
        },
        &cancellation,
    );
    drop(done);
    watchdog
        .join()
        .map_err(|_| io::Error::other("Smoke test watchdog stopped"))?;
    let outcome = outcome?;
    assert!(
        !outcome.cancelled,
        "Provider did not finish within the smoke test deadline"
    );
    assert!(
        outcome.success,
        "{} exited with {:?}",
        provider.label(),
        outcome.exit_code
    );
    let mut events = Vec::new();
    fs::File::open(outcome.run_dir.join("stdout.log"))?
        .take(256 * 1024)
        .read_to_end(&mut events)?;
    assert!(
        provider
            .extract_text(&String::from_utf8_lossy(&events))
            .contains(&marker),
        "Provider response did not contain the fixture marker"
    );
    Ok(())
}
