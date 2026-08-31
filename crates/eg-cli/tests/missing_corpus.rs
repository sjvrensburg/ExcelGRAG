//! L2: a read-only verb against a directory nobody has ever run `eg index`
//! on must refuse, not silently materialize an empty corpus and answer
//! NOTHING MATCHED. See docs/audit-2026-08-31.md L2.

use std::path::PathBuf;
use std::process::Command;

fn eg() -> Command {
    Command::new(env!("CARGO_BIN_EXE_eg"))
}

fn nonexistent_dir(tag: &str) -> PathBuf {
    std::env::temp_dir().join(format!(
        "eg-cli-missing-corpus-{tag}-{}-{:?}",
        std::process::id(),
        std::thread::current().id()
    ))
}

#[test]
fn ask_refuses_a_directory_that_was_never_indexed() {
    let dir = nonexistent_dir("ask");
    let _ = std::fs::remove_dir_all(&dir);

    let output = eg()
        .args(["ask", dir.to_str().unwrap(), "bad", "debt"])
        .output()
        .expect("could not run eg ask");

    assert!(
        !output.status.success(),
        "a missing corpus must not succeed"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("no corpus at"), "{stderr}");
    assert!(
        !dir.join("manifest.json").exists(),
        "a refused read-only verb must not materialize a corpus on disk"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn search_refuses_a_directory_that_was_never_indexed() {
    let dir = nonexistent_dir("search");
    let _ = std::fs::remove_dir_all(&dir);

    let output = eg()
        .args(["search", dir.to_str().unwrap(), "bad", "debt"])
        .output()
        .expect("could not run eg search");

    assert!(!output.status.success());
    assert!(
        !dir.join("manifest.json").exists(),
        "a refused read-only verb must not materialize a corpus on disk"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn workbooks_refuses_a_directory_that_was_never_indexed() {
    let dir = nonexistent_dir("workbooks");
    let _ = std::fs::remove_dir_all(&dir);

    let output = eg()
        .args(["workbooks", dir.to_str().unwrap()])
        .output()
        .expect("could not run eg workbooks");

    assert!(!output.status.success());
    assert!(!dir.join("manifest.json").exists());

    let _ = std::fs::remove_dir_all(&dir);
}
