use std::fs;
use std::path::Path;

use anyhow::{Context, Result, bail};
use toml_edit::{ArrayOfTables, DocumentMut, InlineTable, Item, Table, TableLike, Value};

use crate::path::{FieldPath, ItemSelector, Segment};

#[derive(Debug, Clone)]
pub struct TableConflict {
    pub path: String,
    pub kind: String,
    pub value: String,
}

/// One concrete instantiation of a pattern after wildcard expansion.
/// `identity` carries the matched key values for each `Wildcard` segment in
/// declaration order — engines compare identities across documents to pair
/// items. `path` is the pattern with every `Wildcard` replaced by `Pinned`.
#[derive(Debug, Clone)]
pub struct ResolvedPath {
    pub identity: Vec<String>,
    pub path: FieldPath,
}

/// Format-agnostic view of a structured config file that the sync engine
/// drives. Each impl owns its native value type via the associated `Item`,
/// so JSON / YAML / TOML do not have to share a lowest-common-denominator
/// value enum.
///
/// Contract for implementers:
///
/// - **Surgical writes**: `set` must replace exactly the leaf value at `path`
///   and leave every sibling field at every level along the path untouched.
///   This is the core "surgical sync" promise — violating it breaks the whole
///   product.
/// - **`get` returns owned values**: callers may continue to consult the
///   original document after a `set`, so returned `Item`s should be deep
///   clones, not borrowed references.
/// - **Format preservation**: `render` should preserve original whitespace,
///   key order, and (where the format supports it) comments. `toml_edit`
///   gives this for free; pure `serde_json` / `serde_yaml` do not, so JSON /
///   YAML impls will need either a format-preserving parser or explicit
///   accommodations documented at their call sites.
/// - **Path semantics**: `FieldPath` segments are dotted keys, optionally
///   carrying an `ItemSelector` for array navigation:
///   `arr[name="github"].field` (pinned) and `arr[name].field` (wildcard).
///   Pinned selectors resolve to a single deterministic concrete path that
///   `get`/`set`/`table_conflict` must handle directly. Wildcard selectors
///   are expanded by `expand` into a fan-out of pinned-form paths *before*
///   they reach `get`/`set`, so `set` can bail on wildcard segments.
///   Position-based indexing (`arr[0]`) is intentionally not supported.
/// - **Missing vs explicit-null**: TOML has no `null` concept, so `get`
///   returning `None` unambiguously means "absent". JSON has both an explicit
///   `null` value and the absence of a key; the JSON impl must decide which
///   it returns as `Some(Null)` versus `None`, and the sync rules treat the
///   two cases differently. Document the choice at the impl site.
pub trait Document: Sized {
    /// Native value type for this format. Engine code is generic over `D`,
    /// so this type stays opaque outside the impl except via `items_equal`
    /// and `summarize`.
    type Item;

    /// Open the file at `path`. If `allow_missing` is true and the file does
    /// not exist, return an empty document instead of erroring; the engine
    /// uses this to bootstrap the absent side of a sync.
    fn load(path: &Path, allow_missing: bool) -> Result<Self>;

    /// Return a deep clone of the value at `path`, or `None` if the path
    /// resolves to no value.
    fn get(&self, path: &FieldPath) -> Option<Self::Item>;

    /// Write `item` at `path`, creating any missing intermediate containers.
    /// Sibling fields at every level along `path` must be preserved.
    fn set(&mut self, path: &FieldPath, item: Self::Item) -> Result<()>;

    /// If the prefix of `path` is occupied by a non-table value (so writing
    /// at `path` would clobber that value), return a `TableConflict`
    /// describing the offender. Used to warn the user before a destructive
    /// `set`.
    fn table_conflict(&self, path: &FieldPath) -> Option<TableConflict>;

    /// Serialize the document to a string for writing back to disk.
    /// Should preserve the original formatting as faithfully as the
    /// underlying parser allows.
    fn render(&self) -> String;

    /// Compare two items for "already in sync" purposes. Defined on the
    /// trait because not every native value type implements `PartialEq`
    /// (e.g. `toml_edit::Item` doesn't), and equality semantics can be
    /// format-specific (e.g. `1` vs `1.0`).
    fn items_equal(a: &Self::Item, b: &Self::Item) -> bool;

