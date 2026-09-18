//! Headless browser automation contracts and offline mock drivers.
//!
//! Provides the [`BrowserDriver`] trait for headless automation and [`MockBrowserDriver`]
//! for deterministic, zero-network, offline testing.

use std::collections::HashMap;
use std::sync::Arc;

use kai_core::error::{KaiError, Result, ToolError};
use kai_core::message::ToolResult;
use kai_core::traits::{BoxFuture, PermissionCategory, Tool, ToolContext};
use serde::{Deserialize, Serialize};
use serde_json::json;
use tokio::sync::RwLock;

/// Information about a loaded browser page.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BrowserPageInfo {
    /// URL of the loaded page.
    pub url: String,
    /// HTML document title.
    pub title: String,
    /// HTTP status code reported by the server.
    pub status_code: u16,
}

/// Outcome of a targeted browser interaction.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BrowserActionOutcome {
    /// Whether the action completed successfully.
    pub success: bool,
    /// Selector or target interacted with.
    pub target: String,
    /// Optional supplemental details or error message.
    pub details: Option<String>,
}

/// Headless browser driver contract.
pub trait BrowserDriver: Send + Sync {
    /// Navigates the browser to the specified URL.
    fn navigate<'a>(&'a self, url: &'a str) -> BoxFuture<'a, Result<BrowserPageInfo>>;

    /// Simulates clicking an element matching the given CSS selector.
    fn click<'a>(&'a self, selector: &'a str) -> BoxFuture<'a, Result<BrowserActionOutcome>>;

    /// Captures a viewport screenshot as PNG image bytes.
    fn screenshot<'a>(&'a self) -> BoxFuture<'a, Result<Vec<u8>>>;

    /// Evaluates a JavaScript snippet within the active document context.
    fn evaluate<'a>(&'a self, script: &'a str) -> BoxFuture<'a, Result<serde_json::Value>>;

    /// Retrieves the text content of the active page DOM.
    fn page_content<'a>(&'a self) -> BoxFuture<'a, Result<String>>;
}

#[derive(Debug, Default)]
struct MockState {
    current_url: String,
    pages: HashMap<String, (String, String)>, // url -> (title, content)
    eval_results: HashMap<String, serde_json::Value>,
    clicked_selectors: Vec<String>,
    screenshot_data: Vec<u8>,
}

/// Offline, deterministic in-memory browser driver for testing.
#[derive(Debug, Clone)]
pub struct MockBrowserDriver {
    state: Arc<RwLock<MockState>>,
}

impl Default for MockBrowserDriver {
    fn default() -> Self {
        Self::new()
    }
}

impl MockBrowserDriver {
    /// Constructs a new [`MockBrowserDriver`].
    pub fn new() -> Self {
        Self {
            state: Arc::new(RwLock::new(MockState {
                current_url: "about:blank".to_string(),
                pages: HashMap::new(),
                eval_results: HashMap::new(),
                clicked_selectors: Vec::new(),
                screenshot_data:
                    b"\x89PNG\r\n\x1a\n\x00\x00\x00\rIHDR\x00\x00\x00\x01\x00\x00\x00\x01".to_vec(),
            })),
        }
    }

    /// Registers a mock page at the specified URL.
    pub async fn set_page(
        &self,
        url: impl Into<String>,
        title: impl Into<String>,
        content: impl Into<String>,
    ) {
        let mut guard = self.state.write().await;
        guard
            .pages
            .insert(url.into(), (title.into(), content.into()));
    }

    /// Registers a mock evaluation result for a given script snippet.
    pub async fn set_eval_result(&self, script: impl Into<String>, result: serde_json::Value) {
        let mut guard = self.state.write().await;
        guard.eval_results.insert(script.into(), result);
    }

    /// Sets custom mock screenshot bytes.
    pub async fn set_screenshot(&self, bytes: Vec<u8>) {
        let mut guard = self.state.write().await;
        guard.screenshot_data = bytes;
    }

    /// Retrieves all clicked selectors recorded during execution.
    pub async fn clicked_selectors(&self) -> Vec<String> {
        let guard = self.state.read().await;
        guard.clicked_selectors.clone()
    }
}

