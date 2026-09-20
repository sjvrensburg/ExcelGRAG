//! The model, as an OpenAI-chat-completions-compatible endpoint.
//!
//! One constructor, so a host (`eg-gui`) can build the model from the
//! settings it already holds without depending on Rig itself: a local
//! `llama-server`, Ollama, and every hosted API speak this wire format,
//! which is the same choice `eg gui`'s own chat client made.

use rig_core::client::CompletionClient;
use rig_core::providers::openai::{self, CompletionsClient};

/// The concrete model type [`openai_compatible`] returns.
pub type OpenAiCompatible = openai::completion::CompletionModel<reqwest::Client>;

/// A model behind `base_url` (the `/v1` root), named `model`, with `api_key`
/// as a bearer token when the endpoint wants one. A local server takes any
/// string; the value is never a secret there.
pub fn openai_compatible(
    base_url: &str,
    api_key: Option<&str>,
    model: &str,
) -> Result<OpenAiCompatible, String> {
    let client = CompletionsClient::builder()
        .api_key(api_key.unwrap_or("none"))
        .base_url(base_url)
        .build()
        .map_err(|e| format!("could not build the model client: {e}"))?;
    Ok(client.completion_model(model))
}
