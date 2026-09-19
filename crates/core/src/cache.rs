//! Deterministic in-memory memoization cache for read-only tool invocations.

use std::collections::HashMap;
use std::fs;
use std::path::Path;
use std::sync::RwLock;

use crate::message::ToolResult;
use crate::traits::Tool;

/// Default capacity for the in-memory tool memoization cache.
pub const DEFAULT_CACHE_CAPACITY: usize = 128;

/// File signature comprising modification timestamp and file length in bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
struct FileSignature {
    modified_ms: u64,
    len: u64,
}

impl FileSignature {
    fn from_path(path: &Path) -> Option<Self> {
        let metadata = fs::metadata(path).ok()?;
        let modified_ms = metadata
            .modified()
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);
        Some(Self {
            modified_ms,
            len: metadata.len(),
        })
    }
}

/// Cache entry holding a memoized [`ToolResult`] and verification metadata.
#[derive(Debug, Clone)]
struct CacheEntry {
    result: ToolResult,
    file_signature: Option<FileSignature>,
    last_accessed: u64,
}

/// Thread-safe in-memory cache for deterministic read-only tool results.
#[derive(Debug)]
pub struct ToolResultCache {
    capacity: usize,
    entries: RwLock<HashMap<String, CacheEntry>>,
}

impl Default for ToolResultCache {
    fn default() -> Self {
        Self::new(DEFAULT_CACHE_CAPACITY)
    }
}

impl ToolResultCache {
    /// Constructs a new [`ToolResultCache`] with bounded entry capacity.
    pub fn new(capacity: usize) -> Self {
        Self {
            capacity: capacity.max(1),
            entries: RwLock::new(HashMap::new()),
        }
    }

    /// Generates a composite cache key for a tool and its arguments.
    fn make_key(tool_name: &str, arguments: &serde_json::Value) -> String {
        format!("{tool_name}:{arguments}")
    }

    /// Extracts target file path from common argument patterns (`path`, `file`, `target`).
    fn extract_target_path<'a>(
        working_dir: &'a Path,
        arguments: &'a serde_json::Value,
    ) -> Option<std::path::PathBuf> {
        let raw_path = arguments
            .get("path")
            .or_else(|| arguments.get("file"))
            .or_else(|| arguments.get("target"))
            .and_then(|v| v.as_str())?;
        let p = Path::new(raw_path);
        if p.is_absolute() {
            Some(p.to_path_buf())
        } else {
            Some(working_dir.join(p))
        }
    }

    /// Checks if a tool can be cached (must be read-only).
    pub fn is_cacheable(tool: &dyn Tool) -> bool {
        tool.is_read_only()
    }

    /// Retrieves a cached result if present and validated against current file signature.
    pub fn get(
        &self,
        tool_name: &str,
        arguments: &serde_json::Value,
        working_dir: &Path,
    ) -> Option<ToolResult> {
        let key = Self::make_key(tool_name, arguments);
        let target_path = Self::extract_target_path(working_dir, arguments);
        let current_sig = target_path.as_deref().and_then(FileSignature::from_path);

        let guard = self.entries.read().ok()?;
        let entry = guard.get(&key)?;

        // If file signature was tracked, verify it has not changed
        if let Some(cached_sig) = entry.file_signature {
            let curr = current_sig?;
            if cached_sig != curr {
                return None;
            }
        }

        Some(entry.result.clone())
    }

    /// Stores a [`ToolResult`] in the cache for a read-only tool invocation.
    pub fn insert(
        &self,
        tool_name: &str,
        arguments: &serde_json::Value,
        working_dir: &Path,
        result: ToolResult,
    ) {
        // Do not cache tool errors
        if result.is_error {
            return;
        }

        let key = Self::make_key(tool_name, arguments);
        let target_path = Self::extract_target_path(working_dir, arguments);
        let file_signature = target_path.as_deref().and_then(FileSignature::from_path);

        let now = crate::message::current_timestamp_ms();

        if let Ok(mut guard) = self.entries.write() {
            // Evict oldest if capacity exceeded
            if guard.len() >= self.capacity && !guard.contains_key(&key) {
                if let Some(oldest_key) = guard
                    .iter()
                    .min_by_key(|(_, v)| v.last_accessed)
                    .map(|(k, _)| k.clone())
                {
                    guard.remove(&oldest_key);
                }
            }

            guard.insert(
                key,
                CacheEntry {
                    result,
                    file_signature,
                    last_accessed: now,
                },
            );
        }
    }

    /// Invalidates all cached entries for a given tool name.
    pub fn invalidate_tool(&self, tool_name: &str) {
        if let Ok(mut guard) = self.entries.write() {
            guard.retain(|k, _| !k.starts_with(&format!("{tool_name}:")));
        }
    }

    /// Clears the entire tool result cache.
    pub fn clear(&self) {
        if let Ok(mut guard) = self.entries.write() {
            guard.clear();
        }
    }

    /// Returns the number of entries currently stored in the cache.
    pub fn len(&self) -> usize {
        self.entries.read().map(|g| g.len()).unwrap_or(0)
    }

    /// Returns true if the cache is empty.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}
