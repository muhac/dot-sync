use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow, bail};
use chrono::Local;

use crate::config::{DotSyncConfig, TargetConfig};
use crate::document::{AnyDocument, Document};
use crate::path::FieldPath;

#[derive(Debug, Clone, Copy)]
pub enum Direction {
    Pull,
    Push,
    Sync,
}

impl fmt::Display for Direction {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let name = match self {
            Self::Pull => "pull",
            Self::Push => "push",
            Self::Sync => "sync",
        };
        f.write_str(name)
    }
}

#[derive(Debug, Clone, Copy)]
pub struct SyncOptions {
    pub dry_run: bool,
    pub backup: bool,
}

#[derive(Debug)]
struct Change {
    path: String,
    destination: Destination,
    action: Action,
    source_value: String,
    target_value: String,
}

#[derive(Debug)]
enum Destination {
    Source,
    Target,
}

#[derive(Debug)]
enum Action {
    Add,
    Change,
}

#[derive(Debug)]
struct Warning {
    message: String,
}

pub fn run(
    config: &DotSyncConfig,
    name: Option<&str>,
    direction: Direction,
    options: SyncOptions,
) -> Result<()> {
    let targets = select_targets(config, name)?;
    for target in targets {
        run_target(target, direction, options)
            .with_context(|| format!("failed to process target {}", target.name))?;
    }
    Ok(())
}

fn select_targets<'a>(
    config: &'a DotSyncConfig,
    name: Option<&str>,
) -> Result<Vec<&'a TargetConfig>> {
    if let Some(name) = name {
        let target = config
            .targets
            .get(name)
            .ok_or_else(|| anyhow!("unknown target: {name}"))?;
        Ok(vec![target])
    } else {
        Ok(config.targets.values().collect())
    }
}

fn run_target(target: &TargetConfig, direction: Direction, options: SyncOptions) -> Result<()> {
    AnyDocument::validate_format(&target.format).map_err(|error| {
        anyhow!(
            "target '{}' uses format '{}': {error}",
            target.name,
            target.format
        )
    })?;

    let mut source = load_document_for_target(
        target,
        DocumentRole::Source,
        direction,
        matches!(direction, Direction::Pull | Direction::Sync),
    )?;
    let mut target_doc = load_document_for_target(
        target,
        DocumentRole::Target,
        direction,
        matches!(direction, Direction::Push | Direction::Sync),
    )?;

    let sync_paths = parse_paths(target)?;
    let warnings = detect_table_conflicts(&source, &target_doc, &sync_paths, direction);

    let changes = match direction {
        Direction::Pull => pull(&mut source, &target_doc, &sync_paths)?,
        Direction::Push => push(&source, &mut target_doc, &sync_paths)?,
        Direction::Sync => sync(&mut source, &mut target_doc, &sync_paths)?,
    };

    report_changes(target, direction, options, &changes, &warnings);
    if options.dry_run || changes.is_empty() {
        return Ok(());
    }

    let writes_source = changes
        .iter()
        .any(|change| matches!(change.destination, Destination::Source));
    let writes_target = changes
        .iter()
        .any(|change| matches!(change.destination, Destination::Target));

    if writes_source {
        write_document(&target.source, source.render(), options.backup)?;
    }
    if writes_target {
        write_document(&target.target, target_doc.render(), options.backup)?;
    }

    Ok(())
}

#[derive(Debug, Clone, Copy)]
enum DocumentRole {
    Source,
    Target,
}

impl DocumentRole {
    fn label(self) -> &'static str {
        match self {
            Self::Source => "source",
            Self::Target => "target",
        }
    }

    fn path(self, target: &TargetConfig) -> &Path {
        match self {
            Self::Source => &target.source,
            Self::Target => &target.target,
        }
    }

    fn bootstrap_hint(self) -> &'static str {
        match self {
            Self::Source => "create the source file, or run pull/sync if you want to bootstrap it",
            Self::Target => "create the target file, or run push/sync if you want to bootstrap it",
        }
    }
}

