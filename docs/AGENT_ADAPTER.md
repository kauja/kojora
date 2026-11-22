# Experimental Codex CLI adapter

Kojora now has a provider-neutral internal `AgentAdapter` trait and a
one-shot, **read-only** Codex CLI adapter. It builds a structured command,
then delegates to the same bounded-memory process runner as `kojora run`.

This is a bootstrap. It is **not a daemon, task scheduler, external plugin
ABI, untrusted-code sandbox, or resumable agent session**.

## Requirements

- Rust stable.
- The official `codex` CLI installed in your PATH.
- A user-controlled credential profile already logged in to Codex.
- A separate profile per active agent identity. Concurrent Kojora use of the
  *same* profile is refused with a private exclusive lock file.

## Setup

Initialize a dedicated profile **outside the source workspace**:

```bash
mkdir -p "$HOME/.config/kojora/profiles"
mkdir -m 700 "$HOME/.config/kojora/profiles/codex-primary"
CODEX_HOME="$HOME/.config/kojora/profiles/codex-primary" codex login
```

Do not put credentials inside the repository or a Git worktree. Do not copy
secrets or OAuth files into Kojora's source tree.

## Run

```bash
cargo run -p kojora-cli -- agent codex \
  --profile "$HOME/.config/kojora/profiles/codex-primary" \
  --cwd "$PWD" \
  -- "Summarize the architecture of this Rust workspace"
```

The adapter invokes:

```text
codex exec --json --sandbox read-only --ignore-user-config -- PROMPT
```

The prompt is passed as a literal argument, not shell-evaluated. Agent stdin
is detached (EOF) so unattended runs cannot wait for input from the parent.
It uses
`CODEX_HOME` and `HOME` set to the canonical private profile directory;
the child process also receives `PATH` and optional `LANG`. **All other
parent environment variables are cleared**, including API keys and
`GITHUB_TOKEN`.

The private run directory contains stdout JSONL events, stderr and lifecycle
events. Kojora does not print the prompt or provider credentials in its
`events.jsonl`. Agent-produced logs may still contain sensitive content.

The CLI returns the provider's exit status. A nonzero result is an outcome,
not proof of a failed process launch.

## Security boundaries

- The profile must already exist, be an actual (non-symlink) directory and
  have mode 0700 on Unix.
- A private `.kojora-agent.lock` is acquired using an exclusive file
  creation operation; another Kojora process cannot use that profile at
  the same time.
- A crash can leave a stale lock: **verify the original agent has stopped**
  before removing it manually.
- The workspace and credential profile may not overlap.
- The CLI explicitly requests the provider's read-only sandbox, but that
  does **not** replace OS-level isolation or a trusted workspace policy.
- Project instructions, hooks, provider settings and tool/MCP integrations
  may have their own behaviors. Do not run this against untrusted projects.
- No automatic login, secret copying, token refresh interception, quota
  circumvention or elevated agent permissions are implemented.
- No live Codex credential/integration test is run in GitHub Actions.

## Future work

1. Structured event parsing into typed tasks, runs and sessions.
2. Explicit human-approved edit mode and Git worktree isolation.
3. Provider adapters for Claude Code and remote worker processes.
4. Cancellation, reconciliation after crashes and resource budgets.

References:
- [Codex CLI reference](https://learn.chatgpt.com/docs/cli/reference)
- [Codex non-interactive mode](https://learn.chatgpt.com/docs/non-interactive-mode)
