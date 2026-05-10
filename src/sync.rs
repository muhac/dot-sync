use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow, bail};
use chrono::Local;

use crate::config::{DotSyncConfig, TargetConfig};
use crate::document::{Document, TomlDocument, validate_format};
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
    pub conflict: ConflictMode,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConflictMode {
    TargetWins,
    SourceWins,
    FailOnConflict,
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

    // For fail-on-conflict, every selected target must be checked before
    // any write happens. Otherwise an earlier target could be modified
    // while a later one bails, violating the "write nothing" promise.
    if matches!(direction, Direction::Sync)
        && matches!(options.conflict, ConflictMode::FailOnConflict)
    {
        preflight_fail_on_conflict(&targets)?;
    }

    for target in targets {
        run_target(target, direction, options)
            .with_context(|| format!("failed to process target {}", target.name))?;
    }
    Ok(())
}

fn preflight_fail_on_conflict(targets: &[&TargetConfig]) -> Result<()> {
    let mut violators: Vec<(String, Vec<Conflict>)> = Vec::new();
    for target in targets {
        let conflicts = dispatch_format(target, |t| match t.format.as_str() {
            "toml" => target_conflicts::<TomlDocument>(t),
            _ => unreachable!("dispatch_format guards format"),
        })?;
        if !conflicts.is_empty() {
            violators.push((target.name.clone(), conflicts));
        }
    }
    if violators.is_empty() {
        return Ok(());
    }
    for (name, conflicts) in &violators {
        println!("{name} sync preflight");
        print_conflicts(conflicts);
    }
    let total: usize = violators.iter().map(|(_, c)| c.len()).sum();
    bail!(
        "fail-on-conflict: {total} conflicting field(s) across {} target(s)",
        violators.len()
    );
}

fn target_conflicts<D: Document>(target: &TargetConfig) -> Result<Vec<Conflict>> {
    let source = D::load(&target.source, true)?;
    let target_doc = D::load(&target.target, true)?;
    let sync_paths = parse_paths(target)?;
    Ok(collect_conflicts::<D>(&source, &target_doc, &sync_paths))
}

/// Validate the target's format and dispatch to a typed pipeline. Centralizes
/// the format match so each engine entry point doesn't repeat it.
fn dispatch_format<T>(
    target: &TargetConfig,
    typed: impl FnOnce(&TargetConfig) -> Result<T>,
) -> Result<T> {
    validate_format(&target.format).map_err(|error| {
        anyhow!(
            "target '{}' uses format '{}': {error}",
            target.name,
            target.format
        )
    })?;
    typed(target)
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
    dispatch_format(target, |t| match t.format.as_str() {
        "toml" => run_target_typed::<TomlDocument>(t, direction, options),
        _ => unreachable!("dispatch_format guards format"),
    })
}

