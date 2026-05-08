use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow};
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
}

#[derive(Debug)]
enum Destination {
    Source,
    Target,
}

#[derive(Debug)]
enum Action {
    Update,
    Remove,
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
    let mut source = AnyDocument::load(
        &target.format,
        &target.source,
        matches!(direction, Direction::Pull | Direction::Sync),
    )?;
    let mut target_doc = AnyDocument::load(
        &target.format,
        &target.target,
        matches!(direction, Direction::Push | Direction::Sync),
    )?;

    let sync_paths = parse_paths(&target.sync)?;

    let changes = match direction {
        Direction::Pull => pull(&mut source, &target_doc, &sync_paths, &target.format)?,
        Direction::Push => push(&source, &mut target_doc, &sync_paths)?,
        Direction::Sync => sync(&mut source, &mut target_doc, &sync_paths, &target.format)?,
    };

    report_changes(target, direction, options, &changes);
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
        write_document(&target.source, source.to_string(), options.backup)?;
    }
    if writes_target {
        write_document(&target.target, target_doc.to_string(), options.backup)?;
    }

    Ok(())
}

fn pull(
    source: &mut dyn Document,
    target: &dyn Document,
    paths: &[ParsedPath],
    format: &str,
) -> Result<Vec<Change>> {
    let canonical_source = build_canonical_source(format, paths, |path| target.get(path))?;
    let source_needs_canonical_rewrite = source.to_string() != canonical_source.to_string();
    let mut changes = Vec::new();
    for path in paths {
        let Some(target_item) = canonical_source.get(&path.path) else {
            continue;
        };
        if source
            .get(&path.path)
            .is_some_and(|source_item| same_item(&source_item, &target_item))
        {
            continue;
        }
        source.set(&path.path, target_item)?;
        changes.push(Change {
            path: path.raw.clone(),
            destination: Destination::Source,
            action: Action::Update,
        });
    }
    for path in paths {
        if canonical_source.get(&path.path).is_none() && source.contains(&path.path) {
            changes.push(Change {
                path: path.raw.clone(),
                destination: Destination::Source,
                action: Action::Remove,
            });
        }
    }
    if source_needs_canonical_rewrite
        && !changes
            .iter()
            .any(|change| matches!(change.destination, Destination::Source))
    {
        changes.push(Change {
            path: "canonical source order".to_string(),
            destination: Destination::Source,
            action: Action::Update,
        });
    }
    replace_with_canonical_source(source, canonical_source, paths)?;
    Ok(changes)
}

fn push(
    source: &dyn Document,
    target: &mut dyn Document,
    paths: &[ParsedPath],
) -> Result<Vec<Change>> {
    let mut changes = Vec::new();
    for path in paths {
        let Some(source_item) = source.get(&path.path) else {
            continue;
        };
        if target
            .get(&path.path)
            .is_some_and(|target_item| same_item(&target_item, &source_item))
        {
            continue;
        }
        target.set(&path.path, source_item)?;
        changes.push(Change {
            path: path.raw.clone(),
            destination: Destination::Target,
            action: Action::Update,
        });
    }
    Ok(changes)
}

fn sync(
    source: &mut dyn Document,
    target: &mut dyn Document,
    paths: &[ParsedPath],
    format: &str,
) -> Result<Vec<Change>> {
    let canonical_source = build_canonical_source(format, paths, |path| {
        target.get(path).or_else(|| source.get(path))
    })?;
    let source_needs_canonical_rewrite = source.to_string() != canonical_source.to_string();
    let mut changes = Vec::new();
    for path in paths {
        let target_item = target.get(&path.path);
        let source_item = source.get(&path.path);
        let canonical_item = canonical_source.get(&path.path);

        if canonical_item.as_ref().is_some_and(|canonical_item| {
            source_item
                .as_ref()
                .is_none_or(|source_item| !same_item(source_item, canonical_item))
        }) {
            changes.push(Change {
                path: path.raw.clone(),
                destination: Destination::Source,
                action: Action::Update,
            });
        }

        if target_item.is_none() {
            if let Some(source_item) = source_item {
                target.set(&path.path, source_item)?;
                changes.push(Change {
                    path: path.raw.clone(),
                    destination: Destination::Target,
                    action: Action::Update,
                });
            }
        }
    }
    if source_needs_canonical_rewrite
        && !changes
            .iter()
            .any(|change| matches!(change.destination, Destination::Source))
    {
        changes.push(Change {
            path: "canonical source order".to_string(),
            destination: Destination::Source,
            action: Action::Update,
        });
    }
    replace_with_canonical_source(source, canonical_source, paths)?;
    Ok(changes)
}

fn build_canonical_source<F>(
    format: &str,
    paths: &[ParsedPath],
    mut value_for: F,
) -> Result<AnyDocument>
where
    F: FnMut(&FieldPath) -> Option<toml_edit::Item>,
{
    let mut canonical = AnyDocument::empty(format)?;
    for path in paths {
        if let Some(item) = value_for(&path.path) {
            canonical.set(&path.path, item)?;
        }
    }
    Ok(canonical)
}

