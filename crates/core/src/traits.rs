//! Core traits and execution contracts for KAI.
//!
//! Defines the foundational interfaces that downstream crates must implement:
//! [`Tool`], [`Agent`], [`ContextProcessor`], and [`SessionStore`].
//! All traits are designed to be object-safe and concurrency-friendly (`Send + Sync`).

use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;

use serde::{Deserialize, Serialize};

use crate::error::{Result, ToolError};
use crate::event::{GlobalSteeringReceiver, SteeringState};
use crate::message::{Message, ToolResult};

/// Type alias for pinned, heap-allocated futures used across object-safe traits.
pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// Security permission category governing tool authorization in the sandbox.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PermissionCategory {
    /// Reading files within canonical workspace boundaries.
    FileRead,
    /// Modifying or creating files within canonical workspace boundaries.
    FileWrite,
    /// Spawning external CLI or shell subprocesses.
    ShellExecution,
    /// Outbound HTTP or TCP network communication.
    NetworkAccess,
    /// Headless web browser automation and DOM manipulation.
    BrowserControl,
}

/// Security and confinement contract for filesystem and resource operations.
pub trait SandboxPolicy: Send + Sync {
    /// Canonicalizes and validates that `path` is strictly confined within the sandbox boundary.
    fn canonicalize_path(
        &self,
        path: &Path,
    ) -> std::result::Result<PathBuf, crate::error::SandboxError>;

    /// Verifies whether the specified permission category is authorized.
    fn check_permission(
        &self,
        category: PermissionCategory,
    ) -> std::result::Result<(), crate::error::SandboxError>;
}

/// Operational execution context supplied to tools during invocation.
#[derive(Debug, Clone)]
pub struct ToolContext {
    /// Canonical root directory of the active project workspace.
    pub working_dir: PathBuf,
    /// Unique identifier of the session initiating this tool execution.
    pub session_id: String,
    /// Identifier of the specific agent or sub-agent dispatching the tool.
    pub agent_id: String,
    /// Optional global steering receiver for cooperative cancellation and pause checks.
    pub steering: Option<GlobalSteeringReceiver>,
}

impl ToolContext {
    /// Constructs a new [`ToolContext`].
    pub fn new(
        working_dir: impl Into<PathBuf>,
        session_id: impl Into<String>,
        agent_id: impl Into<String>,
    ) -> Self {
        Self {
            working_dir: working_dir.into(),
            session_id: session_id.into(),
            agent_id: agent_id.into(),
            steering: None,
        }
    }

    /// Attaches a global steering receiver to this execution context.
    pub fn with_steering(mut self, steering: GlobalSteeringReceiver) -> Self {
        self.steering = Some(steering);
        self
    }

    /// Checks whether in-flight execution has been cancelled or terminated.
    pub fn is_cancelled(&self) -> bool {
        if let Some(rx) = &self.steering {
            *rx.borrow() == SteeringState::Terminated
        } else {
            false
        }
    }

    /// Checks whether in-flight execution has been requested to pause.
    pub fn is_paused(&self) -> bool {
        if let Some(rx) = &self.steering {
            *rx.borrow() == SteeringState::Paused
        } else {
            false
        }
    }

    /// Verifies whether execution has been cancelled, returning an interruption error if so.
    pub fn check_cancellation(&self) -> Result<()> {
        if self.is_cancelled() {
            Err(crate::error::KaiError::Orchestrator(
                crate::error::OrchestratorError::Interrupted {
                    reason: "Execution cancelled by steering signal".to_string(),
                },
            ))
        } else {
            Ok(())
        }
    }

    /// Convenience accessor for working directory reference.
    pub fn working_dir(&self) -> &Path {
        &self.working_dir
    }
}

/// Contract for executable tools callable by agents.
pub trait Tool: Send + Sync {
    /// Canonical, unique name of the tool (e.g. `read_window`, `apply_patch`).
    fn name(&self) -> &str;

    /// Human-readable description explaining the tool's purpose and usage.
    fn description(&self) -> &str;

    /// JSON Schema specification defining parameters accepted by this tool.
    fn schema(&self) -> serde_json::Value;

    /// Security permission category required to execute this tool.
    fn permission_category(&self) -> PermissionCategory;

    /// Execution timeout allocated to this tool. Defaults to [`None`] (no timeout limit).
    fn timeout(&self) -> Option<std::time::Duration> {
        None
    }

    /// Returns true if this tool does not mutate the filesystem, process, or environment.
    ///
    /// Read-only tools can be safely executed concurrently by the orchestrator.
    fn is_read_only(&self) -> bool {
        false
    }

    /// Validates JSON arguments against this tool's expected schema prior to execution.
    fn validate_arguments(&self, arguments: &serde_json::Value) -> Result<(), ToolError> {
        if arguments.is_object() || arguments.is_null() {
            Ok(())
        } else {
            Err(ToolError::InvalidArguments {
                name: self.name().to_string(),
                reason: "Arguments must be a valid JSON object or null".to_string(),
            })
        }
    }

    /// Executes the tool asynchronously with validated input arguments and runtime context.
    fn execute<'a>(
        &'a self,
        arguments: serde_json::Value,
        context: &'a ToolContext,
    ) -> BoxFuture<'a, Result<ToolResult>>;
}

/// Outcome of a single operational step executed by an agent.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum StepOutcome {
    /// Agent produced messages and requires another processing cycle.
    Continue(Vec<Message>),
    /// Agent reached successful completion with a final message.
    Completed(Message),
    /// Agent execution was suspended awaiting external guidance or steering.
    Suspended {
        /// Reason explaining why the agent was suspended.
        reason: String,
    },
}

impl StepOutcome {
    /// Returns true if the step outcome is [`StepOutcome::Completed`].
    pub fn is_completed(&self) -> bool {
        matches!(self, Self::Completed(_))
    }

    /// Returns true if the step outcome is [`StepOutcome::Continue`].
    pub fn is_continue(&self) -> bool {
        matches!(self, Self::Continue(_))
    }

