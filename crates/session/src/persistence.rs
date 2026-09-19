//! Transactional filesystem persistence for session DAGs.
//!
//! Provides [`FileSessionStore`] wrapping an in-memory session graph with atomic,
//! crash-resilient disk persistence enforcing atomic replacement via temporary sibling files.

use std::path::{Path, PathBuf};

use kai_core::error::{KaiError, Result, SessionError};
use kai_core::message::current_timestamp_ms;
use kai_core::traits::{BoxFuture, SessionNode, SessionStore};

use crate::graph::{MemorySessionStore, SessionGraphData};

/// Transactional on-disk session store implementing [`SessionStore`].
pub struct FileSessionStore {
    storage_dir: PathBuf,
    session_id: String,
    inner: MemorySessionStore,
    auto_flush: bool,
}

impl FileSessionStore {
    /// Constructs and initializes a [`FileSessionStore`].
    ///
    /// If an existing session snapshot is present on disk at `<storage_dir>/<session_id>.json`,
    /// it is automatically loaded and validated into memory.
    pub fn new(
        storage_dir: impl Into<PathBuf>,
        session_id: impl Into<String>,
        auto_flush: bool,
    ) -> Result<Self> {
        let storage_dir = storage_dir.into();
        let session_id = session_id.into();

        if session_id.contains('/')
            || session_id.contains('\\')
            || session_id.contains("..")
            || session_id.trim().is_empty()
        {
            return Err(KaiError::Session(SessionError::CorruptedState {
                reason: format!(
                    "Invalid session ID '{session_id}': cannot contain path separators, '..', or be empty"
                ),
            }));
        }

        std::fs::create_dir_all(&storage_dir).map_err(KaiError::Io)?;

        let inner = MemorySessionStore::with_session(&session_id);
        let store = Self {
            storage_dir,
            session_id,
            inner,
            auto_flush,
        };

        store.load_if_exists()?;

        Ok(store)
    }

    /// Computes the canonical path to the primary session JSON file.
    pub fn session_file_path(&self) -> PathBuf {
        self.storage_dir.join(format!("{}.json", self.session_id))
    }

    /// Loads existing session DAG data from disk if the file exists.
    fn load_if_exists(&self) -> Result<()> {
        let path = self.session_file_path();
        if path.exists() {
            let content = std::fs::read_to_string(&path).map_err(KaiError::Io)?;
            let data: SessionGraphData = serde_json::from_str(&content).map_err(|err| {
                KaiError::Session(SessionError::CorruptedState {
                    reason: format!("Failed to parse session state JSON: {err}"),
                })
            })?;

            // Import data synchronously without thread blocking or runtime panics
            self.inner.import_data_sync(data)?;
        }
        Ok(())
    }

    /// Atomically flushes in-memory session graph data to disk.
    ///
    /// Enforces transactional safety: writes to a temporary sibling file, flushes to disk,
    /// and atomically renames to the target session file. On failure, temporary files are removed.
    pub async fn flush_to_disk(&self) -> Result<()> {
        let data = self.inner.export_data(&self.session_id).await;
        let serialized = serde_json::to_string_pretty(&data).map_err(|err| {
            KaiError::Session(SessionError::CorruptedState {
                reason: format!("Serialization failure: {err}"),
            })
        })?;

        let target_path = self.session_file_path();
        let pid = std::process::id();
        let ts = current_timestamp_ms();
        let tmp_path = self
            .storage_dir
            .join(format!("{}.tmp.{}.{}", self.session_id, pid, ts));

        // 1. Transactional write to temporary sibling file
        if let Err(err) = std::fs::write(&tmp_path, serialized.as_bytes()) {
            let _ = std::fs::remove_file(&tmp_path);
            return Err(KaiError::Io(err));
        }

        // 2. Atomic replacement via rename
        if let Err(err) = std::fs::rename(&tmp_path, &target_path) {
            let _ = std::fs::remove_file(&tmp_path);
            return Err(KaiError::Io(err));
        }

        Ok(())
    }

