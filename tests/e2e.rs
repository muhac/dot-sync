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

/// Override the OS temp directory for a child process. On Unix, Rust's
/// std::env::temp_dir reads `TMPDIR`; on Windows, GetTempPath2 reads
/// `TMP`/`TEMP` and ignores `TMPDIR`. Set all three so tests work on both.
fn override_temp_dir(cmd: &mut Command, dir: &Path) {
    cmd.env("TMPDIR", dir).env("TMP", dir).env("TEMP", dir);
}

fn read_normalized(path: &Path) -> String {
    normalize_newlines(&read_file(path))
}

#[test]
fn sync_discovers_config_in_current_directory() {
    let fixture = Fixture::load("toml", "codex_basic_sync");

    fixture
        .command()
        .args(["sync", "codex"])
        .assert()
        .success()
        .stdout(predicate::str::contains("codex sync apply"))
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
fn pull_updates_listed_fields_and_preserves_unlisted_layout() {
    let fixture = Fixture::load("toml", "pull_preserves_layout");

    fixture
        .command()
        .args(["pull", "codex"])
        .assert()
        .success()
        .stdout(predicate::str::contains("changed source: tui.theme"))
        .stdout(predicate::str::contains(
            "changed source: project_doc_max_bytes",
        ));

    fixture.assert_file_eq("source.toml", "source.expected.toml");
}

#[test]
fn pull_keeps_listed_source_field_when_target_lacks_it() {
    let fixture = Fixture::load("toml", "pull_keeps_source_only_field");
    let original_source = fixture.read("source.toml");

    fixture
        .command()
        .args(["pull", "codex"])
        .assert()
        .success()
        .stdout(predicate::str::contains("No changes."))
        .stdout(predicate::str::contains("removed source").not());

    assert_eq!(fixture.read("source.toml"), original_source);
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
        .stdout(predicate::str::contains(
            "would change target: settings.theme",
        ))
        .stdout(predicate::str::contains("target: \"plain\""));

    dot_sync_in(dir.path())
        .args(["push", "codex"])
        .assert()
        .success()
        .stdout(predicate::str::contains(
            "warn: target path 'settings.theme' needs 'settings' to be a table",
        ))
        .stdout(predicate::str::contains("changed target: settings.theme"))
        .stdout(predicate::str::contains("target: \"plain\""));
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
        .stdout(predicate::str::contains("codex push apply"))
        .stdout(predicate::str::contains("tooling push apply"));

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

#[test]
fn write_emits_recovery_snapshot_for_existing_files() {
    let fixture = Fixture::load("toml", "dry_run_no_write");
    let snap = TempDir::new().unwrap();

    let mut cmd = fixture.command();
    override_temp_dir(&mut cmd, snap.path());
    let assert = cmd
        .args(["push", "codex"])
        .assert()
        .success()
        .stdout(predicate::str::contains("wrote target:"))
        .stdout(predicate::str::contains("recovery:"));

    let output = String::from_utf8(assert.get_output().stdout.clone()).unwrap();
    let snapshot_line = output
        .lines()
        .find(|line| line.contains("recovery:"))
        .unwrap();
    let snapshot_path = PathBuf::from(snapshot_line.split_once("recovery:").unwrap().1.trim());
    assert!(
        snapshot_path.starts_with(snap.path().join("dot-sync")),
        "snapshot {snapshot_path:?} not under {:?}",
        snap.path().join("dot-sync"),
    );
    assert!(snapshot_path.exists(), "snapshot file missing");
}

#[test]
fn dry_run_does_not_emit_recovery_snapshot() {
    let fixture = Fixture::load("toml", "dry_run_no_write");
    let snap = TempDir::new().unwrap();

    let mut cmd = fixture.command();
    override_temp_dir(&mut cmd, snap.path());
    cmd.args(["push", "codex", "--dry-run"])
        .assert()
        .success()
        .stdout(predicate::str::contains("recovery:").not())
        .stdout(predicate::str::contains("wrote ").not());

    assert!(!snap.path().join("dot-sync").exists());
}

#[test]
fn sync_source_wins_overwrites_target_value() {
    let fixture = Fixture::load("toml", "dry_run_no_write");

    fixture
        .command()
        .args(["sync", "codex", "--source-wins"])
        .assert()
        .success()
        .stdout(predicate::str::contains(
            "changed target: project_doc_max_bytes",
        ))
        .stdout(predicate::str::contains("source: 65536"))
        .stdout(predicate::str::contains("target: 1"));

    assert_eq!(
        read_normalized(&fixture.path().join("target.toml")),
        "project_doc_max_bytes = 65536\n"
    );
    assert_eq!(
        read_normalized(&fixture.path().join("source.toml")),
        "project_doc_max_bytes = 65536\n"
    );
}

#[test]
fn sync_fail_on_conflict_aborts() {
    let fixture = Fixture::load("toml", "dry_run_no_write");
    let original_target = fixture.read("target.toml");
    let original_source = fixture.read("source.toml");

    fixture
        .command()
        .args(["sync", "codex", "--fail-on-conflict"])
        .assert()
        .failure()
        .stdout(predicate::str::contains("conflict: project_doc_max_bytes"))
        .stderr(predicate::str::contains("fail-on-conflict"));

    assert_eq!(fixture.read("target.toml"), original_target);
    assert_eq!(fixture.read("source.toml"), original_source);
}

#[test]
fn sync_conflict_flags_are_mutually_exclusive() {
    let fixture = Fixture::load("toml", "dry_run_no_write");

    fixture
        .command()
        .args(["sync", "codex", "--target-wins", "--source-wins"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("cannot be used with"));
}

#[test]
fn sync_fail_on_conflict_preflights_all_targets_before_writing() {
    // Two targets: alpha would write cleanly, beta has a conflict. Without a
    // global preflight, alpha's writes happen before beta bails — violating
    // the "write nothing" promise.
    let dir = TempDir::new().unwrap();
    write_file(
        &dir.path().join(".sync.yaml"),
        r#"targets:
  alpha:
    format: toml
    source: alpha-source.toml
    target: alpha-target.toml
    sync:
      - field
  beta:
    format: toml
    source: beta-source.toml
    target: beta-target.toml
    sync:
      - field
"#,
    );
    write_file(&dir.path().join("alpha-source.toml"), "field = \"new\"\n");
    write_file(&dir.path().join("alpha-target.toml"), "");
    write_file(&dir.path().join("beta-source.toml"), "field = \"src\"\n");
    write_file(&dir.path().join("beta-target.toml"), "field = \"tgt\"\n");

    dot_sync_in(dir.path())
        .args(["sync", "--fail-on-conflict"])
        .assert()
        .failure()
        .stdout(predicate::str::contains("beta sync preflight"))
        .stdout(predicate::str::contains("conflict: field"))
        .stderr(predicate::str::contains("fail-on-conflict"));

    assert_eq!(
        read_normalized(&dir.path().join("alpha-target.toml")),
        "",
        "alpha-target must be untouched when beta has a conflict"
    );
    assert_eq!(
        read_normalized(&dir.path().join("beta-target.toml")),
        "field = \"tgt\"\n"
    );
}

#[test]
fn restore_lists_then_writes_newest_snapshot() {
    let fixture = Fixture::load("toml", "dry_run_no_write");
    let snap = TempDir::new().unwrap();

    // First push: produces a recovery snapshot of the original target (= "1").
    let mut push = fixture.command();
    override_temp_dir(&mut push, snap.path());
    push.args(["push", "codex"])
        .assert()
        .success()
        .stdout(predicate::str::contains("recovery:"));
    assert_eq!(
        read_normalized(&fixture.path().join("target.toml")),
        "project_doc_max_bytes = 65536\n"
    );

    // List should show the recovery candidate.
    let mut list = fixture.command();
    override_temp_dir(&mut list, snap.path());
    list.args(["restore", "codex", "--list"])
        .assert()
        .success()
        .stdout(predicate::str::contains("candidates (1):"))
        .stdout(predicate::str::contains("[recovery"))
        .stdout(predicate::str::contains(" 1  "));

    // Restore (no flag = newest = the original "1" content).
    let mut restore = fixture.command();
    override_temp_dir(&mut restore, snap.path());
    restore
        .args(["restore", "codex"])
        .assert()
        .success()
        .stdout(predicate::str::contains("selected:"))
        .stdout(predicate::str::contains("wrote target:"));

    assert_eq!(
        read_normalized(&fixture.path().join("target.toml")),
        "project_doc_max_bytes = 1\n"
    );
}

#[test]
fn restore_dry_run_does_not_write() {
    let fixture = Fixture::load("toml", "dry_run_no_write");
    let snap = TempDir::new().unwrap();

    let mut push = fixture.command();
    override_temp_dir(&mut push, snap.path());
    push.args(["push", "codex"]).assert().success();
    let after_push = read_normalized(&fixture.path().join("target.toml"));

    let mut restore = fixture.command();
    override_temp_dir(&mut restore, snap.path());
    restore
        .args(["restore", "codex", "--dry-run"])
        .assert()
        .success()
        .stdout(predicate::str::contains("dry run: no files written"));

    assert_eq!(
        read_normalized(&fixture.path().join("target.toml")),
        after_push
    );
}

#[test]
fn restore_with_no_snapshots_fails() {
    let fixture = Fixture::load("toml", "codex_basic_sync");
    let snap = TempDir::new().unwrap();

    let mut cmd = fixture.command();
    override_temp_dir(&mut cmd, snap.path());
    cmd.args(["restore", "codex"])
        .assert()
        .failure()
        .stdout(predicate::str::contains("No snapshots available"))
        .stderr(predicate::str::contains("no snapshots found"));
}

#[test]
fn restore_pick_out_of_range_fails() {
    let fixture = Fixture::load("toml", "dry_run_no_write");
    let snap = TempDir::new().unwrap();

    let mut push = fixture.command();
    override_temp_dir(&mut push, snap.path());
    push.args(["push", "codex"]).assert().success();

    let mut cmd = fixture.command();
    override_temp_dir(&mut cmd, snap.path());
    cmd.args(["restore", "codex", "--pick", "99"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("out of range"));
}

#[test]
fn push_pinned_array_selector_updates_one_item() {
    let fixture = Fixture::load("toml", "array_pinned_sync");

    fixture
        .command()
        .args(["push", "codex"])
        .assert()
        .success()
        .stdout(predicate::str::contains(
            "changed target: mcp_servers[name=\"github\"].enabled",
        ));

    fixture.assert_file_eq("target.toml", "target.expected.toml");
}

#[test]
fn push_wildcard_array_selector_pairs_items_by_identifier() {
    let fixture = Fixture::load("toml", "array_wildcard_sync");

    fixture
        .command()
        .args(["push", "codex"])
        .assert()
        .success()
        // linear: both have it, source's value wins → target changes.
        .stdout(predicate::str::contains(
            "changed target: mcp_servers[name=\"linear\"].enabled",
        ))
        // github: source-only, push fills target.
        .stdout(predicate::str::contains(
            "added target: mcp_servers[name=\"github\"].enabled",
        ));

    let after = fixture.read("target.toml");
    // linear flipped to source's value.
    assert!(after.contains("name = \"linear\""));
    assert!(after.contains("enabled = false"));
    // github appended.
    assert!(after.contains("name = \"github\""));
    // supabase preserved (push doesn't touch target-only items beyond the listed field).
    assert!(after.contains("name = \"supabase\""));
}

#[test]
fn pinned_selector_multi_match_errors_with_target_context() {
    let dir = TempDir::new().unwrap();
    write_file(
        &dir.path().join(".sync.yaml"),
        r#"targets:
  codex:
    format: toml
    source: source.toml
    target: target.toml
    sync:
      - mcp_servers[name="github"].enabled
"#,
    );
    write_file(
        &dir.path().join("source.toml"),
        r#"[[mcp_servers]]
name = "github"
enabled = true

[[mcp_servers]]
name = "github"
enabled = false
"#,
    );
    write_file(&dir.path().join("target.toml"), "");

    dot_sync_in(dir.path())
        .args(["push", "codex"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("failed to process target codex"))
        .stderr(predicate::str::contains(
            "source pattern 'mcp_servers[name=\"github\"].enabled'",
        ))
        .stderr(predicate::str::contains("ambiguous pinned"))
        .stderr(predicate::str::contains("2 items where name=\"github\""));

    // No write happened on either side.
    assert_eq!(read_file(&dir.path().join("target.toml")), "");
}

#[test]
fn wildcard_selector_duplicate_identifier_errors() {
    let dir = TempDir::new().unwrap();
    write_file(
        &dir.path().join(".sync.yaml"),
        r#"targets:
  codex:
    format: toml
    source: source.toml
    target: target.toml
    sync:
      - mcp_servers[name].enabled
"#,
    );
    write_file(
        &dir.path().join("source.toml"),
        r#"[[mcp_servers]]
name = "github"
enabled = true

[[mcp_servers]]
name = "github"
enabled = false
"#,
    );
    write_file(&dir.path().join("target.toml"), "");

    dot_sync_in(dir.path())
        .args(["push", "codex"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("ambiguous wildcard"))
        .stderr(predicate::str::contains("\"github\"×2"));
}

#[test]
fn config_rejects_unknown_keys() {
    let dir = TempDir::new().unwrap();
    write_file(
        &dir.path().join(".sync.yaml"),
        r#"targets:
  codex:
    format: toml
    source: source.toml
    taregt: target.toml
    sync:
      - tui.theme
"#,
    );
    write_file(&dir.path().join("source.toml"), "");
    write_file(&dir.path().join("target.toml"), "");

    dot_sync_in(dir.path())
        .args(["status"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("unknown field `taregt`"));
}

// =====================================================================
// JSON
// =====================================================================

#[test]
fn json_basic_sync_bootstraps_missing_target() {
    let fixture = Fixture::load("json", "basic_sync");
    fixture.command().args(["sync", "agent"]).assert().success();
    fixture.assert_file_eq("target.json", "target.expected.json");
}

#[test]
fn json_pinned_string_selector_push_updates_one_item() {
    let fixture = Fixture::load("json", "array_pinned_string");
    fixture
        .command()
        .args(["push", "agent"])
        .assert()
        .success()
        .stdout(predicate::str::contains(
            "changed target: mcpServers[name=\"github\"].enabled",
        ));
    fixture.assert_file_eq("target.json", "target.expected.json");
}

#[test]
fn json_pinned_int_selector_push() {
    let fixture = Fixture::load("json", "array_pinned_int");
    fixture
        .command()
        .args(["push", "agent"])
        .assert()
        .success()
        .stdout(predicate::str::contains(
            "changed target: servers[port=8080].host",
        ));
    fixture.assert_file_eq("target.json", "target.expected.json");
}

#[test]
fn json_pinned_bool_selector_push() {
    let fixture = Fixture::load("json", "array_pinned_bool");
    fixture
        .command()
        .args(["push", "agent"])
        .assert()
        .success()
        .stdout(predicate::str::contains(
            "changed target: entries[primary=true].host",
        ));
    fixture.assert_file_eq("target.json", "target.expected.json");
}

#[test]
fn json_wildcard_selector_pairs_items_by_identifier() {
    let fixture = Fixture::load("json", "array_wildcard");
    fixture
        .command()
        .args(["push", "agent"])
        .assert()
        .success()
        // linear: in both, push source → target.
        .stdout(predicate::str::contains(
            "changed target: mcpServers[name=\"linear\"].enabled",
        ))
        // github: source-only, push fills target.
        .stdout(predicate::str::contains(
            "added target: mcpServers[name=\"github\"].enabled",
        ));

    let after = fixture.read("target.json");
    // linear flipped to source's value (false).
    assert!(after.contains("\"linear\""));
    // github appended.
    assert!(after.contains("\"github\""));
    // supabase preserved (push doesn't touch target-only items beyond the listed field).
    assert!(after.contains("\"supabase\""));
    assert!(after.contains("\"private\""));
    assert!(after.contains("\"keep\""));
}

#[test]
fn json_push_propagates_explicit_null_from_source() {
    // Source has `feature.value = null` (explicit). Pushing must produce
    // target with the same explicit null — Some(Value::Null) is a value
    // that gets written, not collapsed to "missing".
    let fixture = Fixture::load("json", "null_vs_missing");
    fixture
        .command()
        .args(["push", "agent"])
        .assert()
        .success()
        .stdout(predicate::str::contains("added target: feature.value"));
    fixture.assert_file_eq("target.json", "target.expected.json");
}

#[test]
fn json_sync_source_wins_overrides_non_null_target_with_explicit_null() {
    // Target has `feature.value = "non-null"`, source has `feature.value = null`.
    // `sync --source-wins` must overwrite the target's value with explicit
    // null — the engine sees both sides as `Some(_)` and follows the
    // conflict policy, treating Null as a real value (not as "missing").
    let fixture = Fixture::load("json", "null_overrides_value");
    fixture
        .command()
        .args(["sync", "agent", "--source-wins"])
        .assert()
        .success()
        .stdout(predicate::str::contains("changed target: feature.value"));
    fixture.assert_file_eq("target.json", "target.expected.json");
}

#[test]
fn json_push_preserves_unmanaged_target_fields() {
    let fixture = Fixture::load("json", "preserves_unmanaged");
    fixture.command().args(["push", "agent"]).assert().success();
    fixture.assert_file_eq("target.json", "target.expected.json");
}

#[test]
fn jsonc_push_preserves_line_comments_around_modified_value() {
    // Both line comments (top-of-file and same-line) and the surrounding
    // structure must round-trip while only `feature.enabled` flips.
    let fixture = Fixture::load("json", "jsonc_comments_preserved");
    fixture
        .command()
        .args(["push", "agent"])
        .assert()
        .success()
        .stdout(predicate::str::contains("changed target: feature.enabled"));
    fixture.assert_file_eq("target.jsonc", "target.expected.jsonc");
}

#[test]
fn jsonc_push_preserves_block_and_inline_comments() {
    // Multi-line block comment at the top + inline `/* */` after a value.
    let fixture = Fixture::load("json", "jsonc_block_comment_preserved");
    fixture
        .command()
        .args(["push", "agent"])
        .assert()
        .success()
        .stdout(predicate::str::contains("changed target: tui.theme"));
    fixture.assert_file_eq("target.jsonc", "target.expected.jsonc");
}

#[test]
fn jsonc_pull_preserves_source_side_comments() {
    // Pull writes the *source* file (target → source). Source-side
    // comments and block-comment trivia must survive the write — same
    // CST round-trip guarantee as push, just on the other side.
    let fixture = Fixture::load("json", "jsonc_pull_preserves_source_comments");
    fixture
        .command()
        .args(["pull", "agent"])
        .assert()
        .success()
        .stdout(predicate::str::contains("changed source: feature.enabled"));
    fixture.assert_file_eq("source.jsonc", "source.expected.jsonc");
}

#[test]
fn jsonc_new_key_appended_does_not_strand_neighbor_comment() {
    // Target has `{"a": 1 // keep existing trailing comment\n}`. Source
    // adds key `b`. Locks the current jsonc-parser placement: comment
    // stays attached to `a`, new key is appended without disturbing it.
    let fixture = Fixture::load("json", "jsonc_new_key_with_neighbor_comment");
    fixture
        .command()
        .args(["push", "agent"])
        .assert()
        .success()
        .stdout(predicate::str::contains("added target: b"));
    let after = fixture.read("target.jsonc");
    assert!(
        after.contains("// keep existing trailing comment"),
        "comment must survive: {after}"
    );
    assert!(after.contains("\"a\": 1"), "a:1 must survive: {after}");
    assert!(after.contains("\"b\": 2"), "b:2 must be added: {after}");
    // Comment is on the same line as `a: 1`, before any new key.
    let comment_pos = after.find("// keep").unwrap();
    let b_pos = after.find("\"b\"").unwrap();
    assert!(
        comment_pos < b_pos,
        "comment must precede the appended key in source order: {after}"
    );
}

#[test]
fn jsonc_vscode_settings_round_trips_real_world_shape() {
    // Representative VS Code settings.jsonc — top-of-section line
    // comments, `// inline comment, blank lines between sections,
    // trailing commas inside nested objects, multi-line block comments.
    // Sync only `editor.tabSize`. Everything else (sibling keys, all
    // comments, blank lines, trailing commas) must round-trip unchanged.
    let fixture = Fixture::load("json", "jsonc_vscode_settings");
    fixture
        .command()
        .args(["push", "vscode"])
        .assert()
        .success()
        .stdout(predicate::str::contains("changed target: editor.tabSize"));
    fixture.assert_file_eq("settings.jsonc", "settings.expected.jsonc");
}

#[test]
fn json_dry_run_reports_changes_without_writing_files() {
    let fixture = Fixture::load("json", "dry_run_no_write");
    let original_target = fixture.read("target.json");

    fixture
        .command()
        .args(["push", "agent", "--dry-run"])
        .assert()
        .success()
        .stdout(predicate::str::contains("dry run: no files written"))
        .stdout(predicate::str::contains("would change target: max_bytes"))
        .stdout(predicate::str::contains("source: 65536"))
        .stdout(predicate::str::contains("target: 1"));

    // Target file is exactly as-was: dry-run never writes.
    assert_eq!(fixture.read("target.json"), original_target);
}

#[test]
fn json_dry_run_reports_added_fields() {
    let fixture = Fixture::load("json", "preserves_unmanaged");
    fixture
        .command()
        .args(["push", "agent", "--dry-run"])
        .assert()
        .success()
        .stdout(predicate::str::contains("would change target: tui.theme"))
        .stdout(predicate::str::contains("source: \"monokai\""));
}

#[test]
fn json_dry_run_does_not_emit_recovery_snapshot() {
    let fixture = Fixture::load("json", "dry_run_no_write");
    let snap = TempDir::new().unwrap();

    let mut cmd = fixture.command();
    override_temp_dir(&mut cmd, snap.path());
    cmd.args(["push", "agent", "--dry-run"])
        .assert()
        .success()
        .stdout(predicate::str::contains("recovery:").not())
        .stdout(predicate::str::contains("wrote ").not());

    assert!(!snap.path().join("dot-sync").exists());
}

#[test]
fn json_push_missing_source_has_actionable_error() {
    let dir = TempDir::new().unwrap();
    write_file(
        &dir.path().join(".sync.yaml"),
        r#"targets:
  agent:
    format: json
    source: missing.json
    target: target.json
    sync:
      - max_bytes
"#,
    );
    write_file(&dir.path().join("target.json"), "{}");

    dot_sync_in(dir.path())
        .args(["push", "agent", "--dry-run"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("source file does not exist"))
        .stderr(predicate::str::contains(
            "run pull/sync if you want to bootstrap it",
        ));
}

#[test]
fn json_pull_missing_target_has_actionable_error() {
    let dir = TempDir::new().unwrap();
    write_file(
        &dir.path().join(".sync.yaml"),
        r#"targets:
  agent:
    format: json
    source: source.json
    target: missing.json
    sync:
      - max_bytes
"#,
    );
    write_file(&dir.path().join("source.json"), "{}");

    dot_sync_in(dir.path())
        .args(["pull", "agent", "--dry-run"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("target file does not exist"))
        .stderr(predicate::str::contains(
            "run push/sync if you want to bootstrap it",
        ));
}

#[test]
fn json_write_emits_recovery_snapshot_for_existing_files() {
    let fixture = Fixture::load("json", "dry_run_no_write");
    let snap = TempDir::new().unwrap();

    let mut cmd = fixture.command();
    override_temp_dir(&mut cmd, snap.path());
    let assert = cmd
        .args(["push", "agent"])
        .assert()
        .success()
        .stdout(predicate::str::contains("wrote target:"))
        .stdout(predicate::str::contains("recovery:"));

    let output = String::from_utf8(assert.get_output().stdout.clone()).unwrap();
    let snapshot_line = output
        .lines()
        .find(|line| line.contains("recovery:"))
        .unwrap();
    let snapshot_path = PathBuf::from(snapshot_line.split_once("recovery:").unwrap().1.trim());
    assert!(
        snapshot_path.starts_with(snap.path().join("dot-sync")),
        "snapshot {snapshot_path:?} not under {:?}",
        snap.path().join("dot-sync"),
    );
    assert!(snapshot_path.exists(), "snapshot file missing");
}

#[test]
fn json_sync_source_wins_overwrites_target_value() {
    let fixture = Fixture::load("json", "dry_run_no_write");

    fixture
        .command()
        .args(["sync", "agent", "--source-wins"])
        .assert()
        .success()
        .stdout(predicate::str::contains("changed target: max_bytes"))
        .stdout(predicate::str::contains("source: 65536"))
        .stdout(predicate::str::contains("target: 1"));

    let target = fixture.read("target.json");
    assert!(
        target.contains("65536"),
        "target should now hold source's value: {target}"
    );
}

#[test]
fn json_sync_fail_on_conflict_aborts() {
    let fixture = Fixture::load("json", "dry_run_no_write");
    let original_target = fixture.read("target.json");
    let original_source = fixture.read("source.json");

    fixture
        .command()
        .args(["sync", "agent", "--fail-on-conflict"])
        .assert()
        .failure()
        .stdout(predicate::str::contains("conflict: max_bytes"))
        .stderr(predicate::str::contains("fail-on-conflict"));

    // Both files untouched — preflight aborts before any write.
    assert_eq!(fixture.read("target.json"), original_target);
    assert_eq!(fixture.read("source.json"), original_source);
}

#[test]
fn json_sync_fail_on_conflict_preflights_all_targets_before_writing() {
    // Two targets: alpha would write cleanly, beta has a conflict. The
    // global preflight must abort beta and prevent alpha's write — same
    // "write nothing" guarantee as the TOML side.
    let dir = TempDir::new().unwrap();
    write_file(
        &dir.path().join(".sync.yaml"),
        r#"targets:
  alpha:
    format: json
    source: alpha-source.json
    target: alpha-target.json
    sync:
      - field
  beta:
    format: json
    source: beta-source.json
    target: beta-target.json
    sync:
      - field
"#,
    );
    write_file(&dir.path().join("alpha-source.json"), r#"{"field": "new"}"#);
    write_file(&dir.path().join("alpha-target.json"), "{}");
    write_file(&dir.path().join("beta-source.json"), r#"{"field": "src"}"#);
    write_file(&dir.path().join("beta-target.json"), r#"{"field": "tgt"}"#);

    dot_sync_in(dir.path())
        .args(["sync", "--fail-on-conflict"])
        .assert()
        .failure()
        .stdout(predicate::str::contains("beta sync preflight"))
        .stdout(predicate::str::contains("conflict: field"))
        .stderr(predicate::str::contains("fail-on-conflict"));

    // alpha must be untouched even though only beta had the conflict.
    assert_eq!(read_normalized(&dir.path().join("alpha-target.json")), "{}");
    assert_eq!(
        read_normalized(&dir.path().join("beta-target.json")),
        r#"{"field": "tgt"}"#
    );
}

#[test]
fn jsonc_format_alias_dispatches_to_json_backend() {
    // `format: jsonc` is accepted as an alias for `format: json` so a
    // user with VS Code / tsconfig files can self-document the fact that
    // the file contains comments / trailing commas.
    let fixture = Fixture::load("json", "jsonc_format_alias");
    fixture
        .command()
        .args(["push", "agent"])
        .assert()
        .success()
        .stdout(predicate::str::contains("changed target: feature.enabled"));
    fixture.assert_file_eq("target.jsonc", "target.expected.jsonc");
}

#[test]
fn jsonc_push_preserves_trailing_commas() {
    // Source and target both use trailing commas inside the array. After
    // a pinned-selector update the trailing-comma style must follow the
    // existing source-side policy (`uses_trailing_commas()` infers it).
    let fixture = Fixture::load("json", "jsonc_trailing_comma_preserved");
    fixture
        .command()
        .args(["push", "agent"])
        .assert()
        .success()
        .stdout(predicate::str::contains(
            "changed target: tools[name=\"parse\"].enabled",
        ));
    fixture.assert_file_eq("target.jsonc", "target.expected.jsonc");
}
