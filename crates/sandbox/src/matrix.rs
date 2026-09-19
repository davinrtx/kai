//! Capability-based authorization matrix and security profiles.
//!
//! Provides [`PermissionMatrix`] governing granular permissions across all
//! [`PermissionCategory`] actions (filesystem, shell, network, browser).

use std::collections::HashMap;

use kai_core::error::SandboxError;
use kai_core::traits::PermissionCategory;

/// Authorization decision for a requested permission category.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AccessDecision {
    /// Action is explicitly granted.
    Allow,
    /// Action is blocked by sandbox policy.
    Deny,
}

impl AccessDecision {
    /// Returns true if access is permitted.
    pub fn is_allowed(&self) -> bool {
        matches!(self, Self::Allow)
    }
}

/// Dynamic permission matrix mapping [`PermissionCategory`] to [`AccessDecision`].
#[derive(Debug, Clone)]
pub struct PermissionMatrix {
    rules: HashMap<PermissionCategory, AccessDecision>,
}

impl Default for PermissionMatrix {
    fn default() -> Self {
        Self::standard()
    }
}

impl PermissionMatrix {
    /// Constructs a standard balanced security profile:
    /// - `FileRead`: Allowed
    /// - `FileWrite`: Allowed
    /// - `ShellExecution`: Allowed
    /// - `NetworkAccess`: Denied
    /// - `BrowserControl`: Denied
    pub fn standard() -> Self {
        let mut rules = HashMap::new();
        rules.insert(PermissionCategory::FileRead, AccessDecision::Allow);
        rules.insert(PermissionCategory::FileWrite, AccessDecision::Allow);
        rules.insert(PermissionCategory::ShellExecution, AccessDecision::Allow);
        rules.insert(PermissionCategory::NetworkAccess, AccessDecision::Deny);
        rules.insert(PermissionCategory::BrowserControl, AccessDecision::Deny);
        Self { rules }
    }

    /// Constructs a strict lock-down security profile:
    /// - `FileRead`: Allowed
    /// - All other categories: Denied
    pub fn strict() -> Self {
        let mut rules = HashMap::new();
        rules.insert(PermissionCategory::FileRead, AccessDecision::Allow);
        rules.insert(PermissionCategory::FileWrite, AccessDecision::Deny);
        rules.insert(PermissionCategory::ShellExecution, AccessDecision::Deny);
        rules.insert(PermissionCategory::NetworkAccess, AccessDecision::Deny);
        rules.insert(PermissionCategory::BrowserControl, AccessDecision::Deny);
        Self { rules }
    }

    /// Constructs a read-only profile:
    /// Only `FileRead` is authorized; writing and subprocesses are denied.
    pub fn read_only() -> Self {
        Self::strict()
    }

    /// Constructs a permissive profile allowing all standard capabilities.
    ///
    /// Note: Path confinement and protected file shielding remain active
    /// regardless of permission matrix settings.
    pub fn permissive() -> Self {
        let mut rules = HashMap::new();
        rules.insert(PermissionCategory::FileRead, AccessDecision::Allow);
        rules.insert(PermissionCategory::FileWrite, AccessDecision::Allow);
        rules.insert(PermissionCategory::ShellExecution, AccessDecision::Allow);
        rules.insert(PermissionCategory::NetworkAccess, AccessDecision::Allow);
        rules.insert(PermissionCategory::BrowserControl, AccessDecision::Allow);
        Self { rules }
    }

    /// Grants permission for the specified category.
    pub fn allow(&mut self, category: PermissionCategory) -> &mut Self {
        self.rules.insert(category, AccessDecision::Allow);
        self
    }

    /// Revokes permission for the specified category.
    pub fn deny(&mut self, category: PermissionCategory) -> &mut Self {
        self.rules.insert(category, AccessDecision::Deny);
        self
    }

    /// Queries the authorization decision for the specified category.
    /// Defaults to [`AccessDecision::Deny`] if not explicitly configured.
    pub fn decision(&self, category: PermissionCategory) -> AccessDecision {
        match self.rules.get(&category) {
            Some(&decision) => decision,
            None => AccessDecision::Deny,
        }
    }

    /// Returns true if the specified category is granted access.
    pub fn is_allowed(&self, category: PermissionCategory) -> bool {
        self.decision(category).is_allowed()
    }

    /// Enforces permission check, returning [`Ok(())`] on allow or [`SandboxError::PermissionDenied`].
    pub fn check_permission(&self, category: PermissionCategory) -> Result<(), SandboxError> {
        if self.is_allowed(category) {
            Ok(())
        } else {
            Err(SandboxError::PermissionDenied {
                operation: format!("{category:?}"),
                resource: "sandbox".to_string(),
            })
        }
    }
}
