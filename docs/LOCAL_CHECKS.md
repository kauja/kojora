# Kojora central local checks (experimental)

Kojora's Worker serializes resource-limited local lint/test requests in one
durable queue. AI agents can request **named, pre-approved** check presets;
they cannot define arbitrary executable paths, argv, shell scripts or
repository Git hook commands through the Worker API.

## Enable manually on a trusted repository

```sh
mkdir -p "$HOME/.local/state/kojora"
chmod 700 "$HOME/.local/state/kojora"
cargo run --release -p kojora-worker -- \
  --data-dir "$HOME/.local/state/kojora" \
  --address-space-mib 8192 --cpu-seconds 900 \
  --enable-checks \
  --post-edit-check rustfmt \
  --post-edit-check clippy \
  --check-timeout-seconds 300 \
  --max-pending-checks 16
```

The `post_edit` hook automatically queues configured checks after a
successful isolated-edit Task. The same Task/Hook/Preset combination is
deduplicated across GUI reconnections and Worker restarts. Manual and
`pre_merge` requests share a matching queued or running check and start a
fresh check after completion. Clients explicitly trigger `pre_merge` against
the current worktree. The Worker observes completed edits before accepting a
shutdown request and waits for pending checks to finish.
If a full queue delays an automatic check, the Worker asks for a shutdown
retry so it can schedule the remaining check first.
The check journal reserves space for pending checks to finish. If a result
has an oversized diagnostic, it saves the terminal status with a short notice.

## Allowlisted checks

| Preset | Exact executable and arguments | Behavior |
|---|---|---|
| rustfmt | `cargo fmt --all -- --check` | Check formatting, no automatic edits |
| clippy | `cargo clippy --workspace --all-targets -- -D warnings` | May execute build scripts/proc macros |
| test | `cargo test --workspace` | Executes project tests |
| ruff | `ruff check .` | Python lint |
| biome | `biome check .` | JS/TS lint/format checking |

The Worker requires check feature opt-in and explicit OS limits on CPU time
and *virtual address space*. It refuses arbitrary target directories: only
the registered, detached Task Worktree may be inspected. A single check
subprocess runs at once, with bounded pending jobs (default 16),
wall-clock timeout (default 300 seconds), private stdout/stderr logs
and atomic check status journaling (`checks/checks.json`). On Worker restart,
queued checks resume when their recipe is still enabled, their original
task worktree can be verified, and startup finds no unfinished agent or
check that had already started. The journal marks unsafe or unverifiable
recovery entries Interrupted, including legacy jobs without a saved
worktree identity.
The Worker also picks up successful edits that were saved just before a
restart and had yet to be queued. It resumes the recipes selected for the
original run and still enabled at startup, with the same worktree identity
and the same recovery conditions as queued checks.
On Unix, a queued check also revalidates its worktree before launch and
records a failure if the directory was moved or replaced.

## Security and performance limits

**Not suitable for untrusted repositories.** Cargo test, plugins, build
scripts and code-quality tools may execute code from the repository. A
cleared environment and per-process `RLIMIT_AS`/`RLIMIT_CPU` limits do
not constitute an OS sandbox. `RLIMIT_AS` does not bound actual physical
RSS or total process-tree memory. Disk space is not quota-enforced, and
checks and agents now share an **exclusive execution gate**. A check
waits until agents finish, then blocks new agent starts until it completes.
This prevents concurrent compiler/agent resource spikes, but is deliberately
conservative: the memory reservation estimates still do not enforce RSS.
Linux cgroups v2 and process-tree RSS/CPU tracking remain future work. Run only against projects you trust.

The GUI's Run inspector lets you queue Rustfmt, Clippy and tests and
inspect each check's bounded stdout/stderr tail.
