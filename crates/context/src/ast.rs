//! AST skeleton extraction for source code compaction.
//!
//! Produces structural outlines of source code (function signatures, type definitions,
//! classes, and interfaces) while replacing implementation bodies with concise markers
//! (`{ /* omitted */ }` or `...`), shrinking token footprints by 70–90%.

use kai_core::ContextError;

/// Language-aware AST skeleton extractor.
#[derive(Debug, Clone, Copy, Default)]
pub struct AstSkeleton;

impl AstSkeleton {
    /// Extracts a structural skeleton based on the file language or extension.
    ///
    /// Recognizes `"rust"`, `"python"`, `"typescript"`, `"javascript"`, `"go"`,
    /// or file extensions like `"rs"`, `"py"`, `"ts"`, `"js"`, `"go"`.
    /// Falls back to generic signature preservation for unrecognized languages.
    pub fn extract(code: &str, language: &str) -> String {
        let lang = language.to_ascii_lowercase();
        match lang.as_str() {
            "rust" | "rs" => Self::extract_rust(code),
            "python" | "py" => Self::extract_python(code),
            "typescript" | "ts" | "javascript" | "js" => Self::extract_typescript(code),
            "go" => Self::extract_go(code),
            _ => Self::extract_generic(code),
        }
    }

    /// Extracts structural outline for Rust source files.
    pub fn extract_rust(code: &str) -> String {
        let mut result = Vec::new();
        let mut depth: usize = 0;
        let mut omit_target_depth: Option<usize> = None;
        let mut in_fn_signature = false;
        let mut fn_sig_accumulator = String::new();
        let mut in_block_comment = false;

        for raw_line in code.lines() {
            let trimmed = raw_line.trim_start();

            // If we are currently omitting a function body
            if let Some(target) = omit_target_depth {
                let (opens, closes) =
                    Self::count_braces_in_line(raw_line, &mut in_block_comment, true);
                depth = depth.saturating_add(opens).saturating_sub(closes);
                if depth <= target {
                    omit_target_depth = None;
                }
                continue;
            }

            // Check if line contains a macro_rules declaration
            if !in_block_comment && trimmed.starts_with("macro_rules!") {
                if let Some(brace_pos) = raw_line.find('{') {
                    let sig_part = raw_line[..brace_pos].trim_end();
                    result.push(format!("{} {{ /* omitted */ }}", sig_part));
                    let (opens, closes) =
                        Self::count_braces_in_line(raw_line, &mut in_block_comment, true);
                    let initial_depth = depth;
                    depth = depth.saturating_add(opens).saturating_sub(closes);
                    if depth > initial_depth {
                        omit_target_depth = Some(initial_depth);
                    }
                    continue;
                }
            }

            // Check if line contains a function declaration
            if !in_block_comment && !in_fn_signature && Self::is_rust_fn_start(trimmed) {
                in_fn_signature = true;
                fn_sig_accumulator.clear();
            }

            if in_fn_signature {
                if !fn_sig_accumulator.is_empty() {
                    fn_sig_accumulator.push('\n');
                }
                fn_sig_accumulator.push_str(raw_line);

                if let Some(brace_pos) = Self::find_fn_body_brace(&fn_sig_accumulator, true) {
                    let sig_part = fn_sig_accumulator[..brace_pos].trim_end();
                    result.push(format!("{} {{ /* omitted */ }}", sig_part));
                    in_fn_signature = false;
                    fn_sig_accumulator.clear();

                    let (opens, closes) =
                        Self::count_braces_in_line(raw_line, &mut in_block_comment, true);
                    let initial_depth = depth;
                    depth = depth.saturating_add(opens).saturating_sub(closes);
                    if depth > initial_depth {
                        omit_target_depth = Some(initial_depth);
                    }
                } else if fn_sig_accumulator.ends_with(';') {
                    // Trait function or extern without body
                    result.push(fn_sig_accumulator.clone());
                    in_fn_signature = false;
                    fn_sig_accumulator.clear();
                }
                continue;
            }

            // Normal line: update depth and preserve
            let (opens, closes) = Self::count_braces_in_line(raw_line, &mut in_block_comment, true);
            depth = depth.saturating_add(opens).saturating_sub(closes);
            result.push(raw_line.to_string());
        }

        // Flush any trailing pending signature
        if in_fn_signature && !fn_sig_accumulator.is_empty() {
            result.push(fn_sig_accumulator);
        }

        result.join("\n")
    }

