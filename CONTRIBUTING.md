# Contributing to Breakspan

Humans own product direction, architecture, issue boundaries, acceptance criteria,
review and merge decisions. Coding agents may explore, implement, test, document,
commit and prepare PRs. Agent-generated code is subject to the same review as any
other contribution.

1. Read `AGENTS.md` and `docs/architecture.md`; agree on a narrowly scoped issue.
2. Create an issue branch. Do not work directly on `main` or expand scope without
   explicit approval. Inspect existing patterns before adding abstractions.
3. Make small changes and logical Conventional Commits (e.g.
   `feat(otlp): normalize tool spans`). Explain material dependency decisions.
4. Add observable-behavior tests, including failures, and update relevant docs.
5. Run:
   ```sh
   cargo fmt --check
   cargo clippy --workspace --all-targets --all-features -- -D warnings
   cargo test --workspace
   cargo build --workspace
   ```
6. Self-review the full diff for correctness, error handling, concurrency,
   allocations, architectural boundaries and scope creep.
7. Open a PR referencing the issue. Include scope, validation, dependency decisions
   and known limitations. **The implementing agent must never merge its own PR.**
   Wait for human review and merge approval.

Clippy lints are configured in `[workspace.lints.clippy]` and enforced by the
warnings-denied Clippy run. They forbid `unwrap()`/`expect()`, `dbg!` and `todo!`
outside tests (`clippy.toml` exempts `#[test]` functions and `#[cfg(test)]`
modules), redundant or implicit `Rc`/`Arc` clones, and possibly truncating casts.
Prefer fixing a lint over suppressing it; a suppression needs `reason = "..."`
and the narrowest practical scope. Integration-test crates allow `unwrap_used`
at crate level because Clippy's test exemption does not cover their helpers.

Use stable Rust (see `rust-toolchain.toml`). Commit `Cargo.lock` for reproducible
application builds. Never commit captured prompts, credentials or private traces.
The project is MIT licensed; contributions are made under that license.
