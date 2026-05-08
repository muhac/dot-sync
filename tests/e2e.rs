use std::fs;
use std::path::{Path, PathBuf};

use assert_cmd::Command;
use assert_fs::TempDir;
use predicates::prelude::*;

fn write_file(path: &Path, content: &str) {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).unwrap();
    }
    fs::write(path, content).unwrap();
}

fn read_file(path: &Path) -> String {
    fs::read_to_string(path).unwrap()
}

fn normalize_newlines(content: &str) -> String {
    content.replace("\r\n", "\n")
}

fn write_config(dir: &TempDir, sync_paths: &[&str]) {
    let sync = sync_paths
        .iter()
        .map(|path| format!("      - {path}"))
        .collect::<Vec<_>>()
        .join("\n");
    write_file(
        &dir.path().join(".sync.yaml"),
        &format!(
            r#"targets:
  codex:
    format: toml
    source: source.toml
    target: target.toml
    sync:
{sync}
"#
        ),
    );
}

fn dot_sync_in(cwd: impl Into<PathBuf>) -> Command {
    let mut command = Command::cargo_bin("dot-sync").unwrap();
    command.current_dir(cwd.into());
    command
}

#[test]
fn sync_discovers_config_in_current_directory() {
    let dir = TempDir::new().unwrap();
    write_file(
        &dir.path().join(".sync.yaml"),
        include_str!("fixtures/codex_basic/.sync.yaml"),
    );
    write_file(
        &dir.path().join("source.toml"),
        include_str!("fixtures/codex_basic/source.toml"),
    );

    dot_sync_in(dir.path())
        .args(["sync", "codex"])
        .assert()
        .success()
        .stdout(predicate::str::contains("codex Sync apply"))
        .stdout(predicate::str::contains(
            "Update target: project_doc_max_bytes",
        ));

    assert_eq!(
        read_file(&dir.path().join("target.toml")),
        normalize_newlines(include_str!("fixtures/codex_basic/target.expected.toml"))
    );
}

#[test]
fn sync_discovers_config_from_parent_directory() {
    let dir = TempDir::new().unwrap();
    let nested = dir.path().join("nested/worktree");
    fs::create_dir_all(&nested).unwrap();
    write_config(&dir, &["project_doc_max_bytes"]);
    write_file(
        &dir.path().join("source.toml"),
        "project_doc_max_bytes = 65536\n",
    );

    dot_sync_in(nested)
        .args(["sync", "codex"])
        .assert()
        .success()
        .stdout(predicate::str::contains(
            "Update target: project_doc_max_bytes",
        ));

    assert_eq!(
        read_file(&dir.path().join("target.toml")),
        "project_doc_max_bytes = 65536\n"
    );
}

