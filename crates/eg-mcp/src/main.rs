//! An MCP server over ExcelGRAG: a spreadsheet as something an agent can ask.
//!
//! Usage: `eg-mcp <corpus-dir> [--redact-values]`
//!
//! The corpus is the directory built by `eg-graph`'s `corpus` example and
//! indexed by `eg-index`; it holds the graphs and the search indexes. Workbooks
//! themselves are opened lazily, from the paths the corpus recorded, and only
//! when a tool needs cells.
//!
//! ```sh
//! cargo run --release -p eg-graph --example corpus -- index private/book.xlsb
//! cargo run --release -p eg-index --example semantic -- index warm up the indexes
//! cargo run --release -p eg-mcp -- index
//! ```
//!
//! Then, from a client:
//!
//! ```sh
//! claude mcp add excelgrag -- /path/to/eg-mcp /path/to/index
//! ```
//!
//! **stdout belongs to the protocol.** Every diagnostic goes to stderr; one
//! stray `println!` would put a line the client cannot parse into the middle of
//! a JSON-RPC stream.

mod rpc;
mod server;
mod state;
mod tools;

use std::io::{BufRead, Write};

use rpc::{code, Request, Response};
use server::Server;
use state::State;

const USAGE: &str = "usage: eg-mcp <corpus-dir> [--redact-values]";

fn main() {
    let mut dir = None;
    let mut redact_values = false;
    for arg in std::env::args().skip(1) {
        match arg.as_str() {
            "--redact-values" => redact_values = true,
            "-h" | "--help" => {
                println!("{USAGE}");
                return;
            }
            other if other.starts_with('-') => {
                eprintln!("eg-mcp: unknown option {other}\n{USAGE}");
                std::process::exit(2);
            }
            other => {
                if dir.is_some() {
                    eprintln!("eg-mcp: unexpected argument {other}\n{USAGE}");
                    std::process::exit(2);
                }
                dir = Some(other.to_string());
            }
        }
    }
    let Some(dir) = dir else {
        eprintln!("eg-mcp: no corpus directory\n{USAGE}");
        std::process::exit(2);
    };

    let state = match State::open(&dir, redact_values) {
        Ok(state) => state,
        Err(message) => {
            eprintln!("eg-mcp: {message}");
            std::process::exit(1);
        }
    };
    eprintln!(
        "eg-mcp: serving {} workbook(s) from {dir}{}",
        state.corpus.len(),
        if redact_values {
            ", values redacted"
        } else {
            ""
        }
    );

    if let Err(e) = serve(Server::new(state)) {
        eprintln!("eg-mcp: {e}");
        std::process::exit(1);
    }
}

/// One JSON object per line, in and out, until stdin closes.
fn serve(mut server: Server) -> std::io::Result<()> {
    let stdin = std::io::stdin();
    let mut stdout = std::io::stdout();
    let mut line = String::new();

    loop {
        line.clear();
        if stdin.lock().read_line(&mut line)? == 0 {
            return Ok(()); // The client hung up.
        }
        if line.trim().is_empty() {
            continue;
        }

        let response = match serde_json::from_str::<Request>(&line) {
            Ok(request) => server.handle(request),
            Err(e) => Some(Response::anonymous_err(
                code::PARSE_ERROR,
                format!("could not read that line as a JSON-RPC request: {e}"),
            )),
        };

        if let Some(response) = response {
            serde_json::to_writer(&mut stdout, &response)?;
            stdout.write_all(b"\n")?;
            stdout.flush()?;
        }
    }
}
