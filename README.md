# Kojora

**An open, lightweight, modular software factory for AI-powered development.**

> **Status: pre-alpha / local Studio MVP.** The repository now contains a
> native GUI, durable Bot/Task/Run records, and one-shot Codex and Claude Code
> execution. It is not yet an unattended autonomous coding platform.

[日本語 README](README.ja.md) · [Architecture](docs/ARCHITECTURE.md) · [Roadmap](docs/ROADMAP.md)

## Vision

Kojora is intended to orchestrate AI coding agents across local machines and
remote servers, while keeping resource use observable and predictable. Humans
interact with long-lived **Bots**, and can drill down into each Task, Run,
Session, Worker and Artifact when needed.

The software factory should ultimately be able to **develop and improve
itself** through reproducible tests, profiling and human-reviewed promotion.

## Design principles

- **Headless first:** one runtime model for desktop and server.
- **Bot first, session transparent:** conversations are the human-facing view;
  internal execution remains inspectable.
- **Resource-aware:** measure, budget, schedule and isolate CPU, memory, I/O and
  tool-server usage.
- **Composable:** stable contracts for planners, schedulers, executors, agents,
  tools, validators, storage and UI.
- **Least privilege:** credentials stay with trusted workers; plugins receive
  only explicit capabilities.
- **Reproducible self-improvement:** production and candidate runtimes are
  isolated; changes are benchmarked and reviewed before promotion.

## Project layout

```text
crates/
  kojora-core/      Experimental core types and scheduler policy
  kojora-runtime/   Experimental single-command process runner
  kojora-agents/    Pluggable Codex and Claude Code adapters
  kojora-cli/       Headless process/agent CLI
  kojora-studio/    Durable Bot/Task/Run controller (no GUI deps)
  kojora-desktop/   Native egui desktop application

docs/
  ARCHITECTURE.md   Domain and component boundaries
  ROADMAP.md        Planned milestones
  adr/              Architecture decisions
```

## Build and run the bootstrap CLI

Rustup selects the toolchain pinned in `rust-toolchain.toml`.

```sh
cargo test --workspace
cargo run -p kojora-cli -- --help
cargo run -p kojora-cli -- doctor
cargo run -p kojora-cli -- run -- git --version
# after a separate private Codex profile has been initialized:
# cargo run -p kojora-cli -- agent codex --profile /absolute/private/profile -- "Summarize this repository"
```

`run` executes a **trusted** command without invoking a shell and saves private logs and JSONL lifecycle events in `.kojora/runs/`. It is **not** a daemon, sandbox or agent adapter. Read [Runner](docs/RUNNER.md) and [Agent adapters and account profiles](docs/AGENT_ADAPTER.md) for usage, privacy and limitations.

## Native Studio MVP (experimental)

The desktop is written in Rust and runs locally on macOS or Linux. It shows
Bot conversations, Task/Run/Session IDs, run status, bounded output, and
a read-only prompt composer for Codex and Claude Code. Confirmed Codex edits
run in a detached Git worktree with a bounded diff viewer.
There is no automatic commit or push.

Choose a provider and the absolute account-profile path when creating a Bot.
Create one Bot per account to switch Claude Code profiles. See the
[account setup guide](docs/AGENT_ADAPTER.md) for both providers.

To initialize a private Codex profile outside the project:

```sh
mkdir -p "$HOME/.config/kojora/profiles"
mkdir -m 700 "$HOME/.config/kojora/profiles/codex-primary"
CODEX_HOME="$HOME/.config/kojora/profiles/codex-primary" codex login
```

Launch the GUI:

```sh
cargo run --release -p kojora-desktop
```

Create a Bot with absolute workspace and profile paths. Metadata and logs
are saved to a private local state directory. The GUI never waits on the
rendering thread, and the headless CLI still works without compiling egui.
Use **Stop task** for explicit cancellation: on macOS/Linux it terminates
the agent's process group and records a durable `Cancelled` status.

Read [Studio MVP](docs/STUDIO_MVP.md) for security boundaries and limitations.

## Persistent local Worker Daemon (experimental)

Run a separate headless Worker to keep managing tasks after the GUI closes.
Both use one owner-private Unix socket; only the Worker holds the state lock.

```sh
mkdir -p "$HOME/.local/state/kojora" && chmod 700 "$HOME/.local/state/kojora"
# Terminal 1: Worker process with explicit CPU and virtual-memory ceilings
cargo run --release -p kojora-worker -- --data-dir "$HOME/.local/state/kojora" --cpu-seconds 900
# Terminal 2: GUI client (can disconnect and reconnect)
cargo run --release -p kojora-desktop -- --worker-socket "$HOME/.local/state/kojora/worker.sock"
# Terminal 3: status and clean shutdown (refused while a task is active)
cargo run -p kojora-worker -- --data-dir "$HOME/.local/state/kojora" --status
cargo run -p kojora-worker -- --data-dir "$HOME/.local/state/kojora" --stop
```

**Resource semantics:** CPU seconds and virtual address space are *per process*,
not wall time, physical RSS or the sum of descendants. Limits are optional;
the Worker defaults to one active task. See [Worker Daemon](docs/WORKER.md).

## Central local checks and hooks (experimental)

Kojora's Worker can serialize trusted lint and test checks in a bounded,
durable queue. Check recipes are operator-configured fixed presets, **not
arbitrary shell commands** supplied by AI agents.

```sh
cargo run --release -p kojora-worker -- \
  --data-dir "$HOME/.local/state/kojora" \
  --address-space-mib 8192 --cpu-seconds 900 \
  --enable-checks --post-edit-check rustfmt --post-edit-check clippy \
  --check-timeout-seconds 300 --max-pending-checks 16
```

Successful isolated edit tasks trigger the configured `post_edit` checks
once. Use the GUI Worktree inspector to queue manual checks and view results.
This is **only for trusted repositories**: Cargo build scripts and tests can
execute project-controlled code. Read [Central Local Checks](docs/LOCAL_CHECKS.md).


## Open design questions

- Versioned plugin contract: in-process Rust, WASM components and/or IPC
- Resource admission, preemption and safe session suspension
- Authentication profile isolation and compatible agent adapters
- Durable event model and replayable execution history
- macOS accessibility and independently controlled graphical worker sessions

See [the roadmap](docs/ROADMAP.md) before proposing implementations.

## Security

Do not commit API keys, session credentials, OAuth tokens or personal data.
See [SECURITY.md](SECURITY.md). Agent-executed processes will require explicit
permissions and isolation before being made available to users.

## Contributing

The architecture is intentionally experimental. Contributions, design reviews
and reproducible performance measurements are welcome. See
[CONTRIBUTING.md](CONTRIBUTING.md).

## License

[MIT](LICENSE).
