//! The chat's optional model.
//!
//! Any endpoint that speaks the OpenAI chat-completions wire format — a
//! local llama.cpp/ollama server or a hosted API — via the maintained
//! `async-openai` client, never a hand-rolled HTTP caller. Off by default:
//! `chat::run_turn` degrades to a deterministic, LLM-free answer (the
//! rendered passage itself) when no [`Client`] is configured, so chat is
//! fully usable with nothing ever leaving the machine.
//!
//! [`Privacy`] is the dial CLAUDE.md's "nothing about a workbook leaves the
//! machine" promise asks for: `Off` makes no network call at all, `Passage`
//! sends only the rendered passage and citations (which by `render()`'s own
//! invariant never carry a cell value), and `Values` additionally attaches
//! the cited ranges' actual contents — the one place in this codebase that
//! deliberately transmits workbook data, and only ever for the one request
//! that opted in.

use async_openai::config::OpenAIConfig;
use async_openai::types::chat::{
    ChatCompletionRequestAssistantMessageArgs, ChatCompletionRequestMessage,
    ChatCompletionRequestSystemMessageArgs, ChatCompletionRequestUserMessageArgs,
    CreateChatCompletionRequestArgs,
};
use async_openai::Client as OpenAiClient;
use serde::{Deserialize, Serialize};

/// How much of a turn's content may reach the configured LLM. `Off` is the
/// default everywhere this is constructed from CLI flags.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default, clap::ValueEnum, Serialize, Deserialize)]
#[value(rename_all = "lower")]
#[serde(rename_all = "lowercase")]
pub enum Privacy {
    #[default]
    Off,
    Passage,
    Values,
}

impl std::fmt::Display for Privacy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Privacy::Off => write!(f, "off"),
            Privacy::Passage => write!(f, "passage"),
            Privacy::Values => write!(f, "values"),
        }
    }
}

impl Privacy {
    pub fn allows_llm(self) -> bool {
        !matches!(self, Privacy::Off)
    }

    pub fn allows_values(self) -> bool {
        matches!(self, Privacy::Values)
    }
}

/// Everything the connection needs except the key itself — what the CLI
/// flags and the GUI's settings panel both produce, and what `GET
/// /api/llm` reports back. The key is named by the environment variable
/// that holds it, never carried: the CLI already refuses a raw key on the
/// command line (it would land in shell history and `ps`), and a browser
/// form is a worse place for one than either.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LlmSettings {
    pub base_url: String,
    pub model: String,
    pub privacy: Privacy,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_key_env: Option<String>,
}

impl LlmSettings {
    /// The two rules every route into a live client goes through, at
    /// startup and on `POST /api/llm` alike: a corpus told not to show cell
    /// values cannot also send them (`values` under `--redact-values`), and
    /// a tier that will make network calls needs somewhere to make them to.
    pub fn check(&self, redact_values: bool) -> Result<(), String> {
        if redact_values && self.privacy.allows_values() {
            return Err(
                "--redact-values and llm privacy `values` contradict each other — a corpus \
                 told not to show cell values cannot also send them to an LLM"
                    .to_string(),
            );
        }
        if self.privacy.allows_llm() && self.base_url.trim().is_empty() {
            return Err("an LLM privacy tier other than `off` needs a base URL".to_string());
        }
        Ok(())
    }

    /// Resolve into a connectable config, reading the key from the named
    /// environment variable now (not at every request) so a missing one is
    /// an error the person configuring it sees, rather than a 401 later.
    pub fn resolve(&self) -> Result<LlmConfig, String> {
        let api_key = match &self.api_key_env {
            None => None,
            Some(var) if var.trim().is_empty() => None,
            Some(var) => Some(std::env::var(var).map_err(|_| {
                format!("the environment variable {var} is not set in eg gui's environment")
            })?),
        };
        Ok(LlmConfig {
            base_url: self.base_url.trim().to_string(),
            api_key,
            model: self.model.clone(),
            privacy: self.privacy,
            api_key_env: self.api_key_env.clone(),
        })
    }
}