    /// Extracts structural outline for Python source files.
    pub fn extract_python(code: &str) -> String {
        let mut result = Vec::new();
        let mut in_docstring = false;
        let mut docstring_delim = "";
        let mut omit_indent: Option<usize> = None;
        let mut pending_docstring = false;
        let mut in_multiline_sig = false;
        let mut sig_depth: usize = 0;
        let mut fn_base_indent: usize = 0;
        let mut sig_accumulator = String::new();

        for raw_line in code.lines() {
            let trimmed = raw_line.trim_start();
            let indent = raw_line.len() - trimmed.len();

            // Handle multi-line docstring
            if in_docstring {
                result.push(raw_line.to_string());
                if trimmed.contains(docstring_delim) {
                    in_docstring = false;
                }
                continue;
            }

            // Multiline signature collection (def foo(\n a,\n b):)
            if in_multiline_sig {
                sig_accumulator.push('\n');
                sig_accumulator.push_str(raw_line);
                if let Some(colon_pos) = Self::find_python_terminal_colon(raw_line, &mut sig_depth)
                {
                    in_multiline_sig = false;
                    let after_colon = raw_line[colon_pos + 1..].trim();
                    if !after_colon.is_empty() && !after_colon.starts_with('#') {
                        result.push(format!("{} ...", sig_accumulator.trim_end()));
                    } else {
                        result.push(sig_accumulator.clone());
                        omit_indent = Some(fn_base_indent);
                        pending_docstring = true;
                    }
                    sig_accumulator.clear();
                }
                continue;
            }

            if trimmed.is_empty() {
                if omit_indent.is_none() {
                    result.push(raw_line.to_string());
                }
                continue;
            }

            // Check if we exited omitted block
            if let Some(target) = omit_indent {
                if indent <= target {
                    omit_indent = None;
                } else {
                    // Check for docstring immediately following def
                    if pending_docstring {
                        if trimmed.starts_with("\"\"\"") || trimmed.starts_with("'''") {
                            let delim = if trimmed.starts_with("\"\"\"") {
                                "\"\"\""
                            } else {
                                "'''"
                            };
                            result.push(raw_line.to_string());
                            let rest = &trimmed[3..];
                            if !rest.contains(delim) {
                                in_docstring = true;
                                docstring_delim = delim;
                            }
                            pending_docstring = false;
                            let indent_str = " ".repeat(indent);
                            result.push(format!("{}...", indent_str));
                            continue;
                        }
                        pending_docstring = false;
                        let indent_str = " ".repeat(indent);
                        result.push(format!("{}...", indent_str));
                    }
                    continue;
                }
            }

            // Preserve Python decorators (@property, @staticmethod, etc.)
            if trimmed.starts_with('@') {
                result.push(raw_line.to_string());
                continue;
            }

            // Check for class or def declarations
            if trimmed.starts_with("def ") || trimmed.starts_with("async def ") {
                fn_base_indent = indent;
                sig_depth = 0;
                if let Some(colon_pos) = Self::find_python_terminal_colon(raw_line, &mut sig_depth)
                {
                    let after_colon = raw_line[colon_pos + 1..].trim();
                    if !after_colon.is_empty() && !after_colon.starts_with('#') {
                        // Single-line function with inline body: def foo(): return 1
                        let sig = &raw_line[..=colon_pos];
                        result.push(format!("{} ...", sig));
                        continue;
                    }

                    result.push(raw_line.to_string());
                    omit_indent = Some(fn_base_indent);
                    pending_docstring = true;
                } else {
                    in_multiline_sig = true;
                    sig_accumulator = raw_line.to_string();
                }
            } else {
                result.push(raw_line.to_string());
            }
        }

        if in_multiline_sig && !sig_accumulator.is_empty() {
            result.push(sig_accumulator);
        }

        result.join("\n")
    }