    /// Returns true if the step outcome is [`StepOutcome::Suspended`].
    pub fn is_suspended(&self) -> bool {
        matches!(self, Self::Suspended { .. })
    }

    /// Returns a reference to the inner completed message, if any.
    pub fn as_completed(&self) -> Option<&Message> {
        match self {
            Self::Completed(msg) => Some(msg),
            _ => None,
        }
    }

    /// Returns a reference to the continuation messages, if any.
    pub fn as_continue(&self) -> Option<&[Message]> {
        match self {
            Self::Continue(messages) => Some(messages.as_slice()),
            _ => None,
        }
    }
}

/// Core lifecycle contract for autonomous agents and sub-agents.
pub trait Agent: Send + Sync {
    /// Returns the unique instance identifier of this agent (e.g. "agent-sub-04").
    fn id(&self) -> &str;

    /// Returns the agent's functional name or role (e.g. "researcher", "code-reviewer").
    fn name(&self) -> &str;

    /// Returns JSON schemas for tools made available to this agent.
    fn tool_schemas(&self) -> Vec<serde_json::Value> {
        Vec::new()
    }

    /// Executes a single discrete reasoning or execution turn.
    fn step<'a>(&'a mut self, inbox: &'a [Message]) -> BoxFuture<'a, Result<StepOutcome>>;

    /// Serializes the internal state of this agent for persistence during suspension or handoff.
    fn serialize_state(&self) -> Option<serde_json::Value> {
        None
    }

    /// Restores the internal state of this agent from a previous serialization.
    fn restore_state(&mut self, _state: serde_json::Value) -> Result<()> {
        Ok(())
    }
}

/// Deterministic context optimization pipeline contract.
///
/// Implementations reduce token count, extract AST skeletons, and scrub terminal outputs
/// before conversational turns are submitted to model inference endpoints.
pub trait ContextProcessor: Send + Sync {
    /// Estimates token usage for a sequence of conversational messages.
    fn estimate_tokens(&self, messages: &[Message]) -> usize;

    /// Processes and condenses a sequence of conversational messages
    /// to fit within the designated target token budget.
    fn process<'a>(
        &'a self,
        messages: &'a [Message],
        target_tokens: usize,
    ) -> BoxFuture<'a, Result<Vec<Message>>>;
}

/// A node within the branchable session Directed Acyclic Graph (DAG).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionNode {
    /// Unique identifier for this graph node.
    pub id: String,
    /// Identifiers of parent nodes within the DAG. Empty for root nodes.
    pub parent_ids: Vec<String>,
    /// Conversational message recorded at this node checkpoint.
    pub message: Message,
    /// Timestamp of node creation in milliseconds since UNIX epoch.
    pub timestamp_ms: u64,
    /// Git commit hash synchronized with this node state, if any.
    pub git_commit: Option<String>,
}

impl SessionNode {
    /// Constructs a root [`SessionNode`] with no parents.
    pub fn root(id: impl Into<String>, message: Message, timestamp_ms: u64) -> Self {
        Self {
            id: id.into(),
            parent_ids: Vec::new(),
            message,
            timestamp_ms,
            git_commit: None,
        }
    }

    /// Constructs a [`SessionNode`] with a single parent.
    pub fn with_parent(
        id: impl Into<String>,
        parent_id: impl Into<String>,
        message: Message,
        timestamp_ms: u64,
    ) -> Self {
        Self {
            id: id.into(),
            parent_ids: vec![parent_id.into()],
            message,
            timestamp_ms,
            git_commit: None,
        }
    }

    /// Constructs a [`SessionNode`] with multiple parents (DAG merge node).
    pub fn with_parents(
        id: impl Into<String>,
        parent_ids: Vec<String>,
        message: Message,
        timestamp_ms: u64,
    ) -> Self {
        Self {
            id: id.into(),
            parent_ids,
            message,
            timestamp_ms,
            git_commit: None,
        }
    }

    /// Associates a Git commit SHA with this node.
    pub fn with_git_commit(mut self, commit_sha: impl Into<String>) -> Self {
        self.git_commit = Some(commit_sha.into());
        self
    }

    /// Returns the primary parent identifier, if any.
    pub fn primary_parent(&self) -> Option<&str> {
        self.parent_ids.first().map(String::as_str)
    }

    /// Returns true if this node is a root node with no parents.
    pub fn is_root(&self) -> bool {
        self.parent_ids.is_empty()
    }

    /// Returns true if this node is a merge node with more than one parent.
    pub fn is_merge(&self) -> bool {
        self.parent_ids.len() > 1
    }

    /// Validates graph invariants for this node (non-empty ID, no self-parenting, unique parent IDs).
    pub fn validate(&self) -> std::result::Result<(), crate::error::SessionError> {
        if self.id.trim().is_empty() {
            return Err(crate::error::SessionError::CorruptedState {
                reason: "Session node ID cannot be empty".to_string(),
            });
        }
        if self.parent_ids.iter().any(|p| p == &self.id) {
            return Err(crate::error::SessionError::CycleDetected {
                node_id: self.id.clone(),
            });
        }
        let mut unique = std::collections::HashSet::with_capacity(self.parent_ids.len());
        for p in &self.parent_ids {
            if !unique.insert(p) {
                return Err(crate::error::SessionError::CorruptedState {
                    reason: format!("Duplicate parent ID '{p}' in node '{}'", self.id),
                });
            }
        }
        Ok(())
    }
}

/// Persistence, retrieval, and branching contract for DAG session trees.
pub trait SessionStore: Send + Sync {
    /// Retrieves a node by its unique identifier.
    fn get_node<'a>(&'a self, node_id: &'a str) -> BoxFuture<'a, Result<Option<SessionNode>>>;

    /// Persists a node into the session graph.
    fn put_node<'a>(&'a self, node: &'a SessionNode) -> BoxFuture<'a, Result<()>>;

