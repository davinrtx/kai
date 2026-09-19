//! # kai-sandbox
//!
//! Confinement, canonical path resolution, capability permissions, and subprocess security.
//!
//! Provides the core security enforcement layer for autonomous agents:
//! - [`PathResolver`]: Canonical path resolution, symlink boundary checks, and credential file defense.
//! - [`PermissionMatrix`]: Granular capability-based authorization per [`kai_core::traits::PermissionCategory`].
//! - [`CommandSanitizer`]: Shell command safety validation and sensitive environment variable scrubbing.
//! - [`StandardSandboxPolicy`]: Integrated implementation of [`kai_core::traits::SandboxPolicy`].

pub mod command;
pub mod isolation;
pub mod matrix;
pub mod path;
pub mod policy;

pub use command::{CommandSanitizer, BLOCKED_ENV_SUBSTRINGS};
pub use isolation::BubblewrapIsolationEngine;
pub use matrix::{AccessDecision, PermissionMatrix};
pub use path::PathResolver;
pub use policy::{SandboxPolicyBuilder, StandardSandboxPolicy};

#[cfg(test)]
mod tests {
    use super::*;
    use kai_core::traits::PermissionCategory;

    #[test]
    fn test_sandbox_defaults() {
        let matrix = PermissionMatrix::standard();
        assert!(matrix.is_allowed(PermissionCategory::FileRead));
        assert!(matrix.is_allowed(PermissionCategory::FileWrite));
        assert!(matrix.is_allowed(PermissionCategory::ShellExecution));
        assert!(!matrix.is_allowed(PermissionCategory::NetworkAccess));
        assert!(!matrix.is_allowed(PermissionCategory::BrowserControl));
    }
}
