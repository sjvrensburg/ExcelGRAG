//! An MCP server over ExcelGRAG, as a library.
//!
//! The binary in this crate is a thin wrapper: argument parsing, then
//! [`serve`]. It is a library as well so that `eg-cli` can offer the same
//! server as a subcommand without a second copy of it, and so the protocol
//! layer can be tested without a process.

pub mod rpc;
pub mod server;
pub mod state;
pub mod tools;

use std::io::{BufRead, Write};

pub use server::Server;
pub use state::State;

/// Serve MCP over a reader and a writer: one JSON object per line, in and out,
/// until the input closes.
///
/// The caller supplies both ends so a test can drive the server over a pair of
/// buffers rather than a process. In the binary they are stdin and stdout —
/// and **stdout belongs to the protocol**, so every diagnostic goes to stderr.
pub fn serve(
    server: &mut Server,
    input: impl BufRead,
    mut output: impl Write,
) -> std::io::Result<()> {
    for line in input.lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        if let Some(response) = dispatch_line(server, &line) {
            serde_json::to_writer(&mut output, &response)?;
            output.write_all(b"\n")?;
            output.flush()?;
        }
    }
    Ok(())
}

/// Decode and dispatch one line, in the two stages JSON-RPC 2.0 requires: is
/// this JSON at all, then is it a request. Splitting them matters because a
/// line can be syntactically valid JSON with a perfectly good `id` and still
/// fail to deserialise into [`rpc::Request`] (a non-string `method`, say) —
/// deserialising straight into `Request` would answer that with -32700 and a
/// lost id, and every correlating client would hang waiting for a reply that
/// will never match.
fn dispatch_line(server: &mut Server, line: &str) -> Option<rpc::Response> {
    let value: serde_json::Value = match serde_json::from_str(line) {
        Ok(v) => v,
        // Not JSON at all: there is no id to recover, so `null` is the only
        // answer the spec allows.
        Err(e) => {
            return Some(rpc::Response::anonymous_err(
                rpc::code::PARSE_ERROR,
                format!("could not read that line as JSON: {e}"),
            ))
        }
    };
    // The `id` field, read before `Request` deserialisation can fail on it.
    // Its *presence* (not its value — an explicit `"id":null` still counts)
    // is what marks the message as a notification, which a client sends
    // expecting no reply even when it turns out malformed.
    let id = value.get("id").cloned();
    if let Err(message) = validate_envelope(&value) {
        return Some(match id {
            Some(id) => rpc::Response::err(id, rpc::code::INVALID_REQUEST, message),
            None => rpc::Response::anonymous_err(rpc::code::INVALID_REQUEST, message),
        });
    }
    match serde_json::from_value::<rpc::Request>(value) {
        Ok(mut request) => {
            // `Option<Value>` normally collapses an explicit JSON null into
            // `None`. JSON-RPC distinguishes that request id from an absent
            // id (a notification), so restore presence after deserialisation.
            request.id = id;
            server.handle(request)
        }
        Err(e) => id.map(|id| {
            rpc::Response::err(
                id,
                rpc::code::INVALID_REQUEST,
                format!("not a valid JSON-RPC request: {e}"),
            )
        }),
    }
}

fn validate_envelope(value: &serde_json::Value) -> Result<(), String> {
    let object = value
        .as_object()
        .ok_or_else(|| "a JSON-RPC request must be an object".to_string())?;
    if object.get("jsonrpc").and_then(serde_json::Value::as_str) != Some("2.0") {
        return Err("jsonrpc must be exactly \"2.0\"".to_string());
    }
    if let Some(id) = object.get("id") {
        if !matches!(
            id,
            serde_json::Value::Null | serde_json::Value::String(_) | serde_json::Value::Number(_)
        ) {
            return Err("id must be a string, number, or null".to_string());
        }
    }
    if let Some(params) = object.get("params") {
        if !params.is_object() && !params.is_array() {
            return Err("params must be an object or array".to_string());
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;

    fn server() -> (Server, tempfile::TempDir) {
        let dir = tempfile::tempdir().expect("a temp dir");
        let state = State::open(dir.path().to_str().expect("utf-8 path"), false)
            .expect("an empty corpus opens");
        (Server::new(state), dir)
    }

    #[test]
    fn unparseable_json_is_parse_error_with_null_id() {
        let (mut server, _dir) = server();
        let response = dispatch_line(&mut server, "not json at all").expect("answered");
        assert_eq!(response.id, Value::Null);
        assert_eq!(
            response.error.expect("an error").code,
            rpc::code::PARSE_ERROR
        );
    }

    #[test]
    fn valid_json_that_is_not_a_valid_request_keeps_its_id() {
        // A syntactically fine object with an id, but a `method` that is not a
        // string — the exact shape a correlating client sends when something
        // upstream mis-serialised the request. It must get -32600 with the
        // original id back, not -32700 with the id silently dropped.
        let (mut server, _dir) = server();
        let response = dispatch_line(&mut server, r#"{"jsonrpc":"2.0","id":7,"method":123}"#)
            .expect("answered");
        assert_eq!(response.id, serde_json::json!(7));
        assert_eq!(
            response.error.expect("an error").code,
            rpc::code::INVALID_REQUEST
        );
    }

    #[test]
    fn a_malformed_notification_gets_no_response() {
        // No `id` at all marks this as a notification: the sender is not
        // waiting for a reply, so even a malformed one must be answered with
        // silence rather than an error the client never asked for.
        let (mut server, _dir) = server();
        let response = dispatch_line(&mut server, r#"{"jsonrpc":"2.0","method":123}"#);
        assert!(response.is_none());
    }

    #[test]
    fn a_well_formed_request_still_dispatches() {
        let (mut server, _dir) = server();
        let response = dispatch_line(&mut server, r#"{"jsonrpc":"2.0","id":1,"method":"ping"}"#)
            .expect("answered");
        assert_eq!(response.id, serde_json::json!(1));
        assert!(response.result.is_some());
    }

    #[test]
    fn an_explicit_null_id_is_answered_but_an_absent_id_is_not() {
        let (mut server, _dir) = server();
        let response = dispatch_line(
            &mut server,
            r#"{"jsonrpc":"2.0","id":null,"method":"ping"}"#,
        )
        .expect("null is an id, not an absent id");
        assert_eq!(response.id, Value::Null);
        assert!(dispatch_line(&mut server, r#"{"jsonrpc":"2.0","method":"ping"}"#).is_none());
    }

    #[test]
    fn invalid_json_rpc_envelopes_are_refused() {
        let (mut server, _dir) = server();
        for line in [
            r#"{"id":1,"method":"ping"}"#,
            r#"{"jsonrpc":"1.0","id":1,"method":"ping"}"#,
            r#"{"jsonrpc":"2.0","id":{},"method":"ping"}"#,
            r#"{"jsonrpc":"2.0","id":1,"method":"ping","params":3}"#,
        ] {
            let response = dispatch_line(&mut server, line).expect("a request is answered");
            assert_eq!(
                response.error.expect("invalid request").code,
                rpc::code::INVALID_REQUEST
            );
        }
    }
}
