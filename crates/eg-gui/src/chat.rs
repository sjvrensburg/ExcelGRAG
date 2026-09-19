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
/// same way a human's own turns already show up live in the browser. Such a
/// reply's `answer` is the one exception to the no-cell-values guarantee
/// above — it is free text an agent chose to type, not something `render()`
/// or `--llm-privacy` constrained — which is why [`reply_to_agent_turn`]
/// refuses it outright under `--redact-values` rather than trusting it.
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
    /// A bare turn: an id `commit_turn` will overwrite, `now()` for the
    /// timestamp, and every other field at its empty/absent default. The
    /// three call sites that build a `ChatTurn` (an engine-answered one, a
    /// directed-at-agent one, and an agent's reply) each set only the
    /// handful of fields that differ, via struct-update syntax, rather than
    /// listing all ten fields by hand — which had let a turn kind silently
    /// omit a field a future one added.
    fn new(source: TurnSource, message: impl Into<String>) -> ChatTurn {
        ChatTurn {
            id: 0,
            source,
            message: message.into(),
            resolved_query: None,
            evidence: String::new(),
            citations: Vec::new(),
            answer: String::new(),
            timestamp: now(),
            directed_to: None,
            reply_to: None,
        }
    }

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
    // Cloned out once per turn: the panel may swap the model mid-session,
    // and a turn should condense and compose with the same one.
    let llm = app.llm();
    let resolved_query = match &llm {
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
            let scoped = api::ask_engine(
                &app_for_engine,
                &params,
                &eg_retrieve::ExpandOptions::default(),
                &eg_retrieve::RenderOptions::default(),
            )?;
            // The sticky sheet is a tiebreak for a follow-up ("total" on the
            // sheet just discussed), not a filter: `find_in` scoped to a
            // sheet cannot see a workbook-scoped defined name at all, and
            // hides every other sheet's better match. Live-testing found
            // "tax rate" answered with the Rates sheet's "Discount Rate"
            // because an earlier turn had pinned the scope to Rates, while
            // `Tax_Rate` — the exact answer — sat unstarred at [14]. So when
            // the scoped top hit does not carry every content word, search
            // the whole workbook too — and keep that result only if its top
            // hit carries strictly *more* of the question's words. The
            // verdict alone is not enough: a question with one word the
            // corpus never indexed ("the total here") is `Partial` on both
            // searches, and adopting the unscoped one on the verdict would
            // hop the scope to whichever sheet's "Total" ranks first
            // corpus-wide, exactly the follow-up the sticky sheet is for.
            let scope_may_hide = params.sheet.is_some()
                && !matches!(
                    scoped.verdict,
                    eg_retrieve::Verdict::Full | eg_retrieve::Verdict::NoContentWords
                );
            if scope_may_hide {
                let unscoped_params = SearchParams {
                    sheet: None,
                    ..params
                };
                let unscoped = api::ask_engine(
                    &app_for_engine,
                    &unscoped_params,
                    &eg_retrieve::ExpandOptions::default(),
                    &eg_retrieve::RenderOptions::default(),
                )?;
                let scoped_found_nothing = matches!(scoped.verdict, eg_retrieve::Verdict::Nothing);
                if unscoped.covered > scoped.covered || scoped_found_nothing {
                    return Ok(unscoped);
                }
            }
            Ok(scoped)
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
    let answer = match &llm {
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
        resolved_query,
        evidence: engine_result.evidence,
        citations: engine_result.citations,
        answer,
        ..ChatTurn::new(source, message)
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
        // Hydrate from disk on first touch, exactly like `history`/
        // `pending_for_agent`/`run_turn`'s own step 1 — never a blank
        // `ChatSession::default()`. `direct_to_agent` calls straight into
        // this with no prior read of the session, so getting this wrong
        // here (as opposed to only in `run_turn`'s already-hydrated case)
        // would silently overwrite a session's persisted history the first
        // time a directed turn reaches a process that hasn't loaded it yet.
        let session = sessions
            .entry(session_id.to_string())
            .or_insert_with(|| load_session(&app.dir, session_id));
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
        directed_to: Some(Directed::Agent),
        ..ChatTurn::new(TurnSource::Human, message)
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
///
/// Refused outright under `--redact-values`. Every other path into
/// `ChatTurn.answer` is either the rendered passage (which `render()`
/// guarantees never carries a cell value) or an LLM's `compose()`, gated by
/// `--llm-privacy`; this one is free text an agent typed, with nothing in
/// this process able to tell whether it quotes a cell value or not. A server
/// started with `--redact-values` exists so that nothing about a workbook's
/// contents leaves the machine — accepting arbitrary agent text into the
/// persisted `chat/<session>.json` would make that promise unenforceable,
/// so the reply is refused rather than accepted and trusted.
pub fn reply_to_agent_turn(
    app: &Arc<App>,
    session_id: &str,
    reply_to: u64,
    answer: &str,
) -> Result<ChatTurnDto, String> {
    if app.redact_values {
        return Err(
            "this server was started with --redact-values; an agent's reply text can't be \
             checked for cell values, so replying to a directed chat turn is refused here — \
             use `read_cells`/`what_if` etc. against the corpus directly instead"
                .to_string(),
        );
    }
    let question = {
        let mut sessions = app.sessions();
        let session = sessions
            .entry(session_id.to_string())
            .or_insert_with(|| load_session(&app.dir, session_id));
        // The id must name a turn that is actually open for a reply: one
        // directed at the agent, and not already answered — not just any
        // turn id. Without this, a stale `pending_for_you` id, a typo, or
        // two agents racing to answer the same question would each succeed
        // silently: the first check catches replying to an ordinary
        // engine-answered turn or another reply; the second catches two
        // replies to the one open question landing as two contradictory
        // answers in the shared session.
        let already_replied = session.turns.iter().any(|t| t.reply_to == Some(reply_to));
        session
            .turns
            .iter()
            .find(|t| t.id == reply_to)
            .filter(|t| t.awaiting_agent() && !already_replied)
            .map(|t| t.message.clone())
    };
    let Some(question) = question else {
        return Err(format!(
            "turn {reply_to} in session {session_id:?} is not an open question directed at the agent"
        ));
    };
    let turn = ChatTurn {
        answer: answer.to_string(),
        reply_to: Some(reply_to),
        ..ChatTurn::new(TurnSource::Agent, question)
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
    async fn sticky_scope_widens_when_the_answer_is_on_another_sheet() {
        let (app, _dir) = indexed_app().await;
        // Pins the sticky sheet to wherever "Discount Rate" ranks first.
        let first = run_turn(
            &app,
            DEFAULT_SESSION,
            TurnSource::Human,
            "discount rate",
            None,
        )
        .await
        .expect("the first turn runs");
        assert!(first.citations[0].contains('!'), "a citation names a sheet");

        // `Tax_Rate` is a workbook-scoped defined name: a sheet-scoped search
        // cannot return it at all, so the scoped top hit for this question
        // carries "rate" but not "tax". That must widen to the workbook and
        // star the name, not answer with the pinned sheet's nearest column.
        let second = run_turn(&app, DEFAULT_SESSION, TurnSource::Human, "tax rate", None)
            .await
            .expect("the second turn runs");
        assert!(
            second.answer.contains("* defined name \"Tax_Rate\""),
            "the sticky sheet hid the defined name:\n{}",
            second.answer
        );
    }

    #[tokio::test]
    async fn sticky_scope_survives_a_word_the_corpus_never_indexed() {
        let (app, _dir) = indexed_app().await;
        run_turn(
            &app,
            DEFAULT_SESSION,
            TurnSource::Human,
            "rates lookup table",
            None,
        )
        .await
        .expect("the first turn runs");
        let pinned = app
            .sessions()
            .get(DEFAULT_SESSION)
            .and_then(|s| s.sheet.clone())
            .expect("the first turn pins a sheet");

        // "qwertyuiop" is in no column name, so the scoped and the unscoped
        // search are both `Partial` — widening on the verdict alone would
        // adopt the workbook-wide ranking, whose "rate" is the Debtors
        // column, and hop the scope off the sheet just discussed. The scoped
        // top hit carries "rate" just as well, so nothing was hidden and
        // the sticky sheet must hold.
        let second = run_turn(
            &app,
            DEFAULT_SESSION,
            TurnSource::Human,
            "rate qwertyuiop",
            None,
        )
        .await
        .expect("the second turn runs");
        let citation = second.citations.first().expect("a citation");
        assert!(
            citation.starts_with(&format!("{pinned}!")),
            "follow-up citation {citation:?} should stay on {pinned:?}:\n{}",
            second.answer
        );
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

    fn redacted_app() -> (Arc<App>, tempfile::TempDir) {
        let dir = tempfile::tempdir().expect("a temp dir");
        let app = Arc::new(
            App::new(dir.path().to_str().expect("utf-8 path"), true, None).expect("engine opens"),
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

    #[test]
    fn replying_to_a_turn_that_was_never_directed_at_the_agent_is_refused() {
        let (app, _dir) = bare_app();
        // An ordinary directed turn's own id names a real turn, but it was
        // never *addressed to* the agent — `reply_to` must still refuse it,
        // not just check the id exists.
        let question = direct_to_agent(&app, DEFAULT_SESSION, "q").expect("directs fine");
        let bystander =
            reply_to_agent_turn(&app, DEFAULT_SESSION, question.id, "first reply").expect("ok");
        let result = reply_to_agent_turn(&app, DEFAULT_SESSION, bystander.id, "answer to a reply");
        assert!(
            result.is_err(),
            "a reply turn is not itself open for a reply"
        );
    }

    #[test]
    fn a_second_reply_to_an_already_answered_question_is_refused() {
        let (app, _dir) = bare_app();
        let question =
            direct_to_agent(&app, DEFAULT_SESSION, "which sheet?").expect("directs fine");
        reply_to_agent_turn(&app, DEFAULT_SESSION, question.id, "RATES")
            .expect("the first reply succeeds");
        let second = reply_to_agent_turn(&app, DEFAULT_SESSION, question.id, "actually, LOOKUP");
        assert!(
            second.is_err(),
            "two agents racing to answer the same question should not both succeed"
        );
    }

    #[test]
    fn a_reply_is_refused_on_a_redact_values_corpus() {
        let (app, _dir) = redacted_app();
        let question = direct_to_agent(&app, DEFAULT_SESSION, "which sheet has it?")
            .expect("directing a question is still fine under --redact-values — it's the reply that's gated");

        let result = reply_to_agent_turn(
            &app,
            DEFAULT_SESSION,
            question.id,
            "It's 1,612 on RATES!B4.",
        );
        let Err(err) = result else {
            panic!("a reply must be refused on a --redact-values corpus");
        };
        assert!(
            err.contains("redact-values"),
            "the refusal should name why, got {err:?}"
        );

        // Refused before anything is appended — no half-written reply, and
        // the question is still open (not silently marked answered).
        assert!(
            pending_for_agent(&app, DEFAULT_SESSION)
                .iter()
                .any(|t| t.id == question.id),
            "the question must still be pending after a refused reply"
        );
        let history = history(&app, DEFAULT_SESSION);
        assert_eq!(
            history.len(),
            1,
            "a refused reply must not be appended to the session"
        );
    }

    #[test]
    fn a_directed_turn_does_not_clobber_history_already_on_disk() {
        // Regression: `commit_turn` must hydrate an unseen session from disk
        // before appending, the same as `run_turn`'s own step 1 — not start
        // it from `ChatSession::default()`, which would silently overwrite
        // whatever `save_session` had already written for this session id.
        let dir = tempfile::tempdir().expect("a temp dir");
        let existing = ChatSession {
            id: DEFAULT_SESSION.to_string(),
            workbook: Some("some-workbook".to_string()),
            sheet: None,
            turns: vec![ChatTurn {
                answer: "an earlier, already-persisted answer".to_string(),
                ..ChatTurn::new(TurnSource::Human, "an earlier question")
            }],
        };
        save_session(dir.path().to_str().unwrap(), &existing).expect("seed the session file");

        // A *fresh* App — its in-memory session map has never touched this
        // session id, matching a freshly started `eg gui` process where a
        // directed turn is the first thing to reach this session.
        let app =
            Arc::new(App::new(dir.path().to_str().unwrap(), false, None).expect("engine opens"));
        direct_to_agent(&app, DEFAULT_SESSION, "a brand new directed question")
            .expect("directing a turn does not need the engine");

        let persisted = load_session(dir.path().to_str().unwrap(), DEFAULT_SESSION);
        assert_eq!(
            persisted.turns.len(),
            2,
            "the earlier turn must survive alongside the new one, not be overwritten"
        );
        assert_eq!(persisted.turns[0].message, "an earlier question");
        assert_eq!(persisted.turns[1].message, "a brand new directed question");
    }
}