    /// Resolve a path's selectors against this document, yielding one
    /// `ResolvedPath` per matched item combination. For a pattern with no
    /// selectors at all, returns a single entry whose path equals the input.
    ///
    /// **Multi-match is an error.** A `Pinned` selector matching more than one
    /// array item, or a `Wildcard` selector encountering two items that share
    /// the same identifier value, is treated as data corruption — surgical
    /// sync requires unambiguous identity. The engine surrounds the call with
    /// per-side context ("source pattern '<raw>'") so the eventual error
    /// chain points at the exact pattern and the doc that owns the duplicate.
    fn expand(&self, pattern: &FieldPath) -> Result<Vec<ResolvedPath>>;

    /// Format-aware short rendering used in change reports. Returns
    /// `<missing>` when the item is `None` so callers do not have to
    /// special-case absent values. Output should look like a literal in the
    /// target format (TOML scalars in TOML syntax, JSON scalars in JSON
    /// syntax) so users reading the report can match it back to the file.
    fn summarize(item: Option<&Self::Item>) -> String;
}

/// Supported document formats. Adding a variant prompts the compiler to
/// flag every dispatch site as non-exhaustive — that is the whole point.
/// Do not paper over with a wildcard arm.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Format {
    Toml,
}

/// Parse the format string from `.sync.yaml` into the typed `Format` enum,
/// failing fast (before any file I/O) with a list of supported format names.
pub fn parse_format(format: &str) -> Result<Format> {
    match format {
        "toml" => Ok(Format::Toml),
        "json" => bail!("format json is recognized but not implemented yet"),
        other => bail!("unsupported format: {other}; supported formats: toml"),
    }
}

pub struct TomlDocument {
    doc: DocumentMut,
}

impl TomlDocument {
    pub fn empty() -> Self {
        Self {
            doc: DocumentMut::new(),
        }
    }
}

impl Document for TomlDocument {
    type Item = Item;

    fn load(path: &Path, allow_missing: bool) -> Result<Self> {
        if !path.exists() {
            if allow_missing {
                return Ok(Self::empty());
            }
            bail!("file does not exist: {}", path.display());
        }

        let content = fs::read_to_string(path)
            .with_context(|| format!("failed to read {}", path.display()))?;
        let doc = content
            .parse::<DocumentMut>()
            .with_context(|| format!("failed to parse TOML {}", path.display()))?;
        Ok(Self { doc })
    }

    fn get(&self, path: &FieldPath) -> Option<Item> {
        descend_get(self.doc.as_table(), path.segments())
    }

    fn set(&mut self, path: &FieldPath, item: Item) -> Result<()> {
        descend_set(self.doc.as_table_mut(), path.segments(), item)
    }

    fn table_conflict(&self, path: &FieldPath) -> Option<TableConflict> {
        table_conflict_descend(self.doc.as_table(), path.segments())
    }

    fn render(&self) -> String {
        self.doc.to_string()
    }

    fn items_equal(a: &Item, b: &Item) -> bool {
        // toml_edit::Item lacks PartialEq; compare via stable serialization.
        a.to_string() == b.to_string()
    }

    fn summarize(item: Option<&Item>) -> String {
        match item {
            None => "<missing>".to_string(),
            Some(item) => summarize_toml_item(item),
        }
    }

    fn expand(&self, pattern: &FieldPath) -> Result<Vec<ResolvedPath>> {
        // No selectors = no array navigation = no multi-match risk; skip the
        // doc walk entirely.
        if pattern.segments().iter().all(|s| s.select.is_none()) {
            return Ok(vec![ResolvedPath {
                identity: Vec::new(),
                path: pattern.clone(),
            }]);
        }
        let mut out = Vec::new();
        expand_walk(
            self.doc.as_table(),
            pattern.segments(),
            Vec::new(),
            Vec::new(),
            &mut out,
        )?;
        Ok(out)
    }
}

// ----- shared helpers -----

fn summarize_toml_item(item: &Item) -> String {
    let mut rendered = item
        .to_string()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    if rendered.is_empty() {
        rendered = item.type_name().to_string();
    }
    const LIMIT: usize = 120;
    if rendered.chars().count() > LIMIT {
        let mut truncated = rendered.chars().take(LIMIT - 3).collect::<String>();
        truncated.push_str("...");
        truncated
    } else {
        rendered
    }
}

/// True when `table[key]` is a string equal to `value`. Used to identify a
/// matching array item for a `[key="value"]` pinned selector.
fn matches_pinned(table: &dyn TableLike, key: &str, value: &str) -> bool {
    table
        .get(key)
        .and_then(|item| item.as_value())
        .and_then(|v| v.as_str())
        == Some(value)
}

