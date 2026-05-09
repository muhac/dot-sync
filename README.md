# .sync — surgical config sync

Sync only the parts of your config that matter.

Keep your preferences. Ignore secrets, local state, and noise.

## Use cases

### AI tool configs

Sync your Codex or Claude settings across machines, without leaking API keys or local trust state.

### Noisy config files

Many tools write back to their own config files (timestamps, caches, counters).
`.sync` lets you keep only the stable parts in Git.

### Multi-machine setup

Keep your environment consistent across machines, while preserving local paths, accounts, and secrets.

## Quick start

Install the latest stable release:

```sh
curl -fsSL https://raw.githubusercontent.com/muhac/dot-sync/main/install.sh | sh
```

Install the nightly prerelease:

```sh
curl -fsSL https://raw.githubusercontent.com/muhac/dot-sync/main/install.sh | sh -s -- --nightly
```

Install a specific version or directory:

```sh
curl -fsSL https://raw.githubusercontent.com/muhac/dot-sync/main/install.sh | sh -s -- --version v0.1.0
curl -fsSL https://raw.githubusercontent.com/muhac/dot-sync/main/install.sh | sh -s -- --dir ~/.local/bin
```

The installer always installs `dot-sync`. It installs the shorter `ds` alias
only when that path is empty or already points to an existing `dot-sync`
installation, so it will not overwrite an unrelated `ds` command.

```sh
dot-sync sync  # or: ds sync
```

`.sync` keeps selected fields in structured config files aligned without taking
ownership of the whole file. Use `dot-sync` in scripts and docs, or `ds` as the
short interactive command.

## Configuration

`dot-sync` reads `.sync.yaml` from the current directory or the nearest parent
directory. Paths in `source` are resolved relative to that file. Paths in
`target` may use `~` for the current user's home directory.

**Supporting formats:**
- **TOML** (Format-preserving via `toml_edit`)

## Concepts

- `source`: managed sync fragment.
- `target`: real app config used by the application.
- `sync`: fields that move both ways.

Example:

```yaml
targets:
  codex:
    format: toml
    source: codex.sync.toml
    target: ~/.codex/config.toml
    sync:
      - project_doc_fallback_filenames
      - project_doc_max_bytes
      - developer_instructions
      - tui.notification_condition
      - tui.status_line
      - tui.theme
      - plugins."github@openai-curated".enabled
```

## Commands

```sh
dot-sync pull codex
dot-sync push codex
dot-sync sync codex
```

The target name is optional. If omitted, `dot-sync` operates on all configured
targets.

```sh
dot-sync pull
dot-sync push
dot-sync sync
```

All commands support `--dry-run` to show planned changes without writing either
file. Real writes do not create backups by default; pass `--backup` to create a
timestamped backup before writing.

```sh
dot-sync pull codex --dry-run
dot-sync push codex --backup
dot-sync sync --dry-run
```

`pull` reads from `target` and updates `source`.

```text
target -> source
```

Only fields listed in `sync` are extracted from `target` into `source`.

`push` reads from `source` and updates `target`.

```text
source -> target
```

Only fields listed in `sync` are written to `target`. All other fields already
present in `target` are preserved.

`sync` does both directions.

```text
target -> source -> target
```

The conflict rule is fixed:

- `target` wins for fields that exist in both files.
- `source` fills missing `sync` fields in `target`.
- fields outside `sync` are not touched.

This keeps app-written local state safe while still allowing managed preferences
to be shared across machines, environments, or projects.

The direction names mirror deployment-style workflows: `pull` brings the
current target state back into the managed source, and `push` applies the
managed source to the target environment.