fn load_document_for_target(
    target: &TargetConfig,
    role: DocumentRole,
    direction: Direction,
    allow_missing: bool,
) -> Result<AnyDocument> {
    let path = role.path(target);
    if !path.exists() && !allow_missing {
        bail!(
            "target '{}' cannot {}: {} file does not exist: {}\n  fix: {}",
            target.name,
            direction,
            role.label(),
            path.display(),
            role.bootstrap_hint()
        );
    }

    AnyDocument::load(&target.format, path, allow_missing).with_context(|| {
        format!(
            "failed to load {} file for target '{}': {}",
            role.label(),
            target.name,
            path.display()
        )
    })
}

fn pull(
    source: &mut dyn Document,
    target: &dyn Document,
    paths: &[ParsedPath],
) -> Result<Vec<Change>> {
    let mut changes = Vec::new();
    for path in paths {
        if let Some(change) = apply_one_way(source, target, path, Destination::Source)? {
            changes.push(change);
        }
    }
    Ok(changes)
}

fn push(
    source: &dyn Document,
    target: &mut dyn Document,
    paths: &[ParsedPath],
) -> Result<Vec<Change>> {
    let mut changes = Vec::new();
    for path in paths {
        if let Some(change) = apply_one_way(target, source, path, Destination::Target)? {
            changes.push(change);
        }
    }
    Ok(changes)
}

fn sync(
    source: &mut dyn Document,
    target: &mut dyn Document,
    paths: &[ParsedPath],
) -> Result<Vec<Change>> {
    let mut changes = Vec::new();
    for path in paths {
        let source_item = source.get(&path.path);
        let target_item = target.get(&path.path);
        match (source_item, target_item) {
            (None, None) => continue,
            (Some(s), Some(t)) if same_item(&s, &t) => continue,
            (_, Some(_)) => {
                if let Some(change) = apply_one_way(source, target, path, Destination::Source)? {
                    changes.push(change);
                }
            }
            (Some(_), None) => {
                if let Some(change) = apply_one_way(target, source, path, Destination::Target)? {
                    changes.push(change);
                }
            }
        }
    }
    Ok(changes)
}

fn apply_one_way(
    into: &mut dyn Document,
    from: &dyn Document,
    path: &ParsedPath,
    destination: Destination,
) -> Result<Option<Change>> {
    let Some(from_item) = from.get(&path.path) else {
        return Ok(None);
    };
    let into_item = into.get(&path.path);
    let into_conflict = into.table_conflict(&path.path);
    if into_item
        .as_ref()
        .is_some_and(|item| same_item(item, &from_item))
    {
        return Ok(None);
    }
    let action = if into_item.is_some() || into_conflict.is_some() {
        Action::Change
    } else {
        Action::Add
    };
    let from_value = summarize_item(Some(&from_item));
    let into_value = into_conflict
        .as_ref()
        .map(|conflict| conflict.value.clone())
        .unwrap_or_else(|| summarize_item(into_item.as_ref()));
    into.set(&path.path, from_item)?;

    // Change records always read source-side / target-side from the user's
    // perspective, regardless of which document we wrote into.
    let (source_value, target_value) = match destination {
        Destination::Source => (into_value, from_value),
        Destination::Target => (from_value, into_value),
    };
    Ok(Some(Change {
        path: path.raw.clone(),
        destination,
        action,
        source_value,
        target_value,
    }))
}

#[derive(Debug)]
struct ParsedPath {
    raw: String,
    path: FieldPath,
}

fn parse_paths(target: &TargetConfig) -> Result<Vec<ParsedPath>> {
    target
        .sync
        .iter()
        .map(|raw| {
            let path = FieldPath::parse(raw).map_err(|error| {
                anyhow!(
                    "invalid sync path '{}' in target '{}': {error}; quote path segments that contain dots, for example plugins.\"github@openai-curated\".enabled",
                    raw,
                    target.name
                )
            })?;
            Ok(ParsedPath {
                raw: raw.clone(),
                path,
            })
        })
        .collect()
}

fn same_item(left: &toml_edit::Item, right: &toml_edit::Item) -> bool {
    left.to_string() == right.to_string()
}