    /// Returns the session identifier.
    pub fn session_id(&self) -> &str {
        &self.session_id
    }

    /// Returns the storage directory path.
    pub fn storage_dir(&self) -> &Path {
        &self.storage_dir
    }

    /// Returns a reference to the underlying [`MemorySessionStore`].
    pub fn inner(&self) -> &MemorySessionStore {
        &self.inner
    }
}

impl SessionStore for FileSessionStore {
    fn get_node<'a>(&'a self, node_id: &'a str) -> BoxFuture<'a, Result<Option<SessionNode>>> {
        self.inner.get_node(node_id)
    }

    fn put_node<'a>(&'a self, node: &'a SessionNode) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            self.inner.put_node(node).await?;
            if self.auto_flush {
                self.flush_to_disk().await?;
            }
            Ok(())
        })
    }

    fn get_children<'a>(&'a self, node_id: &'a str) -> BoxFuture<'a, Result<Vec<SessionNode>>> {
        self.inner.get_children(node_id)
    }

    fn get_branch_history<'a>(
        &'a self,
        leaf_node_id: &'a str,
    ) -> BoxFuture<'a, Result<Vec<SessionNode>>> {
        self.inner.get_branch_history(leaf_node_id)
    }

    fn set_head<'a>(&'a self, branch_name: &'a str, node_id: &'a str) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            self.inner.set_head(branch_name, node_id).await?;
            if self.auto_flush {
                self.flush_to_disk().await?;
            }
            Ok(())
        })
    }

    fn get_head<'a>(&'a self, branch_name: &'a str) -> BoxFuture<'a, Result<Option<String>>> {
        self.inner.get_head(branch_name)
    }

    fn list_branches<'a>(&'a self) -> BoxFuture<'a, Result<Vec<String>>> {
        self.inner.list_branches()
    }

    fn prune_branch<'a>(&'a self, branch_name: &'a str) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            self.inner.prune_branch(branch_name).await?;
            if self.auto_flush {
                self.flush_to_disk().await?;
            }
            Ok(())
        })
    }

    fn list_sessions<'a>(&'a self) -> BoxFuture<'a, Result<Vec<String>>> {
        Box::pin(async move {
            let mut sessions = std::collections::HashSet::new();
            for s in self.inner.list_sessions().await? {
                sessions.insert(s);
            }
            if let Ok(entries) = std::fs::read_dir(&self.storage_dir) {
                for entry in entries.flatten() {
                    let path = entry.path();
                    if path.is_file() && path.extension().and_then(|e| e.to_str()) == Some("json") {
                        if let Some(stem) = path.file_stem().and_then(|s| s.to_str()) {
                            if !stem.contains(".tmp.") {
                                sessions.insert(stem.to_string());
                            }
                        }
                    }
                }
            }
            let mut list: Vec<String> = sessions.into_iter().collect();
            list.sort();
            Ok(list)
        })
    }

    fn get_node_by_commit<'a>(
        &'a self,
        commit_hash: &'a str,
    ) -> BoxFuture<'a, Result<Option<SessionNode>>> {
        self.inner.get_node_by_commit(commit_hash)
    }

    fn delete_session<'a>(&'a self, session_id: &'a str) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            if session_id.contains('/')
                || session_id.contains('\\')
                || session_id.contains("..")
                || session_id.trim().is_empty()
            {
                return Err(KaiError::Session(SessionError::CorruptedState {
                    reason: format!("Invalid session ID '{session_id}'"),
                }));
            }

            let mut deleted = false;
            if session_id == self.session_id {
                let _ = self.inner.delete_session(session_id).await;
                deleted = true;
            }

            let file_path = self.storage_dir.join(format!("{session_id}.json"));
            if file_path.exists() {
                std::fs::remove_file(&file_path).map_err(KaiError::Io)?;
                deleted = true;
            }

            if !deleted {
                return Err(KaiError::Session(SessionError::NotFound {
                    session_id: session_id.to_string(),
                }));
            }

            Ok(())
        })
    }
}
