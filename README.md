# dot-sync — surgical config sync

Sync specific fields across machines — not whole files.

Share `alias.co`, `NODE_VERSION`, `core.editor`; keep `user.email`,
`OPENAI_API_KEY`, and per-machine state local. Comments and
formatting in the target file round-trip byte-stable.

Supports **TOML**, **JSON / JSONC**, **gitconfig**, and **`.env`**.

## Install

```sh
curl -fsSL https://raw.githubusercontent.com/muhac/dot-sync/main/install.sh | sh
```

Installs `dot-sync` (and a `ds` shortcut, if `~/.local/bin/ds` is
unused). Other install flavors — nightly, pinned version, alternate
directory, shell completions — are at the bottom of this file.

## Quick start

Pick something simple — `~/.gitconfig` aliases:

```sh
# Source: the version you want shared across machines
cat > git.sync.gitconfig <<'EOF'
[alias]
    co = checkout
[core]
    editor = nvim
EOF

# Register the target — writes a .sync.yaml in the current directory
dot-sync add git \
  --source git.sync.gitconfig \
  --target ~/.gitconfig \
  --field alias.co --field core.editor

# Push: writes those two fields into ~/.gitconfig, leaves the rest untouched
dot-sync push git
```

That's it. On another machine, after `dot-sync add` (or after checking
out the same `.sync.yaml` + source), `dot-sync push git` applies the
same two fields. The reverse direction — `dot-sync pull git` — copies
whatever's currently in `~/.gitconfig` back into the source so you
can commit the new state.

Add `--dry-run` to any command for a no-write preview.

## Adding a target

