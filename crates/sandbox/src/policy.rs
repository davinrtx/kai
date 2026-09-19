//! Unified sandbox policy implementing [`SandboxPolicy`].
//!
//! Provides [`StandardSandboxPolicy`] integrating canonical path resolution,
//! capability-based permission matrices, and shell command sanitization into
//! a cohesive, thread-safe confinement engine.

use std::path::{Path, PathBuf};

use kai_core::error::SandboxError;
use kai_core::traits::{PermissionCategory, SandboxPolicy};

use crate::command::CommandSanitizer;
use crate::matrix::PermissionMatrix;
use crate::path::PathResolver;

/// Production-grade implementation of [`SandboxPolicy`].
#[derive(Debug, Clone)]
pub struct StandardSandboxPolicy {
    path_resolver: PathResolver,
    matrix: PermissionMatrix,
    sanitizer: CommandSanitizer,
}

impl StandardSandboxPolicy {
    /// Constructs a [`StandardSandboxPolicy`] for the designated workspace root
    /// using standard security permissions.
    pub fn new(workspace_root: impl AsRef<Path>) -> Result<Self, SandboxError> {
        let path_resolver = PathResolver::new(workspace_root)?;
        Ok(Self {
            path_resolver,
            matrix: PermissionMatrix::standard(),
            sanitizer: CommandSanitizer::new(),
        })
    }

    /// Creates a builder for custom sandbox configuration.
    pub fn builder(workspace_root: impl AsRef<Path>) -> SandboxPolicyBuilder {
        SandboxPolicyBuilder::new(workspace_root)
    }

    /// Returns a reference to the inner [`PathResolver`].
    pub fn path_resolver(&self) -> &PathResolver {
        &self.path_resolver
    }

    /// Returns a reference to the inner [`PermissionMatrix`].
    pub fn permission_matrix(&self) -> &PermissionMatrix {
        &self.matrix
    }

    /// Returns a reference to the inner [`CommandSanitizer`].
    pub fn command_sanitizer(&self) -> &CommandSanitizer {
        &self.sanitizer
    }

    /// Validates both execution permission and command safety for shell commands.
    pub fn validate_execution(&self, command: &str) -> Result<(), SandboxError> {
        self.matrix
            .check_permission(PermissionCategory::ShellExecution)?;
        self.sanitizer.validate_command(command)?;
        Ok(())
    }

    /// Scrubs sensitive credentials and tokens from process environment variables.
    pub fn scrub_environment<K, V>(
        &self,
        env: impl IntoIterator<Item = (K, V)>,
    ) -> Vec<(String, String)>
    where
        K: Into<String>,
        V: Into<String>,
    {
        CommandSanitizer::scrub_env(env)
    }
}

impl SandboxPolicy for StandardSandboxPolicy {
    fn canonicalize_path(&self, path: &Path) -> std::result::Result<PathBuf, SandboxError> {
        self.path_resolver.canonicalize_path(path)
    }

    fn check_permission(
        &self,
        category: PermissionCategory,
    ) -> std::result::Result<(), SandboxError> {
        self.matrix.check_permission(category)
    }
}

/// Fluent builder for [`StandardSandboxPolicy`].
#[derive(Debug)]
pub struct SandboxPolicyBuilder {
    workspace_root: PathBuf,
    allowed_roots: Vec<PathBuf>,
    read_only_paths: Vec<PathBuf>,
    matrix: PermissionMatrix,
    sanitizer: CommandSanitizer,
}

impl SandboxPolicyBuilder {
    /// Initializes builder with mandatory workspace root.
    pub fn new(workspace_root: impl AsRef<Path>) -> Self {
        Self {
            workspace_root: workspace_root.as_ref().to_path_buf(),
            allowed_roots: Vec::new(),
            read_only_paths: Vec::new(),
            matrix: PermissionMatrix::standard(),
            sanitizer: CommandSanitizer::new(),
        }
    }

    /// Sets the permission matrix profile.
    pub fn with_permission_matrix(mut self, matrix: PermissionMatrix) -> Self {
        self.matrix = matrix;
        self
    }

    /// Adds an additional allowed root directory.
    pub fn with_allowed_root(mut self, root: impl AsRef<Path>) -> Self {
        self.allowed_roots.push(root.as_ref().to_path_buf());
        self
    }

    /// Designates a path as strictly read-only.
    pub fn with_read_only_path(mut self, path: impl AsRef<Path>) -> Self {
        self.read_only_paths.push(path.as_ref().to_path_buf());
        self
    }

    /// Adds custom blocked command signatures.
    pub fn with_blocked_command_pattern(mut self, pattern: impl Into<String>) -> Self {
        self.sanitizer = self.sanitizer.with_blocked_pattern(pattern);
        self
    }

    /// Builds and validates the [`StandardSandboxPolicy`].
    pub fn build(self) -> Result<StandardSandboxPolicy, SandboxError> {
        let mut path_resolver = PathResolver::new(&self.workspace_root)?;

        for root in self.allowed_roots {
            path_resolver = path_resolver.with_allowed_root(&root)?;
        }

        for ro in self.read_only_paths {
            path_resolver = path_resolver.with_read_only_path(&ro)?;
        }

        Ok(StandardSandboxPolicy {
            path_resolver,
            matrix: self.matrix,
            sanitizer: self.sanitizer,
        })
    }
}
