//! Dynamic procedural skill registry and synthesis for KAI agents.
//!
//! Provides [`SkillRegistry`] for discovering, parsing, and retrieving procedural markdown
//! skill files (with YAML frontmatter) from `.kai/skills/`, as well as [`LearnSkillTool`]
//! for persisting newly synthesized procedural knowledge.

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

use kai_core::error::{Result, ToolError};
use kai_core::message::ToolResult;
use kai_core::traits::{BoxFuture, PermissionCategory, SkillDefinition, Tool, ToolContext};
use serde_json::json;

/// Dynamic procedural skill registry holding parsed [`SkillDefinition`] records.
#[derive(Debug, Default, Clone)]
pub struct SkillRegistry {
    skills: HashMap<String, SkillDefinition>,
}

impl SkillRegistry {
    /// Constructs an empty [`SkillRegistry`].
    pub fn new() -> Self {
        Self {
            skills: HashMap::new(),
        }
    }

    /// Registers a [`SkillDefinition`] in memory.
    pub fn register(&mut self, skill: SkillDefinition) {
        self.skills.insert(skill.name.clone(), skill);
    }

    /// Returns the number of registered skills.
    pub fn len(&self) -> usize {
        self.skills.len()
    }

    /// Returns `true` if the registry contains no skills.
    pub fn is_empty(&self) -> bool {
        self.skills.is_empty()
    }

    /// Retrieves a skill definition by name.
    pub fn get(&self, name: &str) -> Option<&SkillDefinition> {
        self.skills.get(name)
    }

    /// Returns references to all registered skills in deterministic order.
    pub fn all_skills(&self) -> Vec<&SkillDefinition> {
        let mut list: Vec<&SkillDefinition> = self.skills.values().collect();
        list.sort_by(|a, b| a.name.cmp(&b.name));
        list
    }

    /// Finds all skills whose name, description, or triggers match the query in deterministic order.
    pub fn find_matching(&self, query: &str) -> Vec<&SkillDefinition> {
        let mut matches: Vec<&SkillDefinition> = self
            .skills
            .values()
            .filter(|s| s.matches_query(query))
            .collect();
        matches.sort_by(|a, b| a.name.cmp(&b.name));
        matches
    }

    /// Maximum allowed bytes for injected procedural skill prompt blocks (root 4 KB cap).
    pub const MAX_PROMPT_BLOCK_BYTES: usize = 4096;
    /// Maximum number of skills included in a single prompt block.
    pub const MAX_PROMPT_SKILLS: usize = 5;

    /// Formats matching skills into a compact instruction block for agent prompt injection.
    pub fn format_prompt_block(&self, query: &str) -> Option<String> {
        let matches = self.find_matching(query);
        if matches.is_empty() {
            return None;
        }

        let mut block = String::from("## Relevant Procedural Skills\n\n");
        let total_matches = matches.len();
        let mut included = 0;

        for skill in matches.iter().take(Self::MAX_PROMPT_SKILLS) {
            let entry = format!(
                "### Skill: {}\n{}\n\n```markdown\n{}\n```\n\n",
                skill.name,
                skill.description,
                skill.instructions.trim()
            );

            if block.len() + entry.len() > Self::MAX_PROMPT_BLOCK_BYTES {
                break;
            }

            block.push_str(&entry);
            included += 1;
        }

        let remaining = total_matches.saturating_sub(included);
        if remaining > 0 {
            block.push_str(&format!(
                "[Truncated: {remaining} remaining items. Refine query]\n"
            ));
        }

        Some(block)
    }

