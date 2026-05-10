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
      - mcp_servers[name="github"].enabled    # specific item by key
      - mcp_servers[name].enabled              # all items, paired by key
```

### Path syntax

| Form | Meaning |
| --- | --- |
| `tui.theme` | Plain object navigation. |
| `plugins."github@openai-curated".enabled` | Quoted segment, for keys with `.` `[` or whitespace. |
| `arr[name="github"].enabled` | Pin to the array item where `name == "github"`; sync just its `enabled` field. Stable across reorderings. |
| `arr[name].enabled` | Wildcard: fan out across every item in `arr`, pairing source / target items by `name`. |

Pinned and wildcard selectors:

- The identifier value (`"github"`) is matched as a string. Numeric / boolean
  identifiers are not yet supported.
- When the identifier matches an item that exists on one side but not the other,
  the missing side gets a new array entry seeded with the identifier — the
  "fill missing" rule from `pull`/`sync` extended to array members.
- Writes go into TOML `[[arrays]]` form. Inline `arr = [{...}]` arrays are
  read-only for now.
- Plain index syntax (`arr[0]`) is intentionally not supported — array
  positions shift when data changes, so index-based sync is destructive in
  the cases that matter most.

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
file. Pass `--backup` to also keep a persistent timestamped copy
(`<file>.bak.<timestamp>`) next to the destination.

Every real write is atomic (write to a hidden tmp next to the destination, then
`rename`). Before each overwrite, the previous contents are copied to
`$TMPDIR/dot-sync/<sanitized-path>.<timestamp>` so you can recover
in-the-moment without cluttering the working directory. The path is printed
under each `wrote` line; the OS reclaims the snapshots over time.

```sh
dot-sync pull codex --dry-run
dot-sync push codex --backup
dot-sync sync --dry-run
```

`sync` accepts mutually exclusive conflict-mode flags (default: `--target-wins`):

```sh
dot-sync sync codex --target-wins        # (default) target value wins on conflict
dot-sync sync codex --source-wins        # source value wins on conflict
dot-sync sync codex --fail-on-conflict   # exit non-zero, write nothing, list conflicts
```

`pull` is always target-wins by definition; `push` is always source-wins.

## Recovering from a bad write

Every real write captures the previous contents into
`$TMPDIR/dot-sync/<sanitized>.<timestamp>` and prints the path next to the
write. Use `restore` to roll back from there or from any persistent
`<file>.bak.<timestamp>` that `--backup` produced earlier — both pools are
listed together, sorted newest first.

```sh
dot-sync restore codex --list           # show numbered candidates (recovery + backup)
dot-sync restore codex                  # restore newest snapshot of target
dot-sync restore codex --pick 3         # restore the 3rd candidate from the list
dot-sync restore codex --at 20260510-15 # restore by timestamp prefix
dot-sync restore codex --source         # restore source instead of target
dot-sync restore codex --dry-run        # show what would happen
```

The restore itself is atomic and takes a fresh recovery snapshot of the
file before overwriting, so an unwanted restore is itself recoverable.
When timestamps tie, persistent `[backup]` entries are preferred over
`[recovery]` (they are an explicit user signal).

All three commands share a single rule: **only fields listed in `sync` are
touched, and nothing is ever removed**. Fields outside `sync` are preserved on
both sides. `pull` and `push` are mirror images; `sync` is exactly their union.

| State of a listed field            | `pull` (target → source) | `push` (source → target) | `sync` (both ways, default `--target-wins`) |
| ---------------------------------- | ------------------------ | ------------------------ | ------------------------- |
| Both sides equal                   | skip                     | skip                     | skip                      |
| Both sides differ                  | source := target         | target := source         | source := target (mode-dependent) |
| Only target has it                 | source := target (add)   | skip                     | source := target (add)    |
| Only source has it                 | skip                     | target := source (add)   | target := source (add)    |
| Neither has it                     | skip                     | skip                     | skip                      |
| Field not in `sync:` list          | untouched                | untouched                | untouched                 |

Under `--source-wins`, "Both sides differ" flips to `target := source`. Under
`--fail-on-conflict`, the same row aborts with a non-zero exit and no writes.
Other rows are unaffected by mode — "missing on one side" always fills, never
fails.

To stop syncing a field, remove it from `sync:` in `.sync.yaml`. The tool
will not delete it from either file — clean up by hand if you want it gone.

The direction names mirror deployment-style workflows: `pull` brings the
current target state back into the managed source, and `push` applies the
managed source to the target environment. `sync` is convenient when you do
not care which side is more up to date and just want both files to agree on
the listed fields.