/// A matched array item. Either a `[[arrays.of.tables]]` entry (`Table`) or
/// an inline-table inside an `arr = [{...}]` value (`InlineTable`). Both can
/// be navigated via the `TableLike` trait but their owned `Item`
/// representations differ, so we keep them separate.
enum Matched<'a> {
    Table(&'a Table),
    Inline(&'a InlineTable),
}

impl<'a> Matched<'a> {
    fn as_table_like(&self) -> &'a dyn TableLike {
        match self {
            Self::Table(t) => *t,
            Self::Inline(i) => *i,
        }
    }

    fn into_owned_item(self) -> Item {
        match self {
            Self::Table(t) => Item::Table(t.clone()),
            Self::Inline(i) => Item::Value(Value::InlineTable(i.clone())),
        }
    }
}

fn find_pinned_item<'a>(arr_item: &'a Item, key: &str, value: &str) -> Option<Matched<'a>> {
    match arr_item {
        Item::ArrayOfTables(arr) => arr
            .iter()
            .find(|t| matches_pinned(*t, key, value))
            .map(Matched::Table),
        Item::Value(Value::Array(arr)) => arr
            .iter()
            .filter_map(|v| v.as_inline_table())
            .find(|t| matches_pinned(*t, key, value))
            .map(Matched::Inline),
        _ => None,
    }
}

/// Iterate every item in `arr_item` as a TableLike. Handles both
/// `[[arrays.of.tables]]` and inline `arr = [{...}]` shapes. Items that
/// aren't table-like are skipped.
fn iter_array_items(arr_item: &Item) -> Vec<&dyn TableLike> {
    match arr_item {
        Item::ArrayOfTables(arr) => arr.iter().map(|t| t as &dyn TableLike).collect(),
        Item::Value(Value::Array(arr)) => arr
            .iter()
            .filter_map(|v| v.as_inline_table())
            .map(|t| t as &dyn TableLike)
            .collect(),
        _ => Vec::new(),
    }
}

/// Recursive expand walker. Emits a `ResolvedPath` once all selectors have
/// been substituted. Branches die when an intermediate container is missing
/// — fill-missing semantics handle that case via the other side's expansion.
///
/// Returns `Err` when a `Pinned` selector matches more than one item, or a
/// `Wildcard` segment finds two items with the same identifier value. See
/// the `Document::expand` doc for the rationale.
fn expand_walk(
    table: &dyn TableLike,
    remaining: &[Segment],
    identity: Vec<String>,
    resolved: Vec<Segment>,
    out: &mut Vec<ResolvedPath>,
) -> Result<()> {
    let no_more_selectors = remaining.iter().all(|s| s.select.is_none());

    if no_more_selectors {
        let mut full = resolved;
        full.extend(remaining.iter().cloned());
        out.push(ResolvedPath {
            identity,
            path: FieldPath::from_segments(full),
        });
        return Ok(());
    }

    let (seg, rest) = remaining
        .split_first()
        .expect("non-empty: selectors exist downstream");

    match &seg.select {
        None => {
            let Some(item) = table.get(&seg.name) else {
                return Ok(());
            };
            let Some(next) = item.as_table_like() else {
                return Ok(());
            };
            let mut new_resolved = resolved;
            new_resolved.push(seg.clone());
            expand_walk(next, rest, identity, new_resolved, out)?;
        }
        Some(ItemSelector::Pinned { key, value }) => {
            let Some(arr) = table.get(&seg.name) else {
                return Ok(());
            };
            let count = count_pinned_matches(arr, key, value);
            if count > 1 {
                bail!(
                    "ambiguous pinned selector at {}: {count} items where {key}={value:?}",
                    format_segment_for_prefix(seg)
                );
            }
            let Some(matched) = find_pinned_item(arr, key, value) else {
                return Ok(());
            };
            let mut new_resolved = resolved;
            new_resolved.push(seg.clone());
            expand_walk(matched.as_table_like(), rest, identity, new_resolved, out)?;
        }
        Some(ItemSelector::Wildcard { key }) => {
            let Some(arr) = table.get(&seg.name) else {
                return Ok(());
            };
            // Pre-scan for duplicate identifier values across the array.
            let mut counts: std::collections::BTreeMap<String, usize> =
                std::collections::BTreeMap::new();
            for item_table in iter_array_items(arr) {
                if let Some(v) = item_table
                    .get(key)
                    .and_then(|i| i.as_value())
                    .and_then(|v| v.as_str())
                {
                    *counts.entry(v.to_string()).or_default() += 1;
                }
            }
            let dups: Vec<String> = counts
                .iter()
                .filter(|(_, c)| **c > 1)
                .map(|(k, c)| format!("{k:?}×{c}"))
                .collect();
            if !dups.is_empty() {
                bail!(
                    "ambiguous wildcard at {}: duplicate {key} values: {}",
                    format_segment_for_prefix(seg),
                    dups.join(", ")
                );
            }
            for item_table in iter_array_items(arr) {
                let Some(value) = item_table
                    .get(key)
                    .and_then(|i| i.as_value())
                    .and_then(|v| v.as_str())
                else {
                    continue;
                };
                let mut branch_id = identity.clone();
                branch_id.push(value.to_string());
                let mut branch_resolved = resolved.clone();
                branch_resolved.push(Segment {
                    name: seg.name.clone(),
                    select: Some(ItemSelector::Pinned {
                        key: key.clone(),
                        value: value.to_string(),
                    }),
                });
                expand_walk(item_table, rest, branch_id, branch_resolved, out)?;
            }
        }
    }
    Ok(())
}

