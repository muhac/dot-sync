//! `dot-sync add` — bootstrap or extend `.sync.yaml`.
//!
//! Two execution paths:
//!
//! 1. **Non-interactive** — `--field` flags supplied. Skip the picker;
//!    just validate paths and write.
//! 2. **Interactive** — no `--field`. Load source / target documents
//!    (whichever exists), discover their structure, run the TUI picker,
//!    capture user's selections.
//!
//! In both paths the command must end with: a parsed list of valid
//! sync paths, a target name, format, source path, and target path.
//! YAML write is a lossy round-trip via `serde_yaml_ng` — user-added
//! comments in `.sync.yaml` are not preserved (documented limitation).

use std::collections::BTreeMap;
use std::env;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow, bail};
use serde::{Deserialize, Serialize};

use crate::cli::AddArgs;
use crate::discovery::FieldTree;
use crate::document::{
    Document, Format, GitConfigDocument, JsonDocument, TomlDocument, parse_format,
};
use crate::path::FieldPath;
use crate::picker::{self, PickerOutcome};

pub fn run(args: AddArgs) -> Result<()> {
    let cwd = env::current_dir().context("failed to read current directory")?;
    let config_path = locate_or_bootstrap(&cwd)?;
    let config_dir = config_path
        .parent()
        .ok_or_else(|| anyhow!("config path has no parent: {}", config_path.display()))?
        .to_path_buf();

    let mut yaml = YamlConfig::load(&config_path)?;
    let existing = yaml.targets.get(&args.name).cloned();

    let format_str = resolve_format(&args, existing.as_ref())?;
    let source_str = resolve_source(&args, existing.as_ref())?;
    let target_str = resolve_target(&args, existing.as_ref())?;
    let fmt =
        parse_format(&format_str).with_context(|| format!("--format {format_str:?} is invalid"))?;

    let source_abs = resolve_path(&config_dir, &source_str);
    let target_abs = resolve_path(&config_dir, &target_str);

    let new_paths = if args.fields.is_empty() {
        // Interactive: load whichever side exists, discover, run picker.
        let tree = build_tree(fmt, &source_abs, &target_abs)?;
        match picker::run(&args.name, tree)? {
            PickerOutcome::Confirmed(paths) => paths,
            // Bubble cancellation up as an error so the process exits
            // non-zero through the normal `Result` path. Going through
            // `std::process::exit` would skip every `Drop` higher in the
            // stack and is inconsistent with the rest of the codebase.
            PickerOutcome::Cancelled => bail!("cancelled, no changes made"),
        }
    } else {
        let mut out = Vec::with_capacity(args.fields.len());
        for raw in &args.fields {
            let parsed = FieldPath::parse(raw)
                .with_context(|| format!("invalid sync path '{raw}' in --field"))?;
            out.push(parsed);
        }
        out
    };

    if new_paths.is_empty() {
        let prefix = if args.dry_run { "dry run: " } else { "" };
        eprintln!("{prefix}no fields selected — .sync.yaml left unchanged");
        return Ok(());
    }

    let new_path_strings: Vec<String> = new_paths.iter().map(|p| p.to_string()).collect();

    // Merge: existing target (if any) gets new paths appended (dedupe);
    // new target gets a fresh entry.
    let mut entry = existing.clone().unwrap_or_else(|| RawTargetConfig {
        format: format_str.clone(),
        source: source_str.clone(),
        target: target_str.clone(),
        sync: Vec::new(),
    });
    // For an existing target, format / paths are not changed even if
    // the user passed --format / --source / --target — those flags only
    // serve as fallbacks when the target is new. Keep it deterministic
    // and warn the user so it's not a silent surprise.
    if existing.is_some() {
        let mut ignored: Vec<&str> = Vec::new();
        if args.format.is_some() {
            ignored.push("--format");
        }
        if args.source.is_some() {
            ignored.push("--source");
        }
        if args.target.is_some() {
            ignored.push("--target");
        }
        if !ignored.is_empty() {
            eprintln!(
                "warning: target '{}' already exists; {} ignored (use a different target name to create a fresh entry)",
                args.name,
                ignored.join(" / "),
            );
        }
    }
    let mut added = 0usize;
    for path in new_path_strings {
        if !entry.sync.iter().any(|p| p == &path) {
            entry.sync.push(path);
            added += 1;
        }
    }
    yaml.targets.insert(args.name.clone(), entry);

    if args.dry_run {
        let preview = yaml.serialize()?;
        println!("dry run: would write to {}", config_path.display());
        println!("---");
        print!("{preview}");
        return Ok(());
    }

    yaml.write(&config_path)?;
    let action = if existing.is_some() {
        format!(
            "appended {added} field(s) to existing target '{}'",
            args.name
        )
    } else {
        format!(
            "added target '{}' ({format_str}) with {added} field(s)",
            args.name
        )
    };
    println!("{action} in {}", config_path.display());
    Ok(())
}

fn locate_or_bootstrap(cwd: &Path) -> Result<PathBuf> {
    // Walk upward looking for an existing `.sync.yaml`. If none found,
    // create one in the current directory.
    let mut dir = cwd.to_path_buf();
    loop {
        let candidate = dir.join(".sync.yaml");
        if candidate.is_file() {
            return Ok(candidate);
        }
        if !dir.pop() {
            break;
        }
    }
    let new_path = cwd.join(".sync.yaml");
    fs::write(&new_path, "targets: {}\n")
        .with_context(|| format!("failed to bootstrap {}", new_path.display()))?;
    println!("created {}", new_path.display());
    Ok(new_path)
}