fn detect_table_conflicts(
    source: &dyn Document,
    target: &dyn Document,
    paths: &[ParsedPath],
    direction: Direction,
) -> Vec<Warning> {
    let mut warnings = Vec::new();
    for path in paths {
        if matches!(direction, Direction::Pull | Direction::Sync)
            && let Some(conflict) = source.table_conflict(&path.path)
        {
            warnings.push(Warning {
                message: format!(
                    "source path '{}' needs '{}' to be a table, but it is {} and may be overwritten",
                    path.raw, conflict.path, conflict.kind
                ),
            });
        }
        if matches!(direction, Direction::Push | Direction::Sync)
            && let Some(conflict) = target.table_conflict(&path.path)
        {
            warnings.push(Warning {
                message: format!(
                    "target path '{}' needs '{}' to be a table, but it is {} and may be overwritten",
                    path.raw, conflict.path, conflict.kind
                ),
            });
        }
    }
    warnings
}

fn summarize_item(item: Option<&toml_edit::Item>) -> String {
    let Some(item) = item else {
        return "<missing>".to_string();
    };

    let mut rendered = item
        .to_string()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    if rendered.is_empty() {
        rendered = item.type_name().to_string();
    }
    const LIMIT: usize = 120;
    if rendered.chars().count() > LIMIT {
        let mut truncated = rendered.chars().take(LIMIT - 3).collect::<String>();
        truncated.push_str("...");
        truncated
    } else {
        rendered
    }
}

fn report_changes(
    target: &TargetConfig,
    direction: Direction,
    options: SyncOptions,
    changes: &[Change],
    warnings: &[Warning],
) {
    let mode = if options.dry_run { "dry-run" } else { "apply" };
    println!("{} {} {}", target.name, direction, mode);
    if options.dry_run {
        println!("  dry run: no files written");
    }

    for warning in warnings {
        println!("  warn: {}", warning.message);
    }

    if changes.is_empty() {
        println!("  No changes.");
        return;
    }

    for change in changes {
        let destination = match change.destination {
            Destination::Source => "source",
            Destination::Target => "target",
        };
        let (present, past) = match change.action {
            Action::Add => ("add", "added"),
            Action::Change => ("change", "changed"),
        };
        let label = if options.dry_run {
            format!("would {present}")
        } else {
            past.to_string()
        };
        println!("  {label} {destination}: {}", change.path);
        println!("    source: {}", change.source_value);
        println!("    target: {}", change.target_value);
    }
}

fn write_document(path: &Path, content: String, backup: bool) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }

    if backup && path.exists() {
        backup_file(path)?;
    }

    fs::write(path, content).with_context(|| format!("failed to write {}", path.display()))?;
    Ok(())
}

fn backup_file(path: &Path) -> Result<PathBuf> {
    let timestamp = Local::now().format("%Y%m%d-%H%M%S");
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| anyhow!("invalid backup file path: {}", path.display()))?;
    let mut backup = path.with_file_name(format!("{file_name}.bak.{timestamp}"));
    let mut index = 0;
    while backup.exists() {
        index += 1;
        backup = path.with_file_name(format!("{file_name}.bak.{timestamp}.{index}"));
    }
    fs::copy(path, &backup).with_context(|| {
        format!(
            "failed to back up {} to {}",
            path.display(),
            backup.display()
        )
    })?;
    Ok(backup)
}

#[cfg(test)]
mod tests {
    use std::fs;

    use tempfile::tempdir;

    use super::*;
    use crate::config::TargetConfig;
    use crate::document::TomlDocument;

    fn toml_from(content: &str) -> TomlDocument {
        let dir = tempdir().unwrap();
        let path = dir.path().join("doc.toml");
        fs::write(&path, content).unwrap();
        TomlDocument::load(&path, false).unwrap()
    }

    fn parsed(paths: &[&str]) -> Vec<ParsedPath> {
        paths
            .iter()
            .map(|path| ParsedPath {
                raw: (*path).to_string(),
                path: FieldPath::parse(path).unwrap(),
            })
            .collect()
    }