    /// Scans a Python signature fragment, updating nesting depth of `(`, `[`, and `{`.
    /// Returns Some(colon_byte_index) if the terminal `:` is found at nesting depth 0.
    fn find_python_terminal_colon(s: &str, depth: &mut usize) -> Option<usize> {
        let mut in_str = false;
        let mut str_delim = '\0';
        let mut prev_char = '\0';

        for (idx, ch) in s.char_indices() {
            if in_str {
                if ch == str_delim && prev_char != '\\' {
                    in_str = false;
                }
                prev_char = ch;
                continue;
            }

            if ch == '"' || ch == '\'' {
                in_str = true;
                str_delim = ch;
                prev_char = ch;
                continue;
            }

            if ch == '#' {
                break;
            }

            match ch {
                '(' | '[' | '{' => *depth += 1,
                ')' | ']' | '}' => *depth = depth.saturating_sub(1),
                ':' if *depth == 0 => return Some(idx),
                _ => {}
            }
            prev_char = ch;
        }
        None
    }

    /// Checks whether an apostrophe at `idx` in `s` initiates a Rust lifetime token (e.g. `'a`, `'static`, `'_`)
    /// rather than a character literal (e.g. `'a'`, `'\n'`).
    fn is_rust_lifetime_at(s: &str, idx: usize) -> bool {
        let remainder = &s[idx + 1..];
        let mut chars = remainder.chars().peekable();

        // Lifetimes must start with an ASCII alphabetic char or underscore
        match chars.next() {
            Some(c) if c.is_ascii_alphabetic() || c == '_' => {}
            _ => return false,
        }

        // Consume remaining valid identifier chars
        while let Some(&c) = chars.peek() {
            if c.is_ascii_alphanumeric() || c == '_' {
                chars.next();
            } else {
                break;
            }
        }

        // If immediately followed by a closing single quote, this is a char literal ('a' or 'ident'), not a lifetime
        chars.peek() != Some(&'\'')
    }

    /// Finds the byte index of the function body `{` in a signature accumulator.
    ///
    /// The body `{` must occur at parenthesis depth 0 (i.e., outside the parameter list)
    /// to avoid prematurely matching object/struct destructuring patterns inside parameters
    /// such as `fn foo(Point { x, y }: Point)` or `function bar({ a, b }: Props)`.
    fn find_fn_body_brace(s: &str, is_rust: bool) -> Option<usize> {
        let mut paren_depth = 0usize;
        let mut in_str = false;
        let mut in_char = false;
        let mut in_tick = false;
        let mut in_block_comment = false;
        let mut prev_char = '\0';

        let mut chars = s.char_indices().peekable();
        while let Some((idx, ch)) = chars.next() {
            if !in_str && !in_char && !in_tick && !in_block_comment {
                if ch == '/' && chars.peek().map(|&(_, c)| c) == Some('/') {
                    // Line comment begins: skip until newline
                    for (_, c) in chars.by_ref() {
                        if c == '\n' {
                            break;
                        }
                    }
                    prev_char = '\n';
                    continue;
                }
                if ch == '/' && chars.peek().map(|&(_, c)| c) == Some('*') {
                    in_block_comment = true;
                    chars.next(); // consume '*'
                    prev_char = '*';
                    continue;
                }
            }

            if in_block_comment {
                if ch == '*' && chars.peek().map(|&(_, c)| c) == Some('/') {
                    in_block_comment = false;
                    chars.next(); // consume '/'
                    prev_char = '/';
                }
                continue;
            }

            if ch == '\'' && !in_str && !in_tick && prev_char != '\\' {
                if in_char {
                    in_char = false;
                } else if is_rust && Self::is_rust_lifetime_at(s, idx) {
                    // Rust lifetime ('a, 'static, '_), do not toggle in_char
                } else {
                    in_char = true;
                }
            } else if ch == '`' && !in_str && !in_char && prev_char != '\\' {
                in_tick = !in_tick;
            } else if ch == '"' && !in_char && !in_tick && prev_char != '\\' {
                in_str = !in_str;
            } else if !in_str && !in_char && !in_tick {
                match ch {
                    '(' => paren_depth += 1,
                    ')' => paren_depth = paren_depth.saturating_sub(1),
                    '{' if paren_depth == 0 => return Some(idx),
                    _ => {}
                }
            }
            prev_char = ch;
        }
        None
    }

