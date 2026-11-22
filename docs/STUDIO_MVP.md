# Kojora Studio MVP

Kojora Studio is a local, experimental native Rust desktop GUI powered by a
standalone headless Rust state-and-control crate. This is a complete minimal
read-only Codex run/view loop, not an unattended autonomous development system.

## First run

1. Install Rust stable and the official Codex CLI.
2. Prepare a separate private authenticated Codex profile outside your project.
3. Launch the desktop and create a Bot.
4. Send a task; inspect output, run IDs, session IDs, task state and logs.
5. Restart the app; historical tasks and log metadata remain visible.

Initialize the profile using the official CLI:

```sh
mkdir -p "$HOME/.config/kojora/profiles"
mkdir -m 700 "$HOME/.config/kojora/profiles/codex-primary"
CODEX_HOME="$HOME/.config/kojora/profiles/codex-primary" codex login
```

Start the native desktop:

```sh
cargo run --release -p kojora-desktop
```

Or specify an absolute custom local data directory:

```sh
cargo run --release -p kojora-desktop -- --data-dir /absolute/private/state/dir
```

Default state path:
- macOS: `~/Library/Application Support/Kojora`
- Linux: `${XDG_STATE_HOME}/kojora` or `~/.local/state/kojora`

The GUI supports creating/selecting named Bots with absolute workspace and
profile paths, one-shot Codex tasks, a conversation-like view of responses
with a persistent task list, status display, and Run/Session details with
bounded stdout/stderr and raw JSONL inspection.

## Persistence and resource boundaries

The GUI uses the headless `kojora-studio` crate. The state file contains
prompts and metadata, **not provider credentials**. It is atomically
replaced and uses Unix mode 0600 inside a mode 0700 directory. An OS file
lock guards against multiple Studio instances using the same data directory.

The GUI only loads the last 32 KiB of each selected log stream. Logs are
stored beneath `runs/task-ID/RUN-DIR` in the private data directory. Output
may contain source code, personal content or credentials emitted by Codex;
never commit or share your state directory.

The controller enforces **one active Codex process at a time**, launching on
a background thread rather than blocking the graphical event loop. The
Codex adapter runs with a cleared environment and a private credential
profile, and requests a read-only sandbox.

The headless `kojora agent codex` CLI still works independently of the GUI.
Desktop dependencies do not enter the runtime, agent adapter or Studio
controller. Default workspace builds exclude the GUI.

## Explicit limitations

- This is not yet a remote worker or daemon; closing the GUI during a running
  task can interrupt tracking and may leave the child process running.
  Do not close Studio until the agent finishes.
- The startup recovery flag marks unfinished tasks `Interrupted`, but
  does not locate or kill orphan processes.
- There is no process cancellation, multi-agent scheduling, Git worktree
  isolation, permissioned code edits, autonomous PR creation, integrated
  terminal, or resource telemetry.
- Every task creates one new Run/Session ID, but underlying Codex conversations
  are **not resumed** across prompts. Chat-like display is only a local
  history of independent one-shot tasks.
- Japanese text uses an OS-installed CJK font when available. No fonts are
  bundled. If glyphs render as boxes, set
  `KOJORA_GUI_FONT=/absolute/path/to/NotoSansCJK-Regular.ttc`.
- The GUI does not handle account sign-in. Initialize a separate private
  profile with the official `codex login` command before using the Bot.
- After a crash, a profile may contain a stale `.kojora-agent.lock`.
  Symptoms include a new run failing with "file already exists". First
  verify **no Codex process is still using that profile**. Only then remove
  the stale lock, for example:

  ```sh
  rm "$HOME/.config/kojora/profiles/codex-primary/.kojora-agent.lock"
  ```

  Never remove this lock while a task is still running.
- Studio's own instance lock is shared with its worker, even if the GUI
  controller is dropped. This prevents another Studio from opening the same
  state while a previous worker is still executing.
- New task and bot metadata is persisted before appearing in the shared
  in-memory state. A failed write before rename does not leave phantom
  records. A failure to sync the directory after the rename is logged as a
  durability warning; the new state is still published.
- Do not use untrusted workspaces. Neither the prompt nor the GUI is an OS
  security boundary.
- GitHub CI compiles and tests the GUI on Linux, smoke-launches it using a
  virtual X11 display, and checks a macOS Apple Silicon build. Manual GUI
  interaction and credentialed Codex integration tests remain outstanding.

## Architecture

```text
kojora-desktop (native egui/eframe UI)
    |
    v
kojora-studio (GUI-free state, coordinator, log viewer)
    |
    v
kojora-agents (Codex provider and private profile)
    |
    v
kojora-runtime (bounded-memory headless process runner)
```

## Developer verification

```sh
cargo fmt --all -- --check
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo run --release -p kojora-desktop
```
