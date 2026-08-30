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

use eg_mcp::{serve, Server, State};

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

    let mut server = Server::new(state);
    let stdin = std::io::stdin();
    if let Err(e) = serve(&mut server, stdin.lock(), std::io::stdout()) {
        eprintln!("eg-mcp: {e}");
        std::process::exit(1);
    }
}
