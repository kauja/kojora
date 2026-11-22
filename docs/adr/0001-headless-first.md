# ADR 0001 — Headless-first core with a bot-facing UI

- Status: Proposed
- Date: 2026-11-22

## Context

Kojora must work on a local workstation and a remote server while managing
multiple AI agent sessions without requiring the GUI to remain open.

## Decision (proposed)

The execution controller will be a headless service. GUI, CLI, web and chat
adapters will be clients of a shared API/event model. Bot identity, session
state and the OS process lifecycle will be distinct concepts.

## Consequences

- The GUI can reconnect after disconnects and render historical events.
- Auth secrets and tool execution remain with trusted worker boundaries.
- Network API security, reconnect semantics and state migrations require
  explicit design.
- Desktop automation requires a graphical host/session and cannot be assumed
  to work in a generic Linux daemon.
