# Agent adapters and account profiles

Kojora can launch Codex and Claude Code from the CLI, the desktop, or the
local Worker. Each Bot stores a provider and a path to an account profile.
The profile stays on the machine that runs the agent.

| Provider | CLI selector | Profile variable | Modes |
| --- | --- | --- | --- |
| Codex | `codex` | `CODEX_HOME` | Inspection and confirmed worktree edits |
| Claude Code 2.1.248 or later | `claude-code` | `CLAUDE_CONFIG_DIR` | Restricted inspection |

Install the official provider CLI on the Worker's PATH. Use a private,
owner-only directory for each signed-in account, outside every repository
and worktree. For example, initialize two Claude Code accounts separately:

```sh
profiles="$HOME/.config/kojora/profiles"
mkdir -p "$profiles"
mkdir -m 700 "$profiles/claude-personal" "$profiles/claude-work"
CLAUDE_CONFIG_DIR="$profiles/claude-personal" claude auth login
CLAUDE_CONFIG_DIR="$profiles/claude-work" claude auth login
CLAUDE_CONFIG_DIR="$profiles/claude-personal" claude auth status
CLAUDE_CONFIG_DIR="$profiles/claude-work" claude auth status
```

In Desktop, create two Bots with provider **Claude Code**, the desired
workspace, and the two distinct absolute profile paths. Select a Bot to
switch accounts. For the CLI, select the account with `--profile`:

```sh
cargo run --locked -p kojora-cli -- agent claude-code \
  --profile "$profiles/claude-work" --cwd "$PWD" \
  -- "Summarize the architecture of this workspace"
```

For Codex, use a separate profile and sign in with the official CLI:

```sh
mkdir -m 700 "$profiles/codex-primary"
CODEX_HOME="$profiles/codex-primary" codex login
cargo run --locked -p kojora-cli -- agent codex \
  --profile "$profiles/codex-primary" --cwd "$PWD" \
  -- "Summarize the architecture of this workspace"
```

Use a trusted Git checkout for Codex inspection. Kojora records a Codex
workspace rejection in the private run log and propagates its exit status.

## Execution and verification

- A run receives the canonical account directory as `HOME` and as the
  provider's profile variable. Kojora additionally passes `PATH` and
  optional `LANG`. Parent API keys and tokens stay out of the child
  environment. Login and credential storage are handled by the provider CLI.
- The account directory must be a real private directory. The executor
  holds an exclusive lease through process exit and output collection.
  After a crash, remove a stale `.kojora-agent.lock` only after verifying
  that the original agent has stopped.
- The Claude Code adapter uses `--restricted` and allows Read, Glob, and
  Grep. It disables MCP tools, hooks, skills, Chrome integration, and session
  persistence for the run. Use trusted repositories and managed settings.
- The process runner records start, exit, cancellation, and bounded-memory
  output collection. Inspect the private run directory for stderr and raw
  JSONL. Agent output can contain sensitive project information.
- CI exercises both adapters through the real Kojora CLI with test executables.
  It verifies distinct account paths, cross-process exclusion for aliases of
  a single profile, removal of parent secrets, and nonzero exit recording.
  Provider login and Keychain behavior require a signed-in smoke test on
  the target machine.

## Adapter extension points

Implement `AgentAdapter` in `kojora-agents` and register the provider's
inspection and optional edit adapters on `AgentProvider`. Adapters declare
capabilities, construct literal argv, select the profile variable, and
extract user-facing responses from JSON events. The process executor owns
environment clearing and cancellation. The Studio runner is injectable for
tests or embeddings through `Studio::open_with_runner`. The Rust interfaces
are experimental.

[Codex CLI reference](https://developers.openai.com/codex/cli/reference) · [Claude Code CLI reference](https://code.claude.com/docs/en/cli-reference) · [Claude Code authentication](https://code.claude.com/docs/en/authentication)
