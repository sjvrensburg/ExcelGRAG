//! The bundled model: a `llama-server` beside `eg`, and the weights it runs.
//!
//! "Bundled" is a sidecar process and a download, not a link-time
//! dependency — `docs/design.md` records why an in-process runtime was set
//! aside (a 4096-token context cap, no GPU backend for the machine this was
//! built on). Everything here is data: the manifest names one runtime
//! build per platform and accelerator ([`RUNTIMES`]) and one weights file
//! per tier ([`MODELS`]), each
//! with a URL, a size and a sha256, and the code only fetches, verifies,
//! unpacks and spawns what the manifest says. Adding a platform or a model
//! is a row, and swapping a model is a row plus an `eg-agent --score` run.
//!
//! Files land in the same cache the embedding model uses
//! (`eg_index::cache_dir()`, `$EG_MODEL_CACHE` to move it): the runtime
//! under `llama/<build>/`, the weights beside them. A download resumes from
//! a `.part` file — the weights are gigabytes, and a dropped connection
//! must not mean starting over — and nothing is used until its sha256
//! matches the manifest's. The server is spawned on a free loopback port
//! and stopped **by pid**: a name-based kill once took down every other
//! `llama-server` on the machine.

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use futures::StreamExt;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::app::App;
use crate::dto::WsEvent;
use crate::llm::{LlmSettings, Privacy};

/// One `llama-server` build, for one platform and accelerator.
#[derive(Clone, Copy, Debug, Serialize)]
pub struct Runtime {
    /// `std::env::consts::OS`.
    pub os: &'static str,
    /// `std::env::consts::ARCH`.
    pub arch: &'static str,
    pub accelerator: &'static str,
    pub build: &'static str,
    pub url: &'static str,
    pub size: u64,
    pub sha256: &'static str,
    /// The path of `llama-server` inside the archive.
    pub member: &'static str,
}

/// One weights file.
#[derive(Clone, Copy, Debug, Serialize)]
pub struct Model {
    /// The name the GUI and the OpenAI-compatible endpoint use.
    pub id: &'static str,
    pub tier: &'static str,
    /// Why this one, in a sentence a person choosing between tiers can use.
    pub note: &'static str,
    pub url: &'static str,
    pub file: &'static str,
    pub size: u64,
    pub sha256: &'static str,
    /// Extra `llama-server` flags this model wants.
    pub args: &'static [&'static str],
    /// Bytes of RAM (or VRAM) a machine should have free to run it.
    pub needs_bytes: u64,
}

pub const RUNTIMES: &[Runtime] = &[
    Runtime {
        os: "linux",
        arch: "x86_64",
        accelerator: "vulkan",
        build: "b11063",
        url: "https://github.com/ggml-org/llama.cpp/releases/download/b11063/llama-b11063-bin-ubuntu-vulkan-x64.tar.gz",
        size: 30_393_876,
        sha256: "bc3d6ec1c0a2a4c44820ef0adfd0fe5a5d57d05ead8787c8c2ecacd1e36b8f31",
        member: "llama-b11063/llama-server",
    },
    Runtime {
        os: "linux",
        arch: "x86_64",
        accelerator: "cpu",
        build: "b11063",
        url: "https://github.com/ggml-org/llama.cpp/releases/download/b11063/llama-b11063-bin-ubuntu-x64.tar.gz",
        size: 16_885_137,
        sha256: "39476232e3b79b31b7960beacaa3a17aa49c1694511da91c25aa125feaa1ad15",
        member: "llama-b11063/llama-server",
    },
    // Untested here: no Apple hardware was available. The archive layout
    // is the same family as the Linux ones and Metal needs no flag.
    Runtime {
        os: "macos",
        arch: "aarch64",
        accelerator: "metal",
        build: "b11063",
        url: "https://github.com/ggml-org/llama.cpp/releases/download/b11063/llama-b11063-bin-macos-arm64.tar.gz",
        size: 11_179_086,
        sha256: "82d8846dc0a51f4493ff914a0b53e612a3a7a4834f48986283a99af6d51dd793",
        member: "llama-b11063/llama-server",
    },
];

const GIB: u64 = 1 << 30;

