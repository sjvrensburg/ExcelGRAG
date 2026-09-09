//! The conversation: one shared, persisted session per corpus that both a
//! human's browser chat and an agent's `chat` MCP tool call append to. This
//! is the mechanism behind "tell the agent to show me something in the
//! GUI" — an agent's tool call and a browser's chat box run the exact same
//! pipeline and land in the exact same broadcast log.
//!
//! **Lock-ordering rule, load-bearing rather than decorative:**
//! `App::engine()` and `App::sessions()` are two independent
//! `std::sync::Mutex`es. [`run_turn`] never holds both at once — each is
//! locked, used, and released before the next step, and in particular
//! before either `.await` on the optional LLM (`condense`/`compose`), which
//! can take seconds and must never serialize every other browser tab or MCP
//! call behind it.

use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::api::{self, SearchParams};
use crate::app::App;
use crate::dto::{ChatTurnDto, TurnSourceDto, WsEvent};

pub const DEFAULT_SESSION: &str = "default";

/// How many prior turns' worth of history to hand the LLM when condensing a
/// follow-up. Bounded so a long-running session's context request doesn't
/// grow without limit.
const HISTORY_TURNS: usize = 6;

#[derive(Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum TurnSource {
    Human,
    Agent,
}

impl From<TurnSource> for TurnSourceDto {
    fn from(source: TurnSource) -> Self {
        match source {
            TurnSource::Human => TurnSourceDto::Human,
            TurnSource::Agent => TurnSourceDto::Agent,
        }
    }
}

/// One turn, as persisted. Deliberately the same shape regardless of which
/// LLM privacy tier produced it: `answer` is either the LLM's composed reply
/// or, with no LLM configured, the rendered passage itself. Even in
/// `--llm-privacy values` mode, the cell values sent to the model for that
/// one request are never written here — only the passage/citations/reply
/// are, so a corpus's persisted chat history carries no more than `render()`
/// ever puts in a passage.
#[derive(Clone, Serialize, Deserialize)]
pub struct ChatTurn {
    pub id: u64,
    pub source: TurnSource,
    pub message: String,
    pub resolved_query: Option<String>,
    pub evidence: String,
    pub citations: Vec<String>,
    pub answer: String,
    pub timestamp: u64,
}

impl ChatTurn {
    fn to_dto(&self) -> ChatTurnDto {
        ChatTurnDto {
            id: self.id,
            source: self.source.into(),
            message: self.message.clone(),
            resolved_query: self.resolved_query.clone(),
            evidence: self.evidence.clone(),
            citations: self.citations.clone(),
            answer: self.answer.clone(),
            timestamp: self.timestamp,
        }
    }
}

#[derive(Clone, Serialize, Deserialize, Default)]
pub struct ChatSession {
    pub id: String,
    /// Sticky scope from the last turn: a follow-up defaults to the same
    /// workbook/sheet unless the resolved query implies otherwise. This is
    /// the "multi-turn memory" — no LLM required for it to work.
    pub workbook: Option<String>,
    pub sheet: Option<String>,
    pub turns: Vec<ChatTurn>,
}

fn session_path(dir: &str, session_id: &str) -> std::path::PathBuf {
    std::path::Path::new(dir)
        .join("chat")
        .join(format!("{session_id}.json"))
}

fn load_session(dir: &str, session_id: &str) -> ChatSession {
    let path = session_path(dir, session_id);
    std::fs::read_to_string(&path)
        .ok()
        .and_then(|text| serde_json::from_str(&text).ok())
        .unwrap_or_else(|| ChatSession {
            id: session_id.to_string(),
            ..Default::default()
        })
}

fn save_session(dir: &str, session: &ChatSession) -> Result<(), String> {
    let path = session_path(dir, &session.id);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    let text = serde_json::to_string_pretty(session).map_err(|e| e.to_string())?;
    std::fs::write(&path, text).map_err(|e| e.to_string())
}

