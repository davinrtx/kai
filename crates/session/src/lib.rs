//! # kai-session
//!
//! Branchable session Directed Acyclic Graph (DAG), transactional persistence,
//! and automated context compaction for autonomous agent workflows.
//!
//! Provides the foundational session persistence and branching layer:
//! - [`MemorySessionStore`]: In-memory, cycle-safe DAG store implementing [`kai_core::traits::SessionStore`].
//! - [`FileSessionStore`]: Transactional on-disk persistence enforcing atomic file replacement.
//! - [`BranchManager`]: Named branch head coordination, checkout, forking, and DAG merge handling.
//! - [`AutoCompactor`]: Automatic turn depth and token budget monitoring and condensation.

pub mod branch;
pub mod compactor;
pub mod graph;
pub mod persistence;
pub mod search;
pub mod trajectory;

pub use branch::{BranchManager, DEFAULT_BRANCH_NAME};
pub use compactor::{
    AutoCompactor, DEFAULT_KEEP_RECENT_TURNS, DEFAULT_MAX_TOKENS_THRESHOLD,
    DEFAULT_MAX_TURNS_THRESHOLD,
};
pub use graph::{MemorySessionStore, SessionGraphData};
pub use persistence::FileSessionStore;
pub use search::{SearchResult, SessionSearchIndex};
pub use trajectory::{TrajectoryExporter, TrajectoryFormat};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_session_defaults() {
        assert_eq!(DEFAULT_BRANCH_NAME, "main");
        assert_eq!(DEFAULT_MAX_TURNS_THRESHOLD, 20);
        assert_eq!(DEFAULT_MAX_TOKENS_THRESHOLD, 8192);
        assert_eq!(DEFAULT_KEEP_RECENT_TURNS, 5);
    }
}
