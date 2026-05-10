use std::cmp::Ordering;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow, bail};

use crate::config::{DotSyncConfig, TargetConfig};
use crate::sync::{atomic_write, sanitize_for_filename, snapshot_existing};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Side {
    Source,
    Target,
}

impl Side {
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
}

#[derive(Debug, Clone, Copy)]
pub enum Pick {
    Newest,
    Index(usize),
    AtPrefix,
}

#[derive(Debug)]
pub struct RestoreOptions<'a> {
    pub side: Side,
    pub pick: Pick,
    pub at: Option<&'a str>,
    pub list_only: bool,
    pub dry_run: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SnapshotKind {
    Recovery,
    Backup,
}

impl SnapshotKind {
    fn tag(self) -> &'static str {
        match self {
            Self::Recovery => "recovery",
            Self::Backup => "backup",
        }
    }
}

#[derive(Debug)]
struct Snapshot {
    kind: SnapshotKind,
    path: PathBuf,
    timestamp: String,
}

pub fn run(config: &DotSyncConfig, name: Option<&str>, opts: RestoreOptions<'_>) -> Result<()> {
    let target = config
        .targets
        .get(name.ok_or_else(|| anyhow!("restore requires a target name"))?)
        .ok_or_else(|| anyhow!("unknown target: {}", name.unwrap()))?;

    let snapshot_dir = std::env::temp_dir().join("dot-sync");
    let dest = opts.side.path(target);
    let candidates = list_snapshots(dest, &snapshot_dir)?;

    println!(
        "{} restore {} ({})",
        target.name,
        opts.side.label(),
        dest.display()
    );

    if candidates.is_empty() {
        println!("  No snapshots available.");
        bail!("no snapshots found for {}", dest.display());
    }

    print_candidates(&candidates);

    if opts.list_only {
        return Ok(());
    }

    let chosen = pick_snapshot(&candidates, opts.pick, opts.at)?;
    println!(
        "  selected: [{}] {}  {}",
        chosen.kind.tag(),
        chosen.timestamp,
        chosen.path.display()
    );

    if opts.dry_run {
        println!("  dry run: no files written");
        return Ok(());
    }

    let content = fs::read_to_string(&chosen.path)
        .with_context(|| format!("failed to read snapshot {}", chosen.path.display()))?;

    let pre_restore = snapshot_existing(dest, &snapshot_dir)?;
    atomic_write(dest, &content)?;

    println!("  wrote {}: {}", opts.side.label(), dest.display());
    if let Some(snapshot) = pre_restore {
        println!("    recovery: {}", snapshot.display());
    }
    Ok(())
}

fn print_candidates(candidates: &[Snapshot]) {
    println!("  candidates ({}):", candidates.len());
    for (i, snap) in candidates.iter().enumerate() {
        println!(
            "    {:>2}  [{:8}] {}  {}",
            i + 1,
            snap.kind.tag(),
            snap.timestamp,
            snap.path.display()
        );
    }
}

fn pick_snapshot<'a>(
    candidates: &'a [Snapshot],
    pick: Pick,
    at: Option<&str>,
) -> Result<&'a Snapshot> {
    match (pick, at) {
        (Pick::Index(n), _) => {
            if n == 0 || n > candidates.len() {
                bail!("--pick {n} is out of range (1..={})", candidates.len());
            }
            Ok(&candidates[n - 1])
        }
        (Pick::AtPrefix, Some(prefix)) => {
            let matches: Vec<&Snapshot> = candidates
                .iter()
                .filter(|s| s.timestamp.starts_with(prefix))
                .collect();
            match matches.len() {
                0 => bail!("--at {prefix} matched no snapshot"),
                1 => Ok(matches[0]),
                _ => {
                    // Already sorted: tie-breaker prefers backup over recovery
                    // when timestamps tie.
                    Ok(matches[0])
                }
            }
        }
        (Pick::AtPrefix, None) => unreachable!("AtPrefix requires --at value"),
        (Pick::Newest, _) => Ok(&candidates[0]),
    }
}

fn list_snapshots(dest: &Path, snapshot_dir: &Path) -> Result<Vec<Snapshot>> {
    let mut out = Vec::new();
    collect_recovery(dest, snapshot_dir, &mut out)?;
    collect_backup(dest, &mut out)?;
    out.sort_by(|a, b| {
        b.timestamp
            .cmp(&a.timestamp)
            .then_with(|| match (a.kind, b.kind) {
                (SnapshotKind::Backup, SnapshotKind::Recovery) => Ordering::Less,
                (SnapshotKind::Recovery, SnapshotKind::Backup) => Ordering::Greater,
                _ => Ordering::Equal,
            })
    });
    Ok(out)
}

fn collect_recovery(dest: &Path, snapshot_dir: &Path, out: &mut Vec<Snapshot>) -> Result<()> {
    if !snapshot_dir.exists() {
        return Ok(());
    }
    let stem = sanitize_for_filename(dest);
    let prefix = format!("{stem}.");
    for entry in fs::read_dir(snapshot_dir)
        .with_context(|| format!("failed to read {}", snapshot_dir.display()))?
    {
        let entry = entry?;
        let name = entry.file_name();
        let name_str = name.to_string_lossy();
        let Some(rest) = name_str.strip_prefix(&prefix) else {
            continue;
        };
        let Some(timestamp) = parse_timestamp(rest) else {
            continue;
        };
        out.push(Snapshot {
            kind: SnapshotKind::Recovery,
            path: entry.path(),
            timestamp,
        });
    }
    Ok(())
}

