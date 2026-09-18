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
use crate::dto::{ChatTurnDto, DirectedDto, TurnSourceDto, WsEvent};

pub const DEFAULT_SESSION: &str = "default";

/// A selection made on the canvas, carried alongside a chat message. When
/// present it settles which entity the turn is about outright: no text
/// search, no session-scope fallback, no LLM condensation of a follow-up
/// against history. That is the point of it — a label like "Total" recurs
/// across sheets and workbooks, and a stale session can be scoped to a
/// workbook the user has since closed; an explicit node id cannot drift.
#[derive(Clone)]
pub struct EntityContext {
    pub workbook: String,
    pub node: u32,
}

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

/// Who a turn is addressed to, when it is not the built-in find→expand→
/// render/LLM pipeline that should answer it. The only case today is a human
/// routing a message to whichever agent is attached over the MCP bridge
/// instead of the configured LLM — see [[gui-chat-agent-vs-llm-toggle]] in
/// project memory for why this is asynchronous rather than a synchronous
/// hand-off: MCP has no client-implemented reverse channel
/// (`sampling/createMessage`) an agent's Claude Code client can answer today.
#[derive(Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Directed {
    Agent,
}

impl From<Directed> for DirectedDto {
    fn from(directed: Directed) -> Self {
        match directed {
            Directed::Agent => DirectedDto::Agent,
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
///
/// A turn directed at the agent (`directed_to = Some(Agent)`) skips all of
/// that: it is appended with an empty `answer` and no citations, and stays
/// that way — turns are never mutated in place — until the attached agent
/// posts a separate turn with `reply_to` set to this one's `id`. Nothing
/// pushes that reply; the agent notices the open question because `chat`
/// hands back the session's unanswered ones every time it is called, the
/// same way a human's own turns already show up live in the browser.
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub directed_to: Option<Directed>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reply_to: Option<u64>,
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
            directed_to: self.directed_to.map(Into::into),
            reply_to: self.reply_to,
        }
    }

    /// A turn is waiting on the agent when it was addressed to one and no
    /// later turn in the session has replied to it yet.
    fn awaiting_agent(&self) -> bool {
        matches!(self.directed_to, Some(Directed::Agent))
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
    context: Option<EntityContext>,
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

    // 2. Condense (no lock held) — only with an LLM configured, history to
    // resolve a follow-up against, and no explicit selection: a structured
    // context already says exactly what the turn is about, so condensing
    // free text against it would be answering a question nobody asked.
    let resolved_query = match &app.llm {
        Some(llm) if context.is_none() && llm.privacy.allows_llm() && !history.is_empty() => {
            Some(llm.condense(&history, message).await)
        }
        _ => None,
    };
    let query = resolved_query
        .clone()
        .unwrap_or_else(|| message.to_string());

    // 3. Engine step: exactly `/api/ask`'s find -> expand -> render when
    // there is no explicit selection; a direct expand from the selected node
    // when there is one, so an explicit selection always outranks session
    // scope and query condensation. Runs on the blocking pool; the engine
    // lock lives and dies inside it.
    let app_for_engine = Arc::clone(app);
    let workbook_for_engine = workbook.clone();
    let sheet_for_engine = sheet.clone();
    let context_for_engine = context.clone();
    let engine_result = tokio::task::spawn_blocking(move || match context_for_engine {
        Some(ctx) => api::ask_engine_for_node(
            &app_for_engine,
            &ctx.workbook,
            ctx.node,
            &eg_retrieve::ExpandOptions::default(),
            &eg_retrieve::RenderOptions::default(),
        ),
        None => {
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
        }
    })
    .await
    .map_err(|e| format!("the chat turn panicked: {e}"))??;
    let resolved_query = if context.is_some() {
        Some(engine_result.evidence.clone())
    } else {
        resolved_query
    };

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
    let turn = ChatTurn {
        id: 0,
        source,
        message: message.to_string(),
        resolved_query,
        evidence: engine_result.evidence,
        citations: engine_result.citations,
        answer,
        timestamp: now(),
        directed_to: None,
        reply_to: None,
    };
    commit_turn(
        app,
        session_id,
        turn,
        Some((engine_result.workbook, engine_result.sheet)),
    )
}

/// Append a turn to a session, persist it, and broadcast it — the tail every
/// kind of turn shares (an engine-answered one, a human's directed-at-agent
/// one, and an agent's reply to it).
///
/// The fallback for a hit that carried no workbook/sheet of its own is the
/// session's *current* sticky scope, read fresh under this same lock
/// acquisition — not a snapshot taken earlier in the caller. Two turns can
/// race on one session between an earlier read and here (another tab, or an
/// agent and a human at once); falling back to an earlier snapshot would let
/// a slower turn silently revert a faster turn's already-committed scope
/// update. `sticky` is `None` for turns that never touch scope (a
/// directed-at-agent turn and an agent's reply to one neither ran the engine
/// nor should move where a plain follow-up lands).
fn commit_turn(
    app: &Arc<App>,
    session_id: &str,
    mut turn: ChatTurn,
    sticky: Option<(Option<String>, Option<String>)>,
) -> Result<ChatTurnDto, String> {
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
        if let Some((workbook, sheet)) = sticky {
            session.workbook = workbook.or_else(|| session.workbook.clone());
            session.sheet = sheet.or_else(|| session.sheet.clone());
        }
        session.clone()
    };
    save_session(&app.dir, &session_snapshot)?;

