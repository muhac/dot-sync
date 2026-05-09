use std::fs;
use std::path::{Path, PathBuf};

use assert_cmd::Command;
use assert_fs::TempDir;
use predicates::prelude::*;

struct Fixture {
    dir: TempDir,
}

impl Fixture {
    fn load(format: &str, case: &str) -> Self {
        let dir = TempDir::new().unwrap();
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("tests")
            .join("fixtures")
            .join(format)
            .join(case);
        copy_fixture_dir(&root, dir.path());
        Self { dir }
    }

    fn path(&self) -> &Path {
        self.dir.path()
    }

    fn read(&self, path: &str) -> String {
        read_file(&self.path().join(path))
    }

    fn command(&self) -> Command {
        dot_sync_in(self.path())
    }

    fn command_in(&self, relative_cwd: &str) -> Command {
        dot_sync_in(self.path().join(relative_cwd))
    }

    fn assert_file_eq(&self, actual: &str, expected: &str) {
        assert_eq!(
            normalize_newlines(&self.read(actual)),
            normalize_newlines(&self.read(expected)),
            "{actual} did not match {expected}",
        );
    }
}

fn copy_fixture_dir(source: &Path, destination: &Path) {
    for entry in fs::read_dir(source).unwrap() {
        let entry = entry.unwrap();
        let source_path = entry.path();
        let destination_path = destination.join(entry.file_name());
        if source_path.is_dir() {
            fs::create_dir_all(&destination_path).unwrap();
            copy_fixture_dir(&source_path, &destination_path);
        } else {
            write_file(&destination_path, &read_file(&source_path));
        }
    }
}

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

fn dot_sync_in(cwd: impl Into<PathBuf>) -> Command {
    let mut command = Command::cargo_bin("dot-sync").unwrap();
    command.current_dir(cwd.into());
    command
}

#[test]
fn sync_discovers_config_in_current_directory() {
    let fixture = Fixture::load("toml", "codex_basic_sync");

    fixture
        .command()
        .args(["sync", "codex"])
        .assert()
        .success()
        .stdout(predicate::str::contains("codex Sync apply"))
        .stdout(predicate::str::contains("added target: tui.theme"))
        .stdout(predicate::str::contains(
            "added target: project_doc_max_bytes",
        ));

    fixture.assert_file_eq("target.toml", "target.expected.toml");
}

#[test]
fn sync_discovers_config_from_parent_directory() {
    let fixture = Fixture::load("toml", "parent_discovery");
    fs::create_dir_all(fixture.path().join("nested/worktree")).unwrap();

    fixture
        .command_in("nested/worktree")
        .args(["sync", "codex"])
        .assert()
        .success()
        .stdout(predicate::str::contains(
            "added target: project_doc_max_bytes",
        ));

    fixture.assert_file_eq("target.toml", "target.expected.toml");
}

#[test]
fn push_preserves_unmanaged_target_fields() {
    let fixture = Fixture::load("toml", "preserve_unmanaged_target");

    fixture
        .command()
        .args(["push", "codex"])
        .assert()
        .success()
        .stdout(predicate::str::contains(
            "added target: project_doc_max_bytes",
        ))
        .stdout(predicate::str::contains("added target: tui.theme"));

    fixture.assert_file_eq("target.toml", "target.expected.toml");
}

#[test]
fn pull_rewrites_source_in_sync_order() {
    let fixture = Fixture::load("toml", "pull_canonical_order");

    fixture
        .command()
        .args(["pull", "codex"])
        .assert()
        .success()
        .stdout(predicate::str::contains("changed source: tui.theme"))
        .stdout(predicate::str::contains(
            "added source: project_doc_max_bytes",
        ));

    fixture.assert_file_eq("source.toml", "source.expected.toml");
}

#[test]
fn pull_reports_removed_source_fields_missing_from_target() {
    let fixture = Fixture::load("toml", "pull_removes_missing_target_field");

    fixture
        .command()
        .args(["pull", "codex"])
        .assert()
        .success()
        .stdout(predicate::str::contains(
            "removed source: project_doc_fallback_filenames",
        ));

    fixture.assert_file_eq("source.toml", "source.expected.toml");
}

#[test]
fn sync_uses_target_values_and_fills_missing_target_fields() {
    let fixture = Fixture::load("toml", "sync_target_wins_and_fills");

    fixture
        .command()
        .args(["sync", "codex"])
        .assert()
        .success()
        .stdout(predicate::str::contains("changed source: tui.theme"))
        .stdout(predicate::str::contains(
            "added target: project_doc_fallback_filenames",
        ));

    fixture.assert_file_eq("source.toml", "source.expected.toml");
    fixture.assert_file_eq("target.toml", "target.expected.toml");
}

