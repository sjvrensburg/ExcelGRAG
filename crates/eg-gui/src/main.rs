//! `eg-gui` — a dev-only binary for iterating on the frontend without going
//! through `eg gui` (the production entry point, in `eg-cli`). Both call the
//! same [`eg_gui::run`].
//!
//! ```sh
//! eg-gui corpus/                 # serve the GUI on 127.0.0.1:8765
//! eg-gui corpus/ --port 9000     # elsewhere
//! eg-gui corpus/ --open          # and open the browser at it
//! ```

use clap::Parser;
use eg_gui::llm::{LlmConfig, Privacy};
use eg_gui::GuiOptions;

#[derive(Parser)]
#[command(
    name = "eg-gui",
    version,
    about = "Dev entry point for eg-gui — prefer `eg gui` for normal use."
)]
struct Cli {
    /// The corpus directory. Created if absent — index from the UI.
    dir: String,
    /// Port to serve on, on localhost only.
    #[arg(long, default_value_t = 8765)]
    port: u16,
    /// Show the kind of each value rather than the value, wherever the GUI
    /// would show a cell. Matches `eg serve --redact-values`.
    #[arg(long)]
    redact_values: bool,
    /// Open the browser at the served URL.
    #[arg(long)]
    open: bool,
    #[arg(long)]
    llm_base_url: Option<String>,
    #[arg(long)]
    llm_api_key_env: Option<String>,
    #[arg(long, default_value = "gpt-oss-120b")]
    llm_model: String,
    #[arg(long, value_enum, default_value_t = Privacy::Off)]
    llm_privacy: Privacy,
}

#[tokio::main]
async fn main() {
    // `eg_gui::run` initializes tracing itself, so both entry points (this
    // dev binary and `eg gui`) get it without double-initializing.
    let cli = Cli::parse();
    let llm = cli.llm_base_url.map(|base_url| LlmConfig {
        base_url,
        api_key: cli.llm_api_key_env.and_then(|var| std::env::var(var).ok()),
        model: cli.llm_model,
        privacy: cli.llm_privacy,
    });

    if let Err(message) = eg_gui::run(GuiOptions {
        dir: cli.dir,
        port: cli.port,
        open: cli.open,
        redact_values: cli.redact_values,
        llm,
    })
    .await
    {
        eprintln!("eg-gui: {message}");
        std::process::exit(1);
    }
}