    /// Parses a markdown string with optional YAML frontmatter into a [`SkillDefinition`].
    pub fn parse_skill_markdown(content: &str, fallback_name: &str) -> SkillDefinition {
        let trimmed = content.trim();
        if !trimmed.starts_with("---") {
            return SkillDefinition::new(fallback_name, "Procedural skill", trimmed);
        }

        let mut name = fallback_name.to_string();
        let mut description = String::new();
        let mut triggers = Vec::new();
        let mut instructions = String::new();

        let mut in_frontmatter = false;
        let mut frontmatter_ended = false;
        let mut in_triggers_list = false;

        for line in trimmed.lines() {
            let line_trimmed = line.trim();

            if line_trimmed == "---" {
                if !in_frontmatter && !frontmatter_ended {
                    in_frontmatter = true;
                    continue;
                } else if in_frontmatter {
                    in_frontmatter = false;
                    frontmatter_ended = true;
                    continue;
                }
            }

            if in_frontmatter {
                if line_trimmed.starts_with('#') {
                    continue;
                }

                if in_triggers_list {
                    if line_trimmed.starts_with('-') {
                        let item_raw = line_trimmed.trim_start_matches('-').trim();
                        let item_clean = if let Some((before_hash, _)) = item_raw.split_once('#') {
                            before_hash.trim()
                        } else {
                            item_raw
                        };
                        let item = item_clean.trim_matches('"').trim_matches('\'');
                        if !item.is_empty() {
                            triggers.push(item.to_string());
                        }
                        continue;
                    } else if !line.starts_with(' ') && !line.starts_with('\t') {
                        in_triggers_list = false;
                    }
                }

                if let Some((key, val)) = line_trimmed.split_once(':') {
                    let k = key.trim().to_ascii_lowercase();
                    let val_trimmed = val.trim();
                    let val_no_comment = if (val_trimmed.starts_with('"')
                        && val_trimmed.ends_with('"'))
                        || (val_trimmed.starts_with('\'') && val_trimmed.ends_with('\''))
                    {
                        val_trimmed
                    } else if let Some((before_hash, _)) = val_trimmed.split_once('#') {
                        before_hash.trim()
                    } else {
                        val_trimmed
                    };
                    let v = val_no_comment.trim().trim_matches('"').trim_matches('\'');
                    match k.as_str() {
                        "name" => {
                            if !v.is_empty() {
                                name = v.to_string();
                            }
                        }
                        "description" => {
                            description = v.to_string();
                        }
                        "triggers" => {
                            if v.starts_with('[') && v.ends_with(']') {
                                let inner = &v[1..v.len().saturating_sub(1)];
                                for part in inner.split(',') {
                                    let clean = part.trim().trim_matches('"').trim_matches('\'');
                                    if !clean.is_empty() {
                                        triggers.push(clean.to_string());
                                    }
                                }
                            } else if v.is_empty() {
                                in_triggers_list = true;
                            }
                        }
                        _ => {}
                    }
                }
            } else if frontmatter_ended {
                instructions.push_str(line);
                instructions.push('\n');
            }
        }

        if !frontmatter_ended {
            instructions = trimmed.to_string();
        }

        if description.is_empty() {
            description = format!("Procedural instructions for {name}");
        }

        SkillDefinition::new(name, description, instructions.trim()).with_triggers(triggers)
    }

    /// Scans a directory and loads all `.md` files as procedural skills.
    pub fn load_directory(&mut self, dir: &Path) -> Result<usize> {
        if !dir.exists() || !dir.is_dir() {
            return Ok(0);
        }

        let entries = fs::read_dir(dir).map_err(|e| ToolError::ExecutionFailed {
            name: "skill_registry".to_string(),
            reason: format!("Failed to read skills directory '{}': {e}", dir.display()),
        })?;

        let mut loaded_count = 0;
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_file() && path.extension().and_then(|ext| ext.to_str()) == Some("md") {
                let file_stem = path
                    .file_stem()
                    .and_then(|s| s.to_str())
                    .unwrap_or("unnamed_skill");

                if let Ok(content) = fs::read_to_string(&path) {
                    let mut skill = Self::parse_skill_markdown(&content, file_stem);
                    skill = skill.with_source_path(path.to_string_lossy().to_string());
                    self.register(skill);
                    loaded_count += 1;
                }
            }
        }

