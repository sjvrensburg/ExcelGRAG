//! What the server holds open between requests.
//!
//! The engine half is `eg_mcp::State` — corpus, lexical index, a lazily
//! loaded embedder, a workbook cache — because that composition is already
//! the server-shaped one: `eg serve` holds exactly these resources open for
//! the same reasons (memory-mapped indexes are cheap to keep, the model is
//! expensive to load, workbooks are expensive to read). The GUI adds a
//! broadcast channel and an index-job slot, and nothing else.
//!
//! The engine lives behind a `std` mutex, and no handler ever holds it
//! across an `.await`: everything that touches it runs inside
//! `spawn_blocking`, because embedding inference and workbook reads are
//! exactly the CPU-bound work that blocking threads exist for.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, MutexGuard};

use tokio::sync::broadcast;

use crate::chat::ChatSession;
use crate::dto::WsEvent;
use crate::llm;

pub struct App {
    pub dir: String,
    /// Whether cell values may leave this server. A startup policy, the same
    /// knob `eg serve` has, applied to every place the GUI would show one.
    pub redact_values: bool,
    /// `Arc` so the same engine instance is shared with the MCP bridge
    /// (`mcp_bridge.rs`) — an agent's tool call and a browser tab's request
    /// touch the exact same corpus/index/workbook-cache state.
    engine: Arc<Mutex<eg_mcp::State>>,
    /// Every open WebSocket subscribes to this; senders that outlive all
    /// listeners are fine (send is a no-op with no subscribers).
    pub events: broadcast::Sender<WsEvent>,
    /// The path currently being indexed, if any. One at a time: the manifest
    /// has a lock and tantivy's writer has a directory one, so concurrent
    /// index jobs would spend their time fighting rather than finishing.
    indexing: Mutex<Option<String>>,
    /// Chat sessions, keyed by id. A second, independent lock from `engine` —
    /// `chat::run_turn` never holds both at once (see its doc comment).
    sessions: Mutex<HashMap<String, ChatSession>>,
    /// The chat model, if `--llm-privacy` is anything but `off`.
    pub llm: Option<llm::Client>,
}

impl App {
    pub fn new(dir: &str, redact_values: bool, llm: Option<llm::Client>) -> Result<App, String> {
        let state = eg_mcp::State::open(dir, redact_values)?;
        let (events, _) = broadcast::channel(256);
        Ok(App {
            dir: dir.to_string(),
            redact_values,
            engine: Arc::new(Mutex::new(state)),
            events,
            indexing: Mutex::new(None),
            sessions: Mutex::new(HashMap::new()),
            llm,
        })
    }

    /// The engine, recovering from a poisoned lock rather than propagating
    /// it: a panic in one request must not take the whole server's ability to
    /// serve any others with it.
    pub fn engine(&self) -> MutexGuard<'_, eg_mcp::State> {
        self.engine
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// The `Arc` itself, for the MCP bridge to share directly rather than
    /// going through `App`.
    pub fn engine_handle(&self) -> Arc<Mutex<eg_mcp::State>> {
        Arc::clone(&self.engine)
    }

    /// The index job in flight, if there is one.
    pub fn indexing(&self) -> MutexGuard<'_, Option<String>> {
        self.indexing
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Chat sessions. Lock briefly — read or update what's needed, clone it
    /// out, drop the guard — never held across an `.await` or alongside the
    /// engine lock; see `chat::run_turn`.
    pub fn sessions(&self) -> MutexGuard<'_, HashMap<String, ChatSession>> {
        self.sessions
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    pub fn send(&self, event: WsEvent) {
        let _ = self.events.send(event);
    }

    /// Swap in a freshly opened engine, so reads see what changed on disk.
    ///
    /// The lexical index holds its searcher open, and a tantivy searcher
    /// answers from the generation it was opened at: documents committed by
    /// an index job are invisible to it until the whole handle is reopened.
    /// The corpus manifest is the same — an in-memory snapshot, read at
    /// open. Rotation is cheap (mmap + small JSON), so it happens after
    /// every index job and every corpus change the watcher sees.
    pub fn rotate_engine(&self) -> Result<(), String> {
        let fresh = eg_mcp::State::open(&self.dir, self.redact_values)?;
        *self.engine() = fresh;
        Ok(())
    }
}

/// Resolve what a request meant by a workbook: a full hash, a hash prefix,
/// a stored path, or a bare filename — whichever picks out exactly one.
/// Delegates to `eg_mcp::State::resolve` (the same rule `eg ask --workbook`
/// and every MCP tool use) rather than re-matching here, so the GUI cannot
/// drift from the CLI/MCP surface on what a workbook is called.
pub fn resolve_hash(app: &App, wanted: &str) -> Result<String, String> {
    app.engine().resolve(Some(wanted)).map(|(hash, _path)| hash)
}

/// As [`resolve_hash`], but `None` (no filter given) passes straight
/// through, so unfiltered searches keep searching the whole corpus.
pub fn resolve_hash_optional(app: &App, wanted: &Option<String>) -> Result<Option<String>, String> {
    match wanted {
        None => Ok(None),
        Some(wanted) => resolve_hash(app, wanted).map(Some),
    }
}

/// Everything a handler needs, in one Arc.
pub type SharedApp = Arc<App>;
