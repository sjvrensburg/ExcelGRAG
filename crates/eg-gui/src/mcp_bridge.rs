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
use crate::dto::ChatTurnDto;

const GUI_TOOL_NAMES: &[&str] = &["chat", "gui_show"];

fn gui_tools() -> Vec<Tool> {
    vec![
        Tool::new(
            "chat",
            "Talk to this workbook's shared chat session — the same conversation a human sees \
             in the GUI's browser tab. Use this instead of `context`/`search` when you want \
             your question and its answer to show up live for whoever is watching the GUI, with \
             multi-turn memory (follow-ups carry forward the last workbook/sheet and citations). \
             Every call also returns any questions a human routed to you (the \"ask my agent\" \
             toggle in the browser) that no one has answered yet — there is no push channel, so \
             this is how you notice them. Answer one with a *second* call passing `reply_to` set \
             to its id and `message` set to your answer text; that skips the search pipeline \
             entirely and posts your words directly as the reply. Refused on a corpus started \
             with `--redact-values`: your reply text is posted as-is, with nothing here able to \
             tell whether it quotes a cell value, so that corpus's guarantee that no value leaves \
             the machine can't be kept for it.",
            schema_object(json!({
                "type": "object",
                "properties": {
                    "message": { "type": "string", "description": "Normally, what to ask, in words. With `reply_to` set, this is instead your own answer text to that turn — posted as-is, without running search." },
                    "session_id": { "type": "string", "description": "Which chat session — default \"default\", the one the GUI's browser tab shows unless told otherwise." },
                    "reply_to": { "type": "integer", "description": "The id of a turn a human routed to you (from an earlier call's `pending_for_you`), to answer instead of asking a new question. Refused if this corpus was started with --redact-values." },
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

/// A content block naming any questions a human routed to the agent that are
/// still open, prepended to a `chat` call's own answer so a call that only
/// meant to ask something of its own still surfaces them — the only
/// "notice" mechanism available without a push channel from the GUI to the
/// agent (see `chat::Directed`). A separate block rather than a text prefix:
/// a caller that treats the answer text as opaque display content can still
/// tell the two apart structurally, and an answer that happens to start the
/// same way as the marker can't be confused with it.
fn pending_note_block(pending: &[ChatTurnDto]) -> Option<ContentBlock> {
    if pending.is_empty() {
        return None;
    }
    let mut note = String::from(
        "pending_for_you: question(s) routed to you in this chat, unanswered — reply with \
         `chat`'s `reply_to` set to the id\n",
    );
    for turn in pending {
        note.push_str(&format!("  #{}: {}\n", turn.id, turn.message));
    }
    Some(ContentBlock::text(note))
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

                if let Some(reply_to) = args.get("reply_to").and_then(Value::as_u64) {
                    return match chat::reply_to_agent_turn(&self.app, session_id, reply_to, message)
                    {
                        Ok(turn) => CallToolResult::success(vec![ContentBlock::text(turn.answer)]),
                        Err(message) => CallToolResult::error(vec![ContentBlock::text(message)]),
                    };
                }

                match chat::run_turn(&self.app, session_id, TurnSource::Agent, message, None).await
                {
                    Ok(turn) => {
                        let pending = chat::pending_for_agent(&self.app, session_id);
                        let mut blocks = Vec::new();
                        blocks.extend(pending_note_block(&pending));
                        blocks.push(ContentBlock::text(turn.answer));
                        CallToolResult::success(blocks)
                    }
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
                // Resolve to the content hash here: the browser decides
                // whether it must reload by comparing this against the hash
                // of the workbook it has open, so a file name or path — both
                // valid per the schema — passed through verbatim made every
                // `gui_show` reload the graph (new layout, selection and
                // highlight lost) even when that workbook was already open.
                let workbook = match args.get("workbook").and_then(Value::as_str) {
                    None => None,
                    Some(wanted) => {
                        let app = Arc::clone(&self.app);
                        let wanted = wanted.to_string();
                        match tokio::task::spawn_blocking(move || {
                            crate::app::resolve_hash(&app, &wanted)
                        })
                        .await
                        {
                            Ok(Ok(hash)) => Some(hash),
                            Ok(Err(message)) => {
                                return CallToolResult::error(vec![ContentBlock::text(message)]);
                            }
                            Err(e) => {
                                return CallToolResult::error(vec![ContentBlock::text(format!(
                                    "resolving the workbook panicked: {e}"
                                ))]);
                            }
                        }
                    }
                };
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
