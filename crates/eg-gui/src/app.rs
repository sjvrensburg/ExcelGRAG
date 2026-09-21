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
use crate::dto::{LlmStatusDto, WsEvent};
use crate::llm;
use crate::sidecar::{self, Sidecar};

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
    /// The chat model. A lock rather than a field fixed at startup because
    /// the GUI's settings panel can change it (`set_llm`); readers clone
    /// the `Client` out (`llm()`) so nothing holds this across an `.await`.
    llm: Mutex<LlmSlot>,
    /// The bundled model's `llama-server`, when one was started from the
    /// GUI. Stopped by pid when the `App` goes.
    sidecar: Mutex<Sidecar>,
}

/// The connection as configured, and the live client when the privacy tier
/// is anything but `off`. Settings outlive the client so that switching to
/// `off` and back doesn't lose the URL and model typed in.
#[derive(Default)]
struct LlmSlot {
    settings: Option<llm::LlmSettings>,
    client: Option<llm::Client>,
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
            llm: Mutex::new(LlmSlot {
                settings: llm.as_ref().map(|c| c.settings().clone()),
                client: llm,
            }),
            sidecar: Mutex::new(Sidecar::default()),
        })
    }

    fn llm_slot(&self) -> MutexGuard<'_, LlmSlot> {
        self.llm
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// The chat model to use for a turn, if any — a clone, so the caller
    /// holds no lock while it awaits the network.
    pub fn llm(&self) -> Option<llm::Client> {
        self.llm_slot().client.clone()
    }

    /// The connection as the GUI shows it: settings (never the key) plus
    /// whether a key was found behind the named variable.
    pub fn llm_status(&self) -> LlmStatusDto {
        let slot = self.llm_slot();
        LlmStatusDto {
            settings: slot.settings.clone(),
            key_present: slot.client.as_ref().is_some_and(|c| c.key_present()),
            redact_values: self.redact_values,
        }
    }

    /// Replace the chat model from the GUI. Goes through the same checks
    /// startup does (`LlmSettings::check`), reads the key from the named
    /// variable now so a missing one is refused here rather than failing
    /// every turn, and announces `values` mode on stderr exactly as the
    /// flag would — the browser being the origin of the change makes it no
    /// less something the person at the terminal should see. `None` turns
    /// the model off and forgets the settings.
    pub fn set_llm(&self, settings: Option<llm::LlmSettings>) -> Result<LlmStatusDto, String> {
        let (settings, client) = match settings {
            None => (None, None),
            Some(settings) => {
                settings.check(self.redact_values)?;
                let client = if settings.privacy.allows_llm() {
                    let config = settings.resolve()?;
                    config.announce();
                    Some(llm::Client::new(config))
                } else {
                    None
                };
                (Some(settings), client)
            }
        };
        {
            let mut slot = self.llm_slot();
            slot.settings = settings;
            slot.client = client;
        }
        let status = self.llm_status();
        self.send(WsEvent::Llm {
            status: status.clone(),
        });
        Ok(status)
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

    /// The sidecar slot. Held briefly, never across an `.await`.
    pub fn sidecar(&self) -> MutexGuard<'_, Sidecar> {
        sidecar::slot(&self.sidecar)
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::llm::{LlmSettings, Privacy};

    fn app(redact_values: bool) -> (App, tempfile::TempDir) {
        let dir = tempfile::tempdir().expect("a temp dir");
        let app = App::new(dir.path().to_str().unwrap(), redact_values, None).expect("opens");
        (app, dir)
    }

    fn settings(privacy: Privacy, api_key_env: Option<&str>) -> LlmSettings {
        LlmSettings {
            base_url: "http://127.0.0.1:1/v1".to_string(),
            model: "test".to_string(),
            privacy,
            api_key_env: api_key_env.map(str::to_string),
        }
    }

    #[test]
    fn values_is_refused_under_redact_values_from_the_panel_too() {
        let (app, _dir) = app(true);
        let err = app
            .set_llm(Some(settings(Privacy::Values, None)))
            .expect_err("the same rule as the startup flags");
        assert!(err.contains("redact-values"), "{err}");
        assert!(
            app.llm().is_none(),
            "a refused change leaves no client behind"
        );
    }

    #[test]
    fn a_missing_key_variable_is_refused_at_apply_time() {
        let (app, _dir) = app(false);
        let err = app
            .set_llm(Some(settings(
                Privacy::Passage,
                Some("EG_TEST_KEY_THAT_IS_NOT_SET"),
            )))
            .expect_err("an unset variable must not become a keyless client");
        assert!(err.contains("EG_TEST_KEY_THAT_IS_NOT_SET"), "{err}");
    }

    #[test]
    fn off_keeps_the_settings_but_no_client() {
        let (app, _dir) = app(false);
        let status = app
            .set_llm(Some(settings(Privacy::Off, None)))
            .expect("off needs nothing");
        assert!(app.llm().is_none());
        assert_eq!(status.settings.map(|s| s.privacy), Some(Privacy::Off));
    }

    #[test]
    fn passage_with_a_present_key_builds_a_client_and_never_reports_the_key() {
        let (app, _dir) = app(false);
        // Set for this process only; the name is what the panel sends.
        std::env::set_var("EG_TEST_LLM_KEY", "sk-not-a-real-key");
        let status = app
            .set_llm(Some(settings(Privacy::Passage, Some("EG_TEST_LLM_KEY"))))
            .expect("a resolvable key");
        assert!(status.key_present);
        let json = serde_json::to_string(&status).unwrap();
        assert!(
            !json.contains("sk-not-a-real-key"),
            "the key leaked: {json}"
        );
        assert!(
            json.contains("EG_TEST_LLM_KEY"),
            "the variable name is reported"
        );
        assert!(app.llm().is_some_and(|c| c.privacy.allows_llm()));

        // And back off again forgets the client, not the URL.
        let status = app.set_llm(None).expect("off");
        assert!(status.settings.is_none() && app.llm().is_none());
    }
}