#[derive(Clone)]
pub struct LlmConfig {
    pub base_url: String,
    pub api_key: Option<String>,
    pub model: String,
    pub privacy: Privacy,
    /// Where `api_key` came from, for reporting; `None` when no key was
    /// named (a local server that wants none).
    pub api_key_env: Option<String>,
}

impl LlmConfig {
    pub fn settings(&self) -> LlmSettings {
        LlmSettings {
            base_url: self.base_url.clone(),
            model: self.model.clone(),
            privacy: self.privacy,
            api_key_env: self.api_key_env.clone(),
        }
    }

    /// Say where cell contents will go the moment `values` is live —
    /// CLAUDE.md's condition for this mode existing at all is that it is
    /// never silent. Printed to stderr, not `tracing`, so it shows under
    /// any filter.
    pub fn announce(&self) {
        if self.privacy.allows_values() {
            eprintln!(
                "eg gui: llm privacy `values` is active — cited cell contents may be sent to {}",
                self.base_url
            );
        }
    }
}

const CONDENSE_PROMPT: &str = "\
You resolve a follow-up question about a spreadsheet into a standalone search \
query, using the conversation so far. Reply with ONLY the standalone query, \
no preamble, no punctuation around it. If the message is already standalone, \
repeat it unchanged.";

const COMPOSE_PROMPT: &str = "\
You answer a question about a spreadsheet using ONLY the grounded passage \
provided below the question. Every claim must be traceable to a cited range \
in that passage. Never invent a number, cell, or sheet name that is not in \
it. If the passage does not answer the question, say so plainly rather than \
guessing.";

/// A rewrite is a *query*, or it is nothing. Live-testing with a local
/// 35B model found the condense step sometimes answering the question
/// instead of restating it, and that whole paragraph — headings, bullet
/// points, "per the grounded passage" — then became the search string.
/// The turn survived by luck (a long query still ranks by its few real
/// words), so the shape is checked: one line, and not several times the
/// length of the message it rewrites. Anything else falls back to the
/// message, exactly as an unreachable model does.
const REWRITE_MAX_CHARS: usize = 200;

fn accept_rewrite(message: &str, rewrite: &str) -> Option<String> {
    let rewrite = rewrite.trim().trim_matches('"').trim();
    if rewrite.is_empty()
        || rewrite.lines().count() > 1
        || rewrite.len() > REWRITE_MAX_CHARS
        || rewrite.len() > message.len().max(40) * 3
    {
        return None;
    }
    Some(rewrite.to_string())
}

/// The chat LLM, if one is configured — at startup from the flags, or
/// later from the GUI's settings panel (`App::set_llm`).
#[derive(Clone)]
pub struct Client {
    inner: OpenAiClient<OpenAIConfig>,
    model: String,
    pub privacy: Privacy,
    settings: LlmSettings,
    key_present: bool,
    /// Kept for the investigation harness, which builds its own client to
    /// the same endpoint (`investigate.rs`); never reported.
    api_key: Option<String>,
}

impl Client {
    pub fn new(config: LlmConfig) -> Client {
        let settings = config.settings();
        let mut cfg = OpenAIConfig::new().with_api_base(config.base_url);
        let key_present = config.api_key.is_some();
        if let Some(key) = &config.api_key {
            cfg = cfg.with_api_key(key.clone());
        }
        Client {
            inner: OpenAiClient::with_config(cfg),
            model: config.model,
            privacy: config.privacy,
            settings,
            key_present,
            api_key: config.api_key,
        }
    }

    /// The endpoint as a second client would need it: base URL, key, model.
    pub fn connection(&self) -> (&str, Option<&str>, &str) {
        (
            &self.settings.base_url,
            self.api_key.as_deref(),
            &self.model,
        )
    }

    pub fn settings(&self) -> &LlmSettings {
        &self.settings
    }

