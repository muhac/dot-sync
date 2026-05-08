use std::collections::BTreeMap;
use std::env;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow, bail};
use serde::Deserialize;

#[derive(Debug, Deserialize)]
struct RawConfig {
    targets: BTreeMap<String, RawTargetConfig>,
}

#[derive(Debug, Deserialize)]
struct RawTargetConfig {
    format: String,
    source: String,
    target: String,
    sync: Vec<String>,
    #[serde(default)]
    deny: Vec<String>,
}

#[derive(Debug)]
pub struct DotSyncConfig {
    pub targets: BTreeMap<String, TargetConfig>,
}

#[derive(Debug)]
pub struct TargetConfig {
    pub name: String,
    pub format: String,
    pub source: PathBuf,
    pub target: PathBuf,
    pub sync: Vec<String>,
    pub deny: Vec<String>,
}

impl DotSyncConfig {
    pub fn load_from_current_dir() -> Result<Self> {
        let cwd = env::current_dir().context("failed to read current directory")?;
        let config_path = find_config(&cwd)?;
        Self::load_from_path(&config_path)
    }

    pub fn load_from_path(path: &Path) -> Result<Self> {
        let root = path
            .parent()
            .ok_or_else(|| anyhow!("config path has no parent directory: {}", path.display()))?
            .to_path_buf();
        let content = fs::read_to_string(path)
            .with_context(|| format!("failed to read {}", path.display()))?;
        let raw: RawConfig = serde_yaml_ng::from_str(&content)
            .with_context(|| format!("failed to parse {}", path.display()))?;

        let mut targets = BTreeMap::new();
        for (name, raw_target) in raw.targets {
            let target = TargetConfig {
                name: name.clone(),
                format: raw_target.format,
                source: resolve_path(&root, &raw_target.source)?,
                target: resolve_path(&root, &raw_target.target)?,
                sync: raw_target.sync,
                deny: raw_target.deny,
            };
            targets.insert(name, target);
        }

        Ok(Self { targets })
    }
}

fn find_config(start: &Path) -> Result<PathBuf> {
    let mut dir = start.to_path_buf();
    loop {
        for filename in ["dot.sync.yaml", "dotctl.yaml"] {
            let candidate = dir.join(filename);
            if candidate.is_file() {
                return Ok(candidate);
            }
        }
        if !dir.pop() {
            bail!(
                "could not find dot.sync.yaml or dotctl.yaml from {}",
                start.display()
            );
        }
    }
}

fn resolve_path(root: &Path, value: &str) -> Result<PathBuf> {
    if value == "~" {
        return dirs::home_dir().ok_or_else(|| anyhow!("failed to resolve home directory"));
    }

    if let Some(rest) = value.strip_prefix("~/") {
        let home = dirs::home_dir().ok_or_else(|| anyhow!("failed to resolve home directory"))?;
        return Ok(home.join(rest));
    }

    let path = PathBuf::from(value);
    if path.is_absolute() {
        Ok(path)
    } else {
        Ok(root.join(path))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn resolves_relative_paths_from_root() {
        let root = Path::new("/repo");
        let path = resolve_path(root, "dotfiles/codex/config.toml").unwrap();
        assert_eq!(path, PathBuf::from("/repo/dotfiles/codex/config.toml"));
    }

    #[test]
    fn finds_dot_sync_config_before_legacy_dotctl_config() {
        let dir = tempdir().unwrap();
        let new_config = dir.path().join("dot.sync.yaml");
        let legacy_config = dir.path().join("dotctl.yaml");
        fs::write(&new_config, "targets: {}\n").unwrap();
        fs::write(&legacy_config, "targets: {}\n").unwrap();

        let found = find_config(dir.path()).unwrap();

        assert_eq!(found, new_config);
    }

    #[test]
    fn load_from_path_reports_actual_path_without_parent() {
        let err = DotSyncConfig::load_from_path(Path::new("/")).unwrap_err();

        assert!(err.to_string().contains("config path has no parent directory: /"));
    }
}
