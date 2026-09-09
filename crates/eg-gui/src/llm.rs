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

/// How much of a turn's content may reach the configured LLM. `Off` is the
/// default everywhere this is constructed from CLI flags.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default, clap::ValueEnum)]
#[value(rename_all = "lower")]
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

#[derive(Clone)]
pub struct LlmConfig {
    pub base_url: String,
    pub api_key: Option<String>,
    pub model: String,
    pub privacy: Privacy,
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

/// The chat LLM, if one was configured at startup.
#[derive(Clone)]
pub struct Client {
    inner: OpenAiClient<OpenAIConfig>,
    model: String,
    pub privacy: Privacy,
}

impl Client {
    pub fn new(config: LlmConfig) -> Client {
        let mut cfg = OpenAIConfig::new().with_api_base(config.base_url);
        if let Some(key) = config.api_key {
            cfg = cfg.with_api_key(key);
        }
        Client {
            inner: OpenAiClient::with_config(cfg),
            model: config.model,
            privacy: config.privacy,
        }
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
        self.complete(messages)
            .await
            .unwrap_or_else(|| message.to_string())
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
