//! An MCP server over ExcelGRAG: a spreadsheet as something an agent can ask.
//!
//! Usage: `eg-mcp <corpus-dir> [--redact-values]`
//!
//! The corpus is a directory built by `eg index` (see the `eg-cli` crate); it
//! holds the graphs and the search indexes. Workbooks themselves are opened
//! lazily, from the paths the corpus recorded, and only when a tool needs
//! cells.
//!
//! ```sh
//! cargo run --release -p eg-cli -- index corpus/ book.xlsb
//! cargo run --release -p eg-mcp -- corpus/
//! ```
//!
//! (`eg serve corpus/` runs the same server as a subcommand of `eg`, without a
//! second binary.)
//!
//! Then, from a client:
//!
//! ```sh
//! claude mcp add excelgrag -- /path/to/eg-mcp /path/to/corpus
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

    // A read-only entry point: refuse a typo'd or never-indexed directory
    // rather than materializing an empty corpus and serving NOTHING MATCHED
    // for every question. Only `eg index` should ever create one.
    if !std::path::Path::new(&dir).join("manifest.json").exists() {
        eprintln!(
            "eg-mcp: no corpus at {dir} (no manifest.json) — run `eg index {dir} <workbook>` first"
        );
        std::process::exit(1);
    }

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
