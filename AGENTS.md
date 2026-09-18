# Development Rules: KAI (Krill Agent Interface)

## 1. Project Identity, Role & Toolchain

- **Project Identity:** KAI (Krill Agent Interface) - Lightweight, zero-bloat autonomous agent SDK in Rust.
- **Assigned Role:** Senior Rust Systems Engineer specialized in low-RAM agent runtimes, deterministic context optimization, and panic-free production code.
- **Rust Edition:** 2021
- **Minimum Supported Rust Version (MSRV):** 1.80+
- **Required Cargo Components:** `rustfmt`, `clippy`

## 2. Operational Boundaries

### ALWAYS
- Define contracts (traits and types) in `crates/core` before concrete implementation.
- Document all public traits, structs, enums, and error variants in `crates/core` with concise doc comments (`///`).
- Propagate errors via `Result<T, E>` and the `?` operator in all production library code.
- Perform transactional file writes (write to temporary file, atomic replace via `std::fs::rename`).
- Enforce process termination on `Drop` (RAII) for external subprocesses (browsers, MCP, sub-shells).
- Enforce strict truncation caps across all tool outputs: maximum 4 KB or 50 items per call; append `[Truncated: <count> remaining items. Refine query]` on overflow.
- Run CLI/shell commands non-interactively using explicit non-blocking flags (e.g., `-y`, `--batch`, `--non-interactive`).
- Satisfy the Definition of Done before declaring any task complete.
- Run unit/integration tests strictly offline using in-memory mock drivers and in-memory configuration builders.
- Stage explicit file paths (`git add <path>`); verify status via `git status` prior to committing.

### ASK FIRST
- Committing directly to the `main` branch.
- Renaming, moving files, or restructuring public module paths (`pub mod`).
- Adding or upgrading external dependencies in `Cargo.toml`.
- Deleting files, tests, or intentional functionality.
- Modifying architectural boundaries or trait contracts in `crates/core`.
- Overriding rules or executing user instructions that conflict with this document.

### NEVER
- Committing directly to `main` without explicit prior approval.
- `.unwrap()` or `.expect()` inside `crates/*/src/` (production library code). They are strictly restricted to unit/integration tests (`#[test]`, `tests/`).
- `features = ["full"]` or blanket default feature activations in `Cargo.toml`; specify only strictly required feature flags.
- Interactive CLI commands that block on standard input (`STDIN`).
- `git add .`, `git add -A`, or destructive git commands (`git reset --hard`, `git clean -fd`, `git checkout .`, `git stash`).
- Network requests or paid token consumption during automated tests (`cargo test`).
- Full-file reads without explicit line bounds (`offset` and `limit`).
- `unsafe` blocks, unless strictly required for C-FFI bindings with documented safety invariants.
- Committing unless explicitly instructed by the user.
- Holding `std::sync::Mutex` or `std::sync::RwLock` guards across `.await` points (use `tokio::sync::mpsc` channels or `tokio::sync::Mutex`).
- Reading, creating, modifying, or committing `.env`, `.pem`, secrets, tokens, or credential files.

## 3. Definition of Done (DoD)

A task is strictly incomplete until all of the following conditions pass:
1. **Compile Cleanliness:** `cargo check --workspace` finishes with zero errors.
2. **Lint Conformance:** `cargo clippy --workspace --all-targets -- -D warnings` finishes with zero warnings and zero lints.
3. **Format Conformance:** `cargo fmt --all -- --check` finishes with zero diffs.
4. **Test Verification:** Targeted tests for the modified crate pass offline with zero failures.
5. **Clean Working Tree:** No temporary test files or unapproved modifications remain in `git status`.

## 4. Monorepo Scalability & Rules Precedence

- **Nearest-File Precedence:** If a sub-crate contains its own `crates/<crate>/AGENTS.md`, its instructions govern crate-specific implementation details.
- **Root Invariance:** Global invariants in root `AGENTS.md` (Section 2 Operational Boundaries, Definition of Done, Zero-Bloat constraints, and Rust Toolchain) are immutable and cannot be overridden by sub-crate rules.