    /// Retrieves all direct children of a node within the session DAG.
    fn get_children<'a>(&'a self, node_id: &'a str) -> BoxFuture<'a, Result<Vec<SessionNode>>>;

    /// Returns the linear branch history starting from the root node to `leaf_node_id`.
    fn get_branch_history<'a>(
        &'a self,
        leaf_node_id: &'a str,
    ) -> BoxFuture<'a, Result<Vec<SessionNode>>>;

    /// Sets or updates the leaf node pointing to a named branch head.
    fn set_head<'a>(&'a self, branch_name: &'a str, node_id: &'a str) -> BoxFuture<'a, Result<()>>;

    /// Retrieves the node identifier currently designated as the head of a branch.
    fn get_head<'a>(&'a self, branch_name: &'a str) -> BoxFuture<'a, Result<Option<String>>>;

    /// Lists all registered branch names in the session graph.
    fn list_branches<'a>(&'a self) -> BoxFuture<'a, Result<Vec<String>>>;

    /// Prunes an abandoned or inactive branch pointer from the session graph.
    fn prune_branch<'a>(&'a self, branch_name: &'a str) -> BoxFuture<'a, Result<()>>;

    /// Lists all distinct session identifiers stored in the session graph.
    fn list_sessions<'a>(&'a self) -> BoxFuture<'a, Result<Vec<String>>>;

    /// Retrieves a session node associated with a given Git commit SHA, if any.
    fn get_node_by_commit<'a>(
        &'a self,
        commit_hash: &'a str,
    ) -> BoxFuture<'a, Result<Option<SessionNode>>>;

    /// Permanently deletes an entire session and associated DAG graph state.
    fn delete_session<'a>(&'a self, session_id: &'a str) -> BoxFuture<'a, Result<()>>;
}

/// Interceptor contract for lifecycle hooks surrounding agent reasoning steps and tool calls.
pub trait AgentMiddleware: Send + Sync {
    /// Human-readable identifier for this middleware layer.
    fn name(&self) -> &str;

    /// Interceptor hook invoked before an agent or language model processes conversational messages.
    fn before_turn<'a>(
        &'a self,
        _messages: &'a mut Vec<Message>,
        _context: &'a ToolContext,
    ) -> BoxFuture<'a, Result<()>> {
        Box::pin(async { Ok(()) })
    }

    /// Interceptor hook invoked after an agent turn finishes, before outcome finalization.
    fn after_turn<'a>(
        &'a self,
        _outcome: &'a mut StepOutcome,
        _context: &'a ToolContext,
    ) -> BoxFuture<'a, Result<()>> {
        Box::pin(async { Ok(()) })
    }

    /// Interceptor hook invoked when a tool execution produces an error.
    ///
    /// Returns `Some(ToolResult)` if the middleware remediates or wraps the error, or `None` to pass-through.
    fn on_tool_error<'a>(
        &'a self,
        _tool_name: &'a str,
        _error: &'a ToolError,
        _context: &'a ToolContext,
    ) -> BoxFuture<'a, Result<Option<ToolResult>>> {
        Box::pin(async { Ok(None) })
    }
}

/// Structured procedural skill or guideline dynamically loaded into agent context.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SkillDefinition {
    /// Unique identifier or filename slug of the skill.
    pub name: String,
    /// Concise description of what capability this skill provides.
    pub description: String,
    /// Activation triggers, task keywords, or slash-commands associated with this skill.
    pub triggers: Vec<String>,
    /// Procedural markdown instructions, workflows, or rules for the agent.
    pub instructions: String,
    /// Optional source file path where this skill was loaded from.
    pub source_path: Option<String>,
}

impl SkillDefinition {
    /// Constructs a new [`SkillDefinition`].
    pub fn new(
        name: impl Into<String>,
        description: impl Into<String>,
        instructions: impl Into<String>,
    ) -> Self {
        Self {
            name: name.into(),
            description: description.into(),
            triggers: Vec::new(),
            instructions: instructions.into(),
            source_path: None,
        }
    }

    /// Adds triggers to the skill definition.
    pub fn with_triggers(mut self, triggers: impl IntoIterator<Item = impl Into<String>>) -> Self {
        self.triggers = triggers.into_iter().map(Into::into).collect();
        self
    }

    /// Associates an origin file path with the skill definition.
    pub fn with_source_path(mut self, source_path: impl Into<String>) -> Self {
        self.source_path = Some(source_path.into());
        self
    }

    /// Evaluates whether the skill is relevant to a query or prompt based on name, description, or triggers.
    pub fn matches_query(&self, query: &str) -> bool {
        let q_lower = query.to_ascii_lowercase();
        if q_lower.contains(&self.name.to_ascii_lowercase()) {
            return true;
        }
        for trigger in &self.triggers {
            if q_lower.contains(&trigger.to_ascii_lowercase()) {
                return true;
            }
        }
        false
    }
}

/// Decision outcome produced by a [`ToolApprovalPolicy`] prior to tool invocation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ApprovalDecision {
    /// Tool execution is permitted without external intervention.
    Approved,
    /// Tool execution requires explicit user confirmation or supervisor review.
    RequiresConfirmation {
        /// Rationale explaining why confirmation is required.
        reason: String,
    },
    /// Tool execution is blocked by security or governance policy.
    Denied {
        /// Rationale explaining why execution was denied.
        reason: String,
    },
}

impl ApprovalDecision {
    /// Returns true if execution is approved.
    pub fn is_approved(&self) -> bool {
        matches!(self, Self::Approved)
    }

    /// Returns true if execution requires confirmation.
    pub fn requires_confirmation(&self) -> bool {
        matches!(self, Self::RequiresConfirmation { .. })
    }

    /// Returns true if execution was denied.
    pub fn is_denied(&self) -> bool {
        matches!(self, Self::Denied { .. })
    }
}

/// Policy interceptor evaluating risk and authorization before tools execute.
pub trait ToolApprovalPolicy: Send + Sync {
    /// Evaluates whether a tool invocation is approved, requires confirmation, or is denied.
    fn evaluate(
        &self,
        tool_name: &str,
        arguments: &serde_json::Value,
        context: &ToolContext,
    ) -> ApprovalDecision;
}

