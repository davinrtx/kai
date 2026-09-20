//! Ephemeral Git Worktree isolation manager for sub-agents.
//!
//! Provides isolated filesystem worktrees (`git worktree add --detach`) to eliminate
//! multi-agent concurrency file locks and race conditions on the root repository.
//! Sub-agents operate in isolated workspaces and submit immutable [`WorkspaceProposal`]s
//! to the root agent for deterministic review and merging.
//! Cleans up ephemeral worktrees automatically upon [`Drop`].

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::SystemTime;

use kai_core::error::{KaiError, OrchestratorError, Result};
use kai_core::traits::{BoxFuture, WorkspaceManager, WorkspaceProposal, WorktreeScope};

/// Ephemeral git worktree manager provisioning isolated directory trees.
#[derive(Debug, Clone)]
pub struct GitWorkspaceManager {
    repo_root: PathBuf,
    worktree_base: PathBuf,
}

impl GitWorkspaceManager {
    /// Constructs a new [`GitWorkspaceManager`] with worktrees placed under `.kai/worktrees`.
    pub fn new(repo_root: impl Into<PathBuf>) -> Self {
        let root = repo_root.into();
        let base = root.join(".kai").join("worktrees");
        Self {
            repo_root: root,
            worktree_base: base,
        }
    }

    /// Constructs a new [`GitWorkspaceManager`] with a custom worktree base directory.
    pub fn with_base(repo_root: impl Into<PathBuf>, worktree_base: impl Into<PathBuf>) -> Self {
        Self {
            repo_root: repo_root.into(),
            worktree_base: worktree_base.into(),
        }
    }

    /// Returns the repository root path.
    pub fn repo_root(&self) -> &Path {
        &self.repo_root
    }

    /// Returns the base directory for ephemeral worktrees.
    pub fn worktree_base(&self) -> &Path {
        &self.worktree_base
    }
}

/// Active handle to an isolated ephemeral git worktree.
///
/// Ensures strict RAII cleanup on drop by invoking `git worktree remove --force`
/// and wiping leftover files.
#[derive(Debug)]
pub struct EphemeralWorktreeHandle {
    repo_root: PathBuf,
    path: PathBuf,
    agent_id: String,
    base_commit: String,
    cleaned_up: Arc<AtomicBool>,
}

impl EphemeralWorktreeHandle {
    /// Constructs a new [`EphemeralWorktreeHandle`].
    pub fn new(
        repo_root: PathBuf,
        path: PathBuf,
        agent_id: impl Into<String>,
        base_commit: impl Into<String>,
    ) -> Self {
        Self {
            repo_root,
            path,
            agent_id: agent_id.into(),
            base_commit: base_commit.into(),
            cleaned_up: Arc::new(AtomicBool::new(false)),
        }
    }

    /// Returns whether this worktree has already been cleaned up.
    pub fn is_cleaned(&self) -> bool {
        self.cleaned_up.load(Ordering::SeqCst)
    }

    /// Explicitly forces cleanup of the worktree directory and git metadata.
    pub fn force_cleanup(&self) -> Result<()> {
        if !self.cleaned_up.swap(true, Ordering::SeqCst) {
            Self::perform_cleanup(&self.repo_root, &self.path);
        }
        Ok(())
    }

    /// Internal synchronous cleanup routine invoked on explicit request or [`Drop`].
    fn perform_cleanup(repo_root: &Path, worktree_path: &Path) {
        // Attempt git worktree remove --force
        let _ = std::process::Command::new("git")
            .arg("worktree")
            .arg("remove")
            .arg("--force")
            .arg(worktree_path)
            .current_dir(repo_root)
            .output();

        // Fallback directory wipe if git leaves artifacts
        if worktree_path.exists() {
            let _ = std::fs::remove_dir_all(worktree_path);
        }

        // Prune stale worktree references
        let _ = std::process::Command::new("git")
            .arg("worktree")
            .arg("prune")
            .current_dir(repo_root)
            .output();
    }
}