pub const MODELS: &[Model] = &[
    Model {
        id: "ornith-1.5-35b-a3b",
        tier: "large",
        note: "Answers every question of the demo agent file, including the what-if; a reasoning model, so it thinks before it answers. Wants ~24 GB free.",
        url: "https://huggingface.co/ornith-ai/Ornith-1.5-35B-A3B-GGUF/resolve/main/Ornith-1.5-35B-Q4_K_M.gguf",
        file: "Ornith-1.5-35B-Q4_K_M.gguf",
        size: 21_713_463_040,
        sha256: "42739874cc2ccfdb8523b23fbe52e29b2a7555c8176737ca9ca0b5d59859d41f",
        args: &["--jinja", "--reasoning-format", "deepseek"],
        needs_bytes: 24 * GIB,
    },
    Model {
        id: "qwen3-30b-a3b-instruct",
        tier: "large-alternative",
        note: "Fast and thorough on retrieval-shaped questions; will explain a what-if rather than run one. Wants ~21 GB free.",
        url: "https://huggingface.co/unsloth/Qwen3-30B-A3B-Instruct-2507-GGUF/resolve/main/Qwen3-30B-A3B-Instruct-2507-Q4_K_M.gguf",
        file: "Qwen3-30B-A3B-Instruct-2507-Q4_K_M.gguf",
        size: 18_556_686_752,
        sha256: "6c997b8af17debdfb01d890214400ccbab00db6acc0ba8da5de1cc906c4774d0",
        args: &["--jinja"],
        needs_bytes: 21 * GIB,
    },
    Model {
        id: "ornith-1.5-9b",
        tier: "small",
        note: "Sums, counts, formulas and band lookups like the large tier, at a fifth of the size; a reasoning model. Wants ~8 GB free.",
        url: "https://huggingface.co/ornith-ai/Ornith-1.5-9B-GGUF/resolve/main/Ornith-1.5-9B-Q4_K_M.gguf",
        file: "Ornith-1.5-9B-Q4_K_M.gguf",
        size: 5_780_090_816,
        sha256: "70c112196e0b7023803c9762752e46d29e612a92c83f995bc3ba1ceb07e8fab6",
        args: &["--jinja", "--reasoning-format", "deepseek"],
        needs_bytes: 8 * GIB,
    },
    Model {
        id: "qwen3-4b-instruct",
        tier: "laptop",
        note: "Finds things in a workbook but cannot compute over it; the one to run on 8 GB. Wants ~4 GB free.",
        url: "https://huggingface.co/unsloth/Qwen3-4B-Instruct-2507-GGUF/resolve/main/Qwen3-4B-Instruct-2507-Q4_K_M.gguf",
        file: "Qwen3-4B-Instruct-2507-Q4_K_M.gguf",
        size: 2_497_281_120,
        sha256: "3605803b982cb64aead44f6c1b2ae36e3acdb41d8e46c8a94c6533bc4c67e597",
        args: &["--jinja"],
        needs_bytes: 4 * GIB,
    },
];

/// The context window every model is started with. Thirteen tool schemas,
/// a preamble and a handful of rendered passages fit in a quarter of it.
const CONTEXT: &str = "32768";

pub fn model(id: &str) -> Option<&'static Model> {
    MODELS.iter().find(|m| m.id == id)
}

/// The runtime for this machine, accelerator-first.
pub fn runtime_for_host() -> Option<&'static Runtime> {
    let (os, arch) = (std::env::consts::OS, std::env::consts::ARCH);
    RUNTIMES.iter().find(|r| r.os == os && r.arch == arch)
}

pub fn runtime_dir() -> PathBuf {
    eg_index::cache_dir().join("llama")
}

pub fn model_path(model: &Model) -> PathBuf {
    eg_index::cache_dir().join(model.file)
}

pub fn server_path(runtime: &Runtime) -> PathBuf {
    runtime_dir().join(runtime.member)
}

/// Where the sidecar is, as the GUI shows it.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum SidecarStatus {
    #[default]
    Stopped,
    Downloading {
        model: String,
        /// What is being fetched: "runtime" or "weights".
        what: String,
        done: u64,
        total: u64,
    },
    Verifying {
        model: String,
    },
    Starting {
        model: String,
    },
    Running {
        model: String,
        port: u16,
        pid: u32,
    },
    Failed {
        model: String,
        error: String,
    },
}