fn count_pinned_matches(arr_item: &Item, key: &str, value: &str) -> usize {
    match arr_item {
        Item::ArrayOfTables(arr) => arr
            .iter()
            .filter(|t| matches_pinned(*t, key, value))
            .count(),
        Item::Value(Value::Array(arr)) => arr
            .iter()
            .filter_map(|v| v.as_inline_table())
            .filter(|t| matches_pinned(*t, key, value))
            .count(),
        _ => 0,
    }
}

// ----- descend_get -----

fn descend_get(table: &Table, segments: &[Segment]) -> Option<Item> {
    descend_get_like(table, segments)
}

fn descend_get_like(table: &dyn TableLike, segments: &[Segment]) -> Option<Item> {
    let (first, rest) = segments.split_first()?;
    let last = rest.is_empty();
    match &first.select {
        None => {
            let item = table.get(&first.name)?;
            if last {
                return Some(item.clone());
            }
            descend_get_like(item.as_table_like()?, rest)
        }
        Some(ItemSelector::Pinned { key, value }) => {
            let arr = table.get(&first.name)?;
            let matched = find_pinned_item(arr, key, value)?;
            if last {
                return Some(matched.into_owned_item());
            }
            descend_get_like(matched.as_table_like(), rest)
        }
        Some(ItemSelector::Wildcard { .. }) => None,
    }
}

// ----- table_conflict_descend -----

fn table_conflict_descend(table: &Table, segments: &[Segment]) -> Option<TableConflict> {
    table_conflict_like(table, segments, &mut Vec::new())
}

fn table_conflict_like(
    table: &dyn TableLike,
    segments: &[Segment],
    prefix: &mut Vec<String>,
) -> Option<TableConflict> {
    if segments.len() <= 1 {
        return None;
    }
    let (first, rest) = segments.split_first()?;
    prefix.push(format_segment_for_prefix(first));
    let item = table.get(&first.name)?;
    match &first.select {
        None => match item.as_table_like() {
            Some(next) => table_conflict_like(next, rest, prefix),
            None => Some(TableConflict {
                path: prefix.join("."),
                kind: item.type_name().to_string(),
                value: summarize_toml_item(item),
            }),
        },
        Some(ItemSelector::Pinned { key, value }) => {
            // Container at first.name must be an array (of any flavor).
            // If not, that itself is a conflict the user should know about.
            if !matches!(item, Item::ArrayOfTables(_) | Item::Value(Value::Array(_))) {
                return Some(TableConflict {
                    path: prefix.join("."),
                    kind: item.type_name().to_string(),
                    value: summarize_toml_item(item),
                });
            }
            // Missing matched item is not a conflict — `set` will append.
            let matched = find_pinned_item(item, key, value)?;
            table_conflict_like(matched.as_table_like(), rest, prefix)
        }
        Some(ItemSelector::Wildcard { .. }) => {
            // Same array-shape check as Pinned. Per-item validation happens
            // in `expand`; the conflict layer just flags "this can't be an
            // array at all".
            if !matches!(item, Item::ArrayOfTables(_) | Item::Value(Value::Array(_))) {
                return Some(TableConflict {
                    path: prefix.join("."),
                    kind: item.type_name().to_string(),
                    value: summarize_toml_item(item),
                });
            }
            None
        }
    }
}

