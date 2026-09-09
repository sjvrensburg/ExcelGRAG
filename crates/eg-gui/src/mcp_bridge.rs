//! The MCP side of the shared process: an agent's tool calls, over stdio,
//! driven by the same `App` (and so the same engine and chat sessions) as
//! the web server.
//!
//! Built on `rmcp`, the official Rust MCP SDK — not a hand-rolled JSON-RPC
//! loop, unlike `eg-mcp`'s own standalone server, whose reason to hand-roll
//! (no async runtime for the CLI-only path) does not apply here: `eg-gui`
//! already runs on Tokio for Axum. `rmcp`'s stdio transport owns the whole
//! "no MCP peer attached" / "peer disconnected" lifecycle question, so this
//! module only has to implement [`rmcp::ServerHandler`].
//!
//! The existing `eg_mcp` tools are re-exposed generically — `list_tools`
//! and `call_tool` both iterate `eg_mcp::tools::TOOLS` rather than
//! hand-declaring 13 duplicate schemas — plus two GUI-only tools, `chat` and
//! `gui_show`, that only exist in this combined server.

use std::sync::Arc;

use rmcp::model::{
    CallToolRequestParams, CallToolResult, ContentBlock, InitializeResult, JsonObject,
    ListToolsResult, PaginatedRequestParams, ProtocolVersion, ServerCapabilities, Tool,
};
use rmcp::service::RequestContext;
use rmcp::transport::io::stdio;
use rmcp::{ErrorData, RoleServer, ServerHandler, ServiceExt};
use serde_json::{json, Value};

use crate::app::App;
use crate::chat::{self, TurnSource};

const GUI_TOOL_NAMES: &[&str] = &["chat", "gui_show"];

fn gui_tools() -> Vec<Tool> {
    vec![
        Tool::new(
            "chat",
            "Talk to this workbook's shared chat session — the same conversation a human sees \
             in the GUI's browser tab. Use this instead of `context`/`search` when you want \
             your question and its answer to show up live for whoever is watching the GUI, with \
             multi-turn memory (follow-ups carry forward the last workbook/sheet and citations).",
            schema_object(json!({
                "type": "object",
                "properties": {
                    "message": { "type": "string", "description": "What to ask, in words." },
                    "session_id": { "type": "string", "description": "Which chat session — default \"default\", the one the GUI's browser tab shows unless told otherwise." },
                },
                "required": ["message"],
                "additionalProperties": false,
            })),
        ),
        Tool::new(
            "gui_show",
            "Point the GUI's browser tab at a specific node, without asking a question or \
             running a search. Use this after `read_cells`/`precedents`/`search` already told \
             you exactly what to show.",
            schema_object(json!({
                "type": "object",
                "properties": {
                    "node": { "type": "integer", "description": "The graph node id to focus, as returned by `search`/`context`/`graph`." },
                    "workbook": { "type": "string", "description": "The workbook the node belongs to (hash, path, or file name)." },
                    "session_id": { "type": "string", "description": "Which chat session's viewers to notify — default \"default\"." },
                },
                "required": ["node"],
                "additionalProperties": false,
            })),
        ),
    ]
}

fn schema_object(value: Value) -> JsonObject {
    value.as_object().cloned().unwrap_or_default()
}

#[derive(Clone)]
pub struct McpBridge {
    app: Arc<App>,
}

impl McpBridge {
    fn new(app: Arc<App>) -> McpBridge {
        McpBridge { app }
    }

    async fn call_gui_tool(&self, name: &str, args: &Value) -> CallToolResult {
        match name {
            "chat" => {
                let Some(message) = args.get("message").and_then(Value::as_str) else {
                    return CallToolResult::error(vec![ContentBlock::text(
                        "`chat` needs a `message`",
                    )]);
                };
                let session_id = args
                    .get("session_id")
                    .and_then(Value::as_str)
                    .unwrap_or(chat::DEFAULT_SESSION);
                match chat::run_turn(&self.app, session_id, TurnSource::Agent, message).await {
                    Ok(turn) => CallToolResult::success(vec![ContentBlock::text(turn.answer)]),
                    Err(message) => CallToolResult::error(vec![ContentBlock::text(message)]),
                }
            }
            "gui_show" => {
                let Some(node) = args.get("node").and_then(Value::as_u64) else {
                    return CallToolResult::error(vec![ContentBlock::text(
                        "`gui_show` needs a `node`",
                    )]);
                };
                let session_id = args
                    .get("session_id")
                    .and_then(Value::as_str)
                    .unwrap_or(chat::DEFAULT_SESSION);
                let workbook = args
                    .get("workbook")
                    .and_then(Value::as_str)
                    .map(str::to_string);
                chat::navigate(&self.app, session_id, node as u32, workbook);
                CallToolResult::success(vec![ContentBlock::text("shown")])
            }
            other => CallToolResult::error(vec![ContentBlock::text(format!(
                "no tool called {other:?}"
            ))]),
        }
    }
}

impl ServerHandler for McpBridge {
    fn get_info(&self) -> InitializeResult {
        InitializeResult::new(ServerCapabilities::builder().enable_tools().build())
            .with_protocol_version(ProtocolVersion::LATEST)
            .with_instructions(
                "ExcelGRAG's GUI-attached MCP server. All the usual eg tools (workbooks, \
                 search, context, read_cells, precedents, dependents, find_value, recompute, \
                 tables, query_table, schema, what_if, graph) are available, plus `chat` and \
                 `gui_show`, which are visible in a browser tab open on this same corpus.",
            )
    }

    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, ErrorData> {
        let mut tools: Vec<Tool> = eg_mcp::tools::TOOLS
            .iter()
            .map(|tool| Tool::new(tool.name, tool.description, schema_object((tool.schema)())))
            .collect();
        tools.extend(gui_tools());
        Ok(ListToolsResult::with_all_items(tools))
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<rmcp::model::CallToolResponse, ErrorData> {
        let name = request.name.to_string();
        let args = Value::Object(request.arguments.clone().unwrap_or_default());

        if GUI_TOOL_NAMES.contains(&name.as_str()) {
            return Ok(self.call_gui_tool(&name, &args).await.into());
        }

        if eg_mcp::tools::TOOLS.iter().any(|tool| tool.name == name) {
            let app = Arc::clone(&self.app);
            let result = tokio::task::spawn_blocking(move || {
                let mut state = app.engine();
                eg_mcp::tools::call(&mut state, &name, &args)
            })
            .await
            .map_err(|e| ErrorData::internal_error(format!("tool call panicked: {e}"), None))?;
            let result = match result {
                Ok(text) => CallToolResult::success(vec![ContentBlock::text(text)]),
                Err(message) => CallToolResult::error(vec![ContentBlock::text(message)]),
            };
            return Ok(result.into());
        }

        Err(ErrorData::invalid_params(
            format!("no tool called {name:?}"),
            None,
        ))
    }
}

/// Start the MCP bridge on stdio, on its own task. If no MCP client is
/// attached (stdin is not a live pipe, e.g. a human ran `eg gui` from a
/// terminal with `--open` only), `rmcp`'s stdio transport simply sees EOF
/// and this task ends — the web server's lifecycle is untouched either way.
pub fn spawn(app: Arc<App>) {
    tokio::spawn(async move {
        let bridge = McpBridge::new(app);
        match bridge.serve(stdio()).await {
            Ok(service) => {
                if let Err(e) = service.waiting().await {
                    tracing::warn!("mcp bridge stopped: {e}");
                }
            }
            Err(e) => {
                tracing::warn!("no mcp client attached ({e}) — continuing GUI-only");
            }
        }
    });
}