    /// Extracts structural outline for TypeScript / JavaScript files.
    pub fn extract_typescript(code: &str) -> String {
        let mut result = Vec::new();
        let mut depth: usize = 0;
        let mut omit_target_depth: Option<usize> = None;
        let mut in_fn = false;
        let mut fn_accumulator = String::new();
        let mut in_block_comment = false;

        for raw_line in code.lines() {
            let trimmed = raw_line.trim_start();

            if let Some(target) = omit_target_depth {
                let (opens, closes) =
                    Self::count_braces_in_line(raw_line, &mut in_block_comment, false);
                depth = depth.saturating_add(opens).saturating_sub(closes);
                if depth <= target {
                    omit_target_depth = None;
                }
                continue;
            }

            if !in_block_comment && !in_fn && Self::is_ts_fn_start(trimmed) {
                in_fn = true;
                fn_accumulator.clear();
            }

            if in_fn {
                if !fn_accumulator.is_empty() {
                    fn_accumulator.push('\n');
                }
                fn_accumulator.push_str(raw_line);

                if let Some(brace_pos) = Self::find_fn_body_brace(&fn_accumulator, false) {
                    let sig_part = fn_accumulator[..brace_pos].trim_end();
                    result.push(format!("{} {{ /* omitted */ }}", sig_part));
                    in_fn = false;
                    fn_accumulator.clear();

                    let (opens, closes) =
                        Self::count_braces_in_line(raw_line, &mut in_block_comment, false);
                    let initial_depth = depth;
                    depth = depth.saturating_add(opens).saturating_sub(closes);
                    if depth > initial_depth {
                        omit_target_depth = Some(initial_depth);
                    }
                } else if fn_accumulator.ends_with(';') {
                    result.push(fn_accumulator.clone());
                    in_fn = false;
                    fn_accumulator.clear();
                }
                continue;
            }

            let (opens, closes) =
                Self::count_braces_in_line(raw_line, &mut in_block_comment, false);
            depth = depth.saturating_add(opens).saturating_sub(closes);
            result.push(raw_line.to_string());
        }

        result.join("\n")
    }

    /// Extracts structural outline for Go source files.
    pub fn extract_go(code: &str) -> String {
        let mut result = Vec::new();
        let mut depth: usize = 0;
        let mut omit_target_depth: Option<usize> = None;
        let mut in_fn = false;
        let mut fn_accumulator = String::new();
        let mut in_block_comment = false;

        for raw_line in code.lines() {
            let trimmed = raw_line.trim_start();

            if let Some(target) = omit_target_depth {
                let (opens, closes) =
                    Self::count_braces_in_line(raw_line, &mut in_block_comment, false);
                depth = depth.saturating_add(opens).saturating_sub(closes);
                if depth <= target {
                    omit_target_depth = None;
                }
                continue;
            }

            if !in_block_comment && !in_fn && trimmed.starts_with("func ") {
                in_fn = true;
                fn_accumulator.clear();
            }

            if in_fn {
                if !fn_accumulator.is_empty() {
                    fn_accumulator.push('\n');
                }
                fn_accumulator.push_str(raw_line);

                if let Some(brace_pos) = Self::find_fn_body_brace(&fn_accumulator, false) {
                    let sig_part = fn_accumulator[..brace_pos].trim_end();
                    result.push(format!("{} {{ /* omitted */ }}", sig_part));
                    in_fn = false;
                    fn_accumulator.clear();

                    let (opens, closes) =
                        Self::count_braces_in_line(raw_line, &mut in_block_comment, false);
                    let initial_depth = depth;
                    depth = depth.saturating_add(opens).saturating_sub(closes);
                    if depth > initial_depth {
                        omit_target_depth = Some(initial_depth);
                    }
                }
                continue;
            }

            let (opens, closes) =
                Self::count_braces_in_line(raw_line, &mut in_block_comment, false);
            depth = depth.saturating_add(opens).saturating_sub(closes);
            result.push(raw_line.to_string());
        }

        result.join("\n")
    }

    /// Fallback extractor for unrecognized languages preserving comments and signatures.
    pub fn extract_generic(code: &str) -> String {
        let mut result = Vec::new();
        for line in code.lines() {
            let trimmed = line.trim();
            // Preserve top-level comments, declarations, imports
            if trimmed.starts_with('#')
                || trimmed.starts_with("//")
                || trimmed.starts_with("import")
                || trimmed.starts_with("from")
                || trimmed.starts_with("package")
                || trimmed.contains('{')
                || trimmed.contains(':')
            {
                result.push(line.to_string());
            }
        }
        if result.is_empty() {
            code.to_string()
        } else {
            result.join("\n")
        }
    }

