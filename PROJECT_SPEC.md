# PROJECT SPECIFICATION: AGENT CORE SDK

Modular Rust SDK for building autonomous software development agents with multi-agent orchestration, ultra-low RAM footprint, and deterministic token optimization for local and remote inference.

---

## 1. Workspace Modules & Crates

* **`core`**: Async runtime contracts, core traits, message schemas, agent lifecycle, event bus, and live interruption channel (`steering`).
* **`context`**: Deterministic code pipeline (Tree-sitter AST, regex engine, terminal output scrubbers, Git compactors).
* **`tools`**: Extensible registry of native tools, headless browser engine, and MCP client.
* **`orchestrator`**: Multi-agent dispatcher, concurrent Task Inbox, ephemeral sub-agent manager, and 24/7 background daemon (`heartbeats` / cron).
* **`session`**: Directed Acyclic Graph (DAG) state engine, branchable session history, state serialization, and compaction.
* **`sandbox`**: Canonical path resolver, subprocess execution controller, and granular permission matrix.

---

## 2. Context Optimization Pipeline

* **Windowed Reading:** Paged file reading via `offset` and `limit` (strict cap: 150 lines per call). Full-file reading without explicit bounds is strictly disallowed.
* **Grep-First Navigation:** Mandatory regex pattern search before opening files to locate exact lines and symbols.
* **AST Skeletons:** Static extraction of interfaces, classes, signatures, and types via Tree-sitter, stripping implementation bodies.
* **Exit-Code Terminal Scrubbing:**
  * `Exit Code 0`: Return execution time and success confirmation; discard raw `stdout`.
  * `Exit Code != 0`: Extract and return only matched error traces (`panic`, `error:`, `exception`); flush full raw log to disk.
* **Git Native Compaction:** Repository inspection using `git status --short` and `git diff --stat` instead of raw diffs for status checks.
* **KV-Cache Prefix Invariance:** Immutable ordering of system prompt and static tool schemas to ensure high prefix-cache hit rates.
* **Deferred Tools:** Lazy-load tool schemas into the context window on demand based on query keywords.

---

## 3. Multi-Agent Orchestration & Services

* **Agent Supervisor:** Central workflow coordinator and task priority manager.
* **Ephemeral Sub-Agents:** Isolated async processes with clean, zero-state context windows for targeted sub-tasks. Context is destroyed upon completion.
* **Task Inbox:** Concurrent MPSC queue receiving sub-agent outputs, human approvals, and event notifications.
* **Steering Channel:** In-flight cancellation and real-time guidance mechanism without restarting the session.
* **24/7 Daemon & Heartbeats:** Background service executing scheduled tasks (cron) and dispatching proactive alerts via messaging adapters or webhooks.

---

## 4. Session State & Memory Management

* **DAG-Based Session History:** Branchable graph structure supporting parallel alternative exploration and time-travel rollbacks.
* **Threshold Auto-Compaction:** Out-of-band structured summarization triggered when context usage exceeds 80% of the active window.
* **Code Checkpointing:** Session graph nodes synchronized directly with atomic Git commits.

---

## 5. Complete System Tool Catalog

* **File & Code Manipulation:**
  * `read_window`: Bounded range-based line reader.
  * `ast_skeleton`: Structural interface and type extractor.
  * `grep_search`: Concurrent lexical pattern matching via regex.
  * `apply_patch`: Transactional unified diff patcher (automatic rollback on hunk failure).
* **OS & Version Control:**
  * `exec_command`: Shell execution pipeline guarded by Exit-Code scrubbing.
  * `git_commit`: Automatic atomic commit generation with semantic messages.
  * `git_status_compact`: Lightweight repository state check via short format.
* **Navigation & External Validation:**
  * `browser_action`: Headless browser automation (navigation, clicks, console log capture, screenshots).
  * `web_fetch_doc`: Remote web documentation scraper converting pages to clean Markdown.
* **Extensibility & Dispatching:**
  * `mcp_client`: Standardized tool invoker for external Model Context Protocol servers.
  * `task_spawn`: Instantiates and dispatches an ephemeral background sub-agent.

---

## 6. Security & Sandboxing

* **Path Confinement:** Canonical path resolution preventing path traversal outside the project root directory.
* **Granular Permission Matrix:** Independent policy enforcement (`AlwaysAllow`, `PromptUser`, `Deny`) for File Read, File Write, Shell Execution, Network Access, and Browser Control.

---

## 7. Technical Architecture: Ports & Adapters (Hexagonal)

* **Level 1 (Core Contracts):** Pure traits, data structs, enums, and typed errors. Zero dependencies on concrete infrastructure crates.
* **Level 2 (Engine & Services):** Concrete implementations of core traits (`context`, `tools`, `session`, `orchestrator`).
* **Level 3 (Adapters & Interfaces):** Entry points consuming Level 2 services (CLI binary, background daemons, messaging webhooks).
* **Crate Boundary Isolation:** No circular dependencies; modules interact solely through traits defined in `crates/core`.

---

## 8. Development Standard: Type-Driven & Contract-First

* **Strict Contract-First Design:** Type definitions (`structs`, `enums`, `traits`) and typed errors must be fully established and compiled before implementing internal function logic.
* **Deterministic Verification Loop:**
  1. Define types and traits in `crates/core`.
  2. Implement unit test harness exposing expected module behavior.
  3. Implement crate logic satisfying the interface.
  4. Enforce static validation via `cargo check`, `cargo clippy`, and unit test passes.

---

## 9. Zero-Bloat Engineering Directives

* **Deterministic Tooling Over Model Inference:** Code extraction, output scrubbing, and pattern searches must rely exclusively on deterministic native routines (C/Rust parsers, regex), eliminating unnecessary local neural model overhead.
* **Minimal Dependency Surface:** Avoid heavyweight runtime dependencies. Every external crate introduced into the Cargo workspace must be audited for compilation overhead, binary footprint, and memory footprint.
* **Zero Runtime Leaks:** Ephemeral sub-agent tasks, terminal scrub buffers, and AST parsing trees must be dropped immediately following execution to maintain baseline RAM usage under 50 MB.
