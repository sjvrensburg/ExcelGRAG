//! `eg index --no-profiles` must leave no value-bearing profiles behind, even
//! when re-indexing a workbook that was previously profiled with values. See
//! docs/audit-2026-08-31.md C4b: absence of `--profile` must be authoritative.

use std::path::{Path, PathBuf};
use std::process::Command;

fn fixture() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../tests/fixtures/vendor/issues.xlsx")
}

fn corpus_dir() -> PathBuf {
    std::env::temp_dir().join(format!(
        "eg-cli-no-profiles-{}-{:?}",
        std::process::id(),
        std::thread::current().id()
    ))
}

fn eg() -> Command {
    Command::new(env!("CARGO_BIN_EXE_eg"))
}

fn manifest_still_claims_profiles(dir: &Path) -> bool {
    let manifest = std::fs::read_to_string(dir.join("manifest.json")).unwrap();
    manifest.contains("\"profile_values\": true") || !manifest.contains("\"profiled_columns\": 0")
}

#[test]
fn a_no_profiles_reindex_removes_previously_stored_profiles() {
    let dir = corpus_dir();
    let _ = std::fs::remove_dir_all(&dir);
    let wb = fixture();

    let status = eg()
        .args([
            "index",
            dir.to_str().unwrap(),
            wb.to_str().unwrap(),
            "--lexical-only",
        ])
        .status()
        .expect("could not run eg index");
    assert!(status.success(), "first index (with profiles) must succeed");

    let profiles_dir = dir.join("profiles");
    let has_profile_file = |d: &Path| {
        std::fs::read_dir(d.join("profiles"))
            .map(|mut it| it.next().is_some())
            .unwrap_or(false)
    };
    assert!(
        has_profile_file(&dir),
        "profiling is on by default, so a profiles file must exist"
    );

    let status = eg()
        .args([
            "index",
            dir.to_str().unwrap(),
            wb.to_str().unwrap(),
            "--lexical-only",
            "--reindex",
            "--no-profiles",
        ])
        .status()
        .expect("could not run eg index --no-profiles");
    assert!(status.success(), "re-index without profiles must succeed");

    assert!(
        !has_profile_file(&dir),
        "a --no-profiles re-index must remove the previously stored profiles file"
    );
    assert!(
        !manifest_still_claims_profiles(&dir),
        "the manifest must not still advertise profile values after a --no-profiles re-index"
    );

    let _ = std::fs::remove_dir_all(&profiles_dir);
    let _ = std::fs::remove_dir_all(&dir);
}
