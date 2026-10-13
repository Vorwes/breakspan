---
name: rust
description: Implement and review Rust code in Breakspan. Use for domain models, OTLP ingestion, storage, CLI changes and Rust tests.
---

# Breakspan Rust guidance

- Read root AGENTS.md and docs/architecture.md before changing boundaries.
- Use types to make invalid states difficult to represent: IDs have fixed sizes
  and are nonzero; validated timing cannot have an end before its start.
- Keep core dependency-free and transport/framework/runtime independent. OTel,
  tonic, protobuf, SQLite, Ratatui and GPUI types never belong in core.
- Do not unwrap/expect untrusted telemetry. Return explicit errors for recoverable
  failures; invalid siblings must not discard valid spans. `thiserror` is reasonable
  for reusable error types if its benefit justifies the dependency.
- Unknown operations remain ordinary spans. Preserve unknown attributes; content
  recording is optional. Do not guess classifications from names alone.
- Avoid unnecessary clones; owned snapshots are acceptable at query boundaries.
  Do not invent complicated lifetimes for trivial performance savings.
- Do not hold async locks over await or perform blocking work on async workers.
  Keep critical sections short; use spawn_blocking for synchronous output/work.
- Keep public APIs small, prefer pub(crate), and reuse existing patterns before
  adding abstractions. Clarity matters more than iterators versus loops.
- Test end-to-end observable behavior and errors: malformed IDs/timing, partial
  success, missing content, unknown metadata, arrival order and cyclic parents.
- Run fmt, warnings-denied workspace Clippy, workspace tests and build. Keep docs
  synchronized. Never expand issue scope or merge the implementing agent's PR.