impl WorktreeScope for EphemeralWorktreeHandle {
    fn path(&self) -> &Path {
        &self.path
    }

    fn generate_proposal(&self) -> Result<WorkspaceProposal> {
        // Stage intent-to-add for untracked files so git diff HEAD captures newly added files
        let _ = std::process::Command::new("git")
            .arg("add")
            .arg("-N")
            .arg(".")
            .current_dir(&self.path)
            .output();

        let stat_output = std::process::Command::new("git")
            .arg("diff")
            .arg("--stat")
            .arg("HEAD")
            .current_dir(&self.path)
            .output()
            .map_err(|e| {
                KaiError::Orchestrator(OrchestratorError::SubAgentFailed {
                    agent_id: self.agent_id.clone(),
                    reason: format!("Failed to generate diff stat: {e}"),
                })
            })?;

        let patch_output = std::process::Command::new("git")
            .arg("diff")
            .arg("HEAD")
            .current_dir(&self.path)
            .output()
            .map_err(|e| {
                KaiError::Orchestrator(OrchestratorError::SubAgentFailed {
                    agent_id: self.agent_id.clone(),
                    reason: format!("Failed to generate diff patch: {e}"),
                })
            })?;

        let diff_stat = String::from_utf8_lossy(&stat_output.stdout)
            .trim()
            .to_string();
        let patch_payload = String::from_utf8_lossy(&patch_output.stdout).to_string();

        Ok(WorkspaceProposal {
            agent_id: self.agent_id.clone(),
            base_commit: self.base_commit.clone(),
            diff_stat,
            patch_payload,
        })
    }
}

impl Drop for EphemeralWorktreeHandle {
    fn drop(&mut self) {
        if !self.cleaned_up.swap(true, Ordering::SeqCst) {
            Self::perform_cleanup(&self.repo_root, &self.path);
        }
    }
}

impl WorkspaceManager for GitWorkspaceManager {
    type Handle = EphemeralWorktreeHandle;

    fn create_ephemeral_worktree<'a>(
        &'a self,
        agent_id: &'a str,
        commit_ish: &'a str,
    ) -> BoxFuture<'a, Result<Self::Handle>> {
        Box::pin(async move {
            let unique_ts = SystemTime::now()
                .duration_since(SystemTime::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0);
            let sanitized_agent = agent_id.replace(['/', '\\', ' ', ':'], "_");
            let target_path = self.worktree_base.join(format!(
                "wt-{sanitized_agent}-{}-{unique_ts}",
                std::process::id()
            ));

            if let Some(parent) = target_path.parent() {
                let _ = tokio::fs::create_dir_all(parent).await;
            }

            // Resolve target commit SHA asynchronously without blocking worker threads
            let rev_output = tokio::process::Command::new("git")
                .arg("rev-parse")
                .arg(commit_ish)
                .current_dir(&self.repo_root)
                .output()
                .await
                .map_err(|e| {
                    KaiError::Orchestrator(OrchestratorError::SubAgentFailed {
                        agent_id: agent_id.to_string(),
                        reason: format!("Failed to resolve commit '{commit_ish}': {e}"),
                    })
                })?;

            if !rev_output.status.success() {
                let err_msg = String::from_utf8_lossy(&rev_output.stderr);
                return Err(KaiError::Orchestrator(OrchestratorError::SubAgentFailed {
                    agent_id: agent_id.to_string(),
                    reason: format!("Commit '{commit_ish}' resolution failed: {err_msg}"),
                }));
            }

            let base_commit = String::from_utf8_lossy(&rev_output.stdout)
                .trim()
                .to_string();

            // Spawn detached git worktree asynchronously
            let wt_output = tokio::process::Command::new("git")
                .arg("worktree")
                .arg("add")
                .arg("--detach")
                .arg(&target_path)
                .arg(&base_commit)
                .current_dir(&self.repo_root)
                .output()
                .await
                .map_err(|e| {
                    KaiError::Orchestrator(OrchestratorError::SubAgentFailed {
                        agent_id: agent_id.to_string(),
                        reason: format!("Failed to invoke git worktree add: {e}"),
                    })
                })?;

            if !wt_output.status.success() {
                let err_msg = String::from_utf8_lossy(&wt_output.stderr);
                return Err(KaiError::Orchestrator(OrchestratorError::SubAgentFailed {
                    agent_id: agent_id.to_string(),
                    reason: format!("Failed to add ephemeral worktree: {err_msg}"),
                }));
            }

            Ok(EphemeralWorktreeHandle::new(
                self.repo_root.clone(),
                target_path,
                agent_id,
                base_commit,
            ))
        })
    }
}

