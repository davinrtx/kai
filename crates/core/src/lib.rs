//! # kai-core
//!
//! Foundational contracts, message schemas, traits, and error hierarchies for the KAI agent runtime.
//!
//! This crate defines Level 1 of the hexagonal architecture, providing pure traits and data types
//! without dependencies on concrete infrastructure or downstream crates.

pub mod error;
pub mod event;
pub mod message;
pub mod traits;

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
    Message, Role, TokenUsage, ToolCall, ToolResult, MAX_TOOL_OUTPUT_BYTES, MAX_TOOL_OUTPUT_ITEMS,
    TRUNCATION_BYTE_NOTICE,
};
pub use traits::{
    Agent, BoxFuture, ContextProcessor, PermissionCategory, SandboxPolicy, SessionNode,
    SessionStore, StepOutcome, Tool, ToolContext,
};