    /// Whether a key was found — the name of the variable is reported, the
    /// key never is.
    pub fn key_present(&self) -> bool {
        self.key_present
    }

    /// Resolve a follow-up ("what about that one") into a standalone search
    /// query. Falls back to the raw message on any failure — a bad rewrite
    /// must never block the turn.
    pub async fn condense(&self, history: &[(String, String)], message: &str) -> String {
        let mut messages = vec![system(CONDENSE_PROMPT)];
        for (question, answer) in history {
            messages.push(user(question));
            messages.push(assistant(answer));
        }
        messages.push(user(message));
        match self.complete(messages).await {
            Some(rewrite) => {
                accept_rewrite(message, &rewrite).unwrap_or_else(|| message.to_string())
            }
            None => message.to_string(),
        }
    }

    /// Compose a natural-language reply from the rendered passage. Falls
    /// back to the passage itself on any failure, so a turn always has an
    /// answer even if the model is unreachable.
    pub async fn compose(
        &self,
        question: &str,
        passage: &str,
        citations: &[String],
        values: Option<&str>,
    ) -> String {
        let mut prompt = format!(
            "Question: {question}\n\nGrounded passage:\n{passage}\n\nCitations: {}",
            citations.join(", ")
        );
        if let Some(values) = values {
            prompt.push_str(&format!(
                "\n\nCell values for the citations above (opted in this turn):\n{values}"
            ));
        }
        let messages = vec![system(COMPOSE_PROMPT), user(&prompt)];
        self.complete(messages)
            .await
            .unwrap_or_else(|| passage.to_string())
    }

    async fn complete(&self, messages: Vec<ChatCompletionRequestMessage>) -> Option<String> {
        let request = CreateChatCompletionRequestArgs::default()
            .model(&self.model)
            .messages(messages)
            .build()
            .ok()?;
        let response = self.inner.chat().create(request).await.ok()?;
        let text = response
            .choices
            .into_iter()
            .next()
            .and_then(|choice| choice.message.content)
            .unwrap_or_default();
        (!text.trim().is_empty()).then(|| text.trim().to_string())
    }
}

fn system(text: &str) -> ChatCompletionRequestMessage {
    ChatCompletionRequestMessage::System(
        ChatCompletionRequestSystemMessageArgs::default()
            .content(text)
            .build()
            .expect("a static system prompt builds"),
    )
}

fn user(text: &str) -> ChatCompletionRequestMessage {
    ChatCompletionRequestMessage::User(
        ChatCompletionRequestUserMessageArgs::default()
            .content(text)
            .build()
            .expect("plain text content builds"),
    )
}

fn assistant(text: &str) -> ChatCompletionRequestMessage {
    ChatCompletionRequestMessage::Assistant(
        ChatCompletionRequestAssistantMessageArgs::default()
            .content(text)
            .build()
            .expect("plain text content builds"),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_one_line_restatement_is_accepted() {
        assert_eq!(
            accept_rewrite(
                "and where does the tax come into it?",
                "How does Tax_Rate factor into the bad debt provision?"
            )
            .as_deref(),
            Some("How does Tax_Rate factor into the bad debt provision?")
        );
        // Quoted, as some models do, is fine.
        assert_eq!(
            accept_rewrite("tax rate", "\"tax rate\"").as_deref(),
            Some("tax rate")
        );
    }

    #[test]
    fn an_answer_in_place_of_a_rewrite_is_dropped() {
        let answer = "The monthly figures are held in the **Feb**, **Mar**, and **Jan** \
                      sheets:\n- **Feb** sheet: `Feb!D1:D2001` [6]\n- **Mar** sheet";
        assert!(accept_rewrite("which sheet holds the monthly figures?", answer).is_none());
        // Single line but far too long to be a query.
        let long = "x".repeat(REWRITE_MAX_CHARS + 1);
        assert!(accept_rewrite("short", &long).is_none());
        assert!(accept_rewrite("short", "   ").is_none());
    }
}