/// The persisted history for a session, hydrating from disk on first use.
/// Synchronous: callers already run this inside `spawn_blocking`.
pub fn history(app: &App, session_id: &str) -> Vec<ChatTurnDto> {
    let mut sessions = app.sessions();
    let session = sessions
        .entry(session_id.to_string())
        .or_insert_with(|| load_session(&app.dir, session_id));
    session.turns.iter().map(ChatTurn::to_dto).collect()
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Run one turn of the shared conversation. Used identically by
/// `POST /api/chat` (source = `Human`) and the MCP bridge's `chat` tool
/// (source = `Agent`) — the one thing that makes the log actually shared.
pub async fn run_turn(
    app: &Arc<App>,
    session_id: &str,
    source: TurnSource,
    message: &str,
) -> Result<ChatTurnDto, String> {
    // 1. Resolve session (sessions lock, released before engine/LLM work).
    let (workbook, sheet, history) = {
        let mut sessions = app.sessions();
        let session = sessions
            .entry(session_id.to_string())
            .or_insert_with(|| load_session(&app.dir, session_id));
        let history: Vec<(String, String)> = session
            .turns
            .iter()
            .rev()
            .take(HISTORY_TURNS)
            .map(|t| (t.message.clone(), t.answer.clone()))
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect();
        (session.workbook.clone(), session.sheet.clone(), history)
    };

    // 2. Condense (no lock held) — only with an LLM configured and history
    // to resolve a follow-up against.
    let resolved_query = match &app.llm {
        Some(llm) if llm.privacy.allows_llm() && !history.is_empty() => {
            Some(llm.condense(&history, message).await)
        }
        _ => None,
    };
    let query = resolved_query
        .clone()
        .unwrap_or_else(|| message.to_string());

    // 3. Engine step: find -> expand -> render, exactly as `/api/ask`. Runs
    // on the blocking pool; the engine lock lives and dies inside it.
    let app_for_engine = Arc::clone(app);
    let workbook_for_engine = workbook.clone();
    let sheet_for_engine = sheet.clone();
    let engine_result = tokio::task::spawn_blocking(move || {
        let params = SearchParams {
            q: query,
            workbook: workbook_for_engine,
            sheet: sheet_for_engine,
            limit: None,
            lexical_only: None,
        };
        api::ask_engine(
            &app_for_engine,
            &params,
            &eg_retrieve::ExpandOptions::default(),
            &eg_retrieve::RenderOptions::default(),
        )
    })
    .await
    .map_err(|e| format!("the chat turn panicked: {e}"))??;

    // 4. Compose (no lock held). `Values` mode reads cited cells through the
    // existing, tested `read_cells` MCP tool rather than re-deriving A1
    // parsing here — one more blocking engine call, locked and released on
    // its own.
    let answer = match &app.llm {
        Some(llm) if llm.privacy.allows_llm() => {
            let values = if llm.privacy.allows_values() {
                let app_for_values = Arc::clone(app);
                let citations = engine_result.citations.clone();
                let workbook = engine_result.workbook.clone();
                tokio::task::spawn_blocking(move || {
                    read_citation_values(&app_for_values, workbook.as_deref(), &citations)
                })
                .await
                .unwrap_or_default()
            } else {
                None
            };
            llm.compose(
                message,
                &engine_result.passage,
                &engine_result.citations,
                values.as_deref(),
            )
            .await
        }
        _ => engine_result.passage.clone(),
    };

    // 5. Re-lock sessions, append, persist, update sticky scope.
    //
    // The fallback for a hit that carried no workbook/sheet of its own is
    // the session's *current* sticky scope, read fresh under this same lock
    // acquisition — not the `workbook`/`sheet` captured back in step 1. Two
    // turns can race on one session between step 1 and here (another tab,
    // or an agent and a human at once); falling back to a step-1 snapshot
    // would let a slower turn silently revert a faster turn's already
    // -committed scope update.
    let mut turn = ChatTurn {
        id: 0,
        source,
        message: message.to_string(),
        resolved_query,
        evidence: engine_result.evidence,
        citations: engine_result.citations,
        answer,
        timestamp: now(),
    };
    let session_snapshot = {
        let mut sessions = app.sessions();
        let session = sessions
            .entry(session_id.to_string())
            .or_insert_with(|| ChatSession {
                id: session_id.to_string(),
                ..Default::default()
            });
        turn.id = session.turns.last().map(|t| t.id + 1).unwrap_or(1);
        session.turns.push(turn.clone());
        session.workbook = engine_result
            .workbook
            .clone()
            .or_else(|| session.workbook.clone());
        session.sheet = engine_result
            .sheet
            .clone()
            .or_else(|| session.sheet.clone());
        session.clone()
    };
    save_session(&app.dir, &session_snapshot)?;

    // 6. Broadcast (no lock held).
    let dto = turn.to_dto();
    app.send(WsEvent::ChatTurn {
        session_id: session_id.to_string(),
        turn: dto.clone(),
    });
    Ok(dto)
}

/// Best-effort cell values for a turn's citations, for `--llm-privacy
/// values` only. Reuses the existing, tested `read_cells` MCP tool rather
/// than re-deriving A1/range parsing — a citation this can't resolve (an
/// ambiguous workbook, a malformed A1 string) is skipped rather than
/// failing the turn.
fn read_citation_values(app: &App, workbook: Option<&str>, citations: &[String]) -> Option<String> {
    let workbook = workbook?;
    let mut state = app.engine();
    let mut out = String::new();
    for citation in citations {
        let args = serde_json::json!({ "citation": citation, "workbook": workbook });
        if let Ok(text) = eg_mcp::tools::call(&mut state, "read_cells", &args) {
            out.push_str(&text);
            out.push('\n');
        }
    }
    (!out.trim().is_empty()).then_some(out)
}

/// Broadcast a pure "look here" without running a full chat turn — for an
/// agent that already knows exactly what to point at (e.g. after
/// `read_cells`/`precedents`) and just wants to focus the human's view.
pub fn navigate(app: &App, session_id: &str, node: u32, workbook: Option<String>) {
    app.send(WsEvent::Navigate {
        session_id: session_id.to_string(),
        node,
        workbook,
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::index_job::{self, IndexBody};

    fn demo_fixture() -> std::path::PathBuf {
        std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../tests/fixtures/demo/impairment.xlsx")
    }

    /// A corpus with the demo workbook indexed (lexical only — no model
    /// download in a test), ready for `run_turn` to search against.
    async fn indexed_app() -> (Arc<App>, tempfile::TempDir) {
        let dir = tempfile::tempdir().expect("a temp dir");
        let app = Arc::new(
            App::new(dir.path().to_str().expect("utf-8 path"), false, None).expect("engine opens"),
        );
        let body = IndexBody {
            path: demo_fixture().to_str().expect("utf-8 path").to_string(),
            lexical_only: true,
            profiles: true,
        };
        let app_for_index = Arc::clone(&app);
        tokio::task::spawn_blocking(move || index_job::run(&app_for_index, &body))
            .await
            .expect("indexing task did not panic")
            .expect("indexing the demo fixture succeeds");
        (app, dir)
    }

    #[tokio::test]
    async fn a_turn_returns_citations_and_persists() {
        let (app, dir) = indexed_app().await;
        let turn = run_turn(
            &app,
            DEFAULT_SESSION,
            TurnSource::Human,
            "bad debt provision",
        )
        .await
        .expect("the turn runs");
        assert!(
            !turn.citations.is_empty(),
            "an answerable question should cite something"
        );
        assert_eq!(turn.id, 1);

        // Persisted to disk, and readable back by a fresh App over the same dir.
        let persisted = load_session(dir.path().to_str().unwrap(), DEFAULT_SESSION);
        assert_eq!(persisted.turns.len(), 1);
        assert_eq!(persisted.turns[0].message, "bad debt provision");
    }

    #[tokio::test]
    async fn a_follow_up_inherits_sticky_scope() {
        let (app, _dir) = indexed_app().await;
        let first = run_turn(
            &app,
            DEFAULT_SESSION,
            TurnSource::Human,
            "bad debt provision",
        )
        .await
        .expect("the first turn runs");
        assert!(first.citations[0].contains('!'), "a citation names a sheet");
        let first_sheet = first.citations[0].split('!').next().unwrap().to_string();

        // A vague follow-up, with no sheet name of its own — it should still
        // land on the same sheet, via the sticky scope carried from turn 1,
        // not because the words happen to match it too.
        let second = run_turn(&app, DEFAULT_SESSION, TurnSource::Human, "what about it")
            .await
            .expect("the second turn runs");
        if let Some(citation) = second.citations.first() {
            assert!(
                citation.starts_with(&first_sheet),
                "follow-up citation {citation:?} should stay on {first_sheet:?} via sticky scope"
            );
        }
    }
}