fn format_segment_for_prefix(seg: &Segment) -> String {
    match &seg.select {
        None => seg.name.clone(),
        Some(ItemSelector::Pinned { key, value }) => {
            format!("{}[{key}=\"{value}\"]", seg.name)
        }
        Some(ItemSelector::Wildcard { key }) => format!("{}[{key}]", seg.name),
    }
}

// ----- descend_set -----

fn descend_set(table: &mut Table, segments: &[Segment], value: Item) -> Result<()> {
    let Some((first, rest)) = segments.split_first() else {
        bail!("path must not be empty");
    };
    if let Some(ItemSelector::Wildcard { .. }) = first.select {
        bail!("wildcard selectors are not yet supported in TomlDocument writes");
    }
    let last = rest.is_empty();

    match &first.select {
        None => {
            if last {
                table.insert(&first.name, value);
                return Ok(());
            }
            ensure_table_child(table, &first.name);
            let child = table.get_mut(&first.name).expect("just ensured");
            set_in_child(child, rest, value)
        }
        Some(ItemSelector::Pinned { key, value: pinned }) => {
            let arr = ensure_array_of_tables(table, &first.name)?;
            descend_set_in_array(arr, key, pinned, rest, value)
        }
        Some(ItemSelector::Wildcard { .. }) => unreachable!("checked above"),
    }
}

fn ensure_table_child(table: &mut Table, name: &str) {
    if !matches!(table.get(name), Some(item) if item.as_table_like().is_some()) {
        let mut child = Table::new();
        child.set_implicit(true);
        table.insert(name, Item::Table(child));
    }
}

fn ensure_inline_table_child(table: &mut InlineTable, name: &str) {
    if !matches!(TableLike::get(table, name), Some(item) if item.as_table_like().is_some()) {
        TableLike::insert(
            table,
            name,
            Item::Value(Value::InlineTable(InlineTable::new())),
        );
    }
}

/// Ensure `table[name]` is `Item::ArrayOfTables`. Bail if it exists with a
/// different shape (including the inline `arr = [{...}]` form, which we
/// don't yet write into).
fn ensure_array_of_tables<'a>(table: &'a mut Table, name: &str) -> Result<&'a mut ArrayOfTables> {
    match table.get(name) {
        Some(Item::ArrayOfTables(_)) => {}
        Some(Item::Value(Value::Array(_))) => {
            bail!("{name} is an inline array; pinned-selector writes only support [[{name}]] form")
        }
        Some(other) => bail!(
            "{name} is {} but path expects an array of tables",
            other.type_name()
        ),
        None => {
            table.insert(name, Item::ArrayOfTables(ArrayOfTables::new()));
        }
    }
    match table.get_mut(name) {
        Some(Item::ArrayOfTables(arr)) => Ok(arr),
        _ => unreachable!("just ensured"),
    }
}

fn descend_set_in_array(
    arr: &mut ArrayOfTables,
    key: &str,
    pinned: &str,
    rest: &[Segment],
    value: Item,
) -> Result<()> {
    // Find existing match; otherwise append a new table seeded with the key.
    let pos = arr.iter().position(|t| matches_pinned(t, key, pinned));
    let pos = match pos {
        Some(p) => p,
        None => {
            let mut new_table = Table::new();
            new_table.insert(
                key,
                Item::Value(Value::String(toml_edit::Formatted::new(pinned.to_string()))),
            );
            arr.push(new_table);
            arr.len() - 1
        }
    };
    let item_table = arr.get_mut(pos).expect("indexed");

    if rest.is_empty() {
        // Pinned at the leaf: replace the matched item's content with the new
        // value. The new value must itself be a table.
        let replacement = into_table(value)?;
        *item_table = replacement;
        // Restore the pinning key in case the replacement value didn't carry
        // it (callers transferring values across docs sometimes don't).
        if !matches_pinned(item_table, key, pinned) {
            item_table.insert(
                key,
                Item::Value(Value::String(toml_edit::Formatted::new(pinned.to_string()))),
            );
        }
        return Ok(());
    }
    descend_set(item_table, rest, value)
}