impl BrowserDriver for MockBrowserDriver {
    fn navigate<'a>(&'a self, url: &'a str) -> BoxFuture<'a, Result<BrowserPageInfo>> {
        Box::pin(async move {
            let mut guard = self.state.write().await;
            guard.current_url = url.to_string();

            if let Some((title, _)) = guard.pages.get(url) {
                Ok(BrowserPageInfo {
                    url: url.to_string(),
                    title: title.clone(),
                    status_code: 200,
                })
            } else {
                Ok(BrowserPageInfo {
                    url: url.to_string(),
                    title: format!("Page: {url}"),
                    status_code: 200,
                })
            }
        })
    }

    fn click<'a>(&'a self, selector: &'a str) -> BoxFuture<'a, Result<BrowserActionOutcome>> {
        Box::pin(async move {
            let mut guard = self.state.write().await;
            guard.clicked_selectors.push(selector.to_string());
            Ok(BrowserActionOutcome {
                success: true,
                target: selector.to_string(),
                details: Some("Element clicked successfully".to_string()),
            })
        })
    }

    fn screenshot<'a>(&'a self) -> BoxFuture<'a, Result<Vec<u8>>> {
        Box::pin(async move {
            let guard = self.state.read().await;
            Ok(guard.screenshot_data.clone())
        })
    }

    fn evaluate<'a>(&'a self, script: &'a str) -> BoxFuture<'a, Result<serde_json::Value>> {
        Box::pin(async move {
            let guard = self.state.read().await;
            if let Some(val) = guard.eval_results.get(script) {
                Ok(val.clone())
            } else {
                Ok(json!({ "result": "mock_evaluated", "script": script }))
            }
        })
    }

    fn page_content<'a>(&'a self) -> BoxFuture<'a, Result<String>> {
        Box::pin(async move {
            let guard = self.state.read().await;
            if let Some((_, content)) = guard.pages.get(&guard.current_url) {
                Ok(content.clone())
            } else {
                Ok(format!(
                    "<html><body>Mock content for {}</body></html>",
                    guard.current_url
                ))
            }
        })
    }
}

/// Tool for executing browser automation operations.
#[derive(Clone)]
pub struct BrowserActionTool {
    driver: Arc<dyn BrowserDriver>,
}

impl Default for BrowserActionTool {
    fn default() -> Self {
        Self::new()
    }
}

impl BrowserActionTool {
    /// Constructs a [`BrowserActionTool`] using an offline mock driver.
    pub fn new() -> Self {
        Self {
            driver: Arc::new(MockBrowserDriver::new()),
        }
    }

    /// Constructs a [`BrowserActionTool`] with a custom browser driver.
    pub fn with_driver(driver: Arc<dyn BrowserDriver>) -> Self {
        Self { driver }
    }
}

/// Arguments accepted by [`BrowserActionTool`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BrowserActionArgs {
    /// Action verb: "navigate", "click", "screenshot", "evaluate", "page_content".
    pub action: String,
    /// Target URL for navigation.
    pub url: Option<String>,
    /// CSS selector for click targets.
    pub selector: Option<String>,
    /// JavaScript snippet for evaluation.
    pub script: Option<String>,
}

impl Tool for BrowserActionTool {
    fn name(&self) -> &str {
        "browser_action"
    }

    fn description(&self) -> &str {
        "Performs headless browser automation actions including navigation, clicking, DOM inspection, and screenshots."
    }

