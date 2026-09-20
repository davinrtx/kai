//! Model discovery and local endpoint probing engine.
//!
//! Provides multi-protocol auto-detection for Ollama (`/api/tags`), LM Studio
//! (`/api/v1/models`), and OpenAI-compatible inference endpoints (`/models` dual-path),
//! backed by in-memory TTL caching and negative failure caching.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use reqwest::header::{HeaderMap, HeaderValue, AUTHORIZATION, CONTENT_TYPE};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::sync::RwLock;

/// Cache TTL for successfully discovered model catalogs (300 seconds).
pub const POSITIVE_CACHE_TTL: Duration = Duration::from_secs(300);

/// Cache TTL for failed endpoint probes to avoid REPL freezing (30 seconds).
pub const NEGATIVE_CACHE_TTL: Duration = Duration::from_secs(30);

/// Network probe request timeout bounded to prevent REPL lag.
pub const PROBE_TIMEOUT: Duration = Duration::from_millis(1500);

/// Standard local inference endpoint candidates.
pub const DEFAULT_LOCAL_ENDPOINTS: &[(&str, &str)] = &[
    ("ollama", "http://localhost:11434"),
    ("lmstudio", "http://localhost:1234/v1"),
];

/// Discovered model metadata discovered across local or remote endpoints.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiscoveredModel {
    /// Model identifier passed to chat completions.
    pub id: String,
    /// Provider name (`ollama`, `lmstudio`, `openai-compatible`).
    pub provider: String,
    /// Chat completions base URL (e.g. `http://localhost:11434/v1`).
    pub endpoint: String,
    /// Human-readable description, parameter details, or quantization level.
    pub description: Option<String>,
}

/// Thread-safe in-memory cache for discovered models and negative probe results.
#[derive(Debug, Default)]
pub struct DiscoveryCache {
    /// Cached successful discoveries: endpoint URL -> (models, timestamp).
    models: RwLock<HashMap<String, (Vec<DiscoveredModel>, Instant)>>,
    /// Negative failure cache: endpoint URL -> timestamp of failure.
    failures: RwLock<HashMap<String, Instant>>,
}

impl DiscoveryCache {
    /// Constructs a new empty discovery cache.
    pub fn new() -> Self {
        Self {
            models: RwLock::new(HashMap::new()),
            failures: RwLock::new(HashMap::new()),
        }
    }

    /// Retrieves cached models for the specified endpoint if TTL is still valid.
    pub async fn get(&self, endpoint: &str) -> Option<Vec<DiscoveredModel>> {
        let guard = self.models.read().await;
        if let Some((models, timestamp)) = guard.get(endpoint) {
            if timestamp.elapsed() < POSITIVE_CACHE_TTL {
                return Some(models.clone());
            }
        }
        None
    }

    /// Records successfully discovered models for an endpoint.
    pub async fn insert(&self, endpoint: &str, models: Vec<DiscoveredModel>) {
        let mut guard = self.models.write().await;
        guard.insert(endpoint.to_string(), (models, Instant::now()));

        let mut fail_guard = self.failures.write().await;
        fail_guard.remove(endpoint);
    }

    /// Checks whether an endpoint recently failed and should be temporarily skipped.
    pub async fn is_negatively_cached(&self, endpoint: &str) -> bool {
        let guard = self.failures.read().await;
        if let Some(timestamp) = guard.get(endpoint) {
            if timestamp.elapsed() < NEGATIVE_CACHE_TTL {
                return true;
            }
        }
        false
    }

    /// Records a failed probe attempt for an endpoint.
    pub async fn mark_failure(&self, endpoint: &str) {
        let mut guard = self.failures.write().await;
        guard.insert(endpoint.to_string(), Instant::now());
    }

    /// Clears all cached positive and negative entries.
    pub async fn clear(&self) {
        let mut models = self.models.write().await;
        models.clear();
        let mut failures = self.failures.write().await;
        failures.clear();
    }
}

/// Global shared discovery cache.
static GLOBAL_CACHE: std::sync::OnceLock<Arc<DiscoveryCache>> = std::sync::OnceLock::new();

/// Returns a reference to the global discovery cache instance.
pub fn global_discovery_cache() -> Arc<DiscoveryCache> {
    GLOBAL_CACHE
        .get_or_init(|| Arc::new(DiscoveryCache::new()))
        .clone()
}

/// Strips common trailing suffixes to isolate the root base URL.
pub fn strip_endpoint_suffixes(url: &str) -> String {
    let mut clean = url.trim().trim_end_matches('/').to_string();
    for suffix in &[
        "/chat/completions",
        "/api/tags",
        "/api/v1/models",
        "/v1/models",
        "/models",
        "/api",
        "/v1",
    ] {
        if clean.ends_with(suffix) {
            clean = clean[..clean.len() - suffix.len()]
                .trim_end_matches('/')
                .to_string();
        }
    }
    clean
}