        Ok(loaded_count)
    }

    /// Transactionally persists a skill to disk in `.kai/skills/<name>.md`.
    pub fn save_skill_transactional(
        &mut self,
        base_dir: &Path,
        skill: SkillDefinition,
    ) -> Result<PathBuf> {
        let trimmed_name = skill.name.trim();
        if trimmed_name.is_empty() || !trimmed_name.chars().any(|c| c.is_ascii_alphanumeric()) {
            return Err(kai_core::error::KaiError::Tool(
                ToolError::InvalidArguments {
                    name: "skill_registry".to_string(),
                    reason: format!(
                        "Skill name '{}' is invalid: must contain at least one alphanumeric character",
                        skill.name
                    ),
                },
            ));
        }

        let skills_dir = base_dir.join(".kai").join("skills");
        fs::create_dir_all(&skills_dir).map_err(|e| ToolError::ExecutionFailed {
            name: "skill_registry".to_string(),
            reason: format!("Failed to create skills directory: {e}"),
        })?;

        let safe_name = skill
            .name
            .chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                    c
                } else {
                    '_'
                }
            })
            .collect::<String>();
        let target_file = skills_dir.join(format!("{safe_name}.md"));
        let temp_file = skills_dir.join(format!(
            ".tmp_{safe_name}_{}.md",
            kai_core::current_timestamp_ms()
        ));

        // Format YAML frontmatter + markdown instructions
        let mut content = String::from("---\n");
        content.push_str(&format!("name: \"{}\"\n", skill.name));
        content.push_str(&format!("description: \"{}\"\n", skill.description));
        if !skill.triggers.is_empty() {
            content.push_str("triggers:\n");
            for t in &skill.triggers {
                content.push_str(&format!("  - \"{t}\"\n"));
            }
        }
        content.push_str("---\n\n");
        content.push_str(skill.instructions.trim());
        content.push('\n');

        // Transactional write: write to temp file, then atomic rename
        fs::write(&temp_file, &content).map_err(|e| ToolError::ExecutionFailed {
            name: "skill_registry".to_string(),
            reason: format!("Failed writing temporary skill file: {e}"),
        })?;

        fs::rename(&temp_file, &target_file).map_err(|e| {
            let _ = fs::remove_file(&temp_file);
            ToolError::ExecutionFailed {
                name: "skill_registry".to_string(),
                reason: format!("Atomic rename failed for skill file: {e}"),
            }
        })?;

        let mut persisted = skill;
        persisted.source_path = Some(target_file.to_string_lossy().to_string());
        self.register(persisted);

        Ok(target_file)
    }
}

/// Native tool allowing agents to synthesize and persist new procedural skills.
#[derive(Debug, Clone, Default)]
pub struct LearnSkillTool;

impl LearnSkillTool {
    /// Constructs a new [`LearnSkillTool`].
    pub fn new() -> Self {
        Self
    }
}

impl Tool for LearnSkillTool {
    fn name(&self) -> &str {
        "learn_skill"
    }

    fn description(&self) -> &str {
        "Synthesizes and transactionally persists reusable procedural skills or workflows into .kai/skills/ for future turns"
    }

    fn schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "name": {
                    "type": "string",
                    "description": "Identifier slug for the skill (e.g. 'run-migrations', 'audit-deps')"
                },
                "description": {
                    "type": "string",
                    "description": "Concise summary of what workflow this skill executes"
                },
                "triggers": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "Keywords, commands, or triggers that activate this skill"
                },
                "instructions": {
                    "type": "string",
                    "description": "Step-by-step procedural markdown instructions and commands"
                }
            },
            "required": ["name", "description", "instructions"]
        })
    }

    fn permission_category(&self) -> PermissionCategory {
        PermissionCategory::FileWrite
    }

    fn execute<'a>(
        &'a self,
        arguments: serde_json::Value,
        context: &'a ToolContext,
    ) -> BoxFuture<'a, Result<ToolResult>> {
        Box::pin(async move {
            let name = arguments
                .get("name")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .trim();
            if name.is_empty() || !name.chars().any(|c| c.is_ascii_alphanumeric()) {
                return Ok(ToolResult::error(
                    self.name(),
                    "Missing or invalid argument 'name': must contain at least one alphanumeric character",
                ));
            }

            let description = arguments
                .get("description")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .trim();
            if description.is_empty() {
                return Ok(ToolResult::error(
                    self.name(),
                    "Missing required argument 'description'",
                ));
            }

            let instructions = arguments
                .get("instructions")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .trim();
            if instructions.is_empty() {
                return Ok(ToolResult::error(
                    self.name(),
                    "Missing required argument 'instructions'",
                ));
            }

            let triggers = arguments
                .get("triggers")
                .and_then(|v| v.as_array())
                .map(|arr| {
                    arr.iter()
                        .filter_map(|x| x.as_str().map(String::from))
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default();

            let skill =
                SkillDefinition::new(name, description, instructions).with_triggers(triggers);

            let mut registry = SkillRegistry::new();
            let saved_path = match registry.save_skill_transactional(&context.working_dir, skill) {
                Ok(path) => path,
                Err(err) => return Ok(ToolResult::error(self.name(), err.to_string())),
            };

            Ok(ToolResult::success(
                self.name(),
                format!(
                    "Skill '{}' successfully synthesized and persisted to '{}'",
                    name,
                    saved_path.display()
                ),
            ))
        })
    }
}