#[test]
fn sync_bootstraps_missing_target_file() {
    let fixture = Fixture::load("toml", "missing_target_bootstrap");

    fixture
        .command()
        .args(["sync", "codex"])
        .assert()
        .success()
        .stdout(predicate::str::contains(
            "added target: project_doc_max_bytes",
        ))
        .stdout(predicate::str::contains(
            "added target: project_doc_fallback_filenames",
        ));

    fixture.assert_file_eq("target.toml", "target.expected.toml");
}

#[test]
fn push_preserves_inline_table_fields() {
    let fixture = Fixture::load("toml", "inline_table_preservation");

    fixture
        .command()
        .args(["push", "codex"])
        .assert()
        .success()
        .stdout(predicate::str::contains("changed target: settings.theme"));

    fixture.assert_file_eq("target.toml", "target.expected.toml");
}

#[test]
fn push_handles_quoted_path_segments() {
    let fixture = Fixture::load("toml", "quoted_path_plugin");

    fixture
        .command()
        .args(["push", "codex"])
        .assert()
        .success()
        .stdout(predicate::str::contains(
            "added target: plugins.\"github@openai-curated\".enabled",
        ));

    fixture.assert_file_eq("target.toml", "target.expected.toml");
}

#[test]
fn dry_run_reports_changes_without_writing_files() {
    let fixture = Fixture::load("toml", "dry_run_no_write");

    fixture
        .command()
        .args(["push", "codex", "--dry-run"])
        .assert()
        .success()
        .stdout(predicate::str::contains("dry run: no files written"))
        .stdout(predicate::str::contains(
            "would change target: project_doc_max_bytes",
        ))
        .stdout(predicate::str::contains("source: 65536"))
        .stdout(predicate::str::contains("target: 1"));

    fixture.assert_file_eq("target.toml", "target.expected.toml");
}

#[test]
fn dry_run_reports_added_fields() {
    let fixture = Fixture::load("toml", "preserve_unmanaged_target");

    fixture
        .command()
        .args(["push", "codex", "--dry-run"])
        .assert()
        .success()
        .stdout(predicate::str::contains(
            "would add target: project_doc_max_bytes",
        ))
        .stdout(predicate::str::contains("target: <missing>"));
}

#[test]
fn dry_run_reports_removed_fields() {
    let fixture = Fixture::load("toml", "pull_removes_missing_target_field");
    let original_source = fixture.read("source.toml");

    fixture
        .command()
        .args(["pull", "codex", "--dry-run"])
        .assert()
        .success()
        .stdout(predicate::str::contains(
            "would remove source: project_doc_fallback_filenames",
        ))
        .stdout(predicate::str::contains("source: [\"AGENTS.md\"]"))
        .stdout(predicate::str::contains("target: <missing>"));

    assert_eq!(fixture.read("source.toml"), original_source);
}

#[test]
fn status_lists_configured_targets() {
    let fixture = Fixture::load("toml", "multiple_targets");

    fixture
        .command()
        .args(["status"])
        .assert()
        .success()
        .stdout(predicate::str::contains("Config:"))
        .stdout(predicate::str::contains("codex ok toml"))
        .stdout(predicate::str::contains("tooling ok toml"))
        .stdout(predicate::str::contains("fields=1"));
}

#[test]
fn status_can_select_one_target() {
    let fixture = Fixture::load("toml", "multiple_targets");

    fixture
        .command()
        .args(["status", "codex"])
        .assert()
        .success()
        .stdout(predicate::str::contains("codex ok toml"))
        .stdout(predicate::str::contains("tooling").not());
}

#[test]
fn status_warns_about_missing_files_without_failing() {
    let fixture = Fixture::load("toml", "missing_target_bootstrap");

    fixture
        .command()
        .args(["status", "codex"])
        .assert()
        .success()
        .stdout(predicate::str::contains("codex warn toml"))
        .stdout(predicate::str::contains("warn: target file does not exist"));
}

#[test]
fn status_reports_invalid_sync_path() {
    let dir = TempDir::new().unwrap();
    write_file(
        &dir.path().join(".sync.yaml"),
        r#"targets:
  codex:
    format: toml
    source: source.toml
    target: target.toml
    sync:
      - tui..theme
"#,
    );
    write_file(&dir.path().join("source.toml"), "");
    write_file(&dir.path().join("target.toml"), "");

    dot_sync_in(dir.path())
        .args(["status"])
        .assert()
        .failure()
        .stdout(predicate::str::contains(
            "invalid sync path 'tui..theme' in target 'codex'",
        ));
}

