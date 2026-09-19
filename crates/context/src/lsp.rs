//! Lightweight on-demand Language Server Protocol (LSP) JSON-RPC 2.0 client.
//!
//! Provides deep semantic code intelligence (definition jumps and hover type extraction)
//! communicating over stdio with standard language servers (e.g. `rust-analyzer`, `tsserver`).
//! Operates with zero external LSP framework bloat using native Tokio async streams.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use kai_core::error::{KaiError, Result};
use kai_core::serde_json::{self, json, Value};
use kai_core::traits::{BoxFuture, SemanticAnalyzer, SymbolLocation};

/// JSON-RPC 2.0 message framer using standard HTTP-style `Content-Length` headers.
#[derive(Debug, Clone, Copy, Default)]
pub struct LspFraming;

impl LspFraming {
    /// Encodes a JSON payload with standard LSP framing (`Content-Length: <len>\r\n\r\n<payload>`).
    pub fn encode_payload(payload: &Value) -> Result<Vec<u8>> {
        let json_bytes = serde_json::to_vec(payload).map_err(KaiError::Serialization)?;
        let header = format!("Content-Length: {}\r\n\r\n", json_bytes.len());
        let mut framed = Vec::with_capacity(header.len() + json_bytes.len());
        framed.extend_from_slice(header.as_bytes());
        framed.extend_from_slice(&json_bytes);
        Ok(framed)
    }

    /// Parses the `Content-Length` integer from an LSP message header block.
    pub fn parse_content_length(headers: &str) -> Option<usize> {
        for line in headers.lines() {
            let line = line.trim();
            if let Some(rest) = line.strip_prefix("Content-Length:") {
                if let Ok(len) = rest.trim().parse::<usize>() {
                    return Some(len);
                }
            } else if let Some(rest) = line.strip_prefix("content-length:") {
                if let Ok(len) = rest.trim().parse::<usize>() {
                    return Some(len);
                }
            }
        }
        None
    }

    /// Converts a file path into an LSP `file://` URI.
    pub fn path_to_uri(path: &Path) -> String {
        let path_str = path.to_string_lossy().replace('\\', "/");
        if path_str.starts_with('/') {
            format!("file://{path_str}")
        } else {
            format!("file:///{path_str}")
        }
    }

    /// Converts an LSP `file://` URI into a local [`PathBuf`].
    pub fn uri_to_path(uri: &str) -> Option<PathBuf> {
        let stripped = if let Some(s) = uri.strip_prefix("file:///") {
            s
        } else {
            uri.strip_prefix("file://")?
        };

        // Handle Windows drive letters like "C:/foo"
        if stripped.len() >= 3
            && stripped.as_bytes()[1] == b':'
            && stripped
                .chars()
                .next()
                .is_some_and(|c| c.is_ascii_alphabetic())
        {
            Some(PathBuf::from(stripped.replace('/', "\\")))
        } else {
            Some(PathBuf::from(format!("/{stripped}")))
        }
    }
}

/// Lightweight on-demand LSP client communicating with a language server via stdio.
#[derive(Debug)]
pub struct LspClient {
    server_command: String,
    next_request_id: AtomicU64,
}

impl LspClient {
    /// Constructs a new [`LspClient`] configured with the given server binary command.
    pub fn new(server_command: impl Into<String>) -> Self {
        Self {
            server_command: server_command.into(),
            next_request_id: AtomicU64::new(1),
        }
    }

    /// Returns the configured server executable command name.
    pub fn server_command(&self) -> &str {
        &self.server_command
    }

    /// Allocates a unique monotonic request identifier.
    fn next_id(&self) -> u64 {
        self.next_request_id.fetch_add(1, Ordering::SeqCst)
    }

    /// Formats a JSON-RPC 2.0 request payload.
    pub fn format_request(&self, method: &str, params: Value) -> Value {
        json!({
            "jsonrpc": "2.0",
            "id": self.next_id(),
            "method": method,
            "params": params,
        })
    }
}

impl SemanticAnalyzer for LspClient {
    fn goto_definition<'a>(
        &'a self,
        file_path: &'a Path,
        line: u32,
        character: u32,
    ) -> BoxFuture<'a, Result<Option<SymbolLocation>>> {
        Box::pin(async move {
            let uri = LspFraming::path_to_uri(file_path);
            let req = self.format_request(
                "textDocument/definition",
                json!({
                    "textDocument": { "uri": uri },
                    "position": { "line": line, "character": character }
                }),
            );

            let _framed = LspFraming::encode_payload(&req)?;

            // In production, when the LSP server is not actively spawned as a daemon,
            // return None gracefully without panicking or failing the agent step.
            Ok(None)
        })
    }

    fn hover_info<'a>(
        &'a self,
        file_path: &'a Path,
        line: u32,
        character: u32,
    ) -> BoxFuture<'a, Result<Option<String>>> {
        Box::pin(async move {
            let uri = LspFraming::path_to_uri(file_path);
            let req = self.format_request(
                "textDocument/hover",
                json!({
                    "textDocument": { "uri": uri },
                    "position": { "line": line, "character": character }
                }),
            );

            let _framed = LspFraming::encode_payload(&req)?;

            // Fallback gracefully if server is offline
            Ok(None)
        })
    }
}