/// Parses Ollama `/api/tags` JSON response into [`DiscoveredModel`] records.
pub fn parse_ollama_tags_json(body: &Value, root_url: &str) -> Vec<DiscoveredModel> {
    let mut results = Vec::new();
    let chat_endpoint = format!("{}/v1", root_url.trim_end_matches('/'));

    if let Some(models) = body.get("models").and_then(|m| m.as_array()) {
        for m in models {
            let id = m
                .get("name")
                .or_else(|| m.get("model"))
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .to_string();

            if id.is_empty() {
                continue;
            }

            let mut desc_parts = Vec::new();
            if let Some(details) = m.get("details").and_then(|d| d.as_object()) {
                if let Some(param) = details.get("parameter_size").and_then(|p| p.as_str()) {
                    desc_parts.push(param.to_string());
                }
                if let Some(quant) = details.get("quantization_level").and_then(|q| q.as_str()) {
                    desc_parts.push(quant.to_string());
                }
                if let Some(family) = details.get("family").and_then(|f| f.as_str()) {
                    desc_parts.push(family.to_string());
                }
            }

            let description = if desc_parts.is_empty() {
                None
            } else {
                Some(desc_parts.join(", "))
            };

            results.push(DiscoveredModel {
                id,
                provider: "ollama".to_string(),
                endpoint: chat_endpoint.clone(),
                description,
            });
        }
    }

    results
}

/// Parses LM Studio `/api/v1/models` JSON response, filtering out embedding models.
pub fn parse_lmstudio_models_json(body: &Value, root_url: &str) -> Vec<DiscoveredModel> {
    let mut results = Vec::new();
    let chat_endpoint = format!("{}/v1", root_url.trim_end_matches('/'));

    let items = body
        .get("data")
        .or_else(|| body.get("models"))
        .and_then(|d| d.as_array());

    if let Some(models) = items {
        for m in models {
            let id = m.get("id").and_then(|v| v.as_str()).unwrap_or_default();

            if id.is_empty() {
                continue;
            }

            // Exclude embedding-only models
            if let Some(model_type) = m.get("type").and_then(|t| t.as_str()) {
                if model_type.eq_ignore_ascii_case("embedding")
                    || model_type.eq_ignore_ascii_case("embeddings")
                {
                    continue;
                }
            }

            results.push(DiscoveredModel {
                id: id.to_string(),
                provider: "lmstudio".to_string(),
                endpoint: chat_endpoint.clone(),
                description: None,
            });
        }
    }

    results
}

/// Parses standard OpenAI `/models` JSON response into [`DiscoveredModel`] records.
pub fn parse_openai_models_json(
    body: &Value,
    chat_endpoint: &str,
    provider: &str,
) -> Vec<DiscoveredModel> {
    let mut results = Vec::new();

    let items = body
        .get("data")
        .or_else(|| body.get("models"))
        .and_then(|d| d.as_array());

    if let Some(models) = items {
        for m in models {
            let id = m.get("id").and_then(|v| v.as_str()).unwrap_or_default();

            if id.is_empty() {
                continue;
            }

            // Heuristic exclusion for embedding models
            if id.contains("embed") {
                continue;
            }

            // Exclude models that declare supported_parameters but lack tool use capability
            if let Some(params) = m.get("supported_parameters").and_then(|p| p.as_array()) {
                let has_tools = params.iter().any(|param| param.as_str() == Some("tools"));
                if !has_tools {
                    continue;
                }
            }

            let description = m
                .get("name")
                .or_else(|| m.get("owned_by"))
                .and_then(|o| o.as_str())
                .map(|s| s.to_string());

            results.push(DiscoveredModel {
                id: id.to_string(),
                provider: provider.to_string(),
                endpoint: chat_endpoint.to_string(),
                description,
            });
        }
    }

    results
}

/// Probes an Ollama endpoint via native `GET /api/tags`.
pub async fn probe_ollama(
    client: &reqwest::Client,
    base_url: &str,
) -> Option<Vec<DiscoveredModel>> {
    let root = strip_endpoint_suffixes(base_url);
    let tags_url = format!("{root}/api/tags");

    let resp = client
        .get(&tags_url)
        .timeout(PROBE_TIMEOUT)
        .send()
        .await
        .ok()?;

    if !resp.status().is_success() {
        return None;
    }

    let json: Value = resp.json().await.ok()?;
    let models = parse_ollama_tags_json(&json, &root);
    if models.is_empty() {
        None
    } else {
        Some(models)
    }
}

/// Probes an LM Studio endpoint via native `GET /api/v1/models`.
pub async fn probe_lmstudio(
    client: &reqwest::Client,
    base_url: &str,
) -> Option<Vec<DiscoveredModel>> {
    let root = strip_endpoint_suffixes(base_url);
    let models_url = format!("{root}/api/v1/models");

    let resp = client
        .get(&models_url)
        .timeout(PROBE_TIMEOUT)
        .send()
        .await
        .ok()?;

    if !resp.status().is_success() {
        return None;
    }

    let json: Value = resp.json().await.ok()?;
    let models = parse_lmstudio_models_json(&json, &root);
    if models.is_empty() {
        None
    } else {
        Some(models)
    }
}

