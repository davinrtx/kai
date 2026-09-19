# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.6.0] - 2026-09-18

### Added
- `kai-sandbox`: Canonical path resolution engine (`PathResolver`) strictly enforcing workspace boundary containment and handling non-existent atomic file targets.
- `kai-sandbox`: Sensitive credential and secret shielding blocking access to `.env*`, `*.pem`, `*.key`, `id_rsa*`, `credentials*`, `secrets*`, `.ssh`, and `.aws`.
- `kai-sandbox`: Capability permission matrix (`PermissionMatrix`) supporting `standard`, `strict`, `read_only`, and `permissive` profiles across all `PermissionCategory` variants.
- `kai-sandbox`: Subprocess command security validator (`CommandSanitizer`) blocking destructive command patterns (`rm -rf /`, fork bombs, raw block writes) and interactive STDIN deadlocks.
- `kai-sandbox`: Environment variable scrubbing (`scrub_env`) stripping credentials and tokens from spawned process environments.
- `kai-sandbox`: Integrated policy engine (`StandardSandboxPolicy`) and builder implementing `kai_core::traits::SandboxPolicy`.
- `kai-sandbox`: Exhaustive security integration test suite (`integration_sandbox_security.rs`).

## [0.5.0] - 2026-09-18

### Added
- `kai-session`: In-memory cycle-safe DAG store (`MemorySessionStore`) with DFS cycle detection, bidirectional children indexing, and Git commit hash indexing.
- `kai-session`: Branch pointer coordinator (`BranchManager`) with named branch checkout, forking, multi-parent merge nodes, and atomic turn progression (`append_turn`, `append_to_branch`, `active_history`).
- `kai-session`: Transactional crash-resilient disk persistence (`FileSessionStore`) enforcing atomic sibling replacement (`.tmp.<pid>.<ts>` -> target) and path traversal sanitization.
- `kai-session`: Automatic context compactor (`AutoCompactor`) with turn depth and token budgeting, `ContextProcessor` integration, and multi-parent merge preservation.
- `kai-session`: Comprehensive DAG integration test suite (`integration_session_dag.rs`).

## [0.4.0] - 2026-09-18

### Added
- `kai-orchestrator`: Bounded asynchronous task inbox (`TaskInbox`, `InboxSender`, `InboxReceiver`) backed by `tokio::sync::mpsc` with batch draining and backpressure overflow handling.
- `kai-orchestrator`: Supervised sub-agent fleet manager (`SubAgentDispatcher`) with RAII owned permit throttling (16 simultaneous agents) and re-entrancy prevention.
- `kai-orchestrator`: Event-driven cyclic turn coordinator (`OrchestrationEngine`) with cooperative intra-batch steering checks between tool dispatches.
- `kai-orchestrator`: Persistent daemon supervisor (`DaemonSupervisor`) with heartbeat monitoring and non-blocking shutdown signaling.
- `kai-orchestrator`: Multi-agent orchestration integration test suite (`integration_orchestration_engine.rs`).

## [0.3.0] - 2026-09-18

### Added
- `kai-tools`: Bounded line reader (`read_window`) with strict line limit caps (150 lines) and offset bounds.
- `kai-tools`: Transactional unified diff patcher (`apply_patch`) with atomic replacement via temporary sibling files and automatic rollback on hunk mismatch.
- `kai-tools`: Non-blocking shell command execution (`exec_command`) with cooperative steering cancellation, timeout supervision, and RAII child process reaping on `Drop`.
- `kai-tools`: Headless browser driver (`browser_action`) with mock and headless automation abstractions, URL unquoting, and action validation.
- `kai-tools`: Model Context Protocol (`mcp_client`) client with schema validation, deterministic tool sorting, and stdio transport.
- `kai-tools`: System tools integration test suite (`integration_tools_engine.rs`).

## [0.2.0] - 2026-09-18

### Added
- `kai-context`: AST skeleton extraction for Rust, Python, TypeScript/JavaScript, and Go with body omission (`{ /* omitted */ }` and `...`).
- `kai-context`: Grep-first codebase search with `.gitignore` traversal, search horizon capping, large-file skipping, and cooperative steering cancellation.
- `kai-context`: Bounded streaming window reader (`WindowReader`) capping line buffers at 64 KB and fast raw-byte line counting for multi-million line files.
- `kai-context`: Terminal output scrubber (`TerminalScrubber`) stripping ANSI escape codes, normalizing CRLF/CR, injecting exit codes, and head/tail bounded truncation.
- `kai-context`: `DeterministicContextProcessor` implementing turn-atomic conversational compaction protecting against orphan tool results and enforcing single-system prompt invariants.
- `kai-context`: Exhaustive stress test suite (`stress_context_pipeline.rs`) testing 500 KB single-line containment, 100k line streaming, 50-turn compaction, and adversarial grammars.

### Fixed
- `ast`: Parameter destructuring in TypeScript and Rust signatures no longer prematurely terminates signature extraction.
- `ast`: Single-quoted character literals (`'{'`, `'}'`) and backtick literals no longer corrupt brace depth counters in block analysis.
- `ast`: Python multiline type-annotated signatures no longer terminate on parameter type colons.
- `processor`: Multi-system prompt rejection prevented by concatenating compaction notices directly into existing root system prompt.
- `window`: Guard against opening directories on Unix platforms preventing delayed `EISDIR` buffer errors.

## [0.1.0] - 2026-09-18

### Added
- `kai-core`: Foundational contracts, message schemas (`Message`, `Role`, `ContentBlock`, `ToolCall`, `ToolResult`).
- `kai-core`: Core traits (`Tool`, `Agent`, `ContextProcessor`, `SessionStore`).
- `kai-core`: Event bus primitives with `tokio::sync::broadcast` and global steering cancellation channels.
- `kai-core`: Strongly typed error hierarchies with `thiserror` (`KaiError`, `ContextError`, `ToolError`, `InternalError`, `OrchestratorError`).
- `kai-core`: Output truncation invariants strictly bounding tool outputs at 4 KB or 50 items.