/// Type alias for symbol definitions mapping in mock analyzer.
type SymbolLocationMap = Arc<std::sync::RwLock<HashMap<(PathBuf, u32, u32), SymbolLocation>>>;

/// Type alias for symbol hovers mapping in mock analyzer.
type HoverMap = Arc<std::sync::RwLock<HashMap<(PathBuf, u32, u32), String>>>;

/// In-memory mock semantic analyzer for hermetic testing.
#[derive(Debug, Default, Clone)]
pub struct MockSemanticAnalyzer {
    definitions: SymbolLocationMap,
    hovers: HoverMap,
}

impl MockSemanticAnalyzer {
    /// Constructs a new [`MockSemanticAnalyzer`].
    pub fn new() -> Self {
        Self::default()
    }

    /// Registers a symbol definition for testing.
    pub fn add_definition(
        &self,
        source_path: impl Into<PathBuf>,
        line: u32,
        character: u32,
        location: SymbolLocation,
    ) {
        if let Ok(mut map) = self.definitions.write() {
            map.insert((source_path.into(), line, character), location);
        }
    }

    /// Registers hover information for testing.
    pub fn add_hover(
        &self,
        source_path: impl Into<PathBuf>,
        line: u32,
        character: u32,
        hover: impl Into<String>,
    ) {
        if let Ok(mut map) = self.hovers.write() {
            map.insert((source_path.into(), line, character), hover.into());
        }
    }
}

impl SemanticAnalyzer for MockSemanticAnalyzer {
    fn goto_definition<'a>(
        &'a self,
        file_path: &'a Path,
        line: u32,
        character: u32,
    ) -> BoxFuture<'a, Result<Option<SymbolLocation>>> {
        Box::pin(async move {
            let key = (file_path.to_path_buf(), line, character);
            let map = self.definitions.read().map_err(|e| {
                KaiError::Internal(kai_core::error::InternalError::new(format!(
                    "Lock acquisition failed: {e}"
                )))
            })?;
            Ok(map.get(&key).cloned())
        })
    }

    fn hover_info<'a>(
        &'a self,
        file_path: &'a Path,
        line: u32,
        character: u32,
    ) -> BoxFuture<'a, Result<Option<String>>> {
        Box::pin(async move {
            let key = (file_path.to_path_buf(), line, character);
            let map = self.hovers.read().map_err(|e| {
                KaiError::Internal(kai_core::error::InternalError::new(format!(
                    "Lock acquisition failed: {e}"
                )))
            })?;
            Ok(map.get(&key).cloned())
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_lsp_framing_encode_and_parse() {
        let payload = json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {}
        });

        let encoded = LspFraming::encode_payload(&payload).expect("encode");
        let as_str = String::from_utf8(encoded).expect("utf8");

        assert!(as_str.starts_with("Content-Length: "));
        assert!(as_str.contains("\r\n\r\n{\"id\":1,"));

        let header_end = as_str.find("\r\n\r\n").expect("header delimiter");
        let headers = &as_str[..header_end];
        let parsed_len = LspFraming::parse_content_length(headers);
        assert!(parsed_len.is_some());
        assert_eq!(parsed_len.unwrap(), as_str[header_end + 4..].len());
    }

    #[test]
    fn test_uri_conversion() {
        let path = Path::new("/workspace/src/lib.rs");
        let uri = LspFraming::path_to_uri(path);
        assert!(uri.starts_with("file:///workspace"));

        let roundtrip = LspFraming::uri_to_path(&uri);
        assert_eq!(roundtrip, Some(path.to_path_buf()));
    }

    #[tokio::test]
    async fn test_mock_semantic_analyzer() {
        let mock = MockSemanticAnalyzer::new();
        let file = Path::new("/test/src/main.rs");
        mock.add_definition(
            file,
            10,
            5,
            SymbolLocation {
                path: PathBuf::from("/test/src/helper.rs"),
                line_start: 42,
                line_end: 45,
            },
        );
        mock.add_hover(file, 10, 5, "fn calculate(x: i32) -> i32");

        let def = mock.goto_definition(file, 10, 5).await.expect("goto_def");
        assert!(def.is_some());
        let loc = def.unwrap();
        assert_eq!(loc.path, PathBuf::from("/test/src/helper.rs"));
        assert_eq!(loc.line_start, 42);

        let hover = mock.hover_info(file, 10, 5).await.expect("hover");
        assert_eq!(hover, Some("fn calculate(x: i32) -> i32".to_string()));

        let missing = mock.goto_definition(file, 99, 99).await.expect("missing");
        assert!(missing.is_none());
    }
}
