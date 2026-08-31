//! L5: when `eg index` fails partway through a list of workbooks, it must say
//! which ones were already stored and that a rerun heals it — rather than
//! leaving the caller to discover that on their own. See
//! docs/audit-2026-08-31.md L5.

use std::path::PathBuf;
use std::process::Command;

fn fixture() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../tests/fixtures/vendor/issues.xlsx")
}

fn corpus_dir() -> PathBuf {
    std::env::temp_dir().join(format!(
        "eg-cli-partial-index-{}-{:?}",
        std::process::id(),
        std::thread::current().id()
    ))
}

fn eg() -> Command {
    Command::new(env!("CARGO_BIN_EXE_eg"))
}

#[test]
fn a_failure_partway_through_names_what_was_already_stored() {
    let dir = corpus_dir();
    let _ = std::fs::remove_dir_all(&dir);

    let bad_path = dir.join("this-file-does-not-exist.xlsx");
    let output = eg()
        .args([
            "index",
            dir.to_str().unwrap(),
            fixture().to_str().unwrap(),
            bad_path.to_str().unwrap(),
            "--lexical-only",
        ])
        .output()
        .expect("could not run eg index");

    assert!(
        !output.status.success(),
        "the second workbook does not exist, so the run must fail"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("stored 1 of 2"),
        "must say how many of the list were stored before the failure: {stderr}"
    );
    assert!(
        stderr.contains("re-running"),
        "must say a rerun heals it: {stderr}"
    );
    assert!(
        dir.join("manifest.json").exists(),
        "the first workbook's storage must survive the second one's failure"
    );

    let _ = std::fs::remove_dir_all(&dir);
}