    /// Extensible Tree-Sitter parser hook.
    ///
    /// Parses `code` into a syntax tree using a supplied [`tree_sitter::Language`].
    pub fn parse_with_tree_sitter(
        code: &str,
        language: &tree_sitter::Language,
    ) -> Result<tree_sitter::Tree, ContextError> {
        let mut parser = tree_sitter::Parser::new();
        parser
            .set_language(language)
            .map_err(|err| ContextError::AstParsingFailed {
                language: "custom".to_string(),
                reason: format!("Failed to configure parser: {:?}", err),
            })?;

        parser
            .parse(code, None)
            .ok_or_else(|| ContextError::AstParsingFailed {
                language: "custom".to_string(),
                reason: "Parser produced no syntax tree".to_string(),
            })
    }

    /// Checks if a trimmed line starts a Rust function definition.
    fn is_rust_fn_start(line: &str) -> bool {
        let tokens: Vec<&str> = line.split_whitespace().collect();
        for (i, &token) in tokens.iter().enumerate() {
            if token == "fn" {
                return true;
            }
            // Only allow valid fn qualifiers before 'fn'
            if i == 0
                && !matches!(
                    token,
                    "pub"
                        | "pub(crate)"
                        | "pub(super)"
                        | "async"
                        | "const"
                        | "unsafe"
                        | "extern"
                        | "\"C\""
                )
                && !token.starts_with("pub(")
            {
                return false;
            }
        }
        false
    }

    /// Checks if a trimmed line starts a TypeScript/JavaScript function or method.
    fn is_ts_fn_start(line: &str) -> bool {
        if line.starts_with("function ")
            || line.starts_with("export function ")
            || line.starts_with("export default function ")
            || line.starts_with("async function ")
            || line.starts_with("export async function ")
            || line.starts_with("public ")
            || line.starts_with("private ")
            || line.starts_with("protected ")
            || line.starts_with("static ")
            || line.starts_with("async ")
            || line.starts_with("constructor(")
        {
            return true;
        }

        // Arrow functions: const myFunc = (...) => {
        if line.contains("=>")
            && (line.starts_with("const ")
                || line.starts_with("let ")
                || line.starts_with("var ")
                || line.starts_with("export const "))
        {
            return true;
        }

        // Standard class methods without modifiers: methodName(...) {
        if let Some(paren_pos) = line.find('(') {
            let name_part = line[..paren_pos].trim();
            if !name_part.is_empty()
                && name_part
                    .chars()
                    .all(|c| c.is_alphanumeric() || c == '_' || c == '$')
                && !matches!(name_part, "if" | "for" | "while" | "switch" | "catch")
                && (line.ends_with('{') || line.contains(") {"))
            {
                return true;
            }
        }

        false
    }