fn into_table(item: Item) -> Result<Table> {
    match item {
        Item::Table(t) => Ok(t),
        Item::Value(Value::InlineTable(it)) => {
            let mut t = Table::new();
            for (k, v) in it.iter() {
                t.insert(k, Item::Value(v.clone()));
            }
            Ok(t)
        }
        other => bail!(
            "pinned-selector leaf write requires a table value, got {}",
            other.type_name()
        ),
    }
}

fn set_in_child(child: &mut Item, segments: &[Segment], item: Item) -> Result<()> {
    match child {
        Item::Table(table) => descend_set(table, segments, item),
        Item::Value(Value::InlineTable(table)) => set_in_inline_descend(table, segments, item),
        _ => unreachable!("child was checked or inserted as table-like"),
    }
}

fn set_in_inline_descend(table: &mut InlineTable, segments: &[Segment], item: Item) -> Result<()> {
    let Some((first, rest)) = segments.split_first() else {
        bail!("path must not be empty");
    };
    if let Some(ItemSelector::Wildcard { .. }) = first.select {
        bail!("wildcard selectors are not yet supported in TomlDocument writes");
    }
    if first.select.is_some() {
        bail!(
            "pinned selector inside an inline table ({}) is not yet supported",
            first.name
        );
    }
    if rest.is_empty() {
        let value = match item.into_value() {
            Ok(value) => value,
            Err(item) => bail!("cannot insert {} into inline table", item.type_name()),
        };
        table.insert(&first.name, value);
        return Ok(());
    }
    ensure_inline_table_child(table, &first.name);
    let child = TableLike::get_mut(table, &first.name).expect("inserted inline table");
    set_in_child(child, rest, item)
}

#[cfg(test)]
mod tests {
    use toml_edit::value;

    use super::{Document, TomlDocument};
    use crate::path::FieldPath;

    #[test]
    fn sets_and_gets_nested_values() {
        let mut doc = TomlDocument::empty();
        let path = FieldPath::parse("tui.theme").unwrap();
        doc.set(&path, value("monokai")).unwrap();
        assert_eq!(
            doc.get(&path).unwrap().as_value().unwrap().as_str(),
            Some("monokai")
        );
    }

    #[test]
    fn handles_quoted_segments() {
        let mut doc = TomlDocument::empty();
        let path = FieldPath::parse("plugins.\"github@openai-curated\".enabled").unwrap();
        doc.set(&path, value(true)).unwrap();
        assert_eq!(
            doc.get(&path).unwrap().as_value().unwrap().as_bool(),
            Some(true)
        );
    }

    #[test]
    fn preserves_inline_table_fields_when_setting_nested_values() {
        let mut doc = TomlDocument {
            doc: r#"settings = { theme = "old", local = "keep" }"#.parse().unwrap(),
        };
        let path = FieldPath::parse("settings.theme").unwrap();

        assert_eq!(
            doc.get(&path).unwrap().as_value().unwrap().as_str(),
            Some("old")
        );

        doc.set(&path, value("new")).unwrap();

        let settings_path = FieldPath::parse("settings").unwrap();
        let settings_item = doc.get(&settings_path).unwrap();
        let settings = settings_item.as_inline_table().unwrap();
        assert_eq!(settings.get("theme").unwrap().as_str(), Some("new"));
        assert_eq!(settings.get("local").unwrap().as_str(), Some("keep"));
    }

    #[test]
    fn pinned_get_finds_array_of_tables_item() {
        let doc = TomlDocument {
            doc: r#"
[[mcp_servers]]
name = "github"
enabled = true

[[mcp_servers]]
name = "linear"
enabled = false
"#
            .parse()
            .unwrap(),
        };
        let path = FieldPath::parse("mcp_servers[name=\"github\"].enabled").unwrap();
        assert_eq!(
            doc.get(&path).unwrap().as_value().unwrap().as_bool(),
            Some(true)
        );
    }

    #[test]
    fn pinned_get_finds_inline_array_item() {
        let doc = TomlDocument {
            doc: r#"servers = [{ name = "a", port = 80 }, { name = "b", port = 81 }]"#
                .parse()
                .unwrap(),
        };
        let path = FieldPath::parse("servers[name=\"b\"].port").unwrap();
        assert_eq!(
            doc.get(&path).unwrap().as_value().unwrap().as_integer(),
            Some(81)
        );
    }

