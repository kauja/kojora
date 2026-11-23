#![cfg(unix)]
#![allow(clippy::unwrap_used, reason = "test fixtures fail immediately")]

use std::fs;
use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
use std::path::PathBuf;
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

struct TestRoot(PathBuf);

impl Drop for TestRoot {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

#[test]
fn claude_cli_uses_only_the_selected_profile() {
    let unique = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let path = std::env::temp_dir().join(format!("kojora-claude-{}-{unique}", std::process::id()));
    fs::DirBuilder::new().mode(0o700).create(&path).unwrap();
    let root = TestRoot(path);
    let bin = root.0.join("bin");
    let workspace = root.0.join("workspace");
    fs::create_dir(&bin).unwrap();
    fs::create_dir(&workspace).unwrap();
    let fake = bin.join("claude");
    fs::write(
        &fake,
        r#"#!/bin/sh
set -eu
test "$1" = '--print'
test "$2" = '--restricted'
test -z "${ANTHROPIC_API_KEY+x}${CODEX_HOME+x}"
test "$CLAUDE_CONFIG_DIR" = "$HOME"
while test "$1" != '--'; do shift; done
shift
test "$#" -eq 1
test "$1" = '--literal prompt'
IFS= read -r profile < "$CLAUDE_CONFIG_DIR/test-marker"
printf '{"type":"assistant","message":{"content":[{"type":"text","text":"%s"}]}}\n' "$profile"
"#,
    )
    .unwrap();
    fs::set_permissions(&fake, fs::Permissions::from_mode(0o700)).unwrap();

    for marker in ["personal", "work"] {
        let profile = root.0.join(marker);
        fs::DirBuilder::new().mode(0o700).create(&profile).unwrap();
        fs::write(profile.join("test-marker"), format!("{marker}\n")).unwrap();
        let logs = root.0.join(format!("runs-{marker}"));
        let outcome = Command::new(env!("CARGO_BIN_EXE_kojora"))
            .args(["agent", "claude-code", "--profile"])
            .arg(&profile)
            .arg("--cwd")
            .arg(&workspace)
            .arg("--log-root")
            .arg(&logs)
            .args(["--", "--literal prompt"])
            .env("PATH", &bin)
            .env("ANTHROPIC_API_KEY", "test-parent-secret")
            .env("CODEX_HOME", "test-parent-profile")
            .output()
            .unwrap();
        assert!(
            outcome.status.success(),
            "{}",
            String::from_utf8_lossy(&outcome.stderr)
        );
        let run = fs::read_dir(logs).unwrap().next().unwrap().unwrap();
        let stdout = fs::read_to_string(run.path().join("stdout.log")).unwrap();
        assert_eq!(
            kojora_agents::AgentProvider::ClaudeCode.extract_text(&stdout),
            marker
        );
        assert!(!profile.join(".kojora-agent.lock").exists());
    }
}
