# Roadmap (living document)

Milestones are capability targets, not release-date promises.

## Bootstrap (current)

- [x] Public-facing project vision and basic architecture sketch
- [x] Rust workspace, minimal CLI, deterministic scheduling example
- [x] Experimental local process runner with private logs and lifecycle events (not an agent adapter)
- [ ] Confirm plugin boundaries with an ADR and conformance tests
- [ ] Decide durable events, IDs and storage migration strategy

## M1 — Headless single-host runtime

- [x] Bootstrap Codex CLI adapter (one-shot read-only, no live credential test)
- [ ] Validate a real provider CLI end-to-end and define permissions beyond read-only
- [x] Local cancellable process execution with logged Unix process-group shutdown (experimental)
- [ ] Crash-safe independent worker supervision and orphan reconciliation
- [x] Local durable Bot/Task/Run/Session metadata and interrupted-run markers (experimental)
- [ ] True resumable sessions and robust crash/process reconciliation
- [ ] Isolate provider configuration/authentication profiles
- [ ] Expose a versioned local control API and structured event stream

## M2 — Resource-aware orchestration

- [ ] Host process-tree memory, CPU and pressure telemetry
- [ ] Admission control, budgets, queues and audit logs
- [ ] Multiple agent sessions and Git worktree isolation
- [ ] Safe pause/resume where agent adapters support it
- [ ] Real memory, CPU and completion-time benchmarks

## M3 — Bot-first desktop experience

- [x] Native Rust Bot-first Studio GUI (local read-only Codex MVP)
- [x] Task → Run → Session identifiers with bounded live stdout/stderr viewer
- [x] Opt-in isolated Git worktree edit runs and bounded diff preview (experimental)
- [x] Native GUI stop action with Task-ID matching and persisted Cancelled status
- [ ] Integrated terminal and artifact browsing
- [ ] Remote Linux worker control from local macOS desktop
- [ ] macOS GUI automation as a separately permissioned worker plugin

## M4 — Plugin ecosystem

- [ ] Versioned contracts for scheduler, executor, validator, storage and tools
- [ ] Rust/WASM/IPC extension strategy with conformance suites
- [ ] Fine-grained plugin permissions and reliable installation/removal
- [ ] Bundled optional skills for planning, testing and performance

## M5 — Verified self-improvement

- [ ] Automated reproducible performance regression detection
- [ ] Issue -> proposal -> isolated implementation -> benchmark -> PR
- [ ] Stable/candidate Factory separation with human-approved promotion
- [ ] Publish benchmark methodology and results in CI
