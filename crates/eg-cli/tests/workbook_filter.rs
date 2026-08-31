//! L6: `ask` and `search` accept `--workbook`, the same way MCP's `context`
//! and `search` tools already did. See docs/audit-2026-08-31.md L6.

use std::path::{Path, PathBuf};
use std::process::Command;

fn fixture() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../tests/fixtures/vendor/issues.xlsx")
}

fn corpus_dir(tag: &str) -> PathBuf {
    std::env::temp_dir().join(format!(
        "eg-cli-workbook-filter-{tag}-{}-{:?}",
        std::process::id(),
        std::thread::current().id()
    ))
}

fn eg() -> Command {
    Command::new(env!("CARGO_BIN_EXE_eg"))
}

fn indexed(dir: &Path) {
    let status = eg()
        .args([
            "index",
            dir.to_str().unwrap(),
            fixture().to_str().unwrap(),
            "--lexical-only",
        ])
        .status()
        .expect("could not run eg index");
    assert!(status.success());
}

#[test]
fn search_with_an_unmatched_workbook_filter_is_refused() {
    let dir = corpus_dir("search-unmatched");
    let _ = std::fs::remove_dir_all(&dir);
    indexed(&dir);

    let output = eg()
        .args([
            "search",
            dir.to_str().unwrap(),
            "issue",
            "--workbook",
            "not-a-real-workbook",
        ])
        .output()
        .expect("could not run eg search");
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("no workbook"), "{stderr}");

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn search_resolves_a_workbook_filter_by_filename() {
    let dir = corpus_dir("search-by-filename");
    let _ = std::fs::remove_dir_all(&dir);
    indexed(&dir);

    let output = eg()
        .args([
            "search",
            dir.to_str().unwrap(),
            "issue",
            "--workbook",
            "issues.xlsx",
        ])
        .output()
        .expect("could not run eg search");
    assert!(
        output.status.success(),
        "a filename that matches exactly one workbook must resolve: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn ask_with_an_unmatched_workbook_filter_is_refused() {
    let dir = corpus_dir("ask-unmatched");
    let _ = std::fs::remove_dir_all(&dir);
    indexed(&dir);

    let output = eg()
        .args([
            "ask",
            dir.to_str().unwrap(),
            "issue",
            "--workbook",
            "not-a-real-workbook",
        ])
        .output()
        .expect("could not run eg ask");
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("no workbook"), "{stderr}");

    let _ = std::fs::remove_dir_all(&dir);
}
