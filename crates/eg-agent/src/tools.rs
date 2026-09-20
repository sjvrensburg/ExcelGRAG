//! The `eg-mcp` tool table, as the model sees it and as the harness runs it.
//!
//! Nothing is declared here. [`definitions`] reads `eg_mcp::tools::TOOLS`
//! and [`execute`] dispatches through `eg_mcp::tools::call`, so a tool added
//! to the server is a tool the agent has, with the same name, description and
//! schema — the two cannot drift because there is only one.

use std::sync::{Arc, Mutex};

use rig_core::completion::ToolDefinition;
use serde_json::Value;

/// Every tool the server offers, in the server's order, as Rig's
/// [`ToolDefinition`].
pub fn definitions() -> Vec<ToolDefinition> {
    eg_mcp::tools::TOOLS
        .iter()
        .map(|t| ToolDefinition {
            name: t.name.to_string(),
            description: t.description.to_string(),
            parameters: (t.schema)(),
        })
        .collect()
}

/// The names in [`definitions`], for the run's allowed-tool set.
pub fn names() -> impl Iterator<Item = &'static str> {
    eg_mcp::tools::TOOLS.iter().map(|t| t.name)
}

/// Run one tool against the shared engine.
///
/// Runs on Tokio's blocking pool because every tool underneath is
/// synchronous and some — `dependents`, `find_value` — scan every formula or
/// every cell of a workbook. The engine lock is taken inside the blocking
/// task and released with it, so it is never held across an `.await`; the
/// discipline `eg-gui`'s `chat.rs` keeps for the same lock.
///
/// `Err` is the tool's own message for the model — "no sheet called that,
/// here are the ones there are" — not a failure of the harness. The model
/// can act on the first and cannot act on the second, which is why
/// `eg-mcp` returns failures as results rather than protocol errors, and
/// why this keeps the distinction.
pub async fn execute(
    engine: Arc<Mutex<eg_mcp::State>>,
    name: String,
    args: Value,
) -> Result<Result<String, String>, String> {
    tokio::task::spawn_blocking(move || {
        let mut state = engine
            .lock()
            .map_err(|_| "the engine lock was poisoned by an earlier panic".to_string())?;
        Ok(eg_mcp::tools::call(&mut state, &name, &args))
    })
    .await
    .map_err(|e| format!("the tool task did not complete: {e}"))?
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn definitions_are_the_servers_tools_verbatim() {
        let defs = definitions();
        assert_eq!(defs.len(), eg_mcp::tools::TOOLS.len());
        for (def, tool) in defs.iter().zip(eg_mcp::tools::TOOLS) {
            assert_eq!(def.name, tool.name);
            assert_eq!(def.description, tool.description);
            assert_eq!(def.parameters, (tool.schema)());
            assert_eq!(def.parameters["type"], "object", "{}", tool.name);
        }
    }
}
