//! The MCP methods, over the JSON-RPC layer.
//!
//! Four methods carry the whole protocol for a server that only offers tools:
//! `initialize` to agree on a version, `tools/list` to say what there is,
//! `tools/call` to run one, and `ping`. Notifications — `initialized` and the
//! cancellations — are accepted and answered with silence, which is what the
//! spec asks for.
//!
//! A failing *tool* is not a failing *call*: the result comes back with
//! `isError` set and the reason as text, so the model can read what went wrong
//! and try something else. JSON-RPC errors are reserved for the protocol
//! itself — an unknown method, unreadable parameters — which the model cannot
//! do anything about.

use serde_json::{json, Value};

use crate::rpc::{code, Request, Response};
use crate::state::State;
use crate::tools::{self, TOOLS};

pub use crate::tools::Tool;

/// The protocol version this server implements. A client asking for another
/// version is answered with this one, and the spec leaves it to the client to
/// decide whether it can live with that.
pub const PROTOCOL_VERSION: &str = "2025-06-18";

/// Every revision this server actually understands, oldest first.
///
/// A lexicographic comparison against [`PROTOCOL_VERSION`] is not the same
/// question as "does this server implement that revision": `"2025-03-26"`
/// sorts before `"2025-06-18"` and would pass such a check, but that revision
/// introduced batched JSON-RPC requests, which this line-at-a-time transport
/// rejects outright. Echoing a version back is a promise this server speaks
/// it, so only a revision in this explicit list is ever echoed; anything else
/// — older-but-unlisted, newer, or not a date at all — is answered with
/// [`PROTOCOL_VERSION`], per the spec's own fallback.
const SUPPORTED_VERSIONS: &[&str] = &["2024-11-05", PROTOCOL_VERSION];

pub struct Server {
    pub state: State,
}

impl Server {
    pub fn new(state: State) -> Self {
        Server { state }
    }

    /// Answer one request. `None` for a notification, which takes no reply.
    pub fn handle(&mut self, request: Request) -> Option<Response> {
        if request.is_notification() {
            return None;
        }
        let id = request.id.clone().unwrap_or(Value::Null);
        Some(match request.method.as_str() {
            "initialize" => Response::ok(id, self.initialize(&request.params)),
            "tools/list" => Response::ok(id, list_tools()),
            "tools/call" => match self.call_tool(&request.params) {
                Ok(result) => Response::ok(id, result),
                Err(message) => Response::err(id, code::INVALID_PARAMS, message),
            },
            "ping" => Response::ok(id, json!({})),
            other => Response::err(
                id,
                code::METHOD_NOT_FOUND,
                format!("this server implements initialize, tools/list, tools/call and ping, not {other:?}"),
            ),
        })
    }

    fn initialize(&self, params: &Value) -> Value {
        // Echo the client's version when it is one this server actually
        // speaks, so a client on an older revision is not turned away over a
        // field neither side uses; anything else falls back to the version
        // implemented here, which the spec leaves the client free to reject.
        let asked = params.get("protocolVersion").and_then(Value::as_str);
        let version = match asked {
            Some(asked) if SUPPORTED_VERSIONS.contains(&asked) => asked,
            _ => PROTOCOL_VERSION,
        };
        json!({
            "protocolVersion": version,
            "capabilities": { "tools": {} },
            "serverInfo": {
                "name": "excelgrag",
                "version": env!("CARGO_PKG_VERSION"),
            },
            "instructions": INSTRUCTIONS,
        })
    }