fn replace_with_canonical_source(
    source: &mut dyn Document,
    canonical_source: AnyDocument,
    paths: &[ParsedPath],
) -> Result<()> {
    source.clear();
    for path in paths {
        if let Some(item) = canonical_source.get(&path.path) {
            source.set(&path.path, item)?;
        }
    }
    Ok(())
}

#[derive(Debug)]
struct ParsedPath {
    raw: String,
    path: FieldPath,
}

fn parse_paths(raw_paths: &[String]) -> Result<Vec<ParsedPath>> {
    raw_paths
        .iter()
        .map(|raw| {
            Ok(ParsedPath {
                raw: raw.clone(),
                path: FieldPath::parse(raw)?,
            })
        })
        .collect()
}

fn same_item(left: &toml_edit::Item, right: &toml_edit::Item) -> bool {
    left.to_string() == right.to_string()
}

fn report_changes(
    target: &TargetConfig,
    direction: Direction,
    options: SyncOptions,
    changes: &[Change],
) {
    let mode = if options.dry_run { "dry-run" } else { "apply" };
    println!("{} {:?} {}", target.name, direction, mode);

    if changes.is_empty() {
        println!("  No changes.");
        return;
    }

    for change in changes {
        let destination = match change.destination {
            Destination::Source => "source",
            Destination::Target => "target",
        };
        let prefix = match (options.dry_run, &change.action) {
            (true, Action::Update) => "Would update",
            (true, Action::Remove) => "Would remove",
            (false, Action::Update) => "Update",
            (false, Action::Remove) => "Remove",
        };
        println!("  {prefix} {destination}: {}", change.path);
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
        let changes = pull(
            &mut source,
            &target,
            &parsed(&["project_doc_max_bytes"]),
            "toml",
        )
        .unwrap();
        assert_eq!(changes.len(), 1);
        assert!(
            source
                .get(&FieldPath::parse("project_doc_max_bytes").unwrap())
                .is_some()
        );
        assert!(source.get(&FieldPath::parse("projects").unwrap()).is_none());
    }

    #[test]
    fn pull_rewrites_source_in_sync_order() {
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
            "toml",
        )
        .unwrap();

        let rendered = source.to_string();
        assert!(!rendered.contains("[local]"));
        assert!(
            rendered.find("[tui]").unwrap()
                < rendered
                    .find("[plugins.\"github@openai-curated\"]")
                    .unwrap()
        );
        assert!(!rendered.contains("[plugins]\n"));
    }

    #[test]
    fn pull_reports_order_only_source_rewrite() {
        let mut source = toml_from(
            r#"
[notice]
hide_rate_limit_model_nudge = true

[tui]
theme = "monokai"
"#,
        );
        let target = toml_from(
            r#"
[tui]
theme = "monokai"

[notice]
hide_rate_limit_model_nudge = true
"#,
        );

        let changes = pull(
            &mut source,
            &target,
            &parsed(&["tui.theme", "notice.hide_rate_limit_model_nudge"]),
            "toml",
        )
        .unwrap();

        assert_eq!(changes.len(), 1);
        assert_eq!(changes[0].path, "canonical source order");
    }

    #[test]
    fn pull_reports_removed_source_fields_missing_from_target() {
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
            "toml",
        )
        .unwrap();

        assert_eq!(changes.len(), 1);
        assert_eq!(changes[0].path, "project_doc_fallback_filenames");
        assert!(matches!(changes[0].action, Action::Remove));
        assert!(
            source
                .get(&FieldPath::parse("project_doc_fallback_filenames").unwrap())
                .is_none()
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
            "toml",
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
    fn sync_canonical_source_uses_target_values_first() {
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
            "toml",
        )
        .unwrap();

        let rendered = source.to_string();
        assert!(rendered.contains("theme = \"target\""));
        assert!(
            rendered.find("[tui]").unwrap()
                < rendered
                    .find("[plugins.\"github@openai-curated\"]")
                    .unwrap()
        );
        assert!(!rendered.contains("[plugins]\n"));
        assert!(
            target
                .get(&FieldPath::parse("plugins.\"github@openai-curated\".enabled").unwrap())
                .is_some()
        );
    }

    #[test]
    fn sync_reports_order_only_source_rewrite() {
        let mut source = toml_from(
            r#"
[notice]
hide_rate_limit_model_nudge = true

[tui]
theme = "monokai"
"#,
        );
        let mut target = toml_from(
            r#"
[tui]
theme = "monokai"

[notice]
hide_rate_limit_model_nudge = true
"#,
        );

        let changes = sync(
            &mut source,
            &mut target,
            &parsed(&["tui.theme", "notice.hide_rate_limit_model_nudge"]),
            "toml",
        )
        .unwrap();

        assert_eq!(changes.len(), 1);
        assert_eq!(changes[0].path, "canonical source order");
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
