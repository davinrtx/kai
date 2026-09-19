//! Dynamic grammar registry and WASM grammar loader abstraction.
//!
//! Decouples Tree-sitter AST parsing from host C compilers (`gcc`/`clang`) by providing
//! an extensible grammar registry capable of loading pre-compiled grammar binaries and WASM bytecode.

use std::collections::{HashMap, HashSet};
use std::sync::RwLock;

use kai_core::traits::GrammarLoader;

/// Extensible dynamic grammar registry supporting runtime registration and WASM grammar loading.
#[derive(Debug)]
pub struct WasmGrammarRegistry {
    supported_languages: HashSet<String>,
    bytecode_cache: RwLock<HashMap<String, Vec<u8>>>,
}

impl Default for WasmGrammarRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl WasmGrammarRegistry {
    /// Constructs a new [`WasmGrammarRegistry`] pre-configured with standard language definitions.
    pub fn new() -> Self {
        let mut supported = HashSet::new();
        let defaults = [
            "rust",
            "rs",
            "python",
            "py",
            "javascript",
            "js",
            "typescript",
            "ts",
            "go",
            "c",
            "cpp",
            "json",
            "toml",
            "markdown",
            "md",
        ];
        for lang in defaults {
            supported.insert(lang.to_string());
        }

        Self {
            supported_languages: supported,
            bytecode_cache: RwLock::new(HashMap::new()),
        }
    }

    /// Registers a custom grammar bytecode payload for the given language identifier.
    pub fn register_grammar(&mut self, language: impl Into<String>, bytecode: Vec<u8>) {
        let lang = language.into().to_lowercase();
        self.supported_languages.insert(lang.clone());
        if let Ok(mut cache) = self.bytecode_cache.write() {
            cache.insert(lang, bytecode);
        }
    }

    /// Retrieves cached bytecode for the specified language, if registered.
    pub fn get_grammar_bytes(&self, language: &str) -> Option<Vec<u8>> {
        let lang = language.to_lowercase();
        let cache = self.bytecode_cache.read().ok()?;
        cache.get(&lang).cloned()
    }

    /// Returns a list of all currently supported language identifiers.
    pub fn supported_languages(&self) -> Vec<String> {
        let mut langs: Vec<String> = self.supported_languages.iter().cloned().collect();
        langs.sort();
        langs
    }
}

impl GrammarLoader for WasmGrammarRegistry {
    fn supports_language(&self, language: &str) -> bool {
        let lang = language.to_lowercase();
        self.supported_languages.contains(&lang)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_wasm_grammar_registry_defaults_and_custom() {
        let mut registry = WasmGrammarRegistry::new();
        assert!(registry.supports_language("rust"));
        assert!(registry.supports_language("Python"));
        assert!(registry.supports_language("ts"));
        assert!(!registry.supports_language("fortran"));

        // Register custom grammar
        let dummy_wasm = vec![0x00, 0x61, 0x73, 0x6d, 0x01, 0x00, 0x00, 0x00];
        registry.register_grammar("fortran", dummy_wasm.clone());
        assert!(registry.supports_language("fortran"));

        let bytes = registry.get_grammar_bytes("fortran");
        assert_eq!(bytes, Some(dummy_wasm));
    }
}