#[test]
fn push_preserves_unmanaged_target_fields() {
    let dir = TempDir::new().unwrap();
    write_config(&dir, &["project_doc_max_bytes"]);
    write_file(
        &dir.path().join("source.toml"),
        "project_doc_max_bytes = 65536\n",
    );
    write_file(
        &dir.path().join("target.toml"),
        r#"[projects."/secret"]
trust_level = "trusted"
"#,
    );

    dot_sync_in(dir.path())
        .args(["push", "codex"])
        .assert()
        .success()
        .stdout(predicate::str::contains(
            "Update target: project_doc_max_bytes",
        ));

    let target = read_file(&dir.path().join("target.toml"));
    assert!(target.contains("project_doc_max_bytes = 65536"));
    assert!(target.contains(r#"[projects."/secret"]"#));
    assert!(target.contains(r#"trust_level = "trusted""#));
}

#[test]
fn pull_rewrites_source_in_sync_order() {
    let dir = TempDir::new().unwrap();
    write_config(&dir, &["tui.theme", "notice.hide_rate_limit_model_nudge"]);
    write_file(
        &dir.path().join("source.toml"),
        r#"[notice]
hide_rate_limit_model_nudge = true

[tui]
theme = "old"
"#,
    );
    write_file(
        &dir.path().join("target.toml"),
        r#"[notice]
hide_rate_limit_model_nudge = true

[tui]
theme = "monokai"
"#,
    );

    dot_sync_in(dir.path())
        .args(["pull", "codex"])
        .assert()
        .success()
        .stdout(predicate::str::contains("Update source: tui.theme"));

    let source = read_file(&dir.path().join("source.toml"));
    let tui_index = source.find("[tui]").unwrap();
    let notice_index = source.find("[notice]").unwrap();
    assert!(tui_index < notice_index);
    assert!(source.contains(r#"theme = "monokai""#));
}

#[test]
fn sync_bootstraps_missing_target_file() {
    let dir = TempDir::new().unwrap();
    write_config(&dir, &["project_doc_max_bytes"]);
    write_file(
        &dir.path().join("source.toml"),
        "project_doc_max_bytes = 65536\n",
    );

    dot_sync_in(dir.path())
        .args(["sync", "codex"])
        .assert()
        .success()
        .stdout(predicate::str::contains(
            "Update target: project_doc_max_bytes",
        ));

    assert_eq!(
        read_file(&dir.path().join("target.toml")),
        "project_doc_max_bytes = 65536\n"
    );
}

#[test]
fn push_preserves_inline_table_fields() {
    let dir = TempDir::new().unwrap();
    write_config(&dir, &["settings.theme"]);
    write_file(
        &dir.path().join("source.toml"),
        r#"[settings]
theme = "new"
"#,
    );
    write_file(
        &dir.path().join("target.toml"),
        r#"settings = { theme = "old", local = "keep" }
"#,
    );

    dot_sync_in(dir.path())
        .args(["push", "codex"])
        .assert()
        .success();

    let target = read_file(&dir.path().join("target.toml"));
    assert!(target.contains(r#"theme = "new""#));
    assert!(target.contains(r#"local = "keep""#));
}

#[test]
fn push_handles_quoted_path_segments() {
    let dir = TempDir::new().unwrap();
    write_config(&dir, &[r#"plugins."github@openai-curated".enabled"#]);
    write_file(
        &dir.path().join("source.toml"),
        r#"[plugins."github@openai-curated"]
enabled = true
"#,
    );
    write_file(&dir.path().join("target.toml"), "");

    dot_sync_in(dir.path())
        .args(["push", "codex"])
        .assert()
        .success();

    let target = read_file(&dir.path().join("target.toml"));
    assert!(target.contains(r#"[plugins."github@openai-curated"]"#));
    assert!(target.contains("enabled = true"));
}

#[test]
fn dry_run_reports_changes_without_writing_files() {
    let dir = TempDir::new().unwrap();
    write_config(&dir, &["project_doc_max_bytes"]);
    write_file(
        &dir.path().join("source.toml"),
        "project_doc_max_bytes = 65536\n",
    );
    write_file(&dir.path().join("target.toml"), "");

    dot_sync_in(dir.path())
        .args(["push", "codex", "--dry-run"])
        .assert()
        .success()
        .stdout(predicate::str::contains(
            "Would update target: project_doc_max_bytes",
        ));

    assert_eq!(read_file(&dir.path().join("target.toml")), "");
}

#[test]
fn backup_creates_timestamped_copy_before_writing() {
    let dir = TempDir::new().unwrap();
    write_config(&dir, &["project_doc_max_bytes"]);
    write_file(
        &dir.path().join("source.toml"),
        "project_doc_max_bytes = 65536\n",
    );
    write_file(&dir.path().join("target.toml"), "old = true\n");

    dot_sync_in(dir.path())
        .args(["push", "codex", "--backup"])
        .assert()
        .success();

    let backup_count = fs::read_dir(dir.path())
        .unwrap()
        .filter_map(Result::ok)
        .filter(|entry| entry.file_name().to_string_lossy().contains(".bak."))
        .count();
    assert_eq!(backup_count, 1);
    assert!(read_file(&dir.path().join("target.toml")).contains("project_doc_max_bytes = 65536"));
}

#[test]
fn unknown_target_exits_with_error() {
    let dir = TempDir::new().unwrap();
    write_config(&dir, &["project_doc_max_bytes"]);
    write_file(
        &dir.path().join("source.toml"),
        "project_doc_max_bytes = 65536\n",
    );

    dot_sync_in(dir.path())
        .args(["push", "missing"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("unknown target: missing"));
}

#[test]
fn malformed_config_exits_with_parse_error() {
    let dir = TempDir::new().unwrap();
    write_file(&dir.path().join(".sync.yaml"), "targets: [");

    dot_sync_in(dir.path())
        .args(["sync"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("failed to parse"));
}
