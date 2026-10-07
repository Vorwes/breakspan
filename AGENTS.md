# Breakspan agent instructions

Breakspan is a local debugger for agentic AI systems: find where failed behavior
begins, not merely visualize telemetry. It ingests existing harness telemetry;
it must never own the agent loop or call an LLM to implement this milestone.

## Ownership and boundaries
- `breakspan-core`: framework-independent domain model and execution-tree logic.
  No OTel, protobuf, tonic, SQLite, Ratatui, GPUI or async-runtime types here.
- `breakspan-otlp`: OTLP/gRPC, external-input validation, GenAI interpretation.
  Raw transport/OTel types stop here.
- `breakspan-store`: persistence/query ownership; currently bounded memory only.
- `breakspan-cli`: command parsing and wiring, not domain business logic.
- `breakspan-tui`: placeholder; future frontend consumes core/store APIs.

## Every task
- Inspect existing code/patterns before introducing abstractions. Prefer minimal,
  concrete implementations and small reviewable diffs.
- Work on an issue branch, never directly on `main`. Never broaden a GitHub issue
  without explicit approval. Stop and reassess unexpected scope growth.
- Justify new dependencies, check upstream maintenance and compatibility, avoid
  overlapping libraries. Do not add future-feature abstractions speculatively.
- Treat telemetry as untrusted. Never `unwrap()`/`expect()` it; propagate recoverable
  errors. Keep public APIs minimal (`pub(crate)` where possible).
- Avoid needless cloning and elaborate lifetimes. Never hold locks across `.await`
  without justification; put blocking I/O/work off async workers.
- Test observable behavior, including failure paths and out-of-order input. Do not
  weaken valid tests to accommodate broken implementation.
- Update docs when behavior, boundaries or architecture changes. See
  `docs/architecture.md` and `.agents/skills/rust/SKILL.md` for details.

## Validation / definition of done
```sh
cargo fmt --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace
cargo build --workspace
```
Self-review the complete diff for correctness, error handling, concurrency,
allocations, boundary violations and scope creep. Tests/docs/CI must match the
implementation. Use logical Conventional Commits; prepare a PR referencing the
issue. No generated implementation enters `main` without human review.
**Never merge your own PR.** The human owner controls scope, architecture and merge.