fn resolve_format(args: &AddArgs, existing: Option<&RawTargetConfig>) -> Result<String> {
    if let Some(f) = args.format.clone() {
        return Ok(f);
    }
    if let Some(e) = existing {
        return Ok(e.format.clone());
    }
    let inferred = match (
        args.source.as_deref().and_then(infer_format_from_path),
        args.target.as_deref().and_then(infer_format_from_path),
    ) {
        (Some(a), Some(b)) if a == b => Some(a),
        (Some(a), Some(b)) if a != b => bail!(
            "source and target have conflicting format extensions ({a} vs {b}); pass --format explicitly"
        ),
        (Some(a), None) | (None, Some(a)) => Some(a),
        _ => None,
    };
    inferred
        .map(|s| s.to_string())
        .ok_or_else(|| anyhow!("--format is required when source/target extension is unknown"))
}

fn infer_format_from_path(p: &str) -> Option<&'static str> {
    let ext = Path::new(p).extension()?.to_str()?;
    match ext {
        "toml" => Some("toml"),
        "json" => Some("json"),
        "jsonc" => Some("jsonc"),
        "gitconfig" => Some("gitconfig"),
        _ => None,
    }
}

fn resolve_source(args: &AddArgs, existing: Option<&RawTargetConfig>) -> Result<String> {
    args.source
        .clone()
        .or_else(|| existing.map(|e| e.source.clone()))
        .ok_or_else(|| anyhow!("--source is required when adding a new target"))
}

fn resolve_target(args: &AddArgs, existing: Option<&RawTargetConfig>) -> Result<String> {
    args.target
        .clone()
        .or_else(|| existing.map(|e| e.target.clone()))
        .ok_or_else(|| anyhow!("--target is required when adding a new target"))
}

/// Mirror of `config::resolve_path` — kept local so tests don't need
/// the full config crate. Same `~` and relative-to-config-dir rules.
fn resolve_path(root: &Path, value: &str) -> PathBuf {
    if value == "~" {
        return dirs::home_dir().unwrap_or_else(|| PathBuf::from("~"));
    }
    if let Some(rest) = value.strip_prefix("~/")
        && let Some(home) = dirs::home_dir()
    {
        return home.join(rest);
    }
    let p = PathBuf::from(value);
    if p.is_absolute() { p } else { root.join(p) }
}

fn build_tree(fmt: Format, source: &Path, target: &Path) -> Result<FieldTree> {
    // Prefer source for discovery (typically the managed canonical
    // shape); fall back to target if source doesn't exist yet.
    let chosen = if source.exists() {
        source
    } else if target.exists() {
        target
    } else {
        bail!(
            "neither source ({}) nor target ({}) exists yet — pass --field arguments instead",
            source.display(),
            target.display()
        );
    };
    match fmt {
        Format::Toml => {
            let doc = TomlDocument::load(chosen, false)?;
            Ok(doc.discover_field_tree())
        }
        Format::Json => {
            let doc = JsonDocument::load(chosen, false)?;
            Ok(doc.discover_field_tree())
        }
        Format::GitConfig => {
            let doc = GitConfigDocument::load(chosen, false)?;
            Ok(doc.discover_field_tree())
        }
    }
}

// ----- YAML round-trip (lossy) -----

#[derive(Debug, Default, Serialize, Deserialize)]
struct YamlConfig {
    /// Default to an empty map so partial configs (e.g. a file with only
    /// `version: 1` and no `targets:` key yet) parse cleanly. The user
    /// can always add targets via this very command.
    #[serde(default)]
    targets: BTreeMap<String, RawTargetConfig>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct RawTargetConfig {
    format: String,
    source: String,
    target: String,
    sync: Vec<String>,
}

impl YamlConfig {
    fn load(path: &Path) -> Result<Self> {
        let content = fs::read_to_string(path)
            .with_context(|| format!("failed to read {}", path.display()))?;
        if content.trim().is_empty() {
            return Ok(Self::default());
        }
        let parsed: YamlConfig = serde_yaml_ng::from_str(&content)
            .with_context(|| format!("failed to parse {}", path.display()))?;
        Ok(parsed)
    }

    fn serialize(&self) -> Result<String> {
        serde_yaml_ng::to_string(self).context("failed to serialize updated .sync.yaml")
    }

    fn write(&self, path: &Path) -> Result<()> {
        let s = self.serialize()?;
        fs::write(path, s).with_context(|| format!("failed to write {}", path.display()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn infer_format_picks_known_extensions() {
        assert_eq!(infer_format_from_path("a.toml"), Some("toml"));
        assert_eq!(infer_format_from_path("a.json"), Some("json"));
        assert_eq!(infer_format_from_path("a.jsonc"), Some("jsonc"));
        assert_eq!(infer_format_from_path("a.gitconfig"), Some("gitconfig"));
        assert_eq!(infer_format_from_path("a.yaml"), None);
        assert_eq!(infer_format_from_path("noext"), None);
    }

    #[test]
    fn yaml_round_trip_preserves_known_fields() {
        let yaml = r#"
targets:
  codex:
    format: toml
    source: src.toml
    target: tgt.toml
    sync:
      - tui.theme
"#;
        let parsed: YamlConfig = serde_yaml_ng::from_str(yaml).unwrap();
        let serialized = parsed.serialize().unwrap();
        // Re-parse to make sure we round-trip semantically.
        let reparsed: YamlConfig = serde_yaml_ng::from_str(&serialized).unwrap();
        assert_eq!(reparsed.targets["codex"].format, "toml");
        assert_eq!(reparsed.targets["codex"].sync, vec!["tui.theme"]);
    }
}