    #[test]
    fn pull_extracts_only_sync_fields() {
        let mut source = toml_from("");
        let target = toml_from(
            r#"
project_doc_max_bytes = 65536

[projects."/secret"]
trust_level = "trusted"
"#,
        );
        let changes = pull(&mut source, &target, &parsed(&["project_doc_max_bytes"])).unwrap();
        assert_eq!(changes.len(), 1);
        assert!(
            source
                .get(&FieldPath::parse("project_doc_max_bytes").unwrap())
                .is_some()
        );
        assert!(source.get(&FieldPath::parse("projects").unwrap()).is_none());
    }

    #[test]
    fn pull_preserves_unlisted_source_fields_and_layout() {
        let mut source = toml_from(
            r#"
[local]
state = true

[tui]
theme = "old"
"#,
        );
        let target = toml_from(
            r#"
[plugins."github@openai-curated"]
enabled = true

[tui]
theme = "monokai"
"#,
        );

        pull(
            &mut source,
            &target,
            &parsed(&["tui.theme", "plugins.\"github@openai-curated\".enabled"]),
        )
        .unwrap();

        let rendered = source.render();
        assert!(rendered.contains("[local]"));
        assert!(rendered.contains("state = true"));
        assert!(rendered.contains("theme = \"monokai\""));
        assert!(
            rendered.find("[local]").unwrap() < rendered.find("[tui]").unwrap(),
            "unlisted [local] should keep its original position before [tui]"
        );
    }

    #[test]
    fn pull_does_not_remove_listed_field_when_target_lacks_it() {
        let mut source = toml_from(
            r#"
project_doc_max_bytes = 65536
project_doc_fallback_filenames = ["AGENTS.md"]
"#,
        );
        let target = toml_from("project_doc_max_bytes = 65536\n");

        let changes = pull(
            &mut source,
            &target,
            &parsed(&["project_doc_max_bytes", "project_doc_fallback_filenames"]),
        )
        .unwrap();

        assert!(changes.is_empty(), "no changes expected: {changes:?}");
        assert!(
            source
                .get(&FieldPath::parse("project_doc_fallback_filenames").unwrap())
                .is_some()
        );
    }

    #[test]
    fn push_preserves_unmanaged_target_fields() {
        let source = toml_from("project_doc_max_bytes = 65536\n");
        let mut target = toml_from(
            r#"
[projects."/secret"]
trust_level = "trusted"
"#,
        );
        push(&source, &mut target, &parsed(&["project_doc_max_bytes"])).unwrap();
        assert!(
            target
                .get(&FieldPath::parse("project_doc_max_bytes").unwrap())
                .is_some()
        );
        assert!(target.get(&FieldPath::parse("projects").unwrap()).is_some());
    }

    #[test]
    fn sync_uses_target_values_and_fills_missing_target_fields() {
        let mut source = toml_from(
            r#"
project_doc_max_bytes = 1
project_doc_fallback_filenames = ["CLAUDE.md"]
"#,
        );
        let mut target = toml_from("project_doc_max_bytes = 65536\n");
        sync(
            &mut source,
            &mut target,
            &parsed(&["project_doc_max_bytes", "project_doc_fallback_filenames"]),
        )
        .unwrap();

        let source_size = source
            .get(&FieldPath::parse("project_doc_max_bytes").unwrap())
            .unwrap();
        assert_eq!(source_size.as_value().unwrap().as_integer(), Some(65536));
        assert!(
            target
                .get(&FieldPath::parse("project_doc_fallback_filenames").unwrap())
                .is_some()
        );
    }

    #[test]
    fn sync_target_value_wins_and_source_layout_is_preserved() {
        let mut source = toml_from(
            r#"
[plugins."github@openai-curated"]
enabled = true

[tui]
theme = "source"
"#,
        );
        let mut target = toml_from(
            r#"
[tui]
theme = "target"
"#,
        );

        sync(
            &mut source,
            &mut target,
            &parsed(&["tui.theme", "plugins.\"github@openai-curated\".enabled"]),
        )
        .unwrap();

        let rendered = source.render();
        assert!(rendered.contains("theme = \"target\""));
        assert!(
            rendered
                .find("[plugins.\"github@openai-curated\"]")
                .unwrap()
                < rendered.find("[tui]").unwrap(),
            "source ordering should be untouched"
        );
        assert!(
            target
                .get(&FieldPath::parse("plugins.\"github@openai-curated\".enabled").unwrap())
                .is_some()
        );
    }