/// Permissive default approval policy allowing all tool executions.
#[derive(Debug, Default, Clone, Copy)]
pub struct AlwaysApprovePolicy;

impl ToolApprovalPolicy for AlwaysApprovePolicy {
    fn evaluate(
        &self,
        _tool_name: &str,
        _arguments: &serde_json::Value,
        _context: &ToolContext,
    ) -> ApprovalDecision {
        ApprovalDecision::Approved
    }
}

/// Contract for multi-agent task dispatching and sub-agent supervision.
pub trait TaskDispatcher: Send + Sync {
    /// Dispatches a task to a designated sub-agent and awaits completion.
    fn dispatch_task<'a>(
        &'a self,
        sub_agent_id: &'a str,
        task_description: &'a str,
        context: &'a ToolContext,
    ) -> BoxFuture<'a, Result<String>>;
}

/// Contract for robust source code modification using fuzzy matching and AST targeting.
pub trait CodePatcher: Send + Sync {
    /// Applies a series of SEARCH/REPLACE blocks over the target file content.
    fn apply_blocks<'a>(
        &'a self,
        file_path: &'a Path,
        content: &'a str,
        patch_blocks: &'a [PatchBlock],
    ) -> Result<PatchApplicationResult>;
}

/// An atomic search-and-replace modification block.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PatchBlock {
    /// Target code snippet to search for.
    pub search: String,
    /// Code snippet to substitute in place of the search target.
    pub replace: String,
}

impl PatchBlock {
    /// Constructs a new [`PatchBlock`].
    pub fn new(search: impl Into<String>, replace: impl Into<String>) -> Self {
        Self {
            search: search.into(),
            replace: replace.into(),
        }
    }
}

/// Result of applying code patch blocks with diagnostic confidence metrics.
#[derive(Debug, Clone, PartialEq)]
pub struct PatchApplicationResult {
    /// Final modified source code after applying all patch blocks.
    pub modified_content: String,
    /// Number of blocks successfully applied.
    pub applied_count: usize,
    /// Average confidence score [0.0, 1.0] across all applied blocks.
    pub confidence_score: f64,
}

/// Contract for isolated workspace provisioning and ephemeral git worktree management.
pub trait WorkspaceManager: Send + Sync {
    /// Scope handle managing the lifecycle of an ephemeral worktree.
    type Handle: WorktreeScope;

    /// Provisions an ephemeral worktree isolated from the parent repository.
    fn create_ephemeral_worktree<'a>(
        &'a self,
        agent_id: &'a str,
        commit_ish: &'a str,
    ) -> BoxFuture<'a, Result<Self::Handle>>;
}

/// Handle to an active ephemeral worktree providing scoped filesystem access and RAII cleanup.
pub trait WorktreeScope: Send + Sync {
    /// Absolute filesystem path to the root of the ephemeral worktree.
    fn path(&self) -> &Path;

    /// Generates a structured proposal summarizing mutations made in this isolated worktree.
    fn generate_proposal(&self) -> Result<WorkspaceProposal>;
}

/// Immutable mutation proposal produced by an isolated sub-agent for root approval.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkspaceProposal {
    /// Identifier of the sub-agent that produced the proposal.
    pub agent_id: String,
    /// Git commit SHA of the base commit on which the proposal was authored.
    pub base_commit: String,
    /// Compact summary of modified files and line deltas (equivalent to git diff --stat).
    pub diff_stat: String,
    /// Complete unified patch payload representing the proposed changes.
    pub patch_payload: String,
}

/// Contract for deep semantic code inspection bridging syntax and type systems.
pub trait SemanticAnalyzer: Send + Sync {
    /// Resolves the source definition location of a symbol at the given line and character.
    fn goto_definition<'a>(
        &'a self,
        file_path: &'a Path,
        line: u32,
        character: u32,
    ) -> BoxFuture<'a, Result<Option<SymbolLocation>>>;

    /// Retrieves hover documentation, type signature, and macro expansion for a symbol.
    fn hover_info<'a>(
        &'a self,
        file_path: &'a Path,
        line: u32,
        character: u32,
    ) -> BoxFuture<'a, Result<Option<String>>>;
}

/// Resolved source code location identifying a symbol definition.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SymbolLocation {
    /// Target file path containing the definition.
    pub path: PathBuf,
    /// Zero-based starting line number.
    pub line_start: u32,
    /// Zero-based ending line number.
    pub line_end: u32,
}

/// Contract for kernel-level process confinement and filesystem sandboxing.
pub trait CommandIsolationEngine: Send + Sync {
    /// Wraps a command specification within an unprivileged sandbox container (e.g. Landlock/Bubblewrap).
    fn wrap_command(
        &self,
        command: &str,
        working_dir: &Path,
        allow_network: bool,
    ) -> Result<IsolatedCommandSpec>;
}

/// Structured specification of an isolated process execution.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IsolatedCommandSpec {
    /// Executable binary to spawn (e.g. `/usr/bin/bwrap` or `/bin/sh`).
    pub program: PathBuf,
    /// CLI arguments passed to the binary.
    pub args: Vec<String>,
    /// Scrubbed and sanitized environment variables.
    pub env: Vec<(String, String)>,
}

/// Contract for dynamic grammar resolution and AST parser loading.
pub trait GrammarLoader: Send + Sync {
    /// Returns whether the loader supports the given programming language identifier.
    fn supports_language(&self, language: &str) -> bool;
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    struct EchoTool;

    impl Tool for EchoTool {
        fn name(&self) -> &str {
            "echo"
        }

        fn description(&self) -> &str {
            "Echoes input text"
        }

        fn schema(&self) -> serde_json::Value {
            json!({
                "type": "object",
                "properties": {
                    "text": { "type": "string" }
                }
            })
        }

        fn permission_category(&self) -> PermissionCategory {
            PermissionCategory::FileRead
        }

