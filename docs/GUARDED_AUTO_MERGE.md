# Guarded auto-merge

Kojora merges a narrow, trusted subset of PRs without requiring someone to
click GitHub's Enable auto-merge button for every pull request.

## How it works

The workflow runs:
- whenever Rust CI, CodeQL or Dependency Review completes;
- every 15 minutes (CodeRabbit's final review is an external check);
- on manual workflow dispatch for testing.

It does not enable GitHub's per-PR auto-merge flag. When the branch ruleset
and all current required checks are satisfied, it attempts a squash merge.
The merge command is given the exact current head SHA to reject races.

## Eligible pull requests

All these conditions are required:

1. The PR author is the repository owner (currently kauja).
2. Its head branch belongs to the same repository; forks are excluded.
3. It targets main, is open, and is not a draft.
4. It has neither a [manual] or [no-automerge] title marker nor a
   manual-review or no-automerge label.
5. Every changed filename is available and no sensitive file is modified:
   .github/, .cargo/, scripts/, the runtime executor crate, Cargo
   manifests and lockfiles, build scripts, AGENTS.md, CODEOWNERS, license,
   credentials, authentication, permissions, policies, security or sandbox code.
6. GitHub reports a clean, mergeable PR with required checks satisfied.
7. The head SHA has not changed since the file paths were inspected.

Dependabot, external contributors and sensitive changes remain manual.

## Threat model and limitations

The workflow runs from the default branch, has a scoped GITHUB_TOKEN with
contents:write and pull-requests:write, and never checks out or executes PR
code. It does not read PR artifacts or use a personal access token.

Filename filtering is conservative, not a guarantee that code is harmless.
Keep branch rulesets and all required checks enforced; in particular, add
CodeQL and Dependency Review as required checks after their introduction.

A merge may be delayed until the next scheduled run. If CodeRabbit requires
manual review under its small-public-repository limits, this action does NOT
bypass that check. A PR can still remain blocked pending CodeRabbit review.

GitHub may suppress push-triggered GitHub Actions when the merge is performed
using the repository GITHUB_TOKEN. Make mandatory checks run on PRs rather than
depending on push-to-main CI alone.

Kojora PR #1 (executor) and #3 (security automation) intentionally require
human review and manual merge.
