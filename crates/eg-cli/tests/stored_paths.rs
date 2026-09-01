//! `eg index` must store an absolute workbook path even when a relative one
//! was typed. An MCP client starts `eg serve`/`eg-mcp` with its own working
//! directory (documented as not necessarily the one `eg index` was run from),
//! so a relative path stored verbatim resolves against the wrong directory
//! the moment a cell-level tool tries to open it. See
//! docs/audit-2026-08-31.md H5.

use std::path::{Path, PathBuf};
use std::process::Command;

fn fixture() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../tests/fixtures/vendor/issues.xlsx")
        .canonicalize()
        .expect("the fixture exists")
}

fn corpus_dir() -> PathBuf {
    std::env::temp_dir().join(format!(
        "eg-cli-abs-path-{}-{:?}",
        std::process::id(),
        std::thread::current().id()
    ))
}

fn eg() -> Command {
    Command::new(env!("CARGO_BIN_EXE_eg"))
}

/// The `"path"` field of the manifest's one workbook entry.
///
/// This must go through JSON decoding: Windows canonical paths start with
/// `\\?\` and JSON escapes every backslash, so trimming quotes from the raw
/// line would test the encoded representation rather than the stored path.
fn stored_path(corpus: &Path) -> String {
    let manifest = std::fs::read_to_string(corpus.join("manifest.json")).unwrap();
    let manifest: serde_json::Value = serde_json::from_str(&manifest).unwrap();
    manifest["workbooks"]
        .as_object()
        .and_then(|workbooks| workbooks.values().next())
        .and_then(|entry| entry["path"].as_str())
        .expect("a path field in the manifest's workbook entry")
        .to_owned()
}

#[test]
fn the_stored_path_is_absolute_even_when_a_relative_one_was_typed() {
    let corpus = corpus_dir();
    let fixture = fixture();
    let fixture_dir = fixture.parent().unwrap();
    let file_name = fixture.file_name().unwrap();

    // Indexed with a bare filename, from the directory that file sits in —
    // as relative as a workbook path gets.
    let status = eg()
        .current_dir(fixture_dir)
        .args([
            "index",
            corpus.to_str().unwrap(),
            file_name.to_str().unwrap(),
            "--lexical-only",
            "--no-profiles",
        ])
        .status()
        .expect("could not run eg index");
    assert!(status.success());

    let stored = stored_path(&corpus);
    assert!(
        Path::new(&stored).is_absolute(),
        "stored path {stored:?} must be absolute, not relative to wherever `eg index` happened to run"
    );
    assert_eq!(
        Path::new(&stored).file_name(),
        Some(file_name),
        "still names the right file"
    );

    let _ = std::fs::remove_dir_all(&corpus);
}
