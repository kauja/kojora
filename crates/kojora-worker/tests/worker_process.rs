#![cfg(unix)]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "test fixtures and assertions can fail immediately"
)]

//! Exercise the real standalone daemon, not merely an in-process IPC server.
use kojora_studio::worker::WorkerClient;
use kojora_studio::{AgentMode, AgentProvider, TaskStatus};
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

struct WorkerGuard(Child);
impl Drop for WorkerGuard {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[test]
fn worker_help_is_available_without_home() {
    let result = Command::new(env!("CARGO_BIN_EXE_kojora-worker"))
        .arg("--help")
        .env_remove("HOME")
        .output()
        .unwrap();
    assert!(result.status.success());
    let help = String::from_utf8(result.stdout).unwrap();
    assert!(help.contains("--data-dir"));
    assert!(help.contains("--enable-checks"));
}

#[test]
fn worker_process_runs_account_profiles_and_outlives_clients() {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    // Leave space for the socket name under a long macOS temporary path.
    let root: PathBuf = std::env::temp_dir().join(format!("kjp-{}-{now:x}", std::process::id()));
    fs::create_dir(&root).unwrap();
    fs::set_permissions(&root, fs::Permissions::from_mode(0o700)).unwrap();
    let bin_dir = root.join("bin");
    fs::create_dir(&bin_dir).unwrap();
    let provider = bin_dir.join("claude");
    fs::write(
        &provider,
        r#"#!/bin/sh
set -eu
test -z "${ANTHROPIC_API_KEY+x}"
printf '{"type":"assistant","message":{"content":[{"type":"text","text":"%s"}]}}\n' "${CLAUDE_CONFIG_DIR##*/}"
"#,
    )
    .unwrap();
    fs::set_permissions(provider, fs::Permissions::from_mode(0o700)).unwrap();
    let workspace = root.join("workspace");
    fs::create_dir(&workspace).unwrap();

    let binary = env!("CARGO_BIN_EXE_kojora-worker");
    let child = Command::new(binary)
        .args([
            "--data-dir",
            root.to_str().expect("UTF8 test directory"),
            "--address-space-mib",
            "8192",
            "--cpu-seconds",
            "120",
        ])
        .env_remove("HOME")
        .env("PATH", &bin_dir)
        .env("ANTHROPIC_API_KEY", "test-parent-secret")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let mut guard = WorkerGuard(child);

    let socket = root.join("worker.sock");
    let mut client = None;
    for _ in 0..200 {
        match WorkerClient::connect(&socket) {
            Ok(connected) => {
                client = Some(connected);
                break;
            }
            Err(_) => thread::sleep(Duration::from_millis(15)),
        }
    }
    let client = client.expect("standalone worker must accept local connections");
    let first_pid = client.status().unwrap().pid;
    assert_eq!(first_pid, guard.0.id());
    assert_eq!(client.status().unwrap().address_space_mib, Some(8192));
    assert_eq!(client.status().unwrap().cpu_seconds, Some(120));
    assert!(client.snapshot().unwrap().bots.is_empty());

    let mut completed = Vec::new();
    for name in ["claude-personal", "claude-work"] {
        let profile = root.join(name);
        fs::create_dir(&profile).unwrap();
        fs::set_permissions(&profile, fs::Permissions::from_mode(0o700)).unwrap();
        let bot = client
            .add_bot_with_provider(name, workspace.clone(), profile, AgentProvider::ClaudeCode)
            .unwrap();
        let task = client
            .submit_with_mode(bot, "Verify account profile", AgentMode::Inspect)
            .unwrap();
        for _ in 0..200 {
            if client.status().unwrap().active_count == 0 {
                break;
            }
            thread::sleep(Duration::from_millis(15));
        }
        let state = client.snapshot().unwrap();
        let record = state.tasks.iter().find(|record| record.id == task).unwrap();
        assert_eq!(record.status, TaskStatus::Succeeded, "{:?}", state.runs);
        completed.push((task, name));
    }

    // Drop the first GUI-like client; the daemon and task store survive.
    drop(client);
    let second = WorkerClient::connect(&socket).unwrap();
    assert_eq!(second.status().unwrap().pid, first_pid);
    let state = second.snapshot().unwrap();
    assert_eq!(state.bots.len(), 2);
    assert_eq!(state.tasks.len(), 2);
    for (task, name) in completed {
        let output = second.read_log_tail(task, false).unwrap();
        assert_eq!(AgentProvider::ClaudeCode.extract_text(&output), name);
    }
    second.shutdown().unwrap();

    for _ in 0..100 {
        if let Some(status) = guard.0.try_wait().unwrap() {
            assert!(status.success());
            assert!(!socket.exists());
            fs::remove_dir_all(root).unwrap();
            return;
        }
        thread::sleep(Duration::from_millis(15));
    }
    panic!("daemon did not exit after authenticated local shutdown");
}
