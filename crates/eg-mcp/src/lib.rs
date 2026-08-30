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
        let response = match serde_json::from_str::<rpc::Request>(&line) {
            Ok(request) => server.handle(request),
            Err(e) => Some(rpc::Response::anonymous_err(
                rpc::code::PARSE_ERROR,
                format!("could not read that line as a JSON-RPC request: {e}"),
            )),
        };
        if let Some(response) = response {
            serde_json::to_writer(&mut output, &response)?;
            output.write_all(b"\n")?;
            output.flush()?;
        }
    }
    Ok(())
}