    /// Counts open `{` and close `}` in a line while ignoring strings, chars, line comments, and block comments.
    fn count_braces_in_line(
        line: &str,
        in_block_comment: &mut bool,
        is_rust: bool,
    ) -> (usize, usize) {
        let mut opens = 0usize;
        let mut closes = 0usize;
        let mut in_str = false;
        let mut in_char = false;
        let mut in_tick = false;
        let mut prev_char = '\0';

        let mut chars = line.char_indices().peekable();
        while let Some((idx, ch)) = chars.next() {
            if !in_str && !in_char && !in_tick && !*in_block_comment {
                if ch == '/' && chars.peek().map(|&(_, c)| c) == Some('/') {
                    // Line comment begins, stop scanning line
                    break;
                }
                if ch == '/' && chars.peek().map(|&(_, c)| c) == Some('*') {
                    *in_block_comment = true;
                    chars.next(); // consume '*'
                    prev_char = '*';
                    continue;
                }
            }

            if *in_block_comment {
                if ch == '*' && chars.peek().map(|&(_, c)| c) == Some('/') {
                    *in_block_comment = false;
                    chars.next(); // consume '/'
                    prev_char = '/';
                }
                continue;
            }

            if ch == '\'' && !in_str && !in_tick && prev_char != '\\' {
                if in_char {
                    in_char = false;
                } else if is_rust && Self::is_rust_lifetime_at(line, idx) {
                    // Rust lifetime ('a, 'static), do not enter char literal mode
                } else {
                    in_char = true;
                }
            } else if ch == '`' && !in_str && !in_char && prev_char != '\\' {
                in_tick = !in_tick;
            } else if ch == '"' && !in_char && !in_tick && prev_char != '\\' {
                in_str = !in_str;
            } else if !in_str && !in_char && !in_tick {
                if ch == '{' {
                    opens += 1;
                } else if ch == '}' {
                    closes += 1;
                }
            }
            prev_char = ch;
        }

        (opens, closes)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_rust_skeleton_extraction() {
        let code = r#"
pub struct Service {
    name: String,
    port: u16,
}

impl Service {
    pub fn new(name: String) -> Self {
        let default_port = 8080;
        Self { name, port: default_port }
    }

    pub fn run(&self) {
        println!("Running {}", self.name);
    }
}
"#;
        let skeleton = AstSkeleton::extract(code, "rust");
        assert!(skeleton.contains("pub struct Service {"));
        assert!(skeleton.contains("pub fn new(name: String) -> Self { /* omitted */ }"));
        assert!(skeleton.contains("pub fn run(&self) { /* omitted */ }"));
        assert!(!skeleton.contains("println!"));
        assert!(!skeleton.contains("default_port"));
    }

    #[test]
    fn test_rust_macro_extraction() {
        let code = r#"
macro_rules! my_macro {
    ($val:expr) => {
        println!("{}", $val);
    };
}
"#;
        let skeleton = AstSkeleton::extract(code, "rust");
        assert!(skeleton.contains("macro_rules! my_macro { /* omitted */ }"));
        assert!(!skeleton.contains("println!"));
    }

    #[test]
    fn test_python_skeleton_extraction() {
        let code = r#"
class Controller:
    """Class docstring."""
    def __init__(self, name: str):
        self.name = name

    def execute(self) -> bool:
        """Execute method."""
        return True
"#;
        let skeleton = AstSkeleton::extract(code, "python");
        assert!(skeleton.contains("class Controller:"));
        assert!(skeleton.contains("def __init__(self, name: str):"));
        assert!(skeleton.contains("def execute(self) -> bool:"));
    }

    #[test]
    fn test_python_multiline_typed_signature() {
        let code = r#"
class Worker:
    @property
    def is_active(self) -> bool:
        return self._active

    def process(
        self,
        items: list[str],
        timeout: int = 30,
    ) -> bool:
        """Process multiple items with timeout."""
        print("Processing...")
        return len(items) > 0
"#;
        let skeleton = AstSkeleton::extract(code, "python");
        assert!(skeleton.contains("@property"));
        assert!(skeleton.contains("def is_active(self) -> bool:"));
        assert!(skeleton.contains("def process("));
        assert!(skeleton.contains("items: list[str],"));
        assert!(skeleton.contains("timeout: int = 30,"));
        assert!(skeleton.contains(") -> bool:"));
        assert!(skeleton.contains("\"\"\"Process multiple items with timeout.\"\"\""));
        assert!(!skeleton.contains("return self._active"));
        assert!(!skeleton.contains("print(\"Processing...\")"));
    }

    #[test]
    fn test_typescript_skeleton_extraction() {
        let code = r#"
export class ApiClient {
    fetchData() {
        return axios.get("/api");
    }
}

const compute = (x: number) => {
    return x * 2;
};
"#;
        let skeleton = AstSkeleton::extract(code, "typescript");
        assert!(skeleton.contains("fetchData() { /* omitted */ }"));
        assert!(skeleton.contains("const compute = (x: number) => { /* omitted */ }"));
        assert!(!skeleton.contains("axios.get"));
        assert!(!skeleton.contains("return x * 2"));
    }

    #[test]
    fn test_count_braces_with_strings_and_comments() {
        let line = "let msg = \"{ ignored }\"; // { also ignored }";
        let (opens, closes) = AstSkeleton::count_braces_in_line(line, &mut false, true);
        assert_eq!(opens, 0);
        assert_eq!(closes, 0);

        let real_line = "fn test() { if true { } }";
        let (opens, closes) = AstSkeleton::count_braces_in_line(real_line, &mut false, true);
        assert_eq!(opens, 2);
        assert_eq!(closes, 2);

        let block_comment_line = "let x = 1; /* { ignored } */ let y = 2; // {";
        let (opens, closes) =
            AstSkeleton::count_braces_in_line(block_comment_line, &mut false, true);
        assert_eq!(opens, 0);
        assert_eq!(closes, 0);

        let char_literal_line = "let c = '{'; let d = '}';";
        let (opens, closes) =
            AstSkeleton::count_braces_in_line(char_literal_line, &mut false, true);
        assert_eq!(opens, 0);
        assert_eq!(closes, 0);

        // Rust lifetime in struct definition line
        let lifetime_line = "pub struct ItemRef<'a> {";
        let (opens, closes) = AstSkeleton::count_braces_in_line(lifetime_line, &mut false, true);
        assert_eq!(opens, 1);
        assert_eq!(closes, 0);

        // Multi-line block comment spanning lines
        let mut in_comment = false;
        let c1 = "/* begin block";
        let (o1, c_1) = AstSkeleton::count_braces_in_line(c1, &mut in_comment, true);
        assert_eq!((o1, c_1), (0, 0));
        assert!(in_comment);

        let c2 = "   { open brace inside comment }";
        let (o2, c_2) = AstSkeleton::count_braces_in_line(c2, &mut in_comment, true);
        assert_eq!((o2, c_2), (0, 0));
        assert!(in_comment);

        let c3 = "   end of comment */ { valid_code }";
        let (o3, c_3) = AstSkeleton::count_braces_in_line(c3, &mut in_comment, true);
        assert_eq!((o3, c_3), (1, 1));
        assert!(!in_comment);
    }

    #[test]
    fn test_destructuring_parameters_in_signatures() {
        let ts_code = r#"
export function renderWidget({ id, title }: { id: string; title: string }): Html {
    return `<div>${id}: ${title}</div>`;
}

const handle = ({ x, y }: Point): number => {
    return x + y;
};
"#;
        let ts_skeleton = AstSkeleton::extract(ts_code, "typescript");
        assert!(ts_skeleton.contains("export function renderWidget({ id, title }: { id: string; title: string }): Html { /* omitted */ }"));
        assert!(
            ts_skeleton.contains("const handle = ({ x, y }: Point): number => { /* omitted */ }")
        );
        assert!(!ts_skeleton.contains("return `<div>"));

        let rs_code = r#"
pub fn calculate(Point { x, y }: Point) -> i32 {
    let ch = '{';
    x + y
}

pub fn next_fn() -> bool {
    true
}
"#;
        let rs_skeleton = AstSkeleton::extract(rs_code, "rust");
        assert!(rs_skeleton
            .contains("pub fn calculate(Point { x, y }: Point) -> i32 { /* omitted */ }"));
        assert!(rs_skeleton.contains("pub fn next_fn() -> bool { /* omitted */ }"));
        assert!(!rs_skeleton.contains("let ch = '{';"));
    }

    #[test]
    fn test_rust_lifetimes_in_signature_and_struct() {
        let code = r#"
pub fn process<'a, 'b: 'a>(item: &'a str, fallback: &'b str) -> &'a str {
    let internal = 42;
    item
}

pub struct Wrapper<'a> {
    pub inner: &'a str,
}

impl<'a> Wrapper<'a> {
    pub fn get(&'a self) -> &'a str {
        self.inner
    }
}
"#;
        let skeleton = AstSkeleton::extract(code, "rust");
        assert!(skeleton.contains("pub fn process<'a, 'b: 'a>(item: &'a str, fallback: &'b str) -> &'a str { /* omitted */ }"));
        assert!(skeleton.contains("pub fn get(&'a self) -> &'a str { /* omitted */ }"));
        assert!(!skeleton.contains("let internal = 42;"));
    }

    #[test]
    fn test_multiline_block_comments_with_braces() {
        let code = r#"
/*
   fn commented_out() {
       let a = 1;
   }
*/
pub fn active_fn() -> i32 {
    100
}
"#;
        let skeleton = AstSkeleton::extract(code, "rust");
        assert!(skeleton.contains("pub fn active_fn() -> i32 { /* omitted */ }"));
        assert!(!skeleton.contains("fn commented_out() { /* omitted */ }"));
    }
}
