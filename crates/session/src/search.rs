//! Pure-Rust in-memory BM25 full-text search index for historical session DAG nodes.

use std::collections::HashMap;

use kai_core::traits::SessionNode;

/// Single result match from a session text search query.
#[derive(Debug, Clone, PartialEq)]
pub struct SearchResult {
    /// Unique identifier of the matching session DAG node.
    pub node_id: String,
    /// Calculated BM25 relevance score.
    pub score: f64,
    /// Highlighted context snippet containing matched terms.
    pub snippet: String,
    /// List of query terms that matched this node.
    pub matched_terms: Vec<String>,
}

/// Inverted BM25 search index for querying session DAG histories without external C dependencies.
#[derive(Debug, Default, Clone)]
pub struct SessionSearchIndex {
    /// Inverted index: term -> list of (node_id, term_frequency)
    postings: HashMap<String, Vec<(String, usize)>>,
    /// Document lengths: node_id -> total token count
    doc_lengths: HashMap<String, usize>,
    /// Full node text cache: node_id -> text content
    doc_texts: HashMap<String, String>,
    /// Running total of document lengths for O(1) average length updates
    total_doc_len: usize,
    /// Average document length across indexed nodes
    avg_doc_len: f64,
}

impl SessionSearchIndex {
    /// Constructs a new empty [`SessionSearchIndex`].
    pub fn new() -> Self {
        Self::default()
    }

    /// Tokenizes text into lowercase alphanumeric words.
    fn tokenize(text: &str) -> Vec<String> {
        text.split(|c: char| !c.is_alphanumeric() && c != '_' && c != '-')
            .map(|s| s.trim().to_ascii_lowercase())
            .filter(|s| s.len() > 1)
            .collect()
    }

    /// Removes an indexed [`SessionNode`] from the search index.
    pub fn remove_node(&mut self, node_id: &str) {
        if let Some(old_len) = self.doc_lengths.remove(node_id) {
            self.doc_texts.remove(node_id);
            self.total_doc_len = self.total_doc_len.saturating_sub(old_len);

            for postings_list in self.postings.values_mut() {
                postings_list.retain(|(id, _)| id != node_id);
            }

            self.postings.retain(|_, list| !list.is_empty());

            self.avg_doc_len = if self.doc_lengths.is_empty() {
                0.0
            } else {
                self.total_doc_len as f64 / self.doc_lengths.len() as f64
            };
        }
    }

    /// Indexes an individual [`SessionNode`]. Idempotent: re-indexing replaces previous postings.
    pub fn index_node(&mut self, node: &SessionNode) {
        // Evict previous index entries for this node if already present
        if self.doc_lengths.contains_key(&node.id) {
            self.remove_node(&node.id);
        }

        let text = node.message.text_content();
        let tokens = Self::tokenize(&text);
        let doc_len = tokens.len();

        if doc_len == 0 {
            return;
        }

        let node_id = node.id.clone();
        let mut term_freqs: HashMap<String, usize> = HashMap::new();
        for token in tokens {
            *term_freqs.entry(token).or_insert(0) += 1;
        }

        for (term, freq) in term_freqs {
            self.postings
                .entry(term)
                .or_default()
                .push((node_id.clone(), freq));
        }

        self.doc_lengths.insert(node_id.clone(), doc_len);
        self.doc_texts.insert(node_id, text);
        self.total_doc_len += doc_len;
        self.avg_doc_len = self.total_doc_len as f64 / self.doc_lengths.len().max(1) as f64;
    }

    /// Indexes an entire slice of [`SessionNode`] records.
    pub fn index_history(&mut self, nodes: &[SessionNode]) {
        for node in nodes {
            self.index_node(node);
        }
    }

    /// Executes a BM25 query over indexed session nodes, returning ranked results.
    pub fn search(&self, query: &str, limit: usize) -> Vec<SearchResult> {
        let query_terms = Self::tokenize(query);
        if query_terms.is_empty() || self.doc_lengths.is_empty() {
            return Vec::new();
        }

        let n = self.doc_lengths.len() as f64;
        let k1 = 1.2;
        let b = 0.75;

        // node_id -> (score, matched_terms)
        let mut scores: HashMap<String, (f64, Vec<String>)> = HashMap::new();

        for term in &query_terms {
            if let Some(postings_list) = self.postings.get(term) {
                let df = postings_list.len() as f64;
                let idf = ((n - df + 0.5) / (df + 0.5) + 1.0).ln().max(0.1);

                for (node_id, tf) in postings_list {
                    let doc_len = *self.doc_lengths.get(node_id).unwrap_or(&1) as f64;
                    let tf_val = *tf as f64;
                    let numerator = tf_val * (k1 + 1.0);
                    let denominator =
                        tf_val + k1 * (1.0 - b + b * (doc_len / self.avg_doc_len.max(1.0)));
                    let term_score = idf * (numerator / denominator);

                    let entry = scores.entry(node_id.clone()).or_insert((0.0, Vec::new()));
                    entry.0 += term_score;
                    if !entry.1.contains(term) {
                        entry.1.push(term.clone());
                    }
                }
            }
        }

        let mut results: Vec<SearchResult> = scores
            .into_iter()
            .map(|(node_id, (score, matched_terms))| {
                let text = self
                    .doc_texts
                    .get(&node_id)
                    .map(|s| s.as_str())
                    .unwrap_or("");
                let snippet = Self::create_snippet(text, &matched_terms);
                SearchResult {
                    node_id,
                    score,
                    snippet,
                    matched_terms,
                }
            })
            .collect();

        results.sort_by(|a, b| {
            b.score
                .partial_cmp(&a.score)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        results.truncate(limit);
        results
    }

    /// Extracts a concise context snippet centered around matched keywords.
    fn create_snippet(text: &str, terms: &[String]) -> String {
        let lower = text.to_ascii_lowercase();
        let mut first_match = None;

        for term in terms {
            if let Some(pos) = lower.find(term) {
                first_match = Some(pos);
                break;
            }
        }

        let Some(pos) = first_match else {
            let mut end = 120.min(text.len());
            while end > 0 && !text.is_char_boundary(end) {
                end -= 1;
            }
            return text[..end].to_string();
        };

        let start = pos.saturating_sub(40);
        let end = (pos + 80).min(text.len());

        let mut boundary_start = start;
        while boundary_start > 0 && !text.is_char_boundary(boundary_start) {
            boundary_start -= 1;
        }

        let mut boundary_end = end;
        while boundary_end < text.len() && !text.is_char_boundary(boundary_end) {
            boundary_end += 1;
        }

        let prefix = if boundary_start > 0 { "..." } else { "" };
        let suffix = if boundary_end < text.len() { "..." } else { "" };

        format!("{prefix}{}{suffix}", &text[boundary_start..boundary_end])
    }
}