    fn schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "required": ["action"],
            "properties": {
                "action": {
                    "type": "string",
                    "enum": ["navigate", "click", "screenshot", "evaluate", "page_content"],
                    "description": "Browser action to execute"
                },
                "url": {
                    "type": "string",
                    "description": "Target URL for navigation"
                },
                "selector": {
                    "type": "string",
                    "description": "CSS selector for click target"
                },
                "script": {
                    "type": "string",
                    "description": "JavaScript snippet to evaluate"
                }
            }
        })
    }

    fn permission_category(&self) -> PermissionCategory {
        PermissionCategory::BrowserControl
    }

    fn is_read_only(&self) -> bool {
        false
    }

    fn validate_arguments(&self, arguments: &serde_json::Value) -> Result<(), ToolError> {
        if !arguments.is_object() {
            return Err(ToolError::InvalidArguments {
                name: self.name().to_string(),
                reason: "Arguments must be a valid JSON object".to_string(),
            });
        }

        let args: BrowserActionArgs = serde_json::from_value(arguments.clone()).map_err(|err| {
            ToolError::InvalidArguments {
                name: self.name().to_string(),
                reason: err.to_string(),
            }
        })?;

        match args.action.as_str() {
            "navigate" => {
                if args.url.as_deref().unwrap_or("").trim().is_empty() {
                    return Err(ToolError::InvalidArguments {
                        name: self.name().to_string(),
                        reason: "url is required for navigate action".to_string(),
                    });
                }
            }
            "click" => {
                if args.selector.as_deref().unwrap_or("").trim().is_empty() {
                    return Err(ToolError::InvalidArguments {
                        name: self.name().to_string(),
                        reason: "selector is required for click action".to_string(),
                    });
                }
            }
            "evaluate" => {
                if args.script.as_deref().unwrap_or("").trim().is_empty() {
                    return Err(ToolError::InvalidArguments {
                        name: self.name().to_string(),
                        reason: "script is required for evaluate action".to_string(),
                    });
                }
            }
            "screenshot" | "page_content" => {}
            other => {
                return Err(ToolError::InvalidArguments {
                    name: self.name().to_string(),
                    reason: format!("Unknown browser action '{other}'"),
                });
            }
        }

        Ok(())
    }

    fn execute<'a>(
        &'a self,
        arguments: serde_json::Value,
        context: &'a ToolContext,
    ) -> BoxFuture<'a, Result<ToolResult>> {
        Box::pin(async move {
            context.check_cancellation()?;

            let args: BrowserActionArgs = match serde_json::from_value(arguments) {
                Ok(parsed) => parsed,
                Err(err) => {
                    return Ok(ToolResult::error(
                        self.name(),
                        format!("Invalid arguments: {err}"),
                    ));
                }
            };

            match args.action.as_str() {
                "navigate" => {
                    let url = args.url.as_deref().unwrap_or("");
                    if url.trim().is_empty() {
                        return Ok(ToolResult::error(
                            self.name(),
                            "url is required for navigate",
                        ));
                    }
                    match self.driver.navigate(url).await {
                        Ok(info) => Ok(ToolResult::success(
                            self.name(),
                            format!(
                                "Navigated to {} (status: {}, title: '{}')",
                                info.url, info.status_code, info.title
                            ),
                        )),
                        Err(err) => Ok(ToolResult::error(
                            self.name(),
                            format!("Navigation failed: {err}"),
                        )),
                    }
                }
                "click" => {
                    let selector = args.selector.as_deref().unwrap_or("");
                    if selector.trim().is_empty() {
                        return Ok(ToolResult::error(
                            self.name(),
                            "selector is required for click",
                        ));
                    }
                    match self.driver.click(selector).await {
                        Ok(outcome) => Ok(ToolResult::success(
                            self.name(),
                            format!(
                                "Clicked '{}': success={}, details={:?}",
                                outcome.target, outcome.success, outcome.details
                            ),
                        )),
                        Err(err) => Ok(ToolResult::error(
                            self.name(),
                            format!("Click failed: {err}"),
                        )),
                    }
                }
                "screenshot" => match self.driver.screenshot().await {
                    Ok(bytes) => Ok(ToolResult::success(
                        self.name(),
                        format!("[Screenshot captured: {} bytes PNG]", bytes.len()),
                    )),
                    Err(err) => Ok(ToolResult::error(
                        self.name(),
                        format!("Screenshot failed: {err}"),
                    )),
                },
                "evaluate" => {
                    let script = args.script.as_deref().unwrap_or("");
                    if script.trim().is_empty() {
                        return Ok(ToolResult::error(
                            self.name(),
                            "script is required for evaluate",
                        ));
                    }
                    match self.driver.evaluate(script).await {
                        Ok(val) => {
                            let output = match val {
                                serde_json::Value::String(s) => s,
                                other => serde_json::to_string_pretty(&other).unwrap_or_default(),
                            };
                            Ok(ToolResult::success(self.name(), output))
                        }
                        Err(err) => Ok(ToolResult::error(
                            self.name(),
                            format!("Evaluation failed: {err}"),
                        )),
                    }
                }
                "page_content" => match self.driver.page_content().await {
                    Ok(content) => Ok(ToolResult::success(self.name(), content)),
                    Err(err) => Ok(ToolResult::error(
                        self.name(),
                        format!("Page content extraction failed: {err}"),
                    )),
                },
                other => Err(KaiError::Tool(ToolError::InvalidArguments {
                    name: self.name().to_string(),
                    reason: format!("Unknown action '{other}'"),
                })),
            }
        })
    }
}
