# Kojora Worker Daemon (local Unix MVP)

Start the independent Rust headless Worker in one terminal; connect the GUI
in another. Closing the GUI does not stop the Worker or its active task.
Worker shutdown is explicitly rejected while the task is active.

## Quickstart

```sh
mkdir -p "$HOME/.local/state/kojora" && chmod 700 "$HOME/.local/state/kojora"
cargo run --release -p kojora-worker -- --data-dir "$HOME/.local/state/kojora" --address-space-mib 8192 --cpu-seconds 900
cargo run --release -p kojora-desktop -- --worker-socket "$HOME/.local/state/kojora/worker.sock"
cargo run -p kojora-worker -- --data-dir "$HOME/.local/state/kojora" --status
cargo run -p kojora-worker -- --data-dir "$HOME/.local/state/kojora" --stop
```

The first command runs continuously (use a second terminal for the GUI).
Codex CLI must be installed and its private profile initialized separately.
The example uses an explicit directory on both Linux/macOS; on macOS
the legacy standalone GUI default is a different state directory.

The Worker defaults to one active task. Use distinct account profiles and
set `--max-parallel 2 --memory-reservation-mib 512
--total-memory-reservation-mib 1024` with an explicit address-space ceiling
to allow two tasks. The Desktop displays the available task slots. Select
a running task to stop that specific run.

## API and security boundary

- Only a private owner-only Unix-domain socket in a 0700 Studio directory.
- The socket uses mode 0600; the client verifies socket and parent owner,
  type, and permissions before connecting. No TCP port or remote access.
- Build the Worker and Desktop from the same revision so their protocol
  versions match.
- Protocol v3 includes the Bot provider. It uses length-prefixed JSON frames
  with a 64-KiB request cap and bounded replies.
- RPC operations: status, snapshot, Bot creation, Task submission, Task
  cancellation, bounded stdout/stderr logs, bounded worktree diff,
  named local-check enqueue/status/log queries, shutdown.
- IPC callers are trusted processes running as the **same OS user**.
  This is not a security barrier between applications sharing that user.
- The Worker holds the state lock; the GUI must not open that same state
  directory in local mode at the same time.
- A stale worker.sock after a crash must be deleted manually **only after**
  verifying that the previous Worker is not running.

## Resource ceiling semantics

| Flag | Kernel mechanism | Enforces | Does not enforce |
| --- | --- | --- | --- |
| `--address-space-mib N` | Unix RLIMIT_AS | Virtual memory address space for each child | Physical RSS, group or host total |
| `--cpu-seconds N` | Unix RLIMIT_CPU | CPU time of each child | Elapsed wall time, aggregate CPU across children |
| `--max-parallel N` and memory reservations | Studio admission | Active task count and configured reservation budget | Operating system memory use or other host processes |

The numeric limits are optional; without them the Worker does not apply
process-specific memory/CPU ceilings. RLIMIT_AS can prevent a provider from
starting if it reserves large virtual address ranges even at low RSS.
Test values on the intended host before enabling them in production.

## Centrally controlled local checks

Operator CLI flags `--enable-checks`, `--post-edit-check rustfmt`,
`--post-edit-check clippy`, `--check-timeout-seconds 300` and
`--max-pending-checks 16` configure a private, persisted serial queue.
Check clients can request only named built-in presets for existing detached
task worktrees. See [Central Local Checks](LOCAL_CHECKS.md).

The check queue and agent scheduler now **share an exclusive execution
gate**: checks wait while agents are running, and new agents are refused
while a check is executing. This is conservative backpressure, not an
aggregate RSS/CPU quota, cgroup containment or fair queue scheduling.


## Known limitations

- The Worker outlives the GUI, **not** its own crash or machine reboot.
- No waiting task queue, remote TLS/mTLS, automatic
  service installation, or full process-tree RSS/CPU pressure telemetry.
- Abnormally terminated Worker processes can leave orphan Codex processes.
  Startup records incomplete tasks as Interrupted, but cannot kill orphans.
- Explicit user stop via Worker RPC invokes existing task-ID cancellation.
- No provider credentials are used by CI tests.

## Validation

`cargo test --workspace` includes a credential-free integration test that
spawns the actual Worker binary, disconnects/reconnects a client, verifies
configuration, and then cleanly shuts down the Worker.