    #[test]
    fn pinned_set_updates_existing_item_and_preserves_siblings() {
        let mut doc = TomlDocument {
            doc: r#"
[[mcp_servers]]
name = "github"
enabled = true
url = "https://api.github.com"

[[mcp_servers]]
name = "linear"
enabled = false
"#
            .parse()
            .unwrap(),
        };
        let path = FieldPath::parse("mcp_servers[name=\"github\"].enabled").unwrap();
        doc.set(&path, value(false)).unwrap();

        // The github item's enabled flipped, sibling fields preserved.
        assert_eq!(
            doc.get(&path).unwrap().as_value().unwrap().as_bool(),
            Some(false)
        );
        let url_path = FieldPath::parse("mcp_servers[name=\"github\"].url").unwrap();
        assert_eq!(
            doc.get(&url_path).unwrap().as_value().unwrap().as_str(),
            Some("https://api.github.com")
        );
        // Other items untouched.
        let linear = FieldPath::parse("mcp_servers[name=\"linear\"].enabled").unwrap();
        assert_eq!(
            doc.get(&linear).unwrap().as_value().unwrap().as_bool(),
            Some(false)
        );
    }

    #[test]
    fn pinned_set_appends_new_item_when_key_not_found() {
        let mut doc = TomlDocument {
            doc: r#"
[[mcp_servers]]
name = "linear"
enabled = false
"#
            .parse()
            .unwrap(),
        };
        let path = FieldPath::parse("mcp_servers[name=\"github\"].enabled").unwrap();
        doc.set(&path, value(true)).unwrap();

        // The new item exists with both the pinning key and the synced field.
        assert_eq!(
            doc.get(&path).unwrap().as_value().unwrap().as_bool(),
            Some(true)
        );
        let name = FieldPath::parse("mcp_servers[name=\"github\"].name").unwrap();
        assert_eq!(
            doc.get(&name).unwrap().as_value().unwrap().as_str(),
            Some("github")
        );
        // Old item still there.
        let linear = FieldPath::parse("mcp_servers[name=\"linear\"].enabled").unwrap();
        assert_eq!(
            doc.get(&linear).unwrap().as_value().unwrap().as_bool(),
            Some(false)
        );
    }

    #[test]
    fn pinned_set_creates_array_when_missing() {
        let mut doc = TomlDocument::empty();
        let path = FieldPath::parse("mcp_servers[name=\"github\"].enabled").unwrap();
        doc.set(&path, value(true)).unwrap();

        assert_eq!(
            doc.get(&path).unwrap().as_value().unwrap().as_bool(),
            Some(true)
        );
    }

    #[test]
    fn pinned_get_returns_none_when_no_match() {
        let doc = TomlDocument {
            doc: r#"
[[mcp_servers]]
name = "linear"
"#
            .parse()
            .unwrap(),
        };
        let path = FieldPath::parse("mcp_servers[name=\"github\"].enabled").unwrap();
        assert!(doc.get(&path).is_none());
    }

    #[test]
    fn pinned_set_rejects_inline_array_form() {
        let mut doc = TomlDocument {
            doc: r#"servers = [{ name = "a" }]"#.parse().unwrap(),
        };
        let path = FieldPath::parse("servers[name=\"b\"].port").unwrap();
        let err = doc.set(&path, value(80)).unwrap_err();
        assert!(err.to_string().contains("inline array"));
    }

    #[test]
    fn expand_returns_pattern_unchanged_for_no_wildcard() {
        let doc = TomlDocument::empty();
        let path = FieldPath::parse("tui.theme").unwrap();
        let resolved = doc.expand(&path).unwrap();
        assert_eq!(resolved.len(), 1);
        assert!(resolved[0].identity.is_empty());
        assert_eq!(resolved[0].path, path);
    }

    #[test]
    fn expand_fans_out_wildcard_across_array_of_tables() {
        let doc = TomlDocument {
            doc: r#"
[[mcp_servers]]
name = "github"
enabled = true

[[mcp_servers]]
name = "linear"
enabled = false
"#
            .parse()
            .unwrap(),
        };
        let pattern = FieldPath::parse("mcp_servers[name].enabled").unwrap();
        let mut resolved = doc.expand(&pattern).unwrap();
        resolved.sort_by(|a, b| a.identity.cmp(&b.identity));
        assert_eq!(resolved.len(), 2);
        assert_eq!(resolved[0].identity, vec!["github".to_string()]);
        assert_eq!(
            resolved[0].path.to_string(),
            "mcp_servers[name=\"github\"].enabled"
        );
        assert_eq!(resolved[1].identity, vec!["linear".to_string()]);
    }