    let dto = turn.to_dto();
    app.send(WsEvent::ChatTurn {
        session_id: session_id.to_string(),
        turn: dto.clone(),
    });
    Ok(dto)
}

/// Route a human's message to the attached agent instead of the built-in
/// pipeline: append it with an empty answer and `directed_to = Agent`, and
/// stop — no search, no LLM. It stays unanswered in the persisted session
/// until an agent posts a reply (see [`reply_to_agent_turn`]); nothing pages
/// the agent, so a corpus with no MCP client attached simply shows the
/// question as permanently pending, which is visible in the browser rather
/// than silently dropped.
pub fn direct_to_agent(
    app: &Arc<App>,
    session_id: &str,
    message: &str,
) -> Result<ChatTurnDto, String> {
    let turn = ChatTurn {
        id: 0,
        source: TurnSource::Human,
        message: message.to_string(),
        resolved_query: None,
        evidence: String::new(),
        citations: Vec::new(),
        answer: String::new(),
        timestamp: now(),
        directed_to: Some(Directed::Agent),
        reply_to: None,
    };
    commit_turn(app, session_id, turn, None)
}

/// Turns in a session addressed to the agent that no later turn has replied
/// to yet — what `chat` hands back alongside its own answer so an attached
/// agent notices a pending question without a push channel to tell it.
pub fn pending_for_agent(app: &App, session_id: &str) -> Vec<ChatTurnDto> {
    let mut sessions = app.sessions();
    let session = sessions
        .entry(session_id.to_string())
        .or_insert_with(|| load_session(&app.dir, session_id));
    let replied_to: std::collections::HashSet<u64> =
        session.turns.iter().filter_map(|t| t.reply_to).collect();
    session
        .turns
        .iter()
        .filter(|t| t.awaiting_agent() && !replied_to.contains(&t.id))
        .map(ChatTurn::to_dto)
        .collect()
}