        fn execute<'a>(
            &'a self,
            arguments: serde_json::Value,
            context: &'a ToolContext,
        ) -> BoxFuture<'a, Result<ToolResult>> {
            Box::pin(async move {
                if context.is_cancelled() {
                    return Err(crate::error::KaiError::Orchestrator(
                        crate::error::OrchestratorError::Interrupted {
                            reason: "Cancelled before execution".to_string(),
                        },
                    ));
                }
                let text = arguments
                    .get("text")
                    .and_then(|v| v.as_str())
                    .unwrap_or_default();
                let output = format!("{}:{text}", context.agent_id);
                Ok(ToolResult::success("call_echo", output))
            })
        }
    }

    struct MockAgent {
        id: String,
        name: String,
    }

    impl Agent for MockAgent {
        fn id(&self) -> &str {
            &self.id
        }

        fn name(&self) -> &str {
            &self.name
        }

        fn step<'a>(&'a mut self, inbox: &'a [Message]) -> BoxFuture<'a, Result<StepOutcome>> {
            Box::pin(async move {
                let text = format!("Processed {} messages", inbox.len());
                Ok(StepOutcome::Completed(Message::assistant("msg_out", text)))
            })
        }
    }

    #[tokio::test]
    async fn test_tool_trait_execution() {
        let tool = EchoTool;
        let ctx = ToolContext::new("/workspace", "sess_01", "agent_main");
        assert_eq!(tool.name(), "echo");
        assert_eq!(tool.permission_category(), PermissionCategory::FileRead);

        assert!(tool.validate_arguments(&json!({"text": "test"})).is_ok());
        assert!(tool.validate_arguments(&json!("invalid_string")).is_err());

        let result = tool
            .execute(json!({ "text": "hello kai" }), &ctx)
            .await
            .expect("tool execution should succeed");
        assert_eq!(result.output, "agent_main:hello kai");
        assert!(!result.is_error);
    }

    #[tokio::test]
    async fn test_agent_trait_contract() {
        let mut agent = MockAgent {
            id: "agent_01".to_string(),
            name: "evaluator".to_string(),
        };
        assert_eq!(agent.id(), "agent_01");
        assert_eq!(agent.name(), "evaluator");
        assert!(agent.tool_schemas().is_empty());

        let outcome = agent.step(&[]).await.expect("step should succeed");
        match outcome {
            StepOutcome::Completed(msg) => {
                assert_eq!(msg.text_content(), "Processed 0 messages");
            }
            _ => panic!("expected completed outcome"),
        }
    }

    #[test]
    fn test_tool_context_steering_cancellation() {
        let (tx, rx) = crate::event::global_steering_channel();
        let ctx = ToolContext::new("/workspace", "sess_02", "sub_agent").with_steering(rx);

        assert!(!ctx.is_cancelled());
        assert!(!ctx.is_paused());

        tx.send(SteeringState::Paused).unwrap();
        assert!(ctx.is_paused());

        tx.send(SteeringState::Terminated).unwrap();
        assert!(ctx.is_cancelled());
    }

    #[test]
    fn test_session_node_dag_construction() {
        let msg_root = Message::user("msg_0", "Init project");
        let root = SessionNode::root("node_00", msg_root, 1726650000000);
        assert!(root.is_root());
        assert!(!root.is_merge());
        assert_eq!(root.primary_parent(), None);

        let msg_child = Message::user("msg_1", "Test checkpoint");
        let child = SessionNode::with_parent("node_01", "node_00", msg_child, 1726650001000)
            .with_git_commit("a1b2c3d4e5f6");
        assert!(!child.is_root());
        assert!(!child.is_merge());
        assert_eq!(child.primary_parent(), Some("node_00"));

        let msg_merge = Message::assistant("msg_2", "Merged findings");
        let merge = SessionNode::with_parents(
            "node_02",
            vec!["node_01".to_string(), "branch_sub_01".to_string()],
            msg_merge,
            1726650002000,
        );
        assert!(merge.is_merge());
        assert_eq!(merge.parent_ids.len(), 2);
    }

    use std::collections::{HashMap, HashSet};
    use std::sync::RwLock;

    struct InMemorySessionStore {
        nodes: RwLock<HashMap<String, SessionNode>>,
        branches: RwLock<HashMap<String, String>>,
        sessions: RwLock<HashSet<String>>,
    }

    impl InMemorySessionStore {
        fn new() -> Self {
            let mut sessions = HashSet::new();
            sessions.insert("sess_default".to_string());
            Self {
                nodes: RwLock::new(HashMap::new()),
                branches: RwLock::new(HashMap::new()),
                sessions: RwLock::new(sessions),
            }
        }
    }

    impl SessionStore for InMemorySessionStore {
        fn get_node<'a>(&'a self, node_id: &'a str) -> BoxFuture<'a, Result<Option<SessionNode>>> {
            Box::pin(async move {
                let guard = self.nodes.read().map_err(|_| {
                    crate::error::KaiError::Internal(crate::error::InternalError::new(
                        "lock poisoned",
                    ))
                })?;
                Ok(guard.get(node_id).cloned())
            })
        }

        fn put_node<'a>(&'a self, node: &'a SessionNode) -> BoxFuture<'a, Result<()>> {
            Box::pin(async move {
                let mut guard = self.nodes.write().map_err(|_| {
                    crate::error::KaiError::Internal(crate::error::InternalError::new(
                        "lock poisoned",
                    ))
                })?;
                guard.insert(node.id.clone(), node.clone());
                Ok(())
            })
        }

        fn get_children<'a>(&'a self, node_id: &'a str) -> BoxFuture<'a, Result<Vec<SessionNode>>> {
            Box::pin(async move {
                let guard = self.nodes.read().map_err(|_| {
                    crate::error::KaiError::Internal(crate::error::InternalError::new(
                        "lock poisoned",
                    ))
                })?;
                let mut children: Vec<SessionNode> = guard
                    .values()
                    .filter(|n| n.parent_ids.iter().any(|p| p == node_id))
                    .cloned()
                    .collect();
                children.sort_by(|a, b| a.id.cmp(&b.id));
                Ok(children)
            })
        }

        fn get_branch_history<'a>(
            &'a self,
            leaf_node_id: &'a str,
        ) -> BoxFuture<'a, Result<Vec<SessionNode>>> {
            Box::pin(async move {
                let guard = self.nodes.read().map_err(|_| {
                    crate::error::KaiError::Internal(crate::error::InternalError::new(
                        "lock poisoned",
                    ))
                })?;

                let mut history = Vec::new();
                let mut current_id = Some(leaf_node_id.to_string());
                let mut visited = HashSet::new();

                while let Some(id) = current_id {
                    if !visited.insert(id.clone()) {
                        return Err(crate::error::KaiError::Session(
                            crate::error::SessionError::CycleDetected { node_id: id },
                        ));
                    }
                    if let Some(node) = guard.get(&id) {
                        current_id = node.primary_parent().map(String::from);
                        history.push(node.clone());
                    } else {
                        return Err(crate::error::KaiError::Session(
                            crate::error::SessionError::NodeNotFound { node_id: id },
                        ));
                    }
                }

                history.reverse();
                Ok(history)
            })
        }

        fn set_head<'a>(
            &'a self,
            branch_name: &'a str,
            node_id: &'a str,
        ) -> BoxFuture<'a, Result<()>> {
            Box::pin(async move {
                let mut guard = self.branches.write().map_err(|_| {
                    crate::error::KaiError::Internal(crate::error::InternalError::new(
                        "lock poisoned",
                    ))
                })?;
                guard.insert(branch_name.to_string(), node_id.to_string());
                Ok(())
            })
        }

        fn get_head<'a>(&'a self, branch_name: &'a str) -> BoxFuture<'a, Result<Option<String>>> {
            Box::pin(async move {
                let guard = self.branches.read().map_err(|_| {
                    crate::error::KaiError::Internal(crate::error::InternalError::new(
                        "lock poisoned",
                    ))
                })?;
                Ok(guard.get(branch_name).cloned())
            })
        }

        fn list_branches<'a>(&'a self) -> BoxFuture<'a, Result<Vec<String>>> {
            Box::pin(async move {
                let guard = self.branches.read().map_err(|_| {
                    crate::error::KaiError::Internal(crate::error::InternalError::new(
                        "lock poisoned",
                    ))
                })?;
                let mut list: Vec<String> = guard.keys().cloned().collect();
                list.sort();
                Ok(list)
            })
        }

        fn prune_branch<'a>(&'a self, branch_name: &'a str) -> BoxFuture<'a, Result<()>> {
            Box::pin(async move {
                let mut guard = self.branches.write().map_err(|_| {
                    crate::error::KaiError::Internal(crate::error::InternalError::new(
                        "lock poisoned",
                    ))
                })?;
                guard.remove(branch_name);
                Ok(())
            })
        }

        fn list_sessions<'a>(&'a self) -> BoxFuture<'a, Result<Vec<String>>> {
            Box::pin(async move {
                let guard = self.sessions.read().map_err(|_| {
                    crate::error::KaiError::Internal(crate::error::InternalError::new(
                        "lock poisoned",
                    ))
                })?;
                let mut list: Vec<String> = guard.iter().cloned().collect();
                list.sort();
                Ok(list)
            })
        }

        fn get_node_by_commit<'a>(
            &'a self,
            commit_hash: &'a str,
        ) -> BoxFuture<'a, Result<Option<SessionNode>>> {
            Box::pin(async move {
                let guard = self.nodes.read().map_err(|_| {
                    crate::error::KaiError::Internal(crate::error::InternalError::new(
                        "lock poisoned",
                    ))
                })?;
                let node = guard
                    .values()
                    .find(|n| n.git_commit.as_deref() == Some(commit_hash))
                    .cloned();
                Ok(node)
            })
        }

        fn delete_session<'a>(&'a self, session_id: &'a str) -> BoxFuture<'a, Result<()>> {
            Box::pin(async move {
                let mut nodes = self.nodes.write().map_err(|_| {
                    crate::error::KaiError::Internal(crate::error::InternalError::new(
                        "lock poisoned",
                    ))
                })?;
                let mut branches = self.branches.write().map_err(|_| {
                    crate::error::KaiError::Internal(crate::error::InternalError::new(
                        "lock poisoned",
                    ))
                })?;
                let mut sessions = self.sessions.write().map_err(|_| {
                    crate::error::KaiError::Internal(crate::error::InternalError::new(
                        "lock poisoned",
                    ))
                })?;
                nodes.clear();
                branches.clear();
                sessions.remove(session_id);
                Ok(())
            })
        }
    }

    #[tokio::test]
    async fn test_session_store_crud_and_branching() {
        let store = InMemorySessionStore::new();

        // 1. Put and get root node
        let root = SessionNode::root("node_0", Message::user("m0", "Root prompt"), 1000);
        store.put_node(&root).await.unwrap();

        let fetched_root = store.get_node("node_0").await.unwrap();
        assert_eq!(fetched_root, Some(root.clone()));

        // 2. Add children on two separate branches
        let node_a1 = SessionNode::with_parent(
            "node_a1",
            "node_0",
            Message::assistant("m1", "Plan A"),
            2000,
        );
        let node_b1 = SessionNode::with_parent(
            "node_b1",
            "node_0",
            Message::assistant("m2", "Plan B"),
            2100,
        );
        store.put_node(&node_a1).await.unwrap();
        store.put_node(&node_b1).await.unwrap();

        // 3. Verify get_children
        let children = store.get_children("node_0").await.unwrap();
        assert_eq!(children.len(), 2);
        assert_eq!(children[0].id, "node_a1");
        assert_eq!(children[1].id, "node_b1");

        // 4. Branch heads management
        store.set_head("branch_a", "node_a1").await.unwrap();
        store.set_head("branch_b", "node_b1").await.unwrap();

        let head_a = store.get_head("branch_a").await.unwrap();
        assert_eq!(head_a.as_deref(), Some("node_a1"));

        let branches = store.list_branches().await.unwrap();
        assert_eq!(branches, vec!["branch_a", "branch_b"]);

        // 5. Linear branch history
        let history_a = store.get_branch_history("node_a1").await.unwrap();
        assert_eq!(history_a.len(), 2);
        assert_eq!(history_a[0].id, "node_0");
        assert_eq!(history_a[1].id, "node_a1");

        // 6. Prune branch
        store.prune_branch("branch_b").await.unwrap();
        assert_eq!(store.get_head("branch_b").await.unwrap(), None);
        assert_eq!(store.list_branches().await.unwrap(), vec!["branch_a"]);

        // 7. Delete session
        store.delete_session("sess_01").await.unwrap();
        assert_eq!(store.get_node("node_0").await.unwrap(), None);
        assert!(store.list_branches().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn test_session_store_dag_merge_history() {
        let store = InMemorySessionStore::new();

        let root = SessionNode::root("root", Message::user("m0", "Task"), 1000);
        let branch_1 =
            SessionNode::with_parent("b1", "root", Message::assistant("m1", "Analysis 1"), 2000);
        let branch_2 =
            SessionNode::with_parent("b2", "root", Message::assistant("m2", "Analysis 2"), 2100);
        let merge = SessionNode::with_parents(
            "merge_node",
            vec!["b1".to_string(), "b2".to_string()],
            Message::assistant("m3", "Synthesized response"),
            3000,
        );

        store.put_node(&root).await.unwrap();
        store.put_node(&branch_1).await.unwrap();
        store.put_node(&branch_2).await.unwrap();
        store.put_node(&merge).await.unwrap();

        assert!(merge.is_merge());
        assert_eq!(merge.primary_parent(), Some("b1"));

        // Primary parent chain: root -> b1 -> merge_node
        let history = store.get_branch_history("merge_node").await.unwrap();
        assert_eq!(history.len(), 3);
        assert_eq!(history[0].id, "root");
        assert_eq!(history[1].id, "b1");
        assert_eq!(history[2].id, "merge_node");
    }

    #[tokio::test]
    async fn test_session_store_cycle_detection() {
        let store = InMemorySessionStore::new();

        // Cyclic DAG: n1 -> n2 -> n1
        let n1 = SessionNode::with_parent("n1", "n2", Message::user("m1", "A"), 1000);
        let n2 = SessionNode::with_parent("n2", "n1", Message::user("m2", "B"), 1001);

        store.put_node(&n1).await.unwrap();
        store.put_node(&n2).await.unwrap();

        let err = store.get_branch_history("n1").await.unwrap_err();
        match err {
            crate::error::KaiError::Session(crate::error::SessionError::CycleDetected {
                node_id,
            }) => {
                assert!(node_id == "n1" || node_id == "n2");
            }
            other => panic!("Expected SessionError::CycleDetected, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn test_session_store_missing_node_error() {
        let store = InMemorySessionStore::new();

        // Node points to missing parent "ghost_parent"
        let leaf =
            SessionNode::with_parent("leaf", "ghost_parent", Message::user("m1", "Leaf"), 1000);
        store.put_node(&leaf).await.unwrap();

        let err = store.get_branch_history("leaf").await.unwrap_err();
        match err {
            crate::error::KaiError::Session(crate::error::SessionError::NodeNotFound {
                node_id,
            }) => {
                assert_eq!(node_id, "ghost_parent");
            }
            other => panic!("Expected SessionError::NodeNotFound, got {other:?}"),
        }
    }

    #[test]
    fn test_session_node_validation() {
        // Valid node
        let valid = SessionNode::root("node_1", Message::user("m1", "valid"), 1000);
        assert!(valid.validate().is_ok());

        // Empty ID
        let empty_id = SessionNode::root("   ", Message::user("m2", "invalid"), 1000);
        assert!(empty_id.validate().is_err());

        // Self-parenting
        let self_parent =
            SessionNode::with_parent("node_x", "node_x", Message::user("m3", "invalid"), 1000);
        assert!(matches!(
            self_parent.validate(),
            Err(crate::error::SessionError::CycleDetected { .. })
        ));

        // Duplicate parent IDs
        let dup_parents = SessionNode::with_parents(
            "merge_dup",
            vec!["p1".to_string(), "p1".to_string()],
            Message::assistant("m4", "invalid"),
            1000,
        );
        assert!(matches!(
            dup_parents.validate(),
            Err(crate::error::SessionError::CorruptedState { .. })
        ));
    }

    #[tokio::test]
    async fn test_session_store_commit_lookup_and_sessions_list() {
        let store = InMemorySessionStore::new();

        let node = SessionNode::root("node_git", Message::user("m1", "Commit node"), 1000)
            .with_git_commit("abc123commit");
        store.put_node(&node).await.unwrap();

        let found = store.get_node_by_commit("abc123commit").await.unwrap();
        assert_eq!(found.as_ref().map(|n| n.id.as_str()), Some("node_git"));

        let not_found = store.get_node_by_commit("nonexistent").await.unwrap();
        assert!(not_found.is_none());

        let sessions = store.list_sessions().await.unwrap();
        assert!(sessions.contains(&"sess_default".to_string()));
    }

    #[test]
    fn test_tool_defaults_and_context_cancellation_check() {
        let tool = EchoTool;
        assert_eq!(tool.timeout(), None);
        assert!(!tool.is_read_only());

        let (tx, rx) = crate::event::global_steering_channel();
        let ctx = ToolContext::new("/workspace", "sess_cancel", "agent_1").with_steering(rx);

        assert!(ctx.check_cancellation().is_ok());

        tx.send(SteeringState::Terminated).unwrap();
        let err = ctx.check_cancellation().unwrap_err();
        assert!(matches!(
            err,
            crate::error::KaiError::Orchestrator(
                crate::error::OrchestratorError::Interrupted { .. }
            )
        ));
    }

    #[test]
    fn test_step_outcome_helpers() {
        let msg = Message::assistant("m1", "Completed work");
        let completed = StepOutcome::Completed(msg.clone());
        assert!(completed.is_completed());
        assert!(!completed.is_continue());
        assert!(!completed.is_suspended());
        assert_eq!(completed.as_completed(), Some(&msg));
        assert_eq!(completed.as_continue(), None);

        let cont = StepOutcome::Continue(vec![msg.clone()]);
        assert!(cont.is_continue());
        assert!(!cont.is_completed());
        assert_eq!(cont.as_continue().map(|s| s.len()), Some(1));

        let susp = StepOutcome::Suspended {
            reason: "User input required".to_string(),
        };
        assert!(susp.is_suspended());
        assert!(!susp.is_completed());
    }

    struct MockSandbox {
        root: PathBuf,
    }

    impl SandboxPolicy for MockSandbox {
        fn canonicalize_path(
            &self,
            path: &Path,
        ) -> std::result::Result<PathBuf, crate::error::SandboxError> {
            if path.to_string_lossy().contains("..") {
                return Err(crate::error::SandboxError::PathTraversalDetected {
                    path: path.to_string_lossy().to_string(),
                });
            }
            Ok(self.root.join(path))
        }

        fn check_permission(
            &self,
            category: PermissionCategory,
        ) -> std::result::Result<(), crate::error::SandboxError> {
            if category == PermissionCategory::ShellExecution {
                Err(crate::error::SandboxError::PermissionDenied {
                    operation: "shell_exec".to_string(),
                    resource: "system".to_string(),
                })
            } else {
                Ok(())
            }
        }
    }

    #[test]
    fn test_sandbox_policy_contract() {
        let sandbox = MockSandbox {
            root: PathBuf::from("/workspace"),
        };
        assert!(sandbox.canonicalize_path(Path::new("src/main.rs")).is_ok());
        assert!(sandbox.canonicalize_path(Path::new("../secret")).is_err());
        assert!(sandbox
            .check_permission(PermissionCategory::FileRead)
            .is_ok());
        assert!(sandbox
            .check_permission(PermissionCategory::ShellExecution)
            .is_err());
    }

    #[test]
    fn test_agent_state_serialization_defaults() {
        let mut agent = MockAgent {
            id: "agent_state_01".to_string(),
            name: "stateful".to_string(),
        };
        assert_eq!(agent.serialize_state(), None);
        assert!(agent.restore_state(json!({"step": 1})).is_ok());
    }

    struct PassthroughMiddleware;
    impl AgentMiddleware for PassthroughMiddleware {
        fn name(&self) -> &str {
            "passthrough"
        }
    }

    #[tokio::test]
    async fn test_agent_middleware_default_hooks() {
        let mw = PassthroughMiddleware;
        assert_eq!(mw.name(), "passthrough");

        let mut msgs = vec![Message::user("u1", "hello")];
        let ctx = ToolContext::new("/test", "s1", "a1");
        assert!(mw.before_turn(&mut msgs, &ctx).await.is_ok());

        let mut outcome = StepOutcome::Completed(Message::assistant("a1", "hi"));
        assert!(mw.after_turn(&mut outcome, &ctx).await.is_ok());

        let err = ToolError::ExecutionFailed {
            name: "tool".to_string(),
            reason: "fail".to_string(),
        };
        let res = mw.on_tool_error("tool", &err, &ctx).await.unwrap();
        assert!(res.is_none());
    }

    #[test]
    fn test_skill_definition_creation_and_matching() {
        let skill = SkillDefinition::new("cargo-audit", "Audits dependencies", "Run cargo audit")
            .with_triggers(["audit", "security", "deps"])
            .with_source_path(".kai/skills/cargo-audit.md");

        assert_eq!(skill.name, "cargo-audit");
        assert_eq!(
            skill.source_path.as_deref(),
            Some(".kai/skills/cargo-audit.md")
        );
        assert!(skill.matches_query("Please audit our codebase"));
        assert!(skill.matches_query("Check security vulnerabilities"));
        assert!(skill.matches_query("Run cargo-audit now"));
        assert!(!skill.matches_query("Optimize database queries"));
    }

    #[test]
    fn test_approval_decision_helpers_and_policy() {
        let app = ApprovalDecision::Approved;
        assert!(app.is_approved());
        assert!(!app.requires_confirmation());
        assert!(!app.is_denied());

        let conf = ApprovalDecision::RequiresConfirmation {
            reason: "destructive command".to_string(),
        };
        assert!(!conf.is_approved());
        assert!(conf.requires_confirmation());
        assert!(!conf.is_denied());

        let den = ApprovalDecision::Denied {
            reason: "blocked resource".to_string(),
        };
        assert!(!den.is_approved());
        assert!(!den.requires_confirmation());
        assert!(den.is_denied());

        let policy = AlwaysApprovePolicy;
        let ctx = ToolContext::new("/workspace", "s1", "a1");
        let dec = policy.evaluate("exec_command", &json!({}), &ctx);
        assert!(dec.is_approved());
    }

    struct MockDispatcher;
    impl TaskDispatcher for MockDispatcher {
        fn dispatch_task<'a>(
            &'a self,
            sub_agent_id: &'a str,
            task_description: &'a str,
            _context: &'a ToolContext,
        ) -> BoxFuture<'a, Result<String>> {
            Box::pin(async move {
                Ok(format!(
                    "Sub-agent {sub_agent_id} completed: {task_description}"
                ))
            })
        }
    }

    #[tokio::test]
    async fn test_task_dispatcher_contract() {
        let dispatcher = MockDispatcher;
        let ctx = ToolContext::new("/workspace", "s1", "a1");
        let res = dispatcher
            .dispatch_task("researcher", "find benchmarks", &ctx)
            .await
            .unwrap();
        assert_eq!(res, "Sub-agent researcher completed: find benchmarks");
    }
}
