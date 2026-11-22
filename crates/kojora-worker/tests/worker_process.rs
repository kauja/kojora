#![cfg(unix)]

//! Exercise the real standalone daemon, not merely an in-process IPC server.
use kojora_studio::worker::WorkerClient;
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
fn worker_process_outlives_clients_and_shuts_down_cleanly() {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let root: PathBuf = std::env::temp_dir().join(format!(
        "kojora-worker-process-{}-{now}", std::process::id()
    ));
    fs::create_dir(&root).unwrap();
    fs::set_permissions(&root, fs::Permissions::from_mode(0o700)).unwrap();

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

    // Drop the first GUI-like client; the daemon and task store survive.
    drop(client);
    let second = WorkerClient::connect(&socket).unwrap();
    assert_eq!(second.status().unwrap().pid, first_pid);
    assert!(second.snapshot().unwrap().bots.is_empty());
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