fn collect_backup(dest: &Path, out: &mut Vec<Snapshot>) -> Result<()> {
    let Some(parent) = dest.parent() else {
        return Ok(());
    };
    let Some(file_name) = dest.file_name().and_then(|n| n.to_str()) else {
        return Ok(());
    };
    if !parent.exists() {
        return Ok(());
    }
    let prefix = format!("{file_name}.bak.");
    for entry in
        fs::read_dir(parent).with_context(|| format!("failed to read {}", parent.display()))?
    {
        let entry = entry?;
        let name = entry.file_name();
        let name_str = name.to_string_lossy();
        let Some(rest) = name_str.strip_prefix(&prefix) else {
            continue;
        };
        let Some(timestamp) = parse_timestamp(rest) else {
            continue;
        };
        out.push(Snapshot {
            kind: SnapshotKind::Backup,
            path: entry.path(),
            timestamp,
        });
    }
    Ok(())
}

fn parse_timestamp(rest: &str) -> Option<String> {
    // Accepts "<timestamp>" or "<timestamp>.<index>". The timestamp itself is
    // the format chrono emits as "%Y%m%d-%H%M%S" — 15 chars including the dash.
    let candidate = rest.split('.').next()?;
    if candidate.len() != 15 || !candidate.contains('-') {
        return None;
    }
    Some(candidate.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn touch(path: &Path, content: &str) {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(path, content).unwrap();
    }

    #[test]
    fn list_merges_recovery_and_backup_sorted_newest_first() {
        let live_dir = tempdir().unwrap();
        let snap_dir = tempdir().unwrap();
        let dest = live_dir.path().join("config.toml");

        let stem = sanitize_for_filename(&dest);
        touch(
            &snap_dir.path().join(format!("{stem}.20260101-100000")),
            "old",
        );
        touch(
            &snap_dir.path().join(format!("{stem}.20260301-100000")),
            "newer recovery",
        );
        touch(
            &live_dir.path().join("config.toml.bak.20260201-100000"),
            "backup",
        );

        let snaps = list_snapshots(&dest, snap_dir.path()).unwrap();
        assert_eq!(snaps.len(), 3);
        assert_eq!(snaps[0].timestamp, "20260301-100000");
        assert_eq!(snaps[0].kind, SnapshotKind::Recovery);
        assert_eq!(snaps[1].timestamp, "20260201-100000");
        assert_eq!(snaps[1].kind, SnapshotKind::Backup);
        assert_eq!(snaps[2].timestamp, "20260101-100000");
    }

    #[test]
    fn tie_break_prefers_backup_over_recovery() {
        let live_dir = tempdir().unwrap();
        let snap_dir = tempdir().unwrap();
        let dest = live_dir.path().join("config.toml");

        let stem = sanitize_for_filename(&dest);
        touch(
            &snap_dir.path().join(format!("{stem}.20260101-100000")),
            "recovery",
        );
        touch(
            &live_dir.path().join("config.toml.bak.20260101-100000"),
            "backup",
        );

        let snaps = list_snapshots(&dest, snap_dir.path()).unwrap();
        assert_eq!(snaps.len(), 2);
        assert_eq!(snaps[0].kind, SnapshotKind::Backup);
        assert_eq!(snaps[1].kind, SnapshotKind::Recovery);
    }

    #[test]
    fn pick_index_out_of_range_errors() {
        let snaps = vec![Snapshot {
            kind: SnapshotKind::Recovery,
            path: PathBuf::from("/tmp/foo"),
            timestamp: "20260101-100000".to_string(),
        }];
        let err = pick_snapshot(&snaps, Pick::Index(2), None).unwrap_err();
        assert!(err.to_string().contains("out of range"));
    }

    #[test]
    fn pick_at_prefix_matches_unique_timestamp() {
        let snaps = vec![
            Snapshot {
                kind: SnapshotKind::Recovery,
                path: PathBuf::from("/tmp/a"),
                timestamp: "20260301-100000".to_string(),
            },
            Snapshot {
                kind: SnapshotKind::Recovery,
                path: PathBuf::from("/tmp/b"),
                timestamp: "20260201-100000".to_string(),
            },
        ];
        let chosen = pick_snapshot(&snaps, Pick::AtPrefix, Some("202603")).unwrap();
        assert_eq!(chosen.timestamp, "20260301-100000");
    }

    #[test]
    fn pick_at_prefix_no_match_errors() {
        let snaps = vec![Snapshot {
            kind: SnapshotKind::Recovery,
            path: PathBuf::from("/tmp/a"),
            timestamp: "20260301-100000".to_string(),
        }];
        let err = pick_snapshot(&snaps, Pick::AtPrefix, Some("20240101")).unwrap_err();
        assert!(err.to_string().contains("matched no snapshot"));
    }

    #[test]
    fn parse_timestamp_rejects_malformed() {
        assert_eq!(
            parse_timestamp("20260101-120000"),
            Some("20260101-120000".into())
        );
        assert_eq!(
            parse_timestamp("20260101-120000.1"),
            Some("20260101-120000".into())
        );
        assert_eq!(parse_timestamp("notreally"), None);
        assert_eq!(parse_timestamp("20260101120000"), None);
    }
}
