//! `eg-gui` — ExcelGRAG with a face, and a shared conversation.
//!
//! A local web server over an ExcelGRAG corpus: the engine's own crates,
//! in-process, behind a REST API and one WebSocket, serving the built
//! frontend from the same port. It is *also*, in the same process, an MCP
//! server on stdio — so an agent's tool calls and a human's browser tab
//! share one live engine instance and one chat session. Binds to
//! `127.0.0.1` only, always: a corpus is someone's spreadsheet, and the GUI
//! must not become the thing that puts it on a network.
//!
//! The `eg gui` verb in `eg-cli` is the intended entry point: point an MCP
//! client (e.g. Claude Code) at `eg gui <corpus> --port N --open`, and the
//! same process that becomes the agent's MCP server also opens the human's
//! browser tab. Calling `chat` (over MCP) or `POST /api/chat` (over REST)
//! appends to the same session, broadcast to every open tab.

pub mod api;
pub mod app;
pub mod chat;
pub mod dto;
pub mod index_job;
pub mod llm;
pub mod mcp_bridge;
pub mod watch;

use std::sync::Arc;

use app::App;

/// What `eg gui` needs to start.
pub struct GuiOptions {
    pub dir: String,
    pub port: u16,
    pub open: bool,
    pub redact_values: bool,
    pub llm: Option<llm::LlmConfig>,
}

/// Start the GUI: engine, watcher, MCP bridge, and the web server, all
/// sharing one `App`. Returns once the web server stops (normally: never,
/// until the process is killed).
pub async fn run(opts: GuiOptions) -> Result<(), String> {
    // Idempotent: the dev binary and a test harness may both end up calling
    // `run` in the same process; a second `.init()` would panic, so this is
    // best-effort and silent on failure rather than the caller's problem.
    let _ = tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .try_init();

    if opts.redact_values {
        if let Some(llm) = &opts.llm {
            if llm.privacy.allows_values() {
                return Err(
                    "eg gui: --redact-values and --llm-privacy values contradict each other — \
                     a corpus told not to show cell values cannot also send them to an LLM"
                        .to_string(),
                );
            }
        }
    }
    if let Some(llm) = &opts.llm {
        if llm.privacy.allows_values() {
            eprintln!(
                "eg gui: --llm-privacy values is active — cited cell contents may be sent to {}",
                llm.base_url
            );
        }
    }

    let llm_client = opts.llm.map(llm::Client::new);
    let app = Arc::new(App::new(&opts.dir, opts.redact_values, llm_client)?);

    {
        let state = app.engine();
        if state.corpus.is_empty() {
            eprintln!(
                "eg gui: the corpus at {} is empty — index a workbook from the UI, \
                 or with `eg index {} <workbook>`",
                opts.dir, opts.dir
            );
        }
    }

    if let Err(e) = watch::spawn(Arc::clone(&app)) {
        tracing::warn!("not watching the corpus for changes: {e}");
    }

    mcp_bridge::spawn(Arc::clone(&app));

    let url = format!("http://127.0.0.1:{}", opts.port);
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", opts.port))
        .await
        .map_err(|e| format!("could not bind 127.0.0.1:{}: {e}", opts.port))?;
    println!("eg gui serving {} — {}", app.dir, url);
    if opts.open {
        let _ = webbrowser::open(&url);
    }

    axum::serve(listener, api::router(app))
        .await
        .map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn redact_values_and_llm_privacy_values_are_refused_together() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let result = run(GuiOptions {
            dir: dir.path().to_str().unwrap().to_string(),
            port: 0,
            open: false,
            redact_values: true,
            llm: Some(llm::LlmConfig {
                base_url: "http://127.0.0.1:1".to_string(),
                api_key: None,
                model: "test".to_string(),
                privacy: llm::Privacy::Values,
            }),
        })
        .await;
        assert!(
            result.is_err(),
            "values mode must be refused under --redact-values"
        );
    }
}
