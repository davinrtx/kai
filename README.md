# KAI (Krill Agent Interface)

An autonomous software engineering agent SDK built in Rust.

KAI is not a simple copilot tethered to an IDE or a chat wrapper around a single API. It is an agentic execution runtime designed to run autonomously, orchestrate parallel workstreams, navigate complex codebases, and interact directly with system environments.

---

## What is KAI?

KAI decouples software agents from rigid editor plugins. It runs as a standalone CLI, an autonomous background daemon, or an orchestrator across servers, handling end-to-end engineering tasks with high precision and deterministic context management.

### Key Capabilities

* **Deterministic Context Engineering:** Replaces raw file dumping with bounded windowed reads, Tree-sitter AST signature extraction, and targeted terminal error scrubbers. The agent processes code semantically without saturating context windows.
* **Multi-Agent Orchestration & Sub-Agents:** Spawns isolated, ephemeral sub-agents for parallel exploratory tasks (investigating issues, running test suites, analyzing dependencies) and collects structured reports in a non-blocking Task Inbox.
* **Live Steering & Interruption:** Interrupt running tool executions in real time to redirect the agent without resetting session state or losing conversational memory.
* **Branchable Session Graph (DAG):** Session history is modeled as a directed acyclic graph. Fork alternate implementation paths, roll back to prior checkpoints, or automatically compact long-running context histories.
* **Transactional Code Operations:** Modifies codebases via strict unified diffs with validation and automatic rollbacks, preventing partial or corrupted writes.
* **Full Tool Ecosystem & Extensibility:** Native tools for file manipulation, ripgrep-powered pattern search, shell execution, headless browser testing, and external Model Context Protocol (MCP) server integration.
* **Model Agnostic:** Connects to local models or remote inference endpoints without vendor lock-in.

---

## Quickstart

### Prerequisites

* Rust 1.80+ (Edition 2021)
* C compiler (`gcc` or `clang`) for Tree-sitter grammar bindings

### Build & Verify

```bash
# Verify workspace compilation
cargo check --workspace

# Run offline test harness
cargo test --workspace

# Run style and lint enforcement
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings