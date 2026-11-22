# Security automation (Kojora)

Kojora is a public, pre-alpha Rust repository. The checks here reduce risk,
but they do not make agent execution safe, nor replace authorization and sandboxing.

## Included as code

| System | What runs | Policy |
| --- | --- | --- |
| Rust CI | Rustfmt, unit tests, Clippy | Existing `rust` job; read-only token |
| CodeQL | Rust and GitHub Actions security analysis | PR, push to `main`, weekly |
| Dependency Review | Reviews dependencies newly added to PRs | Fails for newly introduced high/critical vulnerabilities |
| Dependabot | Cargo + GitHub Actions version PRs | Weekly in Asia/Tokyo; small update batches |

Action dependencies are pinned to full commit SHAs to reduce the risk of
unexpected third-party changes. Dependabot tracks action versions. Do not
remove SHA pinning without a review.

These workflows run with intentionally narrow `GITHUB_TOKEN` permissions.
They do **not** expose cloud account tokens or agent credentials.

## Repository settings that require an administrator

In **Settings → Advanced Security** (names may vary):

1. Enable **Dependency graph**.
2. Enable **Dependabot alerts**.
3. Enable **Dependabot security updates**. Version update PRs are configured
   separately by `.github/dependabot.yml`; the file alone does not enable alerts.
4. Confirm **Secret scanning** is enabled for the public repository.
5. Enable and confirm **Push protection** for the repository.
6. Enable **Private vulnerability reporting** so security researchers can
   report issues without publishing details.
7. For **Code scanning**, use **this repository's advanced CodeQL workflow**
   after merging this PR. Do **not** additionally enable CodeQL default setup,
   which could duplicate scans.

Then open **Rules → Rulesets → Protect main** and require these checks after
their first successful appearance:
- `rust` (existing Rust CI)
- `dependency-review` (confirm the observed GitHub status context)
- `Analyze (rust)` and `Analyze (actions)` (confirm contexts before enforcing)

**Do not enable auto-merge solely on successful static checks for changes to
authentication, permissions, the process runner or workflow definitions.**
Human review is recommended for security-sensitive updates.

## Expected limits

- CodeQL and dependency review are available for public GitHub repositories.
- Dependabot may not open Cargo updates until external dependencies are added.
- The dependency graph and vulnerable dependency alerts must be enabled for
  meaningful dependency detection.
- GitHub Secret Scanning and Push Protection recognize supported patterns;
  they do not guarantee detecting every credential.
- Analyzer findings are leads for review, not proof of security.
- Non-interactive AI execution is *not* sandboxed by these workflows.

## Security reports

See [SECURITY.md](../SECURITY.md). When Private vulnerability reporting is
enabled, update that document to link users to the repository's private
reporting entry point.
