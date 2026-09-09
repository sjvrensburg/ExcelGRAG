//! `eg gui` — the only verb that needs a Tokio runtime, since `eg-gui`
//! serves over Axum and speaks MCP over `rmcp`. Everything else about this
//! function is a pure library-call wrapper, per CLAUDE.md's "the CLI
//! deliberately wraps library calls only": the actual startup logic
//! (including the `--redact-values`/`--llm-privacy values` refusal) lives in
//! `eg_gui::run`, not here.

pub fn gui(
    dir: &str,
    port: u16,
    open: bool,
    redact_values: bool,
    llm: Option<eg_gui::llm::LlmConfig>,
) -> Result<(), String> {
    let runtime = tokio::runtime::Runtime::new().map_err(|e| e.to_string())?;
    runtime.block_on(eg_gui::run(eg_gui::GuiOptions {
        dir: dir.to_string(),
        port,
        open,
        redact_values,
        llm,
    }))
}
