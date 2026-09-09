//! Watching the corpus, so the GUI is live without the GUI doing the
//! writing.
//!
//! The corpus is a directory of atomically-renamed JSON files, which is all
//! a watcher needs: `eg index` in a terminal, this server's own index jobs,
//! a sync tool — whatever changes it, the files land whole. On each burst of
//! changes (debounced, because one `eg index` writes several) the manifest
//! is re-read and diffed against the last snapshot, and every open
//! WebSocket is told what was added, removed, and rewritten.
//!
//! The granularity is per-workbook, not per-region: a graph appears when it
//! is whole. Streaming it in as `eg-graph` builds would need an event
//! channel through the engine; this gets "type `eg index`, watch the new
//! workbook arrive" with zero changes there.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use notify::Watcher;
use tokio::sync::mpsc;
use tokio::time::{sleep, Instant};

use crate::app::App;
use crate::dto::{WorkbookDto, WsEvent};

/// How long the corpus must be quiet before the diff runs. `eg index` writes
/// a graph, a profiles file, and the manifest in quick succession; reacting
/// to the first of those would diff against a half-written state.
const DEBOUNCE: Duration = Duration::from_millis(400);

type Snapshot = HashMap<String, Option<SystemTime>>;

pub fn spawn(app: Arc<App>) -> notify::Result<()> {
    let (tx, rx) = mpsc::channel(1024);
    let mut watcher = notify::recommended_watcher(move |event| {
        let _ = tx.blocking_send(event);
    })?;
    watcher.watch(
        std::path::Path::new(&app.dir),
        notify::RecursiveMode::Recursive,
    )?;
    tokio::spawn(run(app, rx, watcher));
    Ok(())
}

async fn run(
    app: Arc<App>,
    mut rx: mpsc::Receiver<notify::Result<notify::Event>>,
    _watcher: impl notify::Watcher,
) {
    let mut current = snapshot(&app).await;
    let mut pending = false;
    let debounce = sleep(DEBOUNCE);
    tokio::pin!(debounce);

    loop {
        tokio::select! {
            maybe = rx.recv() => {
                match maybe {
                    Some(Ok(event)) if relevant(&event) => {
                        pending = true;
                        debounce.as_mut().reset(Instant::now() + DEBOUNCE);
                    }
                    Some(_) => {}
                    None => break,
                }
            }
            _ = &mut debounce, if pending => {
                pending = false;
                let fresh = snapshot(&app).await;
                let added: Vec<String> = fresh
                    .keys()
                    .filter(|hash| !current.contains_key(*hash))
                    .cloned()
                    .collect();
                let removed: Vec<String> = current
                    .keys()
                    .filter(|hash| !fresh.contains_key(*hash))
                    .cloned()
                    .collect();
                let changed: Vec<String> = fresh
                    .iter()
                    .filter(|(hash, mtime)| {
                        current.get(*hash).is_some_and(|old| old != *mtime)
                    })
                    .map(|(hash, _)| hash.clone())
                    .collect();
                if !added.is_empty() || !removed.is_empty() || !changed.is_empty() {
                    // The engine's manifest is a snapshot from open; rotate
                    // so workbooks and search see what just landed.
                    let _ = app.rotate_engine();
                    let workbooks = tokio::task::spawn_blocking({
                        let app = Arc::clone(&app);
                        move || WorkbookDto::list(&app)
                    })
                    .await
                    .unwrap_or_default();
                    app.send(WsEvent::Corpus {
                        added,
                        removed,
                        changed,
                        workbooks,
                    });
                }
                current = fresh;
            }
        }
    }
}

/// A `.json` write in the corpus: manifest, a graph, or profiles. Everything
/// else — lock files, the indexes' own files, the temporaries that
/// `write_atomically` renames over — is noise. Reads are too.
///
/// `corpus/chat/*.json` is also excluded: every chat turn (`chat::
/// save_session`) writes one on every message, human or agent, which would
/// otherwise trigger a full corpus re-open and hash/mtime sweep
/// (`snapshot()`) per chat turn for a change that never touches a workbook.
fn relevant(event: &notify::Event) -> bool {
    if matches!(event.kind, notify::EventKind::Access(_)) {
        return false;
    }
    event.paths.iter().any(|path| {
        path.extension()
            .is_some_and(|extension| extension == "json")
            && !path.components().any(|c| c.as_os_str() == "chat")
    })
}

async fn snapshot(app: &Arc<App>) -> Snapshot {
    let app = Arc::clone(app);
    tokio::task::spawn_blocking(move || {
        let Ok(corpus) = eg_graph::store::Corpus::open(&app.dir) else {
            return Snapshot::new();
        };
        corpus
            .entries()
            .map(|(hash, _)| {
                let mtime = corpus
                    .graph_path(hash)
                    .metadata()
                    .and_then(|meta| meta.modified())
                    .ok();
                (hash.to_string(), mtime)
            })
            .collect()
    })
    .await
    .unwrap_or_default()
}