    fn call_tool(&mut self, params: &Value) -> Result<Value, String> {
        let name = params
            .get("name")
            .and_then(Value::as_str)
            .ok_or("tools/call needs a tool name")?;
        if !TOOLS.iter().any(|tool| tool.name == name) {
            return Err(format!("no tool called {name:?}"));
        }
        let arguments = params.get("arguments").cloned().unwrap_or(json!({}));
        let tool = TOOLS
            .iter()
            .find(|tool| tool.name == name)
            .expect("checked above");
        if let Err(message) = validate_schema(&arguments, &(tool.schema)(), "arguments") {
            return Ok(text_result(message, true));
        }
        Ok(match tools::call(&mut self.state, name, &arguments) {
            Ok(text) => text_result(text, false),
            // A tool that could not do what was asked reports back through the
            // result, not through a protocol error: the model is the one who
            // can act on it.
            Err(message) => text_result(message, true),
        })
    }
}

/// Validate the deliberately small JSON-Schema subset used by our tool list.
/// Keeping this beside dispatch guarantees the contract we advertise is also
/// enforced without pulling a full schema engine into the server.
fn validate_schema(value: &Value, schema: &Value, path: &str) -> Result<(), String> {
    if let Some(kind) = schema.get("type").and_then(Value::as_str) {
        let valid = match kind {
            "object" => value.is_object(),
            "array" => value.is_array(),
            "string" => value.is_string(),
            "integer" => value.as_u64().is_some(),
            "number" => value.is_number(),
            "boolean" => value.is_boolean(),
            _ => true,
        };
        if !valid {
            return Err(format!("{path} must be {kind}"));
        }
    }
    if let Some(allowed) = schema.get("enum").and_then(Value::as_array) {
        if !allowed.contains(value) {
            return Err(format!("{path} is not one of the allowed values"));
        }
    }
    if let Some(n) = value.as_u64() {
        if schema
            .get("minimum")
            .and_then(Value::as_u64)
            .is_some_and(|min| n < min)
        {
            return Err(format!("{path} is below the minimum"));
        }
        if schema
            .get("maximum")
            .and_then(Value::as_u64)
            .is_some_and(|max| n > max)
        {
            return Err(format!("{path} is above the maximum"));
        }
    }
    if let Some(object) = value.as_object() {
        let properties = schema.get("properties").and_then(Value::as_object);
        if schema.get("additionalProperties") == Some(&Value::Bool(false)) {
            for key in object.keys() {
                if !properties.is_some_and(|p| p.contains_key(key)) {
                    return Err(format!("{path} contains unknown property {key:?}"));
                }
            }
        }
        if let Some(required) = schema.get("required").and_then(Value::as_array) {
            for key in required.iter().filter_map(Value::as_str) {
                if !object.contains_key(key) {
                    return Err(format!("{path}.{key} is required"));
                }
            }
        }
        if let Some(properties) = properties {
            for (key, child) in object {
                if let Some(child_schema) = properties.get(key) {
                    validate_schema(child, child_schema, &format!("{path}.{key}"))?;
                }
            }
        }
    }
    if let (Some(items), Some(item_schema)) = (value.as_array(), schema.get("items")) {
        for (index, item) in items.iter().enumerate() {
            validate_schema(item, item_schema, &format!("{path}[{index}]"))?;
        }
    }
    Ok(())
}

fn text_result(text: String, is_error: bool) -> Value {
    json!({
        "content": [{ "type": "text", "text": text }],
        "isError": is_error,
    })
}

fn list_tools() -> Value {
    let tools: Vec<Value> = TOOLS
        .iter()
        .map(|tool| {
            json!({
                "name": tool.name,
                "description": tool.description,
                "inputSchema": (tool.schema)(),
            })
        })
        .collect();
    json!({ "tools": tools })
}

/// What a client is told about this server before it asks anything.
const INSTRUCTIONS: &str = "\
This is a spreadsheet, exposed as a graph you can search and then read down to \
individual cells.

Start with `search` or `context` — `context` answers a question with a cited \
passage, which is usually the right first call. Passages contain no cell \
values; they say where to look. When the answer needs a number, follow a \
citation with `read_cells`, and when it needs to be trusted, `recompute` says \
whether a formula still agrees with the value stored beside it.

