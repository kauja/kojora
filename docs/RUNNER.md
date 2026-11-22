# Experimental local runner

The `kojora run` command is a **bootstrap process primitive**, not yet an agent adapter or a headless daemon. It executes one program in the foreground and writes a per-run record. The code lives in `kojora-runtime` and exposes an internal `Executor` trait.

## Example

```sh
cargo run -p kojora-cli -- run -- git --version
cargo run -p kojora-cli -- run --cwd . -- /usr/bin/env
cargo run -p kojora-cli -- run --log-root /path/to/private/runs -- git status
```

No shell is invoked by Kojora; arguments after `--` are passed verbatim. An explicit invocation of `sh -c` does launch a shell, and the caller is responsible for the script.

The command inherits stdin and the invoking user's environment; stdout/stderr are drained in bounded-size chunks to files. Kojora waits for the process to exit and returns its exit status (or 1 if signaled/failed). Output is **not echoed live**; inspect the log files while running.

By default records live in `<working-directory>/.kojora/runs/<run-id>/`. A relative `--log-root` resolves relative to the invoking process, not `--cwd`.

- `events.jsonl`: schema 1, append-only lifecycle events (`started`, `finished`, `spawn_failed`).
- `stdout.log`: binary stdout.
- `stderr.log`: binary stderr.

On Unix the log root must be private (mode 0700). Per-run directories use mode 0700 and files mode 0600. Existing roots with broader permissions are rejected. `.kojora/` is gitignored.

## Security and limitations

**Execute only trusted commands.** This is not a sandbox. There are no resource quotas, process-tree metrics, API authentication, PTY, timeout, cancellation protocol, daemon reconnect, or session resume. Children inherit the parent process's permissions and environment. Their logs may contain credentials or sensitive data; never publish these without review.

A crash can leave a `started` event without a corresponding `finished` event. The runner does not guarantee that a process survives its parent or that these records can be used to resume work. Runs are not LLM session transcripts.

## Next steps

1. Design a provider-neutral agent adapter, profile isolation, and explicit permission boundaries.
2. Implement durable run identity and reconciliation after restarts.
3. Add a headless controller API and resource-aware admission before concurrency or untrusted agents.
