//! # kai-context
//!
//! Deterministic context optimization, terminal scrubbing, bounded window reading,
//! grep-first navigation, and AST skeleton extraction for the KAI agent runtime.
//!
//! Level 2 of the hexagonal architecture, providing concrete context management
//! components implementing [`kai_core::ContextProcessor`].

pub mod ast;
pub mod grep;
pub mod processor;
pub mod scrubber;
pub mod window;

pub use ast::AstSkeleton;
pub use grep::{
    GrepMatch, GrepOptions, GrepResults, GrepSearcher, DEFAULT_MAX_FILE_SIZE_BYTES,
    DEFAULT_MAX_SEARCH_HORIZON,
};
pub use processor::DeterministicContextProcessor;
pub use scrubber::TerminalScrubber;
pub use window::{WindowReader, WindowResult, MAX_LINE_BUFFER_BYTES};