#[test]
fn status_reports_unsupported_format() {
    let dir = TempDir::new().unwrap();
    write_file(
        &dir.path().join(".sync.yaml"),
        r#"targets:
  codex:
    format: yaml
    source: source.yaml
    target: target.yaml
    sync:
      - tui.theme
"#,
    );
    write_file(&dir.path().join("source.yaml"), "");
    write_file(&dir.path().join("target.yaml"), "");

    dot_sync_in(dir.path())
        .args(["status"])
        .assert()
        .failure()
        .stdout(predicate::str::contains(
            "target 'codex' uses format 'yaml'",
        ))
        .stdout(predicate::str::contains("supported formats: toml"));
}

#[test]
fn status_and_dry_run_warn_about_table_value_conflicts() {
    let dir = TempDir::new().unwrap();
    write_file(
        &dir.path().join(".sync.yaml"),
        r#"targets:
  codex:
    format: toml
    source: source.toml
    target: target.toml
    sync:
      - settings.theme
"#,
    );
    write_file(
        &dir.path().join("source.toml"),
        "[settings]\ntheme = \"dark\"\n",
    );
    write_file(&dir.path().join("target.toml"), "settings = \"plain\"\n");

    dot_sync_in(dir.path())
        .args(["status"])
        .assert()
        .success()
        .stdout(predicate::str::contains("codex warn toml"))
        .stdout(predicate::str::contains(
            "target path 'settings.theme' needs 'settings' to be a table",
        ));

    dot_sync_in(dir.path())
        .args(["push", "codex", "--dry-run"])
        .assert()
        .success()
        .stdout(predicate::str::contains(
            "warn: target path 'settings.theme' needs 'settings' to be a table",
        ))
        .stdout(predicate::str::contains("would add target: settings.theme"));
}

#[test]
fn push_missing_source_has_actionable_error() {
    let dir = TempDir::new().unwrap();
    write_file(
        &dir.path().join(".sync.yaml"),
        r#"targets:
  codex:
    format: toml
    source: missing.toml
    target: target.toml
    sync:
      - project_doc_max_bytes
"#,
    );
    write_file(&dir.path().join("target.toml"), "");

    dot_sync_in(dir.path())
        .args(["push", "codex", "--dry-run"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("source file does not exist"))
        .stderr(predicate::str::contains(
            "run pull/sync if you want to bootstrap it",
        ));
}

#[test]
fn pull_missing_target_has_actionable_error() {
    let dir = TempDir::new().unwrap();
    write_file(
        &dir.path().join(".sync.yaml"),
        r#"targets:
  codex:
    format: toml
    source: source.toml
    target: missing.toml
    sync:
      - project_doc_max_bytes
"#,
    );
    write_file(&dir.path().join("source.toml"), "");

    dot_sync_in(dir.path())
        .args(["pull", "codex", "--dry-run"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("target file does not exist"))
        .stderr(predicate::str::contains(
            "run push/sync if you want to bootstrap it",
        ));
}

#[test]
fn backup_creates_timestamped_copy_before_writing() {
    let fixture = Fixture::load("toml", "backup_write");
    let original_target = fixture.read("target.toml");

    fixture
        .command()
        .args(["push", "codex", "--backup"])
        .assert()
        .success()
        .stdout(predicate::str::contains(
            "changed target: project_doc_max_bytes",
        ));

    fixture.assert_file_eq("target.toml", "target.expected.toml");
    let backups = fs::read_dir(fixture.path())
        .unwrap()
        .filter_map(Result::ok)
        .filter(|entry| entry.file_name().to_string_lossy().contains(".bak."))
        .collect::<Vec<_>>();
    assert_eq!(backups.len(), 1);
    assert_eq!(read_file(&backups[0].path()), original_target);
}

#[test]
fn multiple_targets_process_all_when_name_is_omitted() {
    let fixture = Fixture::load("toml", "multiple_targets");

    fixture
        .command()
        .args(["push"])
        .assert()
        .success()
        .stdout(predicate::str::contains("codex Push apply"))
        .stdout(predicate::str::contains("tooling Push apply"));

    fixture.assert_file_eq("codex-target.toml", "codex-target.expected.toml");
    fixture.assert_file_eq("tooling-target.toml", "tooling-target.expected.toml");
}

#[test]
fn unknown_target_exits_with_error() {
    let fixture = Fixture::load("toml", "codex_basic_sync");

    fixture
        .command()
        .args(["push", "missing"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("unknown target: missing"));
}

#[test]
fn malformed_config_exits_with_parse_error() {
    let fixture = Fixture::load("toml", "malformed_config");

    fixture
        .command()
        .args(["sync"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("failed to parse"));
}
