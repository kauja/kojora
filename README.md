# Kojora

**An open, lightweight, modular software factory for AI-powered development.**

> **Status: pre-alpha / design and bootstrap stage.** The repository currently
> contains an experimental Rust workspace and a single-command process runner. It does
> **not** yet run managed coding agents, host bots, or provide a GUI.

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
  kojora-agents/    Provider-neutral adapter trait and Codex CLI bootstrap
  kojora-cli/       Minimal command-line scaffold

docs/
  ARCHITECTURE.md   Domain and component boundaries
  ROADMAP.md        Planned milestones
  adr/              Architecture decisions
```

## Build and run the bootstrap CLI

A current stable Rust toolchain is required.

```sh
cargo test --workspace
cargo run -p kojora-cli -- --help
cargo run -p kojora-cli -- doctor
cargo run -p kojora-cli -- run -- git --version
# after a separate private Codex profile has been initialized:
# cargo run -p kojora-cli -- agent codex --profile /absolute/private/profile -- "Summarize this repository"
```

`run` executes a **trusted** command without invoking a shell and saves private logs and JSONL lifecycle events in `.kojora/runs/`. It is **not** a daemon, sandbox or agent adapter. Read [Runner](docs/RUNNER.md) and [Codex adapter](docs/AGENT_ADAPTER.md) for usage, privacy and limitations.

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