`dot-sync add <name>` registers a target in `.sync.yaml` (and
bootstraps the file if it's missing) or appends fields to an existing
target. Three flavors:

```sh
# Non-interactive — fully flag-driven, scriptable
dot-sync add codex \
  --format toml \
  --source codex.sync.toml \
  --target ~/.codex/config.toml \
  --field tui.theme --field max_bytes

# Append fields to an existing target — only --field needed
dot-sync add codex --field tui.notification_condition

# Interactive — drop --field on a TTY; a tree picker discovers fields
# from the source / target document
dot-sync add claude --source claude.sync.json --target ~/.claude/settings.json
```

Format is inferred from the source / target file extension when
`--format` is omitted (`.toml` / `.json` / `.jsonc` / `.gitconfig` /
`.env` / `.envrc`; also the bare dotfile names `.env` and `.envrc`).

Picker controls: `↑`/`↓` move, `←`/`→` collapse / expand, `space`
toggle, `enter` confirm, `q` / `Esc` cancel. On containers, `space`
cycles `[ ]` (empty) → `[x]` (sync the whole subtree as one path) →
`[*]` (sync each leaf individually) → `[ ]`. Manually toggling some
leaves under a container shows `[~]` (mixed); pressing space on `[~]`
resets the container.

`add --dry-run` previews the YAML write. Writing `.sync.yaml` via
`add` does not preserve user comments inside the YAML itself.

## Sync rules

Only fields listed in `sync:` are touched on either side, and nothing
is ever removed. Everything outside `sync:` round-trips unchanged.

| State of a listed field   | `pull` (target → source) | `push` (source → target) | `sync` (default `--target-wins`) |
| ------------------------- | ------------------------ | ------------------------ | --------------------------------- |
| Both sides equal          | skip                     | skip                     | skip                              |
| Both sides differ         | source := target         | target := source         | source := target (mode-dependent) |
| Only target has it        | source := target (add)   | skip                     | source := target (add)            |
| Only source has it        | skip                     | target := source (add)   | target := source (add)            |
| Neither has it            | skip                     | skip                     | skip                              |
| Field not in `sync:` list | untouched                | untouched                | untouched                         |

`pull` is always target-wins; `push` is always source-wins. `sync`
takes mutually exclusive conflict-mode flags: `--target-wins`
(default), `--source-wins`, or `--fail-on-conflict` (exit non-zero,
write nothing, list the conflicts).

To stop syncing a field, remove it from `sync:` in `.sync.yaml`. The
tool will not delete it from either file — clean up by hand if you
want it gone.

## Configuration

`dot-sync` reads `.sync.yaml` from the current directory or the
nearest parent directory. Paths in `source` are relative to that
file; paths in `target` may use `~`.

```yaml
targets:
  codex:
    format: toml
    source: codex.sync.toml
    target: ~/.codex/config.toml
    sync:
      - tui.theme                                # plain key
      - plugins."github@openai-curated".enabled  # quoted segment
      - mcp_servers[name="github"].enabled       # array, pinned by key
      - mcp_servers[name].enabled                # array, wildcard
```

### Path syntax

| Form | Meaning |
| --- | --- |
| `tui.theme` | Plain object navigation. |
| `plugins."github@openai-curated".enabled` | Quoted segment, for keys with `.` `[` or whitespace. |
| `arr[name="github"].enabled` | Pin to the array item where `name == "github"`. Stable across reorderings. |
| `arr[port=8080].host` | Pinned with an integer literal — strict; matches `port = 8080`, not `"8080"`. |
| `arr[primary=true].host` | Pinned with a boolean literal. |
| `arr[name].enabled` | Wildcard: fan out across every item in `arr`, pairing source / target items by `name`. |

Selector value types: `"quoted string"`, decimal integer (e.g. `8080`,
`-1`), or `true` / `false`. Floats are not supported.

**Multi-match is an error.** Two array items sharing the same
identifier value is treated as data corruption and the sync bails
before any write — surgical sync requires unambiguous identity.

When the identifier matches an item that exists on one side but not
the other, the missing side gets a new array entry seeded with the
identifier — the "fill missing" rule extended to array members.

Plain index syntax (`arr[0]`) is intentionally not supported — array
positions shift when data changes, so index-based sync is destructive
in the cases that matter most.

## Format support

Four backends. Each preserves the original file's formatting
byte-for-byte on the fields it doesn't touch.

- **TOML** (`format: toml`) — via `toml_edit`. Whitespace, key
  order, comments preserved.
- **JSON / JSONC** (`format: json` or `format: jsonc`, same backend)
  — via `jsonc-parser`. Object key order, line/block comments,
  trailing commas, blank lines, indentation all round-trip.
- **gitconfig** (`format: gitconfig`) — via `gix-config` (gitoxide).
  `~/.gitconfig` / per-repo `.git/config`. Subsection quoting
  (`[remote "origin"]`), tab/space indentation, `#` / `;` comments
  all survive.
- **env** (`format: env`) — `.env` / `.envrc`-style flat
  `KEY=value`. Hand-rolled line-based CST: comments, blank lines,
  `export` prefix, all three quote styles (bare / `"..."` /
  `'...'`) preserved.

<details>
<summary><b>TOML & JSON specifics</b></summary>

- **Null vs missing (JSON).** `{"key": null}` is a present field
  with the value `null`; an absent key is "missing". `pull` /
  `push` / `sync` treat them as different values — an explicit
  `null` propagates instead of being dropped.
- **Int vs float in selectors (JSON).** `[k=8080]` matches
  `"k": 8080` only — not `"k": 8080.0`. Floats are not supported
  as selector values at all.
- **JSONC trivia.** Line (`// …`) and block (`/* … */`)
  comments, trailing commas inside arrays / objects, and blank
  lines round-trip through sync. Comments stay attached to the
  side they originally lived on — sync moves *values*, not the
  surrounding trivia.
- **JSON indent style** is inferred from existing structure when
  new entries are added — a 4-space / tab-indented file keeps
  its style; a file using trailing commas keeps using them.
- **JSON string escapes** in *replaced* values are re-emitted in
  canonical form. JSON5-only syntax (single-quoted strings,
  unquoted keys, hex / Infinity / NaN literals) is rejected by
  the parser.
- **TOML inline tables** are read-only for arrays-of-objects
  selectors (`inline = [{...}]`); writes go into `[[arrays]]`
  form.

</details>

<details>
<summary><b>gitconfig specifics</b></summary>

- **Path arity.** Two segments address `section.key`
  (`user.email`); three address `section.subsection.key`
  (`remote.origin.url`, `branch."feature.x".remote`).
  Single-segment and 4+ segment paths are rejected.
- **Subsections with special characters** use the existing
  quoted-segment syntax: `includeIf."gitdir:~/work/".path`.
- **Case-sensitive matching (diverges from git).** git itself
  treats section / subsection / key names case-insensitively, but
  dot-sync's path syntax is case-sensitive across all backends.
  If the path's bytes don't exactly match what's in the file,
  `get` returns absent and `set` bails with a clear error. Use
  the case from your file — the `add` picker emits canonical
  case automatically.
- **No array selectors.** gitconfig has no arrays of objects.
  The closest construct, multivar (multiple `remote.origin.fetch
  =` lines), is treated as data corruption for surgical sync and
  bails. The picker hides multivar keys.
- **Boolean polysemy.** git accepts `true` / `yes` / `on` / `1`
  as the same value; dot-sync compares bytes literally. If two
  sides write the same boolean differently, dot-sync treats it as
  a conflict.
- **Section / key name validation.** Names must match git's
  grammar (alphanumeric + dash, leading alphabetic for keys);
  `gix-config` rejects underscores etc. at write time.
- **Mixed indentation + end-of-line comments.** Tab-indented and
  space-indented sections round-trip byte-identically. Trailing
  `;` / `#` comments are preserved.
- **Backslash-continued multi-line values are rejected at load**
  (`gix-config` 0.56 mangles them). Inline the value or remove
  the continuation. Drops out once gitoxide ships a fix.
- **Cosmetic insert quirk.** When a new key lands inside an
  existing section, `gix-config` places it after any trailing
  blank line. Data is correct, layout looks slightly off — move
  the line up by hand if it bothers you.

</details>

<details>
<summary><b>env specifics</b></summary>

- **Flat namespace.** Paths are exactly one segment
  (`NODE_VERSION`). Multi-segment and array selectors are
  rejected.
- **POSIX key names only.** Keys must match
  `[A-Za-z_][A-Za-z0-9_]*`. Hyphens accepted by some dotenv
  dialects are rejected so syncs stay portable.
- **Case-sensitive.** `PATH` and `path` are different keys.
- **Quote styles.** Bare, double, and single quoting are all
  read on load and preserved on round-trip. Updates keep the
  user's original style by default — bare upgrades to double
  when the new value won't round-trip unquoted (leading /
  trailing whitespace, trailing `\`, leading `"` / `'`); single
  upgrades to double when the new value contains a literal `'`.
- **`export` prefix preserved** on update; new entries
  dot-sync appends never get `export` (style choice the tool
  doesn't try to guess).
- **Last-wins on duplicate keys** (matches bash `export`).
  `set` updates the last occurrence; earlier shadowed entries
  are left alone.
- **Rejected at load:** backslash continuation, unclosed
  quotes, trailing content after a closing quote, shell
  expressions at line level (`if/then`, function defs,
  `[[...]]`), and trailing `# comment` after a value (the `#`
  is taken as part of the value, matching bash on unquoted
  values).
- **No `\n` / `\t` interpolation.** Inside `"..."`, only `\"`
  and `\\` are interpreted as escapes. Real newlines in values
  aren't supported in v1.
- **`${VAR}` / `$(cmd)` are literal.** dot-sync syncs the
  reference, not the resolved result.

</details>

## Commands

```sh
dot-sync pull <name>     # target → source
dot-sync push <name>     # source → target
dot-sync sync <name>     # both directions (see § Sync rules)
dot-sync add <name>      # register a target (see § Adding a target)
dot-sync status [name]   # config + file health
dot-sync restore <name>  # roll back (see § Backup and recovery)
```

`<name>` is the target name from `.sync.yaml`. Omit it on
`pull` / `push` / `sync` / `status` to operate on all configured
targets.

## Backup and recovery

Writes are atomic. Before each overwrite, the previous contents are
saved to a recovery snapshot under `$TMPDIR/dot-sync/`; the path is
printed under each `wrote` line, and `restore` rolls back from there.

```sh
dot-sync pull codex --dry-run       # preview, no writes
dot-sync push codex --backup        # also keep a persistent .bak.<timestamp>
```

`--dry-run` works on every command and shows planned changes without
writing.

`--backup` (on `pull` / `push` / `sync`) writes a persistent
timestamped copy `<file>.bak.<timestamp>` next to the destination, in
addition to the auto recovery snapshot. Recovery snapshots are
ephemeral (the OS reclaims them); backups are explicit and survive.

### Restoring from a snapshot

```sh
dot-sync restore codex --list           # show numbered candidates (recovery + backup)
dot-sync restore codex                  # restore newest snapshot of target
dot-sync restore codex --pick 3         # restore the 3rd candidate from the list
dot-sync restore codex --at 20260510-15 # restore by timestamp prefix
dot-sync restore codex --source         # restore source instead of target
dot-sync restore codex --dry-run        # show what would happen
```

`restore` is itself atomic and snapshots before overwriting, so an
unwanted restore is recoverable too. When timestamps tie,
`[backup]` entries are preferred over `[recovery]` (they are an
explicit user signal).

## Shell completions and man page

Easiest path — re-run the installer with `--with-completions`:

```sh
curl -fsSL https://raw.githubusercontent.com/muhac/dot-sync/main/install.sh | sh -s -- --with-completions
```

Detects `$SHELL`, writes completion files to the standard user-owned
directory (`~/.local/share/bash-completion/completions/`, `~/.zfunc/`,
`~/.config/fish/completions/`), and writes a man page to
`~/.local/share/man/man1/dot-sync.1`. Prints any rc-file edits you
still need to make (mostly just `zsh`).

By hand:

```sh
dot-sync completions bash       > ~/.local/share/bash-completion/completions/dot-sync
dot-sync completions zsh        > ~/.zfunc/_dot-sync
dot-sync completions fish       > ~/.config/fish/completions/dot-sync.fish
dot-sync completions powershell > $PROFILE.dot-sync.ps1
dot-sync man                    > ~/.local/share/man/man1/dot-sync.1
```

## Install options

Nightly prerelease:

```sh
curl -fsSL https://raw.githubusercontent.com/muhac/dot-sync/main/install.sh | sh -s -- --nightly
```

Pinned version or alternate install directory:

```sh
curl -fsSL https://raw.githubusercontent.com/muhac/dot-sync/main/install.sh | sh -s -- --version v0.1.0
curl -fsSL https://raw.githubusercontent.com/muhac/dot-sync/main/install.sh | sh -s -- --dir ~/.local/bin
```

The installer always writes `dot-sync` to the install directory. It
writes the shorter `ds` alias only when that path is empty or already
points at an existing `dot-sync` install — it will not overwrite an
unrelated `ds` command.

Use `dot-sync` in scripts and docs; `ds` is the interactive shortcut.
