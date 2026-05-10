use std::fs;

use anyhow::{Context, Result, anyhow, bail};

use crate::config::{DotSyncConfig, TargetConfig};
use crate::document::{
    Document, EnvDocument, Format, GitConfigDocument, JsonDocument, TomlDocument, parse_format,
};
use crate::path::FieldPath;

pub fn run(config: &DotSyncConfig, name: Option<&str>) -> Result<()> {
    let targets = select_targets(config, name)?;
    println!("Config: {}", config.path.display());
    println!();

    let mut has_error = false;
    for target in targets {
        let status = inspect_target(target);
        if status.has_errors() {
            has_error = true;
        }
        status.print();
    }

    if has_error {
        bail!("status found errors");
    }
    Ok(())
}

fn select_targets<'a>(
    config: &'a DotSyncConfig,
    name: Option<&str>,
) -> Result<Vec<&'a TargetConfig>> {
    if let Some(name) = name {
        let available = config
            .targets
            .keys()
            .map(String::as_str)
            .collect::<Vec<_>>()
            .join(", ");
        let target = config
            .targets
            .get(name)
            .ok_or_else(|| anyhow!("unknown target: {name}; available targets: {available}"))?;
        Ok(vec![target])
    } else {
        Ok(config.targets.values().collect())
    }
}

#[derive(Debug)]
struct TargetStatus {
    name: String,
    format: String,
    source: String,
    target: String,
    fields: usize,
    warnings: Vec<String>,
    errors: Vec<String>,
}

impl TargetStatus {
    fn has_errors(&self) -> bool {
        !self.errors.is_empty()
    }

    fn label(&self) -> &'static str {
        if !self.errors.is_empty() {
            "error"
        } else if !self.warnings.is_empty() {
            "warn"
        } else {
            "ok"
        }
    }

    fn print(&self) {
        println!(
            "{} {} {} source={} target={} fields={}",
            self.name,
            self.label(),
            self.format,
            self.source,
            self.target,
            self.fields
        );
        for warning in &self.warnings {
            println!("  warn: {warning}");
        }
        for error in &self.errors {
            println!("  error: {error}");
        }
    }
}

fn inspect_target(target: &TargetConfig) -> TargetStatus {
    let mut status = TargetStatus {
        name: target.name.clone(),
        format: target.format.clone(),
        source: target.source.display().to_string(),
        target: target.target.display().to_string(),
        fields: target.sync.len(),
        warnings: Vec::new(),
        errors: Vec::new(),
    };

    let format = match parse_format(&target.format) {
        Ok(format) => format,
        Err(error) => {
            status.errors.push(format!(
                "target '{}' uses format '{}': {error}",
                target.name, target.format
            ));
            return status;
        }
    };

    match format {
        Format::Toml => inspect_target_typed::<TomlDocument>(target, &mut status),
        Format::Json => inspect_target_typed::<JsonDocument>(target, &mut status),
        Format::GitConfig => inspect_target_typed::<GitConfigDocument>(target, &mut status),
        Format::Env => inspect_target_typed::<EnvDocument>(target, &mut status),
    }

    status
}

fn inspect_target_typed<D: Document>(target: &TargetConfig, status: &mut TargetStatus) {
    let parsed_paths = target
        .sync
        .iter()
        .filter_map(|raw| match FieldPath::parse(raw) {
            Ok(path) => Some((raw, path)),
            Err(error) => {
                status.errors.push(format!(
                    "invalid sync path '{}' in target '{}': {error}; quote path segments that contain dots, for example plugins.\"github@openai-curated\".enabled",
                    raw, target.name
                ));
                None
            }
        })
        .collect::<Vec<_>>();

    let source = inspect_document::<D>(DocumentRole::Source, target, status);
    let target_doc = inspect_document::<D>(DocumentRole::Target, target, status);

    for (raw, path) in parsed_paths {
        if let Some(doc) = source.as_ref()
            && let Some(conflict) = doc.table_conflict(&path)
        {
            status.warnings.push(format!(
                "source path '{}' needs '{}' to be a table, but it is {}",
                raw, conflict.path, conflict.kind
            ));
        }
        if let Some(doc) = target_doc.as_ref()
            && let Some(conflict) = doc.table_conflict(&path)
        {
            status.warnings.push(format!(
                "target path '{}' needs '{}' to be a table, but it is {}",
                raw, conflict.path, conflict.kind
            ));
        }
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

    fn path(self, target: &TargetConfig) -> &std::path::Path {
        match self {
            Self::Source => &target.source,
            Self::Target => &target.target,
        }
    }
}

fn inspect_document<D: Document>(
    role: DocumentRole,
    target: &TargetConfig,
    status: &mut TargetStatus,
) -> Option<D> {
    let path = role.path(target);
    let label = role.label();

    if !path.exists() {
        status
            .warnings
            .push(format!("{label} file does not exist: {}", path.display()));
        return None;
    }

    if let Err(error) =
        fs::metadata(path).with_context(|| format!("failed to stat {}", path.display()))
    {
        status.errors.push(format!(
            "failed to inspect {label} file {}: {error:#}",
            path.display()
        ));
        return None;
    }

    match D::load(path, false) {
        Ok(doc) => Some(doc),
        Err(error) => {
            status.errors.push(format!(
                "failed to read {label} file {} for target '{}': {error:#}",
                path.display(),
                target.name
            ));
            None
        }
    }
}
