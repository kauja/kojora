#![cfg(unix)]
#![allow(clippy::unwrap_used, reason = "test fixtures fail immediately")]

use kojora_agents::AgentProvider;
use std::fs;
use std::os::unix::fs::{symlink, DirBuilderExt, PermissionsExt};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

struct TestRoot(PathBuf);

impl Drop for TestRoot {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

struct HeldRun {
    child: Child,
    release: PathBuf,
}

impl Drop for HeldRun {
    fn drop(&mut self) {
        if matches!(self.child.try_wait(), Ok(None)) {
            let _ = fs::write(&self.release, "");
            let _ = self.child.wait();
        }
    }
}

#[test]
fn codex_cli_excludes_profile_aliases_across_processes_and_preserves_exit_status() {
    let unique = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let path = std::env::temp_dir().join(format!("kojora-codex-{}-{unique}", std::process::id()));
    fs::DirBuilder::new().mode(0o700).create(&path).unwrap();
    let root = TestRoot(path);
    let bin = root.0.join("bin");
    let workspace = root.0.join("workspace");
    let profiles = root.0.join("profiles");
    let first_profile = profiles.join("primary");
    let other_profile = profiles.join("separate");
    for path in [&bin, &workspace, &profiles, &first_profile, &other_profile] {
        fs::DirBuilder::new().mode(0o700).create(path).unwrap();
    }
    let alias = root.0.join("alias");
    symlink(&profiles, &alias).unwrap();

    let fake = bin.join("codex");
    fs::write(
        &fake,
        r#"#!/bin/sh
set -eu
test "$1" = 'exec'
test "$2" = '--json'
test "$3" = '--sandbox'
test "$4" = 'read-only'
test "$CODEX_HOME" = "$HOME"
test -z "${OPENAI_API_KEY+x}"
while test "$1" != '--'; do shift; done
shift
test "$#" -eq 1
if test "$1" = 'hold'; then
    : > "$CODEX_HOME/entered"
    attempts=0
    while test ! -f "$CODEX_HOME/release"; do
        test "$attempts" -lt 300 || exit 2
        attempts=$((attempts+1))
        /bin/sleep 0.02
    done
    printf '%s\n' 'mock provider failure' >&2
    exit 7
fi
printf '{"type":"item.completed","item":{"type":"agent_message","text":"%s"}}\n' "${CODEX_HOME##*/}"
"#,
    )
    .unwrap();
    fs::set_permissions(&fake, fs::Permissions::from_mode(0o700)).unwrap();

    let command = |profile: &std::path::Path, logs: &str, prompt: &str| {
        let mut command = Command::new(env!("CARGO_BIN_EXE_kojora"));
        command
            .args(["agent", "codex", "--profile"])
            .arg(profile)
            .arg("--cwd")
            .arg(&workspace)
            .arg("--log-root")
            .arg(root.0.join(logs))
            .args(["--", prompt])
            .env("PATH", &bin)
            .env("OPENAI_API_KEY", "test-parent-secret");
        command
    };
    let mut first = HeldRun {
        child: command(&first_profile, "first-runs", "hold")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap(),
        release: first_profile.join("release"),
    };
    let entered = first_profile.join("entered");
    for _ in 0..200 {
        if entered.exists() {
            break;
        }
        thread::sleep(Duration::from_millis(10));
    }
    assert!(entered.exists(), "the first provider must hold the lease");
    let second = command(&alias.join("primary"), "duplicate-runs", "duplicate")
        .output()
        .unwrap();
    assert_eq!(second.status.code(), Some(1));
    assert!(!root.0.join("duplicate-runs").exists());

    let separate = command(&other_profile, "other-runs", "other profile")
        .output()
        .unwrap();
    assert!(separate.status.success());
    let other = fs::read_dir(root.0.join("other-runs"))
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    let stdout = fs::read_to_string(other.join("stdout.log")).unwrap();
    assert_eq!(AgentProvider::Codex.extract_text(&stdout), "separate");
    assert!(first.child.try_wait().unwrap().is_none());

    fs::write(&first.release, "").unwrap();
    assert_eq!(first.child.wait().unwrap().code(), Some(7));
    let run = fs::read_dir(root.0.join("first-runs"))
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    assert_eq!(
        fs::read_to_string(run.join("stderr.log")).unwrap(),
        "mock provider failure\n"
    );
    assert!(fs::read_to_string(run.join("events.jsonl"))
        .unwrap()
        .contains("\"exit_code\":7"));
    assert!(command(&first_profile, "later-runs", "lease released")
        .status()
        .unwrap()
        .success());
}
