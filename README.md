# .sync (`dot-sync`, `ds`)

`.sync` keeps selected fields in structured config files aligned without taking
ownership of the whole file. Use `dot-sync` in scripts and docs, or `ds` as the
short interactive command.

Many config files are not cleanly owned by one source. They can mix stable
preferences with secrets, local machine paths, generated state, account data,
trust records, counters, caches, timestamps, or other fields that change often.
Committing or copying the whole file is risky: it can leak private information,
clobber local state, and create noisy diffs.

`.sync` is for partial config sync. You keep a small managed fragment containing
only the fields you care about, then sync those fields into or out of the real
application config while leaving every other field untouched.

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
current target state back into the repo source, and `push` applies the repo
source to the target environment.

## Configuration

`dot-sync` reads `dot.sync.yaml` from the current directory or the nearest parent
directory. Paths in `source` are resolved relative to that file. Paths in
`target` may use `~` for the current user's home directory.

V1 fully supports TOML targets. JSON targets are part of the format abstraction
but are not implemented yet.

## Binary Names

The canonical command is `dot-sync`. The shorter `ds` command is also provided
for interactive use.

```sh
dot-sync push codex --backup
ds push codex --backup
```
