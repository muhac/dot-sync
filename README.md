# dot-sync

`ds` manages app-owned structured configuration files without taking over
the whole file.

The dotfiles repository already handles ordinary dotfiles well with symlinks.
`ds` is for configuration files that applications also edit themselves,
such as Codex TOML config or Claude JSON settings. These files often mix stable
preferences with local state, trust records, counters, paths, accounts, or other
private data. Managing the whole file directly can leak local information into
Git and create noisy diffs.

The goal is to sync only explicitly selected fields between the repository and
the real app config, while leaving every other field untouched.

## Concepts

- `source`: repo-managed sync fragment.
- `target`: real app config used by the application.
- `sync`: fields that move both ways.
- `deny`: fields that must never be managed.

Example:

```yaml
targets:
  codex:
    format: toml
    source: dotfiles/codex/.codex/config.sync.toml
    target: ~/.codex/config.toml
    sync:
      - project_doc_fallback_filenames
      - project_doc_max_bytes
      - developer_instructions
      - tui.notification_condition
      - tui.status_line
      - tui.theme
      - plugins."github@openai-curated".enabled
    deny:
      - projects
      - tui.model_availability_nux
```

## Commands

```sh
ds pull codex
ds push codex
ds sync codex
```

The target name is optional. If omitted, `ds` operates on all configured
targets.

```sh
ds pull
ds push
ds sync
```

All commands support `--dry-run` to show planned changes without writing either
file. Real writes do not create backups by default; pass `--backup` to create a
timestamped backup before writing.

```sh
ds pull codex --dry-run
ds push codex --backup
ds sync --dry-run
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
- fields matching `deny` must not appear in `source`.

This keeps app-written local state safe while still allowing repo-managed
preferences to be shared across machines.

The direction names mirror deployment-style workflows: `pull` brings the
current target state back into the repo source, and `push` applies the repo
source to the target environment.

## Configuration

`ds` reads `dot.sync.yaml` from the current directory or the nearest parent
directory. Paths in `source` are resolved relative to that file. Paths in
`target` may use `~` for the current user's home directory.

V1 fully supports TOML targets. JSON targets are part of the format abstraction
but are not implemented yet.

## Installer Integration

The published installer downloads the prebuilt `ds` binary from GitHub
Pages into a temporary directory when `ds` is not already available on
`PATH`, then runs:

```sh
ds push codex --backup
```

This applies repo-managed Codex fields from
`dotfiles/codex/.codex/config.sync.toml` into the real
`~/.codex/config.toml` while preserving local-only state such as model provider
tokens, trusted project paths, counters, and other app-owned fields.

The temporary binary is removed when the installer exits. Set
`DOWNLOAD_DOTCTL=0` to skip the fallback download, or `RUN_DOTCTL=0` to skip
the config push during remote install.