/// In-memory mock workspace scope for isolated unit testing without requiring git.
#[derive(Debug)]
pub struct MockWorktreeScope {
    path: PathBuf,
    agent_id: String,
    base_commit: String,
    cleaned: Arc<AtomicBool>,
}

impl MockWorktreeScope {
    /// Constructs a new [`MockWorktreeScope`].
    pub fn new(path: PathBuf, agent_id: impl Into<String>, base_commit: impl Into<String>) -> Self {
        Self {
            path,
            agent_id: agent_id.into(),
            base_commit: base_commit.into(),
            cleaned: Arc::new(AtomicBool::new(false)),
        }
    }

    /// Returns whether this mock scope has been cleaned up.
    pub fn is_cleaned(&self) -> bool {
        self.cleaned.load(Ordering::SeqCst)
    }
}

impl WorktreeScope for MockWorktreeScope {
    fn path(&self) -> &Path {
        &self.path
    }

    fn generate_proposal(&self) -> Result<WorkspaceProposal> {
        Ok(WorkspaceProposal {
            agent_id: self.agent_id.clone(),
            base_commit: self.base_commit.clone(),
            diff_stat: "1 file changed, 1 insertion(+)".to_string(),
            patch_payload: "--- a/test.txt\n+++ b/test.txt\n@@ -1 +1 @@\n-old\n+new\n".to_string(),
        })
    }
}

impl Drop for MockWorktreeScope {
    fn drop(&mut self) {
        self.cleaned.store(true, Ordering::SeqCst);
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

/// Mock workspace manager for offline testing and hermetic test suites.
#[derive(Debug, Clone)]
pub struct MockWorkspaceManager {
    base_dir: PathBuf,
}

impl MockWorkspaceManager {
    /// Constructs a new [`MockWorkspaceManager`].
    pub fn new(base_dir: impl Into<PathBuf>) -> Self {
        Self {
            base_dir: base_dir.into(),
        }
    }
}

impl WorkspaceManager for MockWorkspaceManager {
    type Handle = MockWorktreeScope;

    fn create_ephemeral_worktree<'a>(
        &'a self,
        agent_id: &'a str,
        commit_ish: &'a str,
    ) -> BoxFuture<'a, Result<Self::Handle>> {
        Box::pin(async move {
            let unique_id = SystemTime::now()
                .duration_since(SystemTime::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0);
            let dir = self
                .base_dir
                .join(format!("mock-wt-{agent_id}-{unique_id}"));
            let _ = tokio::fs::create_dir_all(&dir).await;

            Ok(MockWorktreeScope::new(dir, agent_id, commit_ish))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_mock_workspace_lifecycle_and_raii() {
        let base = std::env::temp_dir().join(format!("kai_test_wm_{}", std::process::id()));
        let _ = std::fs::create_dir_all(&base);

        let mgr = MockWorkspaceManager::new(&base);
        let handle = mgr
            .create_ephemeral_worktree("subagent-01", "HEAD")
            .await
            .expect("create worktree");

        let wt_path = handle.path().to_path_buf();
        assert!(wt_path.exists());

        let proposal = handle.generate_proposal().expect("proposal");
        assert_eq!(proposal.agent_id, "subagent-01");
        assert_eq!(proposal.base_commit, "HEAD");
        assert!(proposal.diff_stat.contains("1 file changed"));

        // RAII cleanup on drop
        drop(handle);
        assert!(!wt_path.exists());

        let _ = std::fs::remove_dir_all(&base);
    }
}
