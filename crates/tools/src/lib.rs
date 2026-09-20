//! # kai-tools
//!
//! System tool execution engine for KAI (Krill Agent Interface).
//!
//! Provides production-ready implementations of core agent tools adhering strictly
//! to zero-bloat constraints, RAII process termination, transactional filesystem
//! mutations, and deterministic output truncation:
//!
//! - [`ReadWindowTool`]: Bounded windowed file reader (strictly caps at 150 lines).
//! - [`ApplyPatchTool`]: Transactional unified diff patcher with atomic replacement.
//! - [`ExecCommandTool`]: Non-blocking subprocess execution with RAII termination guards.
//! - [`BrowserActionTool`]: Headless browser automation contract and offline mock drivers.
//! - [`McpClientTool`]: Model Context Protocol (MCP) JSON-RPC 2.0 tool discovery and caller.

use std::sync::Arc;

pub mod apply_patch;
pub mod browser;
pub mod cache;
pub mod exec_command;
pub mod list_dir;
pub mod mcp;
pub mod patcher;
pub mod read_window;
pub mod skill;

pub use apply_patch::{ApplyPatchArgs, ApplyPatchTool, DiffHunk};
pub use browser::{
    BrowserActionArgs, BrowserActionOutcome, BrowserActionTool, BrowserDriver, BrowserPageInfo,
    MockBrowserDriver,
};
pub use cache::ToolResultCache;
pub use exec_command::{
    ExecCommandArgs, ExecCommandTool, ProcessGuard, DEFAULT_COMMAND_TIMEOUT_MS,
    MAX_COMMAND_TIMEOUT_MS, MIN_COMMAND_TIMEOUT_MS,
};
pub use list_dir::{ListDirArgs, ListDirTool, DEFAULT_LIST_DIR_LIMIT, MAX_LIST_DIR_LIMIT};
pub use mcp::{
    McpCallResult, McpClient, McpClientArgs, McpClientTool, McpContent, McpToolDefinition,
    McpTransport, MockMcpTransport,
};
pub use patcher::{FuzzyBlockPatcher, DEFAULT_SIMILARITY_THRESHOLD};
pub use read_window::{ReadWindowArgs, ReadWindowTool, DEFAULT_WINDOW_LIMIT, MAX_WINDOW_LIMIT};
pub use skill::{LearnSkillTool, SkillRegistry};

/// Returns a default registry of core KAI tools configured for local execution.
pub fn default_tools() -> Vec<Arc<dyn kai_core::Tool>> {
    vec![
        Arc::new(ListDirTool::new()),
        Arc::new(ReadWindowTool::new()),
        Arc::new(ApplyPatchTool::new()),
        Arc::new(ExecCommandTool::new()),
        Arc::new(BrowserActionTool::new()),
        Arc::new(McpClientTool::new()),
        Arc::new(LearnSkillTool::new()),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use kai_core::traits::PermissionCategory;
    use kai_core::Tool;

    #[test]
    fn test_default_tools_registry() {
        let tools = default_tools();
        assert_eq!(tools.len(), 7);

        let names: Vec<&str> = tools.iter().map(|t| t.name()).collect();
        assert!(names.contains(&"list_dir"));
        assert!(names.contains(&"read_window"));
        assert!(names.contains(&"apply_patch"));
        assert!(names.contains(&"exec_command"));
        assert!(names.contains(&"browser_action"));
        assert!(names.contains(&"mcp_client"));
        assert!(names.contains(&"learn_skill"));
    }

    #[test]
    fn test_tool_permissions_and_schemas() {
        let tools = default_tools();
        for tool in tools {
            assert!(!tool.name().is_empty());
            assert!(!tool.description().is_empty());
            let schema = tool.schema();
            assert_eq!(schema.get("type").and_then(|v| v.as_str()), Some("object"));
        }

        let rw = ReadWindowTool::new();
        assert_eq!(rw.permission_category(), PermissionCategory::FileRead);
        assert!(rw.is_read_only());

        let ap = ApplyPatchTool::new();
        assert_eq!(ap.permission_category(), PermissionCategory::FileWrite);
        assert!(!ap.is_read_only());

        let ec = ExecCommandTool::new();
        assert_eq!(ec.permission_category(), PermissionCategory::ShellExecution);
        assert!(!ec.is_read_only());

        let ba = BrowserActionTool::new();
        assert_eq!(ba.permission_category(), PermissionCategory::BrowserControl);
        assert!(!ba.is_read_only());

        let mc = McpClientTool::new();
        assert_eq!(mc.permission_category(), PermissionCategory::NetworkAccess);
        assert!(!mc.is_read_only());
    }
}
