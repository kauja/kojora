# Architecture (draft)

> The architecture is deliberately incomplete. These are boundaries and
> invariants to test during early dogfooding, not a promise of stable APIs.

## Domain concepts

| Concept | Meaning |
| --- | --- |
| Bot | Durable human-facing identity, goals, memory and permission context |
| Conversation | Human/bot interaction history across channels |
| Task | Durable work item with dependencies, acceptance criteria and artifacts |
| Run | One attempt to execute a Task, with events and outcome |
| Session | Agent conversational/execution context; may outlive a process |
| Worker | Process or isolated execution environment on a specific Host |
| Artifact | Diff, commit, PR, logs, test result, benchmark or build output |

A Bot is not a Session; a Session is not a process. A Bot may coordinate
multiple concurrent Runs, each with separate Sessions and Workers.

## Logical layers

Bot channels (native GUI, CLI, future web/chat connectors)
-> API / Event Stream
-> Domain Services + State Store
-> Policy Engine + Resource Controller
-> Scheduler and Admission Controller
-> Execution / Tool / Validation Ports
-> Local and Remote Workers + Agent adapters

## Core invariants

1. Trust boundaries and resource admission are enforced outside model prompts.
2. The GUI is never required for a task to continue.
3. Policy plugins cannot bypass central authorization or runtime quotas.
4. Every Run is auditable and tied to its Task, Bot, Worker and artifacts.
5. A crashed or upgraded worker must not silently orphan task ownership.
6. Candidate Factory builds never overwrite the stable controlling runtime.

## Local MVP feature map

| Feature | How to reach it | Implementation and evidence |
| --- | --- | --- |
| Inspect with a chosen account | Create a Bot in Desktop or run `kojora agent` with `--profile` | `kojora-agents` owns the adapter contract and profile lease. The CLI integration test launches a fixture with two profiles and checks the selected account and cleared credentials. |
| Start a task from Desktop | Create a Bot, write a prompt, and select Send task | `kojora-studio` owns task admission and durable Run state. The standalone Worker test submits tasks to two accounts over the private socket and checks their separate results after reconnecting. |
| Review a Codex edit | Enable edits, confirm the task, then select Worktree diff in the Run inspector | Studio saves the worktree before launching the agent. A Git fixture checks the saved worktree, bounded diff and original checkout. |
| Stop or inspect a running task | Use the task's Stop button and Run inspector | `kojora-runtime` records process outcomes. Cancellation and bounded log tests exercise the local process runner. |
| Queue local checks | Enable check presets in Worker and open the edit Run inspector | The check queue and admission tests cover exclusive execution and post-edit deduplication. |

## Dependency and verification rules

The Desktop uses Studio for Bot and Task operations. The CLI and Studio use
`AgentAdapter` to construct provider commands. The runtime owns provider
process lifecycle and environment handling. Provider support, including optional edit
capability, is checked before admitting a Task.

Run `./scripts/verify` locally and in CI. The script rejects unapproved crate
dependencies and records tests and strict lint results. Keep permission and
resource constraints in executable code. Use one provider launch path for
local and Worker modes. Remove obsolete implementations and add a regression
to the user-facing entry point where a failure appeared. The Rust workspace
forbids `unsafe` code. Use safe APIs for OS operations.
[Lauren Tan on verification](https://www.youtube.com/watch?v=NjoZoUm85x0&t=510s) · [Lauren Tan on Dune](https://www.youtube.com/watch?v=NjoZoUm85x0&t=1514s)

## Plugin strategy (proposed)

- **Built-ins:** fast, trusted in-process Rust implementations.
- **WASM Components:** versioned WIT contracts for untrusted or portable logic.
- **External processes:** language-agnostic IPC for heavyweight runtimes,
  isolation boundaries and special-purpose integrations.
- **Skills:** instructions/workflows for LLM behavior, separate from executable
  capabilities and centrally enforced authorization.

The Rust traits in kojora-core are *internal experiments*, not the external
plugin ABI. Versioning and conformance tests are required before publishing an
SDK. Hot swapping must use explicit draining and state handoff.

## Resource-aware execution (proposed)

- Observe process-tree memory, peak usage, CPU, IO and load/pressure signals.
- Record a predicted peak resource cost for each Run, not only average use.
- Reserve headroom for editors, system workloads and build spikes.
- Reject or queue new work before starting an overcommitted process.
- Prefer resuming an Agent at explicit safe boundaries rather than sending
  termination signals during tool execution.
- Reuse MCP-like services only when their transport, state and credentials
  permit safe sharing; otherwise isolate them per session or principal.

## Self-hosted development

Use a **stable Factory** to launch tests for a **candidate Factory** in an
isolated environment. Collect benchmark comparisons, regression checks and
explicit promotion approval. Retain rollback artifacts. No candidate may
mutate the production controller in-place.
