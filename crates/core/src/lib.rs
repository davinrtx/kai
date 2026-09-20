//! # kai-core
//!
//! Foundational contracts, message schemas, traits, and error hierarchies for the KAI agent runtime.
//!
//! This crate defines Level 1 of the hexagonal architecture, providing pure traits and data types
//! without dependencies on concrete infrastructure or downstream crates.

pub mod cache;
pub mod error;
pub mod event;
pub mod message;
pub mod traits;

pub use cache::ToolResultCache;

pub use error::{
    ConfigError, ContextError, InferenceError, InternalError, KaiError, OrchestratorError, Result,
    SandboxError, SessionError, ToolError,
};
pub use event::{
    check_steering_signal, global_steering_channel, steering_channel, Event, EventBus,
    EventBusError, GlobalSteeringReceiver, GlobalSteeringSender, SteeringReceiver, SteeringSender,
    SteeringSignal, SteeringState,
};
pub use message::{
    current_timestamp_ms, truncate_items, truncate_output, truncate_tool_output, ContentBlock,
    FailureTombstone, Message, Role, TokenUsage, ToolCall, ToolResult, MAX_TOOL_OUTPUT_BYTES,
    MAX_TOOL_OUTPUT_ITEMS, TRUNCATION_BYTE_NOTICE,
};
pub use traits::{
    Agent, AgentMiddleware, AlwaysApprovePolicy, ApprovalDecision, BoxFuture, CodePatcher,
    CommandIsolationEngine, CommandOutputCompressor, ContextProcessor, GrammarLoader,
    IsolatedCommandSpec, PatchApplicationResult, PatchBlock, PermissionCategory, SandboxPolicy,
    SemanticAnalyzer, SessionNode, SessionStore, SkillDefinition, StepOutcome, SymbolLocation,
    TaskDispatcher, Tool, ToolApprovalPolicy, ToolContext, WorkspaceManager, WorkspaceProposal,
    WorktreeScope,
};

pub use serde;
pub use serde_json;
