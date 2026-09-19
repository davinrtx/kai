# Development Roadmap: KAI (Krill Agent Interface)

Implementation milestones and release versions for the KAI autonomous agent runtime in Rust, ordered by architectural dependency.

---

## v0.1.0 - Foundation & Core Contracts (Phase 1: `crates/core`)

Establish foundational interfaces, message schemas, and typed error systems. All downstream crates depend on these contracts.

* **Target Crate:** `kai-core`
* **Status:** Completed
* **Deliverables:**
  * Base message structs: `Message`, `Role`, `ContentBlock`, `ToolCall`, `ToolResult`.
  * Core traits: `Tool`, `Agent`, `ContextProcessor`, `SessionStore`.
  * Event bus primitives using `tokio::sync::broadcast`.
  * Strongly-typed error hierarchies via `thiserror`.
* **Acceptance Criteria:**
  * Compiles with zero dependencies on concrete downstream implementations.
  * 100% of public traits, structs, and error variants documented with `///`.
  * Unit test harness covering message serialization and deserialization.

---

## v0.2.0 - Deterministic Context Pipeline (Phase 2: `crates/context`)

Implement token reduction engines relying strictly on native C/Rust parsing routines.

* **Target Crate:** `kai-context`
* **Status:** Completed
* **Deliverables:**
  * AST skeleton extractor via `tree-sitter` (Rust, Python, TypeScript/JavaScript grammars).
  * Grep-First search engine wrapping the `ignore` and `regex` crates.
  * Terminal output scrubber (Exit-Code aware: discard stdout on 0, extract error traces on non-zero).
  * Git state compactors (`git status --short`, `git diff --stat`).
* **Acceptance Criteria:**
  * AST extractor strips function bodies while preserving signatures.
  * Scrubber clips output to error boundaries without loading full logs into memory.
  * Offline tests verifying token savings against fixture repositories.

---

## v0.3.0 - System Tool Engine (Phase 3: `crates/tools`)

Build the execution tooling layer adhering to operational bounds.

* **Target Crate:** `kai-tools`
* **Status:** Completed
* **Deliverables:**
  * `read_window`: Bounded line reader with strict `offset`/`limit` caps (150-line limit).
  * `apply_patch`: Transactional unified diff patcher with automatic rollback on failure.
  * `exec_command`: Non-blocking subprocess execution with RAII cleanup and output truncation (4 KB / 50 items).
  * `browser_action`: Headless browser driver (DOM navigation, console capture, screenshots).
  * `mcp_client`: Model Context Protocol client for external tool invocation.
* **Acceptance Criteria:**
  * Transactional file writes guarantee atomic replacement without in-place corruption.
  * All external child processes terminate deterministically on `Drop`.
  * Unit tests pass offline using mock subprocesses and filesystem fixtures.

---

## v0.4.0 - Multi-Agent Orchestration (Phase 4: `crates/orchestrator`)

Implement concurrent task dispatching, ephemeral sub-agents, and background services.

* **Target Crate:** `kai-orchestrator`
* **Status:** Completed
* **Deliverables:**
  * Concurrent Task Inbox built on `tokio::sync::mpsc`.
  * Ephemeral sub-agent manager with isolated, zero-state context lifecycles.
  * Real-time steering channel for in-flight cancellation and guidance.
  * Background daemon service with periodic heartbeat checks and webhook dispatches.
* **Acceptance Criteria:**
  * Ephemeral sub-agents report structured outcomes and drop memory immediately upon completion.
  * Steering signal interrupts running tools without causing async deadlocks or orphan tasks.
  * Multi-agent tests run deterministically with mock drivers.

---

## v0.5.0 - Branchable Session Graph (Phase 5: `crates/session`)

Implement branchable conversational memory and automated context management.

* **Target Crate:** `kai-session`
* **Status:** Completed
* **Deliverables:**
  * Directed Acyclic Graph (DAG) session tree supporting checkpoints and parallel branches.
  * State serializer and disk persistence engine.
  * Out-of-band threshold auto-compactor (>80% context window threshold).
  * Git commit synchronization for graph nodes.
* **Acceptance Criteria:**
  * Ability to fork and switch branches without data loss.
  * Compaction condenses older node sequences into structured summary entries.
  * Unit tests validating DAG integrity, cycle prevention, and branch rewinds.

---

## v0.6.0 - Production Sandbox & Security Matrix (Phase 6: `crates/sandbox`)

Enforce security policies, boundary confinement, and credential defense.

* **Target Crate:** `kai-sandbox`
* **Status:** Completed
* **Deliverables:**
  * Canonical path resolver preventing directory traversal beyond project roots.
  * Granular permission matrix (`standard`, `strict`, `read_only`, `permissive`).
  * Destructive shell command detection and sensitive environment variable scrubbing.
* **Acceptance Criteria:**
  * Path confinement blocks `../` traversal attempts across all tools.
  * Sensitive files (.env, .pem, private keys, credentials) are blocked at the sandbox perimeter.
  * Full workspace satisfies Definition of Done: zero clippy warnings, clean formatting, all tests passing.