//! Handler for the `kai tools` subcommand.

use kai_tools::default_tools;
use serde_json::json;

use crate::args::ToolsCommand;
use crate::error::Result;

/// Executes the `tools` command, listing registered capabilities.
pub fn execute(cmd: ToolsCommand) -> Result<()> {
    let tools = default_tools();

    if cmd.json {
        let list: Vec<serde_json::Value> = tools
            .iter()
            .map(|t| {
                json!({
                    "name": t.name(),
                    "description": t.description(),
                    "category": format!("{:?}", t.permission_category()),
                    "read_only": t.is_read_only(),
                    "schema": t.schema(),
                })
            })
            .collect();
        println!("{}", serde_json::to_string_pretty(&list)?);
        return Ok(());
    }

    println!(
        "{}{}KAI Registered Tools ({} available):{}\n",
        crate::ui::bold(),
        crate::ui::cyan(),
        tools.len(),
        crate::ui::reset()
    );
    println!(
        "{:<18} {:<18} {:<12} DESCRIPTION",
        "TOOL", "CATEGORY", "READ-ONLY"
    );
    println!("{:-<18} {:-<18} {:-<12} {:-<40}", "", "", "", "");

    for tool in tools {
        let category = format!("{:?}", tool.permission_category());
        let read_only = if tool.is_read_only() { "yes" } else { "no" };
        let desc = tool.description();
        let short_desc = if desc.len() > 50 {
            format!("{}...", &desc[..47])
        } else {
            desc.to_string()
        };

        println!(
            "{:<18} {:<18} {:<12} {}",
            tool.name(),
            category,
            read_only,
            short_desc
        );
    }
    println!();

    Ok(())
}
