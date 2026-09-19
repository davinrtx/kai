//! Canonical path resolution, confinement boundaries, and protected resource shielding.
//!
//! Provides [`PathResolver`] enforcing strict filesystem containment within designated
//! workspace roots, preventing directory traversal attacks (`..`), symlink escapes,
//! and blocking access to sensitive credential and secret files.

use std::path::{Component, Path, PathBuf};

use kai_core::error::SandboxError;

/// Core filesystem confinement and canonical path resolution engine.
#[derive(Debug, Clone)]
pub struct PathResolver {
    workspace_root: PathBuf,
    allowed_roots: Vec<PathBuf>,
    read_only_paths: Vec<PathBuf>,
}

impl PathResolver {
    /// Constructs a new [`PathResolver`] rooted at the specified workspace directory.
    ///
    /// The root path is immediately canonicalized to resolve symlinks and absolute prefixes.
    pub fn new(workspace_root: impl AsRef<Path>) -> Result<Self, SandboxError> {
        let root = workspace_root.as_ref();
        let canonical_root =
            root.canonicalize()
                .map_err(|err| SandboxError::PathTraversalDetected {
                    path: format!("{}: {err}", root.display()),
                })?;

        Ok(Self {
            workspace_root: canonical_root,
            allowed_roots: Vec::new(),
            read_only_paths: Vec::new(),
        })
    }

    /// Appends an additional allowed root directory (e.g. temporary build or scratch directories).
    pub fn with_allowed_root(mut self, root: impl AsRef<Path>) -> Result<Self, SandboxError> {
        let canonical =
            root.as_ref()
                .canonicalize()
                .map_err(|err| SandboxError::PathTraversalDetected {
                    path: format!("{}: {err}", root.as_ref().display()),
                })?;
        self.allowed_roots.push(canonical);
        Ok(self)
    }

    /// Designates a subpath within the workspace as strictly read-only.
    pub fn with_read_only_path(mut self, path: impl AsRef<Path>) -> Result<Self, SandboxError> {
        let canonical = self.canonicalize_path(path.as_ref())?;
        self.read_only_paths.push(canonical);
        Ok(self)
    }

    /// Returns the canonical workspace root path.
    pub fn workspace_root(&self) -> &Path {
        &self.workspace_root
    }

    /// Returns the list of secondary allowed root directories.
    pub fn allowed_roots(&self) -> &[PathBuf] {
        &self.allowed_roots
    }

    /// Checks if a file path targets a protected secret, token, or credential file.
    pub fn is_protected_resource(path: &Path) -> bool {
        let file_name = match path.file_name().and_then(|n| n.to_str()) {
            Some(name) => name.to_ascii_lowercase(),
            None => return false,
        };

        // 1. Environment and secret files
        if file_name == ".env" || file_name.starts_with(".env.") {
            return true;
        }

        // 2. Cryptographic keys and certificates
        let sensitive_extensions = [
            "pem", "key", "p12", "pfx", "pkcs12", "kdbx", "jks", "keystore",
        ];
        if let Some(ext) = path.extension().and_then(|e| e.to_str()) {
            let lower_ext = ext.to_ascii_lowercase();
            if sensitive_extensions.contains(&lower_ext.as_str()) {
                return true;
            }
        }

        // 3. SSH private keys
        if file_name.starts_with("id_rsa")
            || file_name.starts_with("id_dsa")
            || file_name.starts_with("id_ecdsa")
            || file_name.starts_with("id_ed25519")
        {
            return true;
        }

        // 4. Common credentials and token storage files
        if file_name == "credentials"
            || file_name == "credentials.json"
            || file_name == "secrets.json"
            || file_name == "secrets.yaml"
            || file_name == "secrets.yml"
            || file_name == "secrets.toml"
            || file_name == "shadow"
            || file_name == "master.passwd"
            || file_name == ".netrc"
            || file_name == ".npmrc"
            || file_name.ends_with(".token")
            || file_name == "token.json"
        {
            return true;
        }

        // 5. Sensitive directories or paths
        for comp in path.components() {
            if let Component::Normal(os_str) = comp {
                let s = os_str.to_string_lossy().to_ascii_lowercase();
                if s == ".ssh" || s == ".aws" {
                    return true;
                }
            }
        }

        false
    }

    /// Canonicalizes the path and verifies that it is strictly confined within authorized boundaries.
    ///
    /// Automatically detects and rejects:
    /// - Path traversal via `..` escaping root
    /// - Symlink redirection escaping authorized roots
    /// - Access to protected files (`.env`, `.pem`, `id_rsa`, tokens)
    pub fn canonicalize_path(&self, path: &Path) -> Result<PathBuf, SandboxError> {
        let path_str = path.to_string_lossy();
        if path_str.contains('\0') {
            return Err(SandboxError::PathTraversalDetected {
                path: "<null byte in path>".to_string(),
            });
        }

        // 1. Anchor relative paths to the workspace root
        let target = if path.is_absolute() {
            path.to_path_buf()
        } else {
            self.workspace_root.join(path)
        };

        // 2. Canonicalize path
        let resolved = if target.exists() {
            target
                .canonicalize()
                .map_err(|err| SandboxError::PathTraversalDetected {
                    path: format!("{}: {err}", target.display()),
                })?
        } else {
            // For files targeted for creation that do not yet exist, find nearest existing ancestor
            self.resolve_nonexistent(&target)?
        };

        // 3. Verify boundary confinement: must start with workspace_root or an allowed root
        let within_boundary = resolved.starts_with(&self.workspace_root)
            || self.allowed_roots.iter().any(|r| resolved.starts_with(r));

        if !within_boundary {
            return Err(SandboxError::PathTraversalDetected {
                path: path.display().to_string(),
            });
        }

        // 4. Verify protected resource defense
        if Self::is_protected_resource(&resolved) {
            return Err(SandboxError::ProtectedResourceAccess {
                path: path.display().to_string(),
                reason:
                    "Access to credentials, tokens, or secret files is blocked by sandbox policy"
                        .to_string(),
            });
        }

        Ok(resolved)
    }

    /// Checks if a canonicalized path is within a designated read-only path zone.
    pub fn is_read_only(&self, path: &Path) -> bool {
        self.read_only_paths.iter().any(|ro| path.starts_with(ro))
    }

    /// Resolves non-existent file paths by canonicalizing their nearest existing ancestor directory.
    fn resolve_nonexistent(&self, target: &Path) -> Result<PathBuf, SandboxError> {
        let mut components: Vec<&Path> = Vec::new();
        let mut curr: &Path = target;

        while !curr.exists() {
            if let Some(name) = curr.file_name() {
                components.push(Path::new(name));
                if let Some(parent) = curr.parent() {
                    curr = parent;
                } else {
                    break;
                }
            } else {
                break;
            }
        }

        if !curr.exists() {
            return Err(SandboxError::PathTraversalDetected {
                path: format!("Cannot resolve existing ancestor for {}", target.display()),
            });
        }

        let canonical_ancestor =
            curr.canonicalize()
                .map_err(|err| SandboxError::PathTraversalDetected {
                    path: format!("{}: {err}", curr.display()),
                })?;

        let mut resolved = canonical_ancestor;
        // Re-append the non-existent child components in original hierarchy order
        for comp in components.into_iter().rev() {
            let comp_str = comp.to_string_lossy();
            if comp_str == ".." || comp_str == "." || comp_str.is_empty() {
                return Err(SandboxError::PathTraversalDetected {
                    path: target.display().to_string(),
                });
            }
            resolved.push(comp);
        }

        Ok(resolved)
    }
}