/// An agent answering a turn a human directed at it. Appended as its own
/// `Agent`-sourced turn — the log is append-only, so this never rewrites the
/// question it answers — carrying the original question's text forward so
/// the pair reads as one exchange, and `reply_to` so [`pending_for_agent`]
/// stops counting the question as open.
pub fn reply_to_agent_turn(
    app: &Arc<App>,
    session_id: &str,
    reply_to: u64,
    answer: &str,
) -> Result<ChatTurnDto, String> {
    let question = {
        let mut sessions = app.sessions();
        let session = sessions
            .entry(session_id.to_string())
            .or_insert_with(|| load_session(&app.dir, session_id));
        session
            .turns
            .iter()
            .find(|t| t.id == reply_to)
            .map(|t| t.message.clone())
    };
    let Some(question) = question else {
        return Err(format!(
            "no turn {reply_to} in session {session_id:?} to reply to"
        ));
    };
    let turn = ChatTurn {
        id: 0,
        source: TurnSource::Agent,
        message: question,
        resolved_query: None,
        evidence: String::new(),
        citations: Vec::new(),
        answer: answer.to_string(),
        timestamp: now(),
        directed_to: None,
        reply_to: Some(reply_to),
    };
    commit_turn(app, session_id, turn, None)
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
            None,
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
            None,
        )
        .await
        .expect("the first turn runs");
        assert!(first.citations[0].contains('!'), "a citation names a sheet");
        let first_sheet = first.citations[0].split('!').next().unwrap().to_string();

        // A vague follow-up, with no sheet name of its own — it should still
        // land on the same sheet, via the sticky scope carried from turn 1,
        // not because the words happen to match it too.
        let second = run_turn(
            &app,
            DEFAULT_SESSION,
            TurnSource::Human,
            "what about it",
            None,
        )
        .await
        .expect("the second turn runs");
        if let Some(citation) = second.citations.first() {
            assert!(
                citation.starts_with(&first_sheet),
                "follow-up citation {citation:?} should stay on {first_sheet:?} via sticky scope"
            );
        }
    }

    #[tokio::test]
    async fn an_explicit_selection_overrides_the_free_text_query() {
        let (app, _dir) = indexed_app().await;
        let (hash, node_id) = {
            let state = app.engine();
            let (hash, _) = state.resolve(None).expect("one workbook in the corpus");
            let stored = state
                .corpus
                .get(&hash)
                .expect("the stored graph reads back")
                .expect("the workbook is in the corpus");
            (hash, stored.root)
        };
        // The message text names nothing in the workbook; only the explicit
        // selection can ground an answer.
        let turn = run_turn(
            &app,
            DEFAULT_SESSION,
            TurnSource::Human,
            "what is this?",
            Some(EntityContext {
                workbook: hash,
                node: node_id,
            }),
        )
        .await
        .expect("the turn runs");
        assert!(
            turn.resolved_query
                .as_deref()
                .is_some_and(|q| q.starts_with("selected:")),
            "an explicit selection should be recorded as what the turn actually answered, got {:?}",
            turn.resolved_query
        );
    }

    fn bare_app() -> (Arc<App>, tempfile::TempDir) {
        let dir = tempfile::tempdir().expect("a temp dir");
        let app = Arc::new(
            App::new(dir.path().to_str().expect("utf-8 path"), false, None).expect("engine opens"),
        );
        (app, dir)
    }

    #[test]
    fn a_turn_directed_at_the_agent_skips_the_engine_and_stays_open() {
        let (app, _dir) = bare_app();
        let turn = direct_to_agent(&app, DEFAULT_SESSION, "what should we do about this?")
            .expect("directing a turn does not need the engine");
        assert_eq!(turn.answer, "", "a directed turn has no answer yet");
        assert!(turn.citations.is_empty());

        let pending = pending_for_agent(&app, DEFAULT_SESSION);
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].id, turn.id);
    }

    #[test]
    fn a_reply_clears_the_pending_question_without_mutating_it() {
        let (app, _dir) = bare_app();
        let question = direct_to_agent(&app, DEFAULT_SESSION, "which sheet has it?")
            .expect("directing a turn does not need the engine");

        let reply = reply_to_agent_turn(&app, DEFAULT_SESSION, question.id, "It's on RATES.")
            .expect("replying to an open question succeeds");
        assert_eq!(reply.reply_to, Some(question.id));
        assert_eq!(reply.answer, "It's on RATES.");
        assert_eq!(
            reply.message, question.message,
            "the reply carries the question forward"
        );

        assert!(
            pending_for_agent(&app, DEFAULT_SESSION).is_empty(),
            "a replied-to question is no longer pending"
        );

        // The original turn itself is untouched — the log is append-only.
        let history = history(&app, DEFAULT_SESSION);
        let original = history
            .iter()
            .find(|t| t.id == question.id)
            .expect("the original turn is still there");
        assert_eq!(
            original.answer, "",
            "the question's own record never gets an answer written into it"
        );
    }

    #[test]
    fn replying_to_an_unknown_turn_is_refused() {
        let (app, _dir) = bare_app();
        let result = reply_to_agent_turn(&app, DEFAULT_SESSION, 999, "answer");
        let Err(err) = result else {
            panic!("no turn 999 exists");
        };
        assert!(err.contains("999"));
    }
}