    #[test]
    fn sync_never_removes_from_either_side() {
        let mut source = toml_from(
            r#"
project_doc_max_bytes = 65536
project_doc_fallback_filenames = ["AGENTS.md"]
"#,
        );
        let mut target = toml_from(
            r#"
tui_theme = "monokai"
"#,
        );
        sync(
            &mut source,
            &mut target,
            &parsed(&["project_doc_max_bytes", "project_doc_fallback_filenames"]),
        )
        .unwrap();

        assert!(
            source
                .get(&FieldPath::parse("project_doc_fallback_filenames").unwrap())
                .is_some(),
            "sync must not delete source-only listed field"
        );
        assert!(
            target
                .get(&FieldPath::parse("project_doc_fallback_filenames").unwrap())
                .is_some(),
            "sync should fill target with source-only listed field"
        );
        assert!(
            target
                .get(&FieldPath::parse("tui_theme").unwrap())
                .is_some(),
            "sync must not touch unlisted target field"
        );
    }

    #[test]
    fn sync_no_change_when_both_sides_match() {
        let mut source = toml_from("project_doc_max_bytes = 65536\n");
        let mut target = toml_from("project_doc_max_bytes = 65536\n");
        let changes = sync(
            &mut source,
            &mut target,
            &parsed(&["project_doc_max_bytes"]),
        )
        .unwrap();
        assert!(changes.is_empty());
    }

    #[test]
    fn sync_bootstraps_missing_target_file() {
        let dir = tempdir().unwrap();
        let source = dir.path().join("source.toml");
        let target_path = dir.path().join("target.toml");
        fs::write(&source, "project_doc_max_bytes = 65536\n").unwrap();
        let target = TargetConfig {
            name: "codex".to_string(),
            format: "toml".to_string(),
            source,
            target: target_path.clone(),
            sync: vec!["project_doc_max_bytes".to_string()],
        };

        run_target(
            &target,
            Direction::Sync,
            SyncOptions {
                dry_run: false,
                backup: true,
            },
        )
        .unwrap();

        assert_eq!(
            fs::read_to_string(target_path).unwrap(),
            "project_doc_max_bytes = 65536\n"
        );
    }

    #[test]
    fn push_requires_existing_source() {
        let dir = tempdir().unwrap();
        let target = TargetConfig {
            name: "codex".to_string(),
            format: "toml".to_string(),
            source: dir.path().join("missing.toml"),
            target: dir.path().join("target.toml"),
            sync: vec!["project_doc_max_bytes".to_string()],
        };

        let err = run_target(
            &target,
            Direction::Push,
            SyncOptions {
                dry_run: true,
                backup: true,
            },
        )
        .unwrap_err();
        assert!(err.to_string().contains("file does not exist"));
    }

    #[test]
    fn write_document_skips_backup_by_default() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("config.toml");
        fs::write(&path, "old = true\n").unwrap();

        write_document(&path, "new = true\n".to_string(), false).unwrap();

        assert_eq!(fs::read_to_string(&path).unwrap(), "new = true\n");
        let backups = fs::read_dir(dir.path())
            .unwrap()
            .filter_map(Result::ok)
            .filter(|entry| entry.file_name().to_string_lossy().contains(".bak."))
            .count();
        assert_eq!(backups, 0);
    }

    #[test]
    fn write_document_creates_backup_when_requested() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("config.toml");
        fs::write(&path, "old = true\n").unwrap();

        write_document(&path, "new = true\n".to_string(), true).unwrap();

        assert_eq!(fs::read_to_string(&path).unwrap(), "new = true\n");
        let backups = fs::read_dir(dir.path())
            .unwrap()
            .filter_map(Result::ok)
            .filter(|entry| entry.file_name().to_string_lossy().contains(".bak."))
            .count();
        assert_eq!(backups, 1);
    }
}