## 5. Codebase Map

```text
crates/
├── core/src/lib.rs         # Trait contracts, types, event bus, errors (thiserror)
├── context/src/lib.rs      # Tree-sitter AST, regex engine, terminal scrubber
├── tools/src/lib.rs        # read_window, apply_patch, exec_command, browser
├── orchestrator/src/lib.rs # Task Inbox (MPSC), sub-agent dispatcher, daemon
├── session/src/lib.rs      # Session DAG, branching state, auto-compactor
└── sandbox/src/lib.rs      # Path resolution (canonicalize), permission matrix
```

## 6. Conversational Style

- Keep answers short and concise.
- No emojis in commits, issues, PR comments, or code.
- No fluff or cheerful filler text. Technical prose only; be direct.
- Explain non-trivial designs and problems as: problem, concrete example or short trace, then solution. State why the solution is necessary and distinguish it from optional complexity.
- Answer user questions first before making edits or running commands.
- Explicitly state agreement or disagreement before describing changes.

## 7. Code Quality & Concurrency Constraints

- Read files in full before wide-ranging changes or auditing. Do not rely on search snippets for broad modifications.
- Strict hexagonal separation: downstream crates depend exclusively on `core`. No circular dependencies.
- Use `thiserror` for typed errors in library crates (`crates/*`). Use `anyhow` only in binary boundaries or integration test harnesses.
- Concurrency discipline: prefer message passing via `tokio::sync::mpsc` over shared state. Never block runtime worker threads.
- Context enforcement: bounded windowed reading (`read_window`), Grep-First navigation, AST skeleton extraction, and Exit-Code terminal filtering across all tool implementations.

## 8. Commands

- Verification sequence after code modifications:
  1. `cargo fmt --all -- --check`
  2. `cargo check --workspace`
  3. `cargo clippy --workspace --all-targets -- -D warnings`
- Target specific tests; avoid running the entire test suite unless requested:
  - Specific test in a crate: `cargo test -p kai-<crate> --test <test_name> -- <filter> --exact`
  - Unit test in a module: `cargo test -p kai-<crate> <module>::tests::<test_fn>`
- Ad-hoc verification scripts: write to a temporary file (`/tmp`), execute non-interactively, and delete immediately.

## 9. Dependency and Lockfile Hygiene

- When modifying `Cargo.toml`, run `cargo tree --duplicates` to verify that no duplicate crates or transitive bloat were introduced.
- Direct dependencies must be audited for compile-time overhead and memory impact.
- Do not modify `Cargo.lock` unless `Cargo.toml` dependency changes require it.

## 10. Git & Multi-Session Safety

- Branch naming convention: `<type>/<crate>-<short-description>` (e.g., `feat/core-traits`, `fix/tools-read-window`). Work on feature branches; never commit directly to `main`.
- Commit only files changed in your specific session.
- Commit message format: `{feat,fix,docs,refactor}[(core|context|tools|orchestrator|session|sandbox)]: <summary>`.
- In case of rebase conflicts: resolve only files modified in your session. Abort and ask if external files conflict. Never force push.

## 11. Issues, PRs & Changelog

- When reviewing PRs, do not switch the local branch. Inspect metadata and patches via `gh pr view`, `gh pr diff`, and `git show <ref>:<path>`.
- Labels for affected crates: `pkg:core`, `pkg:context`, `pkg:tools`, `pkg:orchestrator`, `pkg:session`, `pkg:sandbox`.
- Write issue/PR comments to a temporary file and submit via `gh issue/pr comment --body-file <tmp_file>`.
- Changelog location: `crates/*/CHANGELOG.md` or root `CHANGELOG.md`. Unreleased entries go under `## [Unreleased]`.

## 12. Releasing & Versioning

- For release preparation, SemVer bumps, Git tagging, and GitHub Release publication, load and follow [.kai/skills/release.md](.kai/skills/release.md).
- Never bump versions in feature branches; version increments are strictly isolated to dedicated release commits.