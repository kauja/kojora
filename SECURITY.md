# Security policy

Kojora is pre-alpha and not suitable for production workloads or untrusted
agent execution. No security support or response SLA is currently offered.

## Please do not

- Commit API keys, tokens, passwords, private SSH keys or agent login state.
- Expose a future control API to the public internet without authentication.
- Grant broad filesystem or network access to third-party plugins by default.
- Assume that an LLM prompt is an enforceable security policy.

## Reporting

Until a private reporting channel is configured, avoid publishing detailed
exploit instructions or secrets in public GitHub issues. Contact the
maintainer privately through an available trusted channel.