Cite what you use. Every node in a passage is numbered and every cell has an \
address: an answer that names them can be checked, and one that does not \
cannot.";

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// A server over an empty corpus in a temporary directory. Both `open`
    /// calls create what they do not find, so this costs a `mkdir`.
    fn server() -> (Server, tempfile::TempDir) {
        let dir = tempfile::tempdir().expect("a temp dir");
        let state = State::open(dir.path().to_str().expect("utf-8 path"), false)
            .expect("an empty corpus opens");
        (Server::new(state), dir)
    }

    fn request(method: &str, params: Value) -> Request {
        serde_json::from_value(json!({ "id": 1, "method": method, "params": params }))
            .expect("a well-formed request")
    }

    fn call(server: &mut Server, tool: &str, args: Value) -> (String, bool) {
        let response = server
            .handle(request(
                "tools/call",
                json!({ "name": tool, "arguments": args }),
            ))
            .expect("a call is not a notification");
        let result = response.result.expect("a result, not an error");
        let text = result["content"][0]["text"]
            .as_str()
            .expect("text content")
            .to_string();
        (text, result["isError"].as_bool().expect("isError"))
    }

    #[test]
    fn initialize_agrees_on_a_version_and_offers_tools() {
        let (mut server, _dir) = server();
        let response = server
            .handle(request(
                "initialize",
                json!({ "protocolVersion": PROTOCOL_VERSION }),
            ))
            .expect("initialize is answered");
        let result = response.result.expect("a result");
        assert_eq!(result["protocolVersion"], PROTOCOL_VERSION);
        assert!(result["capabilities"]["tools"].is_object());
        assert_eq!(result["serverInfo"]["name"], "excelgrag");
        assert!(result["instructions"]
            .as_str()
            .is_some_and(|s| !s.is_empty()));
    }

    #[test]
    fn an_older_client_is_answered_in_its_own_version() {
        // Turning a client away over a field neither side uses would be a poor
        // trade; a newer one is answered with what this server actually speaks.
        let (mut server, _dir) = server();
        let older = server
            .handle(request(
                "initialize",
                json!({ "protocolVersion": "2024-11-05" }),
            ))
            .and_then(|r| r.result)
            .expect("a result");
        assert_eq!(older["protocolVersion"], "2024-11-05");

        let newer = server
            .handle(request(
                "initialize",
                json!({ "protocolVersion": "2099-01-01" }),
            ))
            .and_then(|r| r.result)
            .expect("a result");
        assert_eq!(newer["protocolVersion"], PROTOCOL_VERSION);
    }

    #[test]
    fn only_whitelisted_versions_are_echoed() {
        // "2025-03-26" sorts lexicographically before PROTOCOL_VERSION and
        // would pass a naive `<=` comparison, but it is a revision this
        // transport does not implement (it introduced batched requests) and
        // must not be echoed back as if it were supported.
        let (mut server, _dir) = server();
        let unsupported = server
            .handle(request(
                "initialize",
                json!({ "protocolVersion": "2025-03-26" }),
            ))
            .and_then(|r| r.result)
            .expect("a result");
        assert_eq!(unsupported["protocolVersion"], PROTOCOL_VERSION);

        let nonsense = server
            .handle(request("initialize", json!({ "protocolVersion": "1.0" })))
            .and_then(|r| r.result)
            .expect("a result");
        assert_eq!(nonsense["protocolVersion"], PROTOCOL_VERSION);
    }

    #[test]
    fn every_tool_declares_a_schema_that_matches_itself() {
        let listed = list_tools();
        let tools = listed["tools"].as_array().expect("an array of tools");
        assert_eq!(tools.len(), TOOLS.len());
        for tool in tools {
            let name = tool["name"].as_str().expect("a name");
            assert!(
                tool["description"].as_str().is_some_and(|d| d.len() > 40),
                "{name} needs a description a model can choose on"
            );
            let schema = &tool["inputSchema"];
            assert_eq!(schema["type"], "object", "{name}");
            assert_eq!(
                schema["additionalProperties"], false,
                "{name} should refuse arguments it does not know"
            );
            for required in schema["required"].as_array().unwrap_or(&Vec::new()) {
                let key = required.as_str().expect("a property name");
                assert!(
                    schema["properties"].get(key).is_some(),
                    "{name} requires {key}, which it does not declare"
                );
            }
        }
    }

    #[test]
    fn a_notification_is_answered_with_silence() {
        let (mut server, _dir) = server();
        let notification: Request =
            serde_json::from_value(json!({ "method": "notifications/initialized" }))
                .expect("a notification");
        assert!(server.handle(notification).is_none());
    }

    #[test]
    fn an_unknown_method_is_a_protocol_error() {
        let (mut server, _dir) = server();
        let response = server
            .handle(request("tools/run", json!({})))
            .expect("answered");
        assert_eq!(
            response.error.expect("an error").code,
            code::METHOD_NOT_FOUND
        );
    }

    #[test]
    fn an_unknown_tool_is_a_protocol_error_but_a_failing_tool_is_not() {
        // The distinction matters to the caller: it can retry a tool that
        // failed, and it cannot invent a tool this server does not have.
        let (mut server, _dir) = server();
        let unknown = server
            .handle(request("tools/call", json!({ "name": "evaluate" })))
            .expect("answered");
        assert_eq!(unknown.error.expect("an error").code, code::INVALID_PARAMS);

        let (text, is_error) = call(
            &mut server,
            "read_cells",
            json!({ "citation": "Sheet1!A1" }),
        );
        assert!(
            is_error,
            "an empty corpus cannot answer, and says so in the result"
        );
        assert!(text.contains("index a workbook"), "{text}");
    }

    #[test]
    fn a_tool_that_needs_no_workbook_answers_an_empty_corpus() {
        let (mut server, _dir) = server();
        let (text, is_error) = call(&mut server, "workbooks", json!({}));
        assert!(!is_error);
        assert!(text.contains("empty"), "{text}");
    }

    #[test]
    fn a_missing_argument_is_reported_to_the_caller() {
        let (mut server, _dir) = server();
        let (text, is_error) = call(&mut server, "search", json!({}));
        assert!(is_error);
        assert!(text.contains("query is required"), "{text}");
    }

    #[test]
    fn searching_an_empty_corpus_finds_nothing_rather_than_failing() {
        let (mut server, _dir) = server();
        let (text, is_error) = call(&mut server, "search", json!({ "query": "bad debt" }));
        assert!(!is_error, "{text}");
        assert!(text.contains("nothing matched"), "{text}");
    }

    #[test]
    fn tool_arguments_enforce_the_schema_the_server_advertises() {
        let (mut server, _dir) = server();
        let response = server
            .handle(request(
                "tools/call",
                json!({ "name": "workbooks", "arguments": { "surprise": true } }),
            ))
            .expect("answered");
        assert_eq!(response.result.expect("tool result")["isError"], true);

        let response = server
            .handle(request(
                "tools/call",
                json!({
                    "name": "query_table",
                    "arguments": {
                        "table": "Sheet1!A1:B2",
                        "aggregate": [{ "of": "count", "typo": true }]
                    }
                }),
            ))
            .expect("answered");
        assert_eq!(response.result.expect("tool result")["isError"], true);
    }

    #[test]
    fn zero_and_overlarge_limits_are_rejected_not_coerced() {
        let (mut server, _dir) = server();
        for limit in [0, 101] {
            let response = server
                .handle(request(
                    "tools/call",
                    json!({
                        "name": "search",
                        "arguments": { "query": "revenue", "limit": limit }
                    }),
                ))
                .expect("answered");
            assert_eq!(response.result.expect("tool result")["isError"], true);
        }
    }
}
