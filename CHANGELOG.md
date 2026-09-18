# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

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