    #[test]
    fn expand_returns_empty_when_array_missing() {
        let doc = TomlDocument::empty();
        let pattern = FieldPath::parse("mcp_servers[name].enabled").unwrap();
        let resolved = doc.expand(&pattern).unwrap();
        assert!(resolved.is_empty());
    }

    #[test]
    fn expand_combines_pinned_and_wildcard_segments() {
        let doc = TomlDocument {
            doc: r#"
[[providers]]
name = "openai"

[[providers.models]]
id = "gpt-4"
enabled = true

[[providers.models]]
id = "gpt-5"
enabled = false

[[providers]]
name = "anthropic"

[[providers.models]]
id = "opus"
enabled = true
"#
            .parse()
            .unwrap(),
        };
        // pinned then wildcard: only openai's models, but every model.
        let pattern = FieldPath::parse("providers[name=\"openai\"].models[id].enabled").unwrap();
        let mut resolved = doc.expand(&pattern).unwrap();
        resolved.sort_by(|a, b| a.identity.cmp(&b.identity));
        assert_eq!(resolved.len(), 2);
        assert_eq!(resolved[0].identity, vec!["gpt-4".to_string()]);
        assert_eq!(
            resolved[0].path.to_string(),
            "providers[name=\"openai\"].models[id=\"gpt-4\"].enabled"
        );
        assert_eq!(resolved[1].identity, vec!["gpt-5".to_string()]);
    }

    #[test]
    fn pinned_set_then_get_works_through_nested_arrays() {
        let mut doc = TomlDocument {
            doc: r#"
[[providers]]
name = "openai"

[[providers.models]]
id = "gpt-4"
enabled = false
"#
            .parse()
            .unwrap(),
        };
        let path =
            FieldPath::parse("providers[name=\"openai\"].models[id=\"gpt-4\"].enabled").unwrap();
        doc.set(&path, value(true)).unwrap();
        assert_eq!(
            doc.get(&path).unwrap().as_value().unwrap().as_bool(),
            Some(true)
        );
    }

    #[test]
    fn table_conflict_warns_when_wildcard_target_is_not_an_array() {
        let doc = TomlDocument {
            doc: r#"mcp_servers = "not an array""#.parse().unwrap(),
        };
        let path = FieldPath::parse("mcp_servers[name].enabled").unwrap();
        let conflict = doc.table_conflict(&path).expect("expected conflict");
        assert_eq!(conflict.path, "mcp_servers[name]");
        assert!(conflict.value.contains("not an array"));
    }

    #[test]
    fn expand_errors_on_pinned_multi_match() {
        let doc = TomlDocument {
            doc: r#"
[[mcp_servers]]
name = "github"
enabled = true

[[mcp_servers]]
name = "github"
enabled = false
"#
            .parse()
            .unwrap(),
        };
        let pattern = FieldPath::parse("mcp_servers[name=\"github\"].enabled").unwrap();
        let err = doc.expand(&pattern).unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains("ambiguous pinned"), "msg: {msg}");
        assert!(msg.contains("name=\"github\""), "msg: {msg}");
        assert!(msg.contains("2 items"), "msg: {msg}");
    }

    #[test]
    fn expand_errors_on_wildcard_duplicate_identifier() {
        let doc = TomlDocument {
            doc: r#"
[[mcp_servers]]
name = "github"
enabled = true

[[mcp_servers]]
name = "github"
enabled = false
"#
            .parse()
            .unwrap(),
        };
        let pattern = FieldPath::parse("mcp_servers[name].enabled").unwrap();
        let err = doc.expand(&pattern).unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains("ambiguous wildcard"), "msg: {msg}");
        assert!(msg.contains("\"github\""), "msg: {msg}");
        assert!(msg.contains("×2"), "msg: {msg}");
    }

    #[test]
    fn expand_skips_items_lacking_the_identifier_key() {
        let doc = TomlDocument {
            doc: r#"
[[mcp_servers]]
name = "github"
enabled = true

[[mcp_servers]]
enabled = true
"#
            .parse()
            .unwrap(),
        };
        let pattern = FieldPath::parse("mcp_servers[name].enabled").unwrap();
        let resolved = doc.expand(&pattern).unwrap();
        assert_eq!(resolved.len(), 1);
        assert_eq!(resolved[0].identity, vec!["github".to_string()]);
    }
}