fn run_target_typed<D: Document>(
    target: &TargetConfig,
    direction: Direction,
    options: SyncOptions,
) -> Result<()> {
    let mut source = load_document_for_target::<D>(
        target,
        DocumentRole::Source,
        direction,
        matches!(direction, Direction::Pull | Direction::Sync),
    )?;
    let mut target_doc = load_document_for_target::<D>(
        target,
        DocumentRole::Target,
        direction,
        matches!(direction, Direction::Push | Direction::Sync),
    )?;

    let sync_paths = parse_paths(target)?;
    let warnings = detect_table_conflicts::<D>(&source, &target_doc, &sync_paths, direction);

    let changes = match direction {
        Direction::Pull => pull::<D>(&mut source, &target_doc, &sync_paths)?,
        Direction::Push => push::<D>(&source, &mut target_doc, &sync_paths)?,
        Direction::Sync => sync::<D>(&mut source, &mut target_doc, &sync_paths, options.conflict)?,
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

    let snapshot_dir = std::env::temp_dir().join("dot-sync");

    if writes_source {
        let outcome = write_document(
            &target.source,
            source.render(),
            options.backup,
            &snapshot_dir,
        )?;
        report_write(DocumentRole::Source, &target.source, &outcome);
    }
    if writes_target {
        let outcome = write_document(
            &target.target,
            target_doc.render(),
            options.backup,
            &snapshot_dir,
        )?;
        report_write(DocumentRole::Target, &target.target, &outcome);
    }

    Ok(())
}

fn report_write(role: DocumentRole, path: &Path, outcome: &WriteOutcome) {
    println!("  wrote {}: {}", role.label(), path.display());
    if let Some(snapshot) = &outcome.snapshot {
        println!("    recovery: {}", snapshot.display());
    }
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

fn load_document_for_target<D: Document>(
    target: &TargetConfig,
    role: DocumentRole,
    direction: Direction,
    allow_missing: bool,
) -> Result<D> {
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

    D::load(path, allow_missing).with_context(|| {
        format!(
            "failed to load {} file for target '{}': {}",
            role.label(),
            target.name,
            path.display()
        )
    })
}

fn pull<D: Document>(source: &mut D, target: &D, paths: &[ParsedPath]) -> Result<Vec<Change>> {
    let mut changes = Vec::new();
    for path in paths {
        if let Some(change) = apply_one_way::<D>(source, target, path, Destination::Source)? {
            changes.push(change);
        }
    }
    Ok(changes)
}

fn push<D: Document>(source: &D, target: &mut D, paths: &[ParsedPath]) -> Result<Vec<Change>> {
    let mut changes = Vec::new();
    for path in paths {
        if let Some(change) = apply_one_way::<D>(target, source, path, Destination::Target)? {
            changes.push(change);
        }
    }
    Ok(changes)
}

fn sync<D: Document>(
    source: &mut D,
    target: &mut D,
    paths: &[ParsedPath],
    mode: ConflictMode,
) -> Result<Vec<Change>> {
    if matches!(mode, ConflictMode::FailOnConflict) {
        let conflicts = collect_conflicts::<D>(source, target, paths);
        if !conflicts.is_empty() {
            print_conflicts(&conflicts);
            bail!("fail-on-conflict: {} conflicting field(s)", conflicts.len());
        }
    }

    let mut changes = Vec::new();
    for path in paths {
        let source_item = source.get(&path.path);
        let target_item = target.get(&path.path);
        match (source_item, target_item) {
            (None, None) => continue,
            (Some(s), Some(t)) if D::items_equal(&s, &t) => continue,
            (Some(_), Some(_)) => {
                // Both sides have a value and they differ — conflict mode picks the winner.
                let (into, from, dest): (&mut D, &D, Destination) = match mode {
                    ConflictMode::TargetWins => (source, target, Destination::Source),
                    ConflictMode::SourceWins => (target, source, Destination::Target),
                    ConflictMode::FailOnConflict => unreachable!("checked above"),
                };
                if let Some(change) = apply_one_way::<D>(into, from, path, dest)? {
                    changes.push(change);
                }
            }
            (None, Some(_)) => {
                // Only target has the value; fill source regardless of mode.
                if let Some(change) = apply_one_way::<D>(source, target, path, Destination::Source)?
                {
                    changes.push(change);
                }
            }
            (Some(_), None) => {
                // Only source has the value; fill target regardless of mode.
                if let Some(change) = apply_one_way::<D>(target, source, path, Destination::Target)?
                {
                    changes.push(change);
                }
            }
        }
    }
    Ok(changes)
}

fn collect_conflicts<D: Document>(source: &D, target: &D, paths: &[ParsedPath]) -> Vec<Conflict> {
    let mut conflicts = Vec::new();
    for path in paths {
        let (Some(s), Some(t)) = (source.get(&path.path), target.get(&path.path)) else {
            continue;
        };
        if D::items_equal(&s, &t) {
            continue;
        }
        conflicts.push(Conflict {
            path: path.raw.clone(),
            source_value: D::summarize(Some(&s)),
            target_value: D::summarize(Some(&t)),
        });
    }
    conflicts
}

fn print_conflicts(conflicts: &[Conflict]) {
    for conflict in conflicts {
        println!("  conflict: {}", conflict.path);
        println!("    source: {}", conflict.source_value);
        println!("    target: {}", conflict.target_value);
    }
}

#[derive(Debug)]
struct Conflict {
    path: String,
    source_value: String,
    target_value: String,
}

fn apply_one_way<D: Document>(
    into: &mut D,
    from: &D,
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
        .is_some_and(|item| D::items_equal(item, &from_item))
    {
        return Ok(None);
    }
    let action = if into_item.is_some() || into_conflict.is_some() {
        Action::Change
    } else {
        Action::Add
    };
    let from_value = D::summarize(Some(&from_item));
    let into_value = into_conflict
        .as_ref()
        .map(|conflict| conflict.value.clone())
        .unwrap_or_else(|| D::summarize(into_item.as_ref()));
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

fn detect_table_conflicts<D: Document>(
    source: &D,
    target: &D,
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

#[derive(Debug, Default)]
struct WriteOutcome {
    snapshot: Option<PathBuf>,
}

fn write_document(
    path: &Path,
    content: String,
    backup: bool,
    snapshot_dir: &Path,
) -> Result<WriteOutcome> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }

    if backup && path.exists() {
        backup_file(path)?;
    }

    let snapshot = snapshot_existing(path, snapshot_dir)?;
    atomic_write(path, &content)?;
    Ok(WriteOutcome { snapshot })
}

pub(crate) fn snapshot_existing(path: &Path, snapshot_dir: &Path) -> Result<Option<PathBuf>> {
    if !path.exists() {
        return Ok(None);
    }
    fs::create_dir_all(snapshot_dir).with_context(|| {
        format!(
            "failed to create snapshot directory {}",
            snapshot_dir.display()
        )
    })?;
    let timestamp = Local::now().format("%Y%m%d-%H%M%S");
    let stem = sanitize_for_filename(path);
    let mut snapshot = snapshot_dir.join(format!("{stem}.{timestamp}"));
    let mut index = 0;
    while snapshot.exists() {
        index += 1;
        snapshot = snapshot_dir.join(format!("{stem}.{timestamp}.{index}"));
    }
    fs::copy(path, &snapshot).with_context(|| {
        format!(
            "failed to snapshot {} to {}",
            path.display(),
            snapshot.display()
        )
    })?;
    Ok(Some(snapshot))
}

pub(crate) fn sanitize_for_filename(path: &Path) -> String {
    let raw = path.display().to_string();
    let trimmed = raw.trim_start_matches(['/', '\\']);
    trimmed
        .chars()
        .map(|c| match c {
            '/' | '\\' | ':' => '-',
            _ => c,
        })
        .collect()
}

pub(crate) fn atomic_write(path: &Path, content: &str) -> Result<()> {
    // Resolve symlinks before writing: a tmp+rename next to the symlink would
    // replace the link with a regular file, silently breaking dotfile setups
    // where the destination is a link into a managed directory.
    let resolved = resolve_symlink_target(path)?;
    let file_name = resolved
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| anyhow!("invalid write target: {}", resolved.display()))?;
    let tmp = resolved.with_file_name(format!(".{}.tmp.{}", file_name, std::process::id()));

    fs::write(&tmp, content)
        .with_context(|| format!("failed to stage write at {}", tmp.display()))?;

    // POSIX rename is atomic only on the same filesystem; tmp lives next to
    // the (resolved) destination, so this is safe.
    if let Err(error) = fs::rename(&tmp, &resolved) {
        let _ = fs::remove_file(&tmp);
        return Err(error)
            .with_context(|| format!("failed to publish write to {}", resolved.display()));
    }
    Ok(())
}

fn resolve_symlink_target(path: &Path) -> Result<PathBuf> {
    match fs::symlink_metadata(path) {
        Ok(meta) if meta.file_type().is_symlink() => fs::canonicalize(path)
            .with_context(|| format!("failed to resolve symlink {}", path.display())),
        _ => Ok(path.to_path_buf()),
    }
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
    fn pull_overwrites_source_scalar_when_path_demands_table() {
        let mut source = toml_from("settings = \"plain\"\n");
        let target = toml_from("[settings]\ntheme = \"dark\"\n");

        let changes = pull(&mut source, &target, &parsed(&["settings.theme"])).unwrap();

        assert_eq!(changes.len(), 1);
        let change = &changes[0];
        assert!(matches!(change.action, Action::Change));
        assert_eq!(change.source_value, "\"plain\"");
        assert_eq!(change.target_value, "\"dark\"");
        assert_eq!(
            source
                .get(&FieldPath::parse("settings.theme").unwrap())
                .and_then(|item| item.as_value().and_then(|v| v.as_str().map(String::from))),
            Some("dark".to_string())
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
            ConflictMode::TargetWins,
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
            ConflictMode::TargetWins,
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
            ConflictMode::TargetWins,
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
            ConflictMode::TargetWins,
        )
        .unwrap();
        assert!(changes.is_empty());
    }

    #[test]
    fn sync_source_wins_overwrites_target_on_conflict() {
        let mut source = toml_from("project_doc_max_bytes = 1\n");
        let mut target = toml_from("project_doc_max_bytes = 65536\n");

        let changes = sync(
            &mut source,
            &mut target,
            &parsed(&["project_doc_max_bytes"]),
            ConflictMode::SourceWins,
        )
        .unwrap();

        assert_eq!(changes.len(), 1);
        assert!(matches!(changes[0].destination, Destination::Target));
        let new_target = target
            .get(&FieldPath::parse("project_doc_max_bytes").unwrap())
            .unwrap();
        assert_eq!(new_target.as_value().unwrap().as_integer(), Some(1));
        let preserved_source = source
            .get(&FieldPath::parse("project_doc_max_bytes").unwrap())
            .unwrap();
        assert_eq!(
            preserved_source.as_value().unwrap().as_integer(),
            Some(1),
            "source value should be untouched under source-wins"
        );
    }

    #[test]
    fn sync_source_wins_still_fills_missing_source() {
        let mut source = toml_from("");
        let mut target = toml_from("project_doc_max_bytes = 65536\n");

        sync(
            &mut source,
            &mut target,
            &parsed(&["project_doc_max_bytes"]),
            ConflictMode::SourceWins,
        )
        .unwrap();

        assert_eq!(
            source
                .get(&FieldPath::parse("project_doc_max_bytes").unwrap())
                .unwrap()
                .as_value()
                .unwrap()
                .as_integer(),
            Some(65536),
            "missing-on-source case fills regardless of mode"
        );
    }

    #[test]
    fn sync_fail_on_conflict_bails_without_writing() {
        let mut source = toml_from(
            r#"
project_doc_max_bytes = 1
project_doc_fallback_filenames = ["AGENTS.md"]
"#,
        );
        let mut target = toml_from("project_doc_max_bytes = 65536\n");
        let original_source = source.render();
        let original_target = target.render();

        let err = sync(
            &mut source,
            &mut target,
            &parsed(&["project_doc_max_bytes", "project_doc_fallback_filenames"]),
            ConflictMode::FailOnConflict,
        )
        .unwrap_err();

        assert!(err.to_string().contains("fail-on-conflict"));
        assert_eq!(source.render(), original_source);
        assert_eq!(target.render(), original_target);
    }

    #[test]
    fn sync_fail_on_conflict_succeeds_when_no_conflict() {
        let mut source = toml_from("project_doc_max_bytes = 65536\n");
        let mut target = toml_from(
            r#"
project_doc_max_bytes = 65536
project_doc_fallback_filenames = ["AGENTS.md"]
"#,
        );

        sync(
            &mut source,
            &mut target,
            &parsed(&["project_doc_max_bytes", "project_doc_fallback_filenames"]),
            ConflictMode::FailOnConflict,
        )
        .unwrap();

        assert!(
            source
                .get(&FieldPath::parse("project_doc_fallback_filenames").unwrap())
                .is_some(),
            "missing-on-source case still fills under fail-on-conflict"
        );
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
                conflict: ConflictMode::TargetWins,
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
                conflict: ConflictMode::TargetWins,
            },
        )
        .unwrap_err();
        assert!(err.to_string().contains("file does not exist"));
    }

    #[test]
    fn write_document_skips_backup_by_default() {
        let dir = tempdir().unwrap();
        let snap = tempdir().unwrap();
        let path = dir.path().join("config.toml");
        fs::write(&path, "old = true\n").unwrap();

        write_document(&path, "new = true\n".to_string(), false, snap.path()).unwrap();

        assert_eq!(fs::read_to_string(&path).unwrap(), "new = true\n");
        let backups = fs::read_dir(dir.path())
            .unwrap()
            .filter_map(Result::ok)
            .filter(|entry| entry.file_name().to_string_lossy().contains(".bak."))
            .count();
        assert_eq!(backups, 0);
    }

    #[test]
    fn atomic_write_leaves_no_tmp_remnant_after_success() {
        let dir = tempdir().unwrap();
        let snap = tempdir().unwrap();
        let path = dir.path().join("config.toml");
        fs::write(&path, "old = true\n").unwrap();

        write_document(&path, "new = true\n".to_string(), false, snap.path()).unwrap();

        assert_eq!(fs::read_to_string(&path).unwrap(), "new = true\n");
        let tmp_files: Vec<_> = fs::read_dir(dir.path())
            .unwrap()
            .filter_map(Result::ok)
            .filter(|entry| entry.file_name().to_string_lossy().contains(".tmp."))
            .collect();
        assert!(tmp_files.is_empty(), "leftover tmp files: {tmp_files:?}");
    }

    #[test]
    fn write_document_creates_backup_when_requested() {
        let dir = tempdir().unwrap();
        let snap = tempdir().unwrap();
        let path = dir.path().join("config.toml");
        fs::write(&path, "old = true\n").unwrap();

        write_document(&path, "new = true\n".to_string(), true, snap.path()).unwrap();

        assert_eq!(fs::read_to_string(&path).unwrap(), "new = true\n");
        let backups = fs::read_dir(dir.path())
            .unwrap()
            .filter_map(Result::ok)
            .filter(|entry| entry.file_name().to_string_lossy().contains(".bak."))
            .count();
        assert_eq!(backups, 1);
    }

    #[test]
    fn write_document_creates_recovery_snapshot_for_existing_file() {
        let dir = tempdir().unwrap();
        let snap = tempdir().unwrap();
        let path = dir.path().join("config.toml");
        fs::write(&path, "old = true\n").unwrap();

        let outcome =
            write_document(&path, "new = true\n".to_string(), false, snap.path()).unwrap();

        let snapshot_path = outcome.snapshot.expect("expected snapshot path");
        assert_eq!(fs::read_to_string(&snapshot_path).unwrap(), "old = true\n");
        assert!(snapshot_path.starts_with(snap.path()));
    }

    #[cfg(unix)]
    #[test]
    fn atomic_write_writes_through_symlink() {
        use std::os::unix::fs::symlink;
        let real_dir = tempdir().unwrap();
        let link_dir = tempdir().unwrap();
        let real_path = real_dir.path().join("real.toml");
        let link_path = link_dir.path().join("link.toml");
        fs::write(&real_path, "old\n").unwrap();
        symlink(&real_path, &link_path).unwrap();

        atomic_write(&link_path, "new\n").unwrap();

        assert_eq!(fs::read_to_string(&real_path).unwrap(), "new\n");
        assert!(
            fs::symlink_metadata(&link_path)
                .unwrap()
                .file_type()
                .is_symlink(),
            "symlink at link_path was replaced by a regular file"
        );
    }

    #[test]
    fn write_document_skips_snapshot_for_new_file() {
        let dir = tempdir().unwrap();
        let snap = tempdir().unwrap();
        let path = dir.path().join("brand-new.toml");

        let outcome =
            write_document(&path, "new = true\n".to_string(), false, snap.path()).unwrap();

        assert!(outcome.snapshot.is_none());
        assert_eq!(fs::read_to_string(&path).unwrap(), "new = true\n");
    }
}