/// Probes OpenAI-compatible endpoints with dual-path heuristics (`/models` vs `/v1/models`).
pub async fn probe_openai_compatible(
    client: &reqwest::Client,
    base_url: &str,
    api_key: Option<&str>,
) -> Option<(Vec<DiscoveredModel>, String)> {
    let clean_base = base_url.trim().trim_end_matches('/');
    let root = strip_endpoint_suffixes(base_url);

    // Build candidate paths in prioritized sequence
    let mut candidates = Vec::new();
    // 1. Prioritize direct /models under the configured base URL (e.g. https://openrouter.ai/api/v1/models)
    candidates.push((format!("{clean_base}/models"), clean_base.to_string()));
    // 2. If base URL does not end in /v1, try root/v1/models
    if !clean_base.ends_with("/v1") {
        candidates.push((format!("{root}/v1/models"), format!("{root}/v1")));
    }
    // 3. Fallback to root/models
    if format!("{root}/models") != format!("{clean_base}/models") {
        candidates.push((format!("{root}/models"), root.clone()));
    }

    let is_remote = base_url.starts_with("https://")
        || (!base_url.contains("localhost") && !base_url.contains("127.0.0.1"));
    let timeout = if is_remote {
        std::time::Duration::from_millis(5000)
    } else {
        PROBE_TIMEOUT
    };

    let mut headers = HeaderMap::new();
    headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
    if let Some(key) = api_key {
        if !key.trim().is_empty() {
            if let Ok(val) = HeaderValue::from_str(&format!("Bearer {}", key.trim())) {
                headers.insert(AUTHORIZATION, val);
            }
        }
    }

    for (probe_url, chat_endpoint) in candidates {
        if let Ok(resp) = client
            .get(&probe_url)
            .headers(headers.clone())
            .timeout(timeout)
            .send()
            .await
        {
            if resp.status().is_success() {
                if let Ok(json) = resp.json::<Value>().await {
                    let models =
                        parse_openai_models_json(&json, &chat_endpoint, "openai-compatible");
                    if !models.is_empty() {
                        return Some((models, chat_endpoint));
                    }
                }
            }
        }
    }

    None
}

/// Probes an arbitrary URL attempting all known protocols in prioritized sequence.
pub async fn probe_endpoint(
    client: &reqwest::Client,
    url: &str,
    api_key: Option<&str>,
    cache: &DiscoveryCache,
) -> Option<Vec<DiscoveredModel>> {
    if cache.is_negatively_cached(url).await {
        return None;
    }

    if let Some(cached) = cache.get(url).await {
        return Some(cached);
    }

    // 1. Check Ollama tags first
    if let Some(models) = probe_ollama(client, url).await {
        cache.insert(url, models.clone()).await;
        return Some(models);
    }

    // 2. Check LM Studio next
    if let Some(models) = probe_lmstudio(client, url).await {
        cache.insert(url, models.clone()).await;
        return Some(models);
    }

    // 3. Fallback to OpenAI-compatible dual path
    if let Some((models, _endpoint)) = probe_openai_compatible(client, url, api_key).await {
        cache.insert(url, models.clone()).await;
        return Some(models);
    }

    cache.mark_failure(url).await;
    None
}

/// Discovers available local models across standard local servers (Ollama, LM Studio)
/// as well as the actively configured base URL.
pub async fn discover_all_local_models(
    client: &reqwest::Client,
    configured_base_url: Option<&str>,
    api_key: Option<&str>,
    cache: &DiscoveryCache,
) -> Vec<DiscoveredModel> {
    let mut endpoints_to_probe = Vec::new();

    if let Some(active) = configured_base_url {
        if !active.trim().is_empty() {
            endpoints_to_probe.push(active.to_string());
        }
    }

    for &(_name, default_url) in DEFAULT_LOCAL_ENDPOINTS {
        if !endpoints_to_probe
            .iter()
            .any(|u| u.starts_with(default_url))
        {
            endpoints_to_probe.push(default_url.to_string());
        }
    }

    let mut discovered = Vec::new();
    let mut seen_keys = std::collections::HashSet::new();

    for endpoint in endpoints_to_probe {
        if let Some(models) = probe_endpoint(client, &endpoint, api_key, cache).await {
            for m in models {
                let key = (m.id.clone(), m.endpoint.clone());
                if seen_keys.insert(key) {
                    discovered.push(m);
                }
            }
        }
    }

    discovered.sort_by(|a, b| match a.provider.cmp(&b.provider) {
        std::cmp::Ordering::Equal => a.id.cmp(&b.id),
        other => other,
    });

    discovered
}