/// What the browser is told about the manifest: which models exist, which
/// are already on disk, and whether this platform has a runtime.
#[derive(Serialize)]
pub struct SidecarInfo {
    pub status: SidecarStatus,
    pub runtime: Option<Runtime>,
    pub models: Vec<ModelInfo>,
    pub cache_dir: String,
}

#[derive(Serialize)]
pub struct ModelInfo {
    #[serde(flatten)]
    pub model: Model,
    pub downloaded: bool,
}

pub fn info(app: &App) -> SidecarInfo {
    SidecarInfo {
        status: app.sidecar().status.clone(),
        runtime: runtime_for_host().copied(),
        models: MODELS
            .iter()
            .map(|m| ModelInfo {
                model: *m,
                downloaded: model_path(m).is_file(),
            })
            .collect(),
        cache_dir: eg_index::cache_dir().display().to_string(),
    }
}

/// The live sidecar, held by `App`. The child is stopped by pid on `stop`
/// and on drop, so a GUI that exits takes its server with it and no one
/// else's.
#[derive(Default)]
pub struct Sidecar {
    pub status: SidecarStatus,
    child: Option<Child>,
}

impl Sidecar {
    fn stop_child(&mut self) {
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

impl Drop for Sidecar {
    fn drop(&mut self) {
        self.stop_child();
    }
}

fn set_status(app: &App, status: SidecarStatus) {
    app.sidecar().status = status.clone();
    app.send(WsEvent::Sidecar { status });
}

/// Fetch what is missing, verify it, start the server, and point the chat
/// model at it. Runs to completion on its own; progress goes out as
/// `WsEvent::Sidecar` and the final state is readable from `info`.
/// Returns immediately with an error if a sidecar is already in flight.
pub fn launch(app: Arc<App>, model_id: &str) -> Result<(), String> {
    let model = model(model_id).ok_or_else(|| {
        let ids: Vec<&str> = MODELS.iter().map(|m| m.id).collect();
        format!(
            "no bundled model called {model_id:?}; the manifest has: {}",
            ids.join(", ")
        )
    })?;
    let runtime = runtime_for_host().ok_or_else(|| {
        format!(
            "no bundled runtime for {}/{} yet — point the Chat model panel at a llama-server, \
             Ollama or hosted endpoint instead",
            std::env::consts::OS,
            std::env::consts::ARCH
        )
    })?;
    {
        let mut slot = app.sidecar();
        if matches!(
            slot.status,
            SidecarStatus::Downloading { .. }
                | SidecarStatus::Verifying { .. }
                | SidecarStatus::Starting { .. }
        ) {
            return Err("a sidecar is already being set up".into());
        }
        slot.stop_child();
        slot.status = SidecarStatus::Stopped;
    }
    tokio::spawn(async move {
        if let Err(error) = run(&app, model, runtime).await {
            tracing::warn!("sidecar {}: {error}", model.id);
            app.sidecar().stop_child();
            set_status(
                &app,
                SidecarStatus::Failed {
                    model: model.id.to_string(),
                    error,
                },
            );
        }
    });
    Ok(())
}

/// Stop the running sidecar, if any, and switch the chat model off.
pub fn stop(app: &App) -> SidecarStatus {
    {
        let mut slot = app.sidecar();
        slot.stop_child();
        slot.status = SidecarStatus::Stopped;
    }
    let _ = app.set_llm(None);
    app.send(WsEvent::Sidecar {
        status: SidecarStatus::Stopped,
    });
    SidecarStatus::Stopped
}

async fn run(
    app: &Arc<App>,
    model: &'static Model,
    runtime: &'static Runtime,
) -> Result<(), String> {
    let server = server_path(runtime);
    if !server.is_file() {
        let archive =
            runtime_dir().join(format!("{}-{}.tar.gz", runtime.build, runtime.accelerator));
        fetch(
            app,
            model.id,
            "runtime",
            runtime.url,
            runtime.size,
            runtime.sha256,
            &archive,
        )
        .await?;
        let dir = runtime_dir();
        let archive_for_unpack = archive.clone();
        tokio::task::spawn_blocking(move || unpack(&archive_for_unpack, &dir))
            .await
            .map_err(|e| e.to_string())??;
        if !server.is_file() {
            return Err(format!(
                "the runtime archive did not contain {}",
                runtime.member
            ));
        }
    }

    let weights = model_path(model);
    if !weights.is_file() {
        fetch(
            app,
            model.id,
            "weights",
            model.url,
            model.size,
            model.sha256,
            &weights,
        )
        .await?;
    }

    set_status(
        app,
        SidecarStatus::Starting {
            model: model.id.to_string(),
        },
    );
    let port = free_port()?;
    let mut command = Command::new(&server);
    command
        .arg("-m")
        .arg(&weights)
        .args(["--host", "127.0.0.1", "--port", &port.to_string()])
        .args(["-c", CONTEXT, "-ngl", "99", "--alias", model.id])
        .args(model.args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    // A GUI that is killed cannot run `Drop`; the child must not outlive it
    // on that path either. On Linux the child asks the kernel to SIGTERM
    // it when its parent exits, whatever the exit was. Elsewhere the
    // signal handler in `lib.rs` covers SIGINT/SIGTERM.
    #[cfg(target_os = "linux")]
    {
        use std::os::unix::process::CommandExt;
        // SAFETY: `prctl` with PR_SET_PDEATHSIG only sets a flag on the
        // calling (child) process; it allocates nothing and touches no
        // state shared with the parent, which is what a `pre_exec` hook
        // may do.
        unsafe {
            command.pre_exec(|| {
                libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGTERM);
                Ok(())
            });
        }
    }
    let child = command
        .spawn()
        .map_err(|e| format!("could not start {}: {e}", server.display()))?;
    let pid = child.id();
    app.sidecar().child = Some(child);

    wait_healthy(port, Duration::from_secs(600)).await?;

    // A server on loopback keeps "nothing leaves the machine": `values` is
    // the tier that lets an investigation read cells, and it is announced
    // on stderr exactly as the flag would be.
    app.set_llm(Some(LlmSettings {
        base_url: format!("http://127.0.0.1:{port}/v1"),
        model: model.id.to_string(),
        privacy: if app.redact_values {
            Privacy::Passage
        } else {
            Privacy::Values
        },
        api_key_env: None,
    }))?;
    set_status(
        app,
        SidecarStatus::Running {
            model: model.id.to_string(),
            port,
            pid,
        },
    );
    Ok(())
}

fn free_port() -> Result<u16, String> {
    let listener =
        std::net::TcpListener::bind(("127.0.0.1", 0)).map_err(|e| format!("no free port: {e}"))?;
    listener
        .local_addr()
        .map(|a| a.port())
        .map_err(|e| e.to_string())
}

async fn wait_healthy(port: u16, timeout: Duration) -> Result<(), String> {
    let client = reqwest::Client::new();
    let url = format!("http://127.0.0.1:{port}/health");
    let start = Instant::now();
    loop {
        if let Ok(response) = client.get(&url).send().await {
            if response.status().is_success() {
                return Ok(());
            }
        }
        if start.elapsed() > timeout {
            return Err(format!(
                "llama-server did not become healthy on port {port} within {timeout:?}"
            ));
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
}

/// Download `url` to `dest`, resuming from `dest.part` when one exists,
/// then verify the whole file against `sha256` before renaming it into
/// place. A file that fails verification is removed, not kept.
async fn fetch(
    app: &Arc<App>,
    model_id: &str,
    what: &str,
    url: &str,
    total: u64,
    sha256: &str,
    dest: &Path,
) -> Result<(), String> {
    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    let part = dest.with_extension(format!(
        "{}part",
        dest.extension()
            .map(|e| format!("{}.", e.to_string_lossy()))
            .unwrap_or_default()
    ));
    let mut done = std::fs::metadata(&part).map(|m| m.len()).unwrap_or(0);
    if done > total {
        std::fs::remove_file(&part).map_err(|e| e.to_string())?;
        done = 0;
    }
    let report = |done: u64| {
        set_status(
            app,
            SidecarStatus::Downloading {
                model: model_id.to_string(),
                what: what.to_string(),
                done,
                total,
            },
        )
    };
    report(done);

    if done < total {
        let client = reqwest::Client::builder()
            .user_agent(concat!("excelgrag/", env!("CARGO_PKG_VERSION")))
            .build()
            .map_err(|e| e.to_string())?;
        let mut request = client.get(url);
        if done > 0 {
            request = request.header("Range", format!("bytes={done}-"));
        }
        let response = request
            .send()
            .await
            .map_err(|e| format!("could not fetch {url}: {e}"))?;
        let status = response.status();
        let resumed = status == reqwest::StatusCode::PARTIAL_CONTENT;
        if !status.is_success() {
            return Err(format!("{url} answered {status}"));
        }
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(resumed)
            .write(true)
            .truncate(!resumed)
            .open(&part)
            .map_err(|e| e.to_string())?;
        if !resumed {
            done = 0;
        }
        let mut stream = response.bytes_stream();
        let mut last_report = Instant::now();
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.map_err(|e| format!("download of {url} broke: {e}"))?;
            file.write_all(&chunk).map_err(|e| e.to_string())?;
            done += chunk.len() as u64;
            if last_report.elapsed() > Duration::from_millis(500) {
                report(done);
                last_report = Instant::now();
            }
        }
        file.flush().map_err(|e| e.to_string())?;
        report(done);
    }
    if done != total {
        return Err(format!(
            "{url}: got {done} bytes, the manifest says {total} — the file changed upstream, \
             or the download stopped short; run again to resume"
        ));
    }

    set_status(
        app,
        SidecarStatus::Verifying {
            model: model_id.to_string(),
        },
    );
    let part_for_hash = part.clone();
    let actual = tokio::task::spawn_blocking(move || hash_file(&part_for_hash))
        .await
        .map_err(|e| e.to_string())??;
    if actual != sha256 {
        let _ = std::fs::remove_file(&part);
        return Err(format!(
            "{}: sha256 {actual} does not match the manifest's {sha256}; the file was removed",
            dest.display()
        ));
    }
    std::fs::rename(&part, dest).map_err(|e| e.to_string())
}

fn hash_file(path: &Path) -> Result<String, String> {
    let mut file = std::fs::File::open(path).map_err(|e| e.to_string())?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 1 << 20];
    loop {
        let n = file.read(&mut buf).map_err(|e| e.to_string())?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(format!("{:x}", hasher.finalize()))
}

fn unpack(archive: &Path, into: &Path) -> Result<(), String> {
    let file = std::fs::File::open(archive).map_err(|e| e.to_string())?;
    let gz = flate2::read::GzDecoder::new(file);
    let mut tar = tar::Archive::new(gz);
    tar.unpack(into)
        .map_err(|e| format!("could not unpack {}: {e}", archive.display()))
}

/// `App`'s accessor, here so the lock's recovery rule lives beside its
/// users.
pub fn slot(slot: &Mutex<Sidecar>) -> MutexGuard<'_, Sidecar> {
    slot.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_manifest_is_well_formed() {
        for r in RUNTIMES {
            assert_eq!(r.sha256.len(), 64, "{}", r.url);
            assert!(r.url.ends_with(".tar.gz"), "{}", r.url);
            assert!(r.member.ends_with("llama-server"));
        }
        let mut ids = std::collections::BTreeSet::new();
        for m in MODELS {
            assert_eq!(m.sha256.len(), 64, "{}", m.id);
            assert!(m.url.ends_with(m.file), "{}", m.id);
            assert!(m.size > 0 && m.needs_bytes > m.size, "{}", m.id);
            assert!(ids.insert(m.id), "duplicate id {}", m.id);
        }
        assert!(model("ornith-1.5-35b-a3b").is_some());
        assert!(model("nope").is_none());
    }

    #[test]
    fn a_hash_is_the_files_sha256() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("x");
        std::fs::write(&path, b"abc").unwrap();
        assert_eq!(
            hash_file(&path).unwrap(),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }
}
