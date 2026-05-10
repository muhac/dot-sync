use std::collections::BTreeMap;
use std::fs;
use std::path::Path;

use anyhow::{Context, Result, bail};
use toml_edit::{ArrayOfTables, DocumentMut, InlineTable, Item, Table, TableLike, Value};

use crate::path::{FieldPath, ItemSelector, Segment, SelectorValue};

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
///
/// Identity uses `SelectorValue` so cross-side pairing is type-strict:
/// a wildcard hit on `Int(8080)` never pairs with one on `String("8080")`.
#[derive(Debug, Clone)]
pub struct ResolvedPath {
    pub identity: Vec<SelectorValue>,
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
    Json,
}

/// Parse the format string from `.sync.yaml` into the typed `Format` enum,
/// failing fast (before any file I/O) with a list of supported format names.
pub fn parse_format(format: &str) -> Result<Format> {
    match format {
        "toml" => Ok(Format::Toml),
        "json" => Ok(Format::Json),
        other => bail!("unsupported format: {other}; supported formats: toml, json"),
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

/// True when `table[key]` matches the typed pinned-selector value. Type-strict:
/// `String` only matches `Value::String`, `Int` only `Value::Integer`, `Bool`
/// only `Value::Boolean`. A path of `[k=8080]` never matches `k = "8080"`.
fn matches_pinned(table: &dyn TableLike, key: &str, value: &SelectorValue) -> bool {
    let Some(v) = table.get(key).and_then(|item| item.as_value()) else {
        return false;
    };
    match value {
        SelectorValue::String(s) => v.as_str() == Some(s.as_str()),
        SelectorValue::Int(i) => v.as_integer() == Some(*i),
        SelectorValue::Bool(b) => v.as_bool() == Some(*b),
    }
}

/// Extract the wildcard-key value from `table[key]` as a `SelectorValue` if it
/// is one of the supported scalar types. `None` if the field is absent or has
/// an unsupported type (e.g. float, array, table) — those items are skipped
/// during wildcard expansion just as untyped items used to be.
fn item_selector_value(table: &dyn TableLike, key: &str) -> Option<SelectorValue> {
    let v = table.get(key).and_then(|item| item.as_value())?;
    if let Some(s) = v.as_str() {
        Some(SelectorValue::String(s.to_string()))
    } else if let Some(i) = v.as_integer() {
        Some(SelectorValue::Int(i))
    } else {
        v.as_bool().map(SelectorValue::Bool)
    }
}

/// Build a `toml_edit::Item` from a `SelectorValue`, used when seeding a new
/// array entry whose pinning key didn't exist on this side yet.
fn selector_value_to_item(value: &SelectorValue) -> Item {
    match value {
        SelectorValue::String(s) => {
            Item::Value(Value::String(toml_edit::Formatted::new(s.clone())))
        }
        SelectorValue::Int(i) => Item::Value(Value::Integer(toml_edit::Formatted::new(*i))),
        SelectorValue::Bool(b) => Item::Value(Value::Boolean(toml_edit::Formatted::new(*b))),
    }
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

fn find_pinned_item<'a>(
    arr_item: &'a Item,
    key: &str,
    value: &SelectorValue,
) -> Option<Matched<'a>> {
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
    identity: Vec<SelectorValue>,
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
                bail!("ambiguous pinned selector at {seg}: {count} items where {key}={value}");
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
            let mut counts: BTreeMap<SelectorValue, usize> = BTreeMap::new();
            for item_table in iter_array_items(arr) {
                if let Some(v) = item_selector_value(item_table, key) {
                    *counts.entry(v).or_default() += 1;
                }
            }
            let dups: Vec<String> = counts
                .iter()
                .filter(|(_, c)| **c > 1)
                .map(|(v, c)| format!("{v}×{c}"))
                .collect();
            if !dups.is_empty() {
                bail!(
                    "ambiguous wildcard at {seg}: duplicate {key} values: {}",
                    dups.join(", ")
                );
            }
            for item_table in iter_array_items(arr) {
                let Some(value) = item_selector_value(item_table, key) else {
                    continue;
                };
                let mut branch_id = identity.clone();
                branch_id.push(value.clone());
                let mut branch_resolved = resolved.clone();
                branch_resolved.push(Segment {
                    name: seg.name.clone(),
                    select: Some(ItemSelector::Pinned {
                        key: key.clone(),
                        value,
                    }),
                });
                expand_walk(item_table, rest, branch_id, branch_resolved, out)?;
            }
        }
    }
    Ok(())
}

fn count_pinned_matches(arr_item: &Item, key: &str, value: &SelectorValue) -> usize {
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
    let (first, rest) = segments.split_first()?;
    prefix.push(first.to_string());
    let item = table.get(&first.name)?;
    match &first.select {
        None => {
            if rest.is_empty() {
                // Leaf with no selector can be any value type.
                return None;
            }
            match item.as_table_like() {
                Some(next) => table_conflict_like(next, rest, prefix),
                None => Some(TableConflict {
                    path: prefix.join("."),
                    kind: item.type_name().to_string(),
                    value: summarize_toml_item(item),
                }),
            }
        }
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
    pinned: &SelectorValue,
    rest: &[Segment],
    value: Item,
) -> Result<()> {
    // Find existing match; otherwise append a new table seeded with the key.
    let pos = arr.iter().position(|t| matches_pinned(t, key, pinned));
    let pos = match pos {
        Some(p) => p,
        None => {
            let mut new_table = Table::new();
            new_table.insert(key, selector_value_to_item(pinned));
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
            item_table.insert(key, selector_value_to_item(pinned));
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

// =====================================================================
// JsonDocument
// =====================================================================
//
// Backed by `jsonc-parser` with the `cst` feature. Storage is a CST tree
// that retains comments, trailing commas, blank lines, and original
// whitespace — round-trips through `pull` / `push` / `sync` lose nothing
// the user wrote.
//
// `Self::Item` stays as `serde_json::Value` because Item is the cross-side
// data carrier the engine moves between source and target. Comments don't
// transfer with values during sync — only the value moves; each side's
// pre-existing comments stay attached where the user put them.
//
// What's preserved:
// - Object key order (CST preserves; new keys append)
// - Comments (line `//` and block `/* */`), trailing commas, blank lines
// - Indent style: jsonc-parser infers `indent_text()` from existing
//   structure when inserting — 4-space, tab, 2-space all round-trip
// - Trailing-comma policy follows the source (`uses_trailing_commas`)
//
// What's not:
// - Original string-escape sequences when a string value is *replaced*
//   (jsonc-parser re-emits canonical escapes for the new value)
// - JSON5 forms beyond JSONC (single-quoted strings, unquoted keys,
//   hex / Infinity / NaN literals, etc.)
//
// Limitations explicitly accepted in v1:
// - Floats are not supported as selector values; `[k=1.5]` is rejected
//   by the path parser, and a wildcard hit on a numeric field that is
//   not representable as `i64` is silently skipped (same policy as
//   non-scalar items).

use jsonc_parser::ParseOptions;
use jsonc_parser::cst::{CstArray, CstInputValue, CstObject, CstObjectProp, CstRootNode};
use serde_json::Value as JsonValue;

pub struct JsonDocument {
    /// CST root. Holds the full source text plus parsed structure with
    /// all trivia (comments, whitespace) attached. `CstRootNode` is
    /// `Rc`-based with interior mutability — `&mut self` on the trait
    /// methods is honored at the struct level even though the underlying
    /// CST mutates through `&self` accessors.
    root: CstRootNode,
}

impl JsonDocument {
    pub fn empty() -> Self {
        // Parsing `{}` is the cheapest way to get a root with an empty
        // object value; the alternative (`set_value(CstInputValue::Object)`)
        // requires a root to exist already.
        let root = CstRootNode::parse("{}", &ParseOptions::default())
            .expect("hardcoded {} parses cleanly");
        Self { root }
    }
}

impl Document for JsonDocument {
    type Item = JsonValue;

    fn load(path: &Path, allow_missing: bool) -> Result<Self> {
        if !path.exists() {
            if allow_missing {
                return Ok(Self::empty());
            }
            bail!("file does not exist: {}", path.display());
        }

        let content = fs::read_to_string(path)
            .with_context(|| format!("failed to read {}", path.display()))?;

        // Treat truly empty files as `{}`. JSON itself doesn't allow this,
        // but a freshly created config file commonly is empty until first
        // write — bootstrapping should not error.
        if content.trim().is_empty() {
            return Ok(Self::empty());
        }

        let root = CstRootNode::parse(&content, &ParseOptions::default())
            .map_err(|e| anyhow::anyhow!("failed to parse JSONC {}: {e}", path.display()))?;
        if root.object_value().is_none() {
            bail!("expected JSON object at root of {}", path.display());
        }
        Ok(Self { root })
    }

    fn get(&self, path: &FieldPath) -> Option<JsonValue> {
        // `Some(Value::Null)` distinguishes explicit null from absent
        // (`None`). Sync rules treat the two cases differently — null is
        // a value that propagates; missing is a no-op or fill-in.
        let root_obj = self.root.object_value()?;
        json_get(&root_obj, path.segments())
    }

    fn set(&mut self, path: &FieldPath, item: JsonValue) -> Result<()> {
        let root_obj = self.root.object_value_or_set();
        json_set(&root_obj, path.segments(), item)
    }

    fn table_conflict(&self, path: &FieldPath) -> Option<TableConflict> {
        let root_obj = self.root.object_value()?;
        json_table_conflict_walk(&root_obj, path.segments(), &mut Vec::new())
    }

    fn render(&self) -> String {
        // CST `Display` re-emits the full source with every trivia child
        // intact — comments, blank lines, trailing commas, original
        // indentation. We just guarantee a trailing newline for POSIX
        // cleanliness when one isn't already present (e.g. `empty()`,
        // which parses `{}` without a final newline).
        let mut s = self.root.to_string();
        if !s.ends_with('\n') {
            s.push('\n');
        }
        s
    }

    fn items_equal(a: &JsonValue, b: &JsonValue) -> bool {
        a == b
    }

    fn summarize(item: Option<&JsonValue>) -> String {
        match item {
            None => "<missing>".to_string(),
            Some(item) => summarize_json_item(item),
        }
    }

    fn expand(&self, pattern: &FieldPath) -> Result<Vec<ResolvedPath>> {
        if pattern.segments().iter().all(|s| s.select.is_none()) {
            return Ok(vec![ResolvedPath {
                identity: Vec::new(),
                path: pattern.clone(),
            }]);
        }
        let mut out = Vec::new();
        let root_obj = self
            .root
            .object_value()
            .expect("JsonDocument root is always an object");
        json_expand_walk(
            &root_obj,
            pattern.segments(),
            Vec::new(),
            Vec::new(),
            &mut out,
        )?;
        Ok(out)
    }
}

// ----- JSON helpers -----

fn json_type_name(v: &JsonValue) -> &'static str {
    match v {
        JsonValue::Null => "null",
        JsonValue::Bool(_) => "bool",
        JsonValue::Number(_) => "number",
        JsonValue::String(_) => "string",
        JsonValue::Array(_) => "array",
        JsonValue::Object(_) => "object",
    }
}

fn summarize_json_item(item: &JsonValue) -> String {
    let mut rendered = serde_json::to_string(item).unwrap_or_default();
    if rendered.is_empty() {
        rendered = json_type_name(item).to_string();
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

/// Convert a `serde_json::Value` (the engine's cross-side carrier) into a
/// `CstInputValue` (jsonc-parser's insert/replace value type). Recursion
/// mirrors the value tree; all leaves go through `From` impls.
fn value_to_cst_input(v: &JsonValue) -> CstInputValue {
    match v {
        JsonValue::Null => CstInputValue::Null,
        JsonValue::Bool(b) => CstInputValue::Bool(*b),
        JsonValue::Number(n) => CstInputValue::Number(n.to_string()),
        JsonValue::String(s) => CstInputValue::String(s.clone()),
        JsonValue::Array(a) => CstInputValue::Array(a.iter().map(value_to_cst_input).collect()),
        JsonValue::Object(o) => CstInputValue::Object(
            o.iter()
                .map(|(k, v)| (k.clone(), value_to_cst_input(v)))
                .collect(),
        ),
    }
}

/// Build a `CstInputValue` from a `SelectorValue`. Used to seed a new
/// array entry whose pinning key didn't exist on this side yet.
fn selector_value_to_cst_input(value: &SelectorValue) -> CstInputValue {
    match value {
        SelectorValue::String(s) => CstInputValue::String(s.clone()),
        SelectorValue::Int(i) => CstInputValue::Number(i.to_string()),
        SelectorValue::Bool(b) => CstInputValue::Bool(*b),
    }
}

/// True when CST `obj[key]` matches the typed pinned-selector value.
/// Type-strict: `Int(8080)` never matches the JSON string `"8080"`.
///
/// Inspects leaves directly (avoiding `to_serde_value` on the value node)
/// because jsonc-parser 0.32.3 has a bug where `arr.elements()` after an
/// `append()` can expose a phantom whitespace "string lit" whose
/// `decoded_value()` panics inside `parse_string`. Going through
/// `as_string_lit().decoded_value()` is fine for *real* string lits the
/// caller has already filtered to (e.g. via `as_object()` first), and
/// for booleans / numbers we don't use `decoded_value` at all.
fn cst_object_matches(obj: &CstObject, key: &str, value: &SelectorValue) -> bool {
    let Some(prop) = obj.get(key) else {
        return false;
    };
    let Some(val) = prop.value() else {
        return false;
    };
    match value {
        SelectorValue::String(s) => match val.as_string_lit() {
            Some(sl) => sl
                .decoded_value()
                .ok()
                .as_deref()
                .is_some_and(|got| got == s.as_str()),
            None => false,
        },
        SelectorValue::Int(i) => match val.as_number_lit() {
            Some(nl) => nl.to_string().parse::<i64>().is_ok_and(|got| got == *i),
            None => false,
        },
        SelectorValue::Bool(b) => match val.as_boolean_lit() {
            Some(bl) => bl.value() == *b,
            None => false,
        },
    }
}

/// Extract the wildcard-key value from CST `obj[key]` as a `SelectorValue`
/// if it's one of the supported scalar types. Floats / arrays / objects /
/// null are skipped — the item drops out of wildcard expansion.
fn cst_object_selector_value(obj: &CstObject, key: &str) -> Option<SelectorValue> {
    let prop = obj.get(key)?;
    let val = prop.value()?;
    if let Some(sl) = val.as_string_lit() {
        return sl.decoded_value().ok().map(SelectorValue::String);
    }
    if let Some(nl) = val.as_number_lit() {
        return nl.to_string().parse::<i64>().ok().map(SelectorValue::Int);
    }
    if let Some(bl) = val.as_boolean_lit() {
        return Some(SelectorValue::Bool(bl.value()));
    }
    None
}

fn json_get(obj: &CstObject, segments: &[Segment]) -> Option<JsonValue> {
    let (first, rest) = segments.split_first()?;
    let last = rest.is_empty();
    match &first.select {
        None => {
            let prop = obj.get(&first.name)?;
            let val_node = prop.value()?;
            if last {
                return val_node.to_serde_value();
            }
            let next_obj = val_node.as_object()?;
            json_get(&next_obj, rest)
        }
        Some(ItemSelector::Pinned { key, value }) => {
            let arr = obj.array_value(&first.name)?;
            for elem in arr.elements() {
                let Some(elem_obj) = elem.as_object() else {
                    continue;
                };
                if cst_object_matches(&elem_obj, key, value) {
                    if last {
                        return elem.to_serde_value();
                    }
                    return json_get(&elem_obj, rest);
                }
            }
            None
        }
        Some(ItemSelector::Wildcard { .. }) => None,
    }
}

fn json_table_conflict_walk(
    obj: &CstObject,
    segments: &[Segment],
    prefix: &mut Vec<String>,
) -> Option<TableConflict> {
    let (first, rest) = segments.split_first()?;
    prefix.push(first.to_string());
    let prop = obj.get(&first.name)?;
    let val_node = prop.value()?;
    match &first.select {
        None => {
            if rest.is_empty() {
                return None;
            }
            match val_node.as_object() {
                Some(next) => json_table_conflict_walk(&next, rest, prefix),
                None => Some(cst_conflict_report(prefix, &val_node)),
            }
        }
        Some(ItemSelector::Pinned { key, value: pv }) => {
            let Some(arr) = val_node.as_array() else {
                return Some(cst_conflict_report(prefix, &val_node));
            };
            let matched = arr.elements().into_iter().find_map(|el| {
                let elem_obj = el.as_object()?;
                if cst_object_matches(&elem_obj, key, pv) {
                    Some(elem_obj)
                } else {
                    None
                }
            })?;
            json_table_conflict_walk(&matched, rest, prefix)
        }
        Some(ItemSelector::Wildcard { .. }) => {
            if val_node.as_array().is_none() {
                return Some(cst_conflict_report(prefix, &val_node));
            }
            None
        }
    }
}

/// Build a `TableConflict` from a CST node directly. We deliberately do NOT
/// call `to_serde_value` on the offending node — for container nodes that
/// recurses into descendants and trips over jsonc-parser 0.32.3's phantom
/// whitespace "string lit" bug after an array `append()`. Instead, derive
/// `kind` from CST type accessors and `value` from `Display` (which uses
/// `raw_value` and is panic-safe).
fn cst_conflict_report(prefix: &[String], node: &jsonc_parser::cst::CstNode) -> TableConflict {
    let kind = if node.as_object().is_some() {
        "object"
    } else if node.as_array().is_some() {
        "array"
    } else if node.as_string_lit().is_some() {
        "string"
    } else if node.as_number_lit().is_some() {
        "number"
    } else if node.as_boolean_lit().is_some() {
        "bool"
    } else if node.as_null_keyword().is_some() {
        "null"
    } else {
        "unknown"
    };
    // `Display` of a CstNode walks the original raw text — comments,
    // whitespace and all. Compact it onto one line and truncate so a
    // conflict report stays readable.
    let raw = node
        .to_string()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    const LIMIT: usize = 120;
    let value = if raw.chars().count() > LIMIT {
        let mut t = raw.chars().take(LIMIT - 3).collect::<String>();
        t.push_str("...");
        t
    } else {
        raw
    };
    TableConflict {
        path: prefix.join("."),
        kind: kind.to_string(),
        value,
    }
}

fn json_set(obj: &CstObject, segments: &[Segment], item: JsonValue) -> Result<()> {
    let Some((first, rest)) = segments.split_first() else {
        bail!("path must not be empty");
    };
    if let Some(ItemSelector::Wildcard { .. }) = first.select {
        bail!("wildcard selectors are not supported in JsonDocument writes");
    }
    let last = rest.is_empty();

    match &first.select {
        None => {
            if last {
                let cst = value_to_cst_input(&item);
                if let Some(prop) = obj.get(&first.name) {
                    prop.set_value(cst);
                } else {
                    obj.append(&first.name, cst);
                }
                return Ok(());
            }
            // Ensure object child. `object_value_or_set` overwrites
            // non-object children with an empty object — same behavior
            // as the TOML side's `ensure_table_child`.
            let next_obj = obj.object_value_or_set(&first.name);
            json_set(&next_obj, rest, item)
        }
        Some(ItemSelector::Pinned { key, value: pv }) => {
            let arr = ensure_array_for_pinned(obj, &first.name)?;
            let target_obj = find_or_seed_pinned_item(&arr, key, pv);
            if last {
                replace_pinned_target_with_object(&target_obj, key, pv, item)?;
                return Ok(());
            }
            json_set(&target_obj, rest, item)
        }
        Some(ItemSelector::Wildcard { .. }) => unreachable!("checked above"),
    }
}

/// Ensure `obj[name]` is a CST array; bail on shape conflict (existing
/// non-array value), append a fresh empty array when the key is missing.
fn ensure_array_for_pinned(obj: &CstObject, name: &str) -> Result<CstArray> {
    if let Some(arr) = obj.array_value(name) {
        return Ok(arr);
    }
    if obj.get(name).is_some() {
        bail!("{name} is not an array but path expects one");
    }
    let prop: CstObjectProp = obj.append(name, CstInputValue::Array(Vec::new()));
    Ok(prop
        .array_value()
        .expect("just appended array via CstInputValue::Array"))
}

/// Find the array element whose `key` matches the pinned `SelectorValue`,
/// or append a new object seeded with the pinning key (typed). Returns
/// the matched / new object handle ready for further descent or replace.
fn find_or_seed_pinned_item(arr: &CstArray, key: &str, value: &SelectorValue) -> CstObject {
    for elem in arr.elements() {
        let Some(elem_obj) = elem.as_object() else {
            continue;
        };
        if cst_object_matches(&elem_obj, key, value) {
            return elem_obj;
        }
    }
    // Seed a new object with just the typed pinning key. The wrapping
    // append handles indentation and trailing-comma policy from siblings.
    let seeded = CstInputValue::Object(vec![(key.to_string(), selector_value_to_cst_input(value))]);
    let new_node = arr.append(seeded);
    new_node
        .as_object()
        .expect("just appended CstInputValue::Object")
}

/// Pinned-at-leaf write: replace the entire matched object with `item`'s
/// content, ensuring the pinning key remains so the next selector match
/// still finds the item. `item` must itself be an object (caller error
/// otherwise).
fn replace_pinned_target_with_object(
    target: &CstObject,
    pin_key: &str,
    pin_value: &SelectorValue,
    item: JsonValue,
) -> Result<()> {
    let JsonValue::Object(map) = item else {
        bail!(
            "pinned-selector leaf write requires an object value, got {}",
            json_type_name(&item)
        );
    };
    // Build the replacement property list, ensuring the pinning key is
    // present (callers transferring values across docs may not include it).
    let mut props: Vec<(String, CstInputValue)> = map
        .iter()
        .map(|(k, v)| (k.clone(), value_to_cst_input(v)))
        .collect();
    if !props.iter().any(|(k, _)| k == pin_key) {
        props.insert(
            0,
            (pin_key.to_string(), selector_value_to_cst_input(pin_value)),
        );
    }
    let target_clone = target.clone();
    target_clone.replace_with(CstInputValue::Object(props));
    Ok(())
}

fn json_expand_walk(
    obj: &CstObject,
    remaining: &[Segment],
    identity: Vec<SelectorValue>,
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
            let Some(next_obj) = obj.object_value(&seg.name) else {
                return Ok(());
            };
            let mut new_resolved = resolved;
            new_resolved.push(seg.clone());
            json_expand_walk(&next_obj, rest, identity, new_resolved, out)?;
        }
        Some(ItemSelector::Pinned { key, value }) => {
            let Some(arr) = obj.array_value(&seg.name) else {
                return Ok(());
            };
            let elements = arr.elements();
            let matches: Vec<CstObject> = elements
                .iter()
                .filter_map(|el| {
                    let elem_obj = el.as_object()?;
                    if cst_object_matches(&elem_obj, key, value) {
                        Some(elem_obj)
                    } else {
                        None
                    }
                })
                .collect();
            if matches.len() > 1 {
                bail!(
                    "ambiguous pinned selector at {seg}: {} items where {key}={value}",
                    matches.len()
                );
            }
            let Some(matched) = matches.into_iter().next() else {
                return Ok(());
            };
            let mut new_resolved = resolved;
            new_resolved.push(seg.clone());
            json_expand_walk(&matched, rest, identity, new_resolved, out)?;
        }
        Some(ItemSelector::Wildcard { key }) => {
            let Some(arr) = obj.array_value(&seg.name) else {
                return Ok(());
            };
            let elements = arr.elements();
            let mut counts: BTreeMap<SelectorValue, usize> = BTreeMap::new();
            for el in &elements {
                if let Some(elem_obj) = el.as_object()
                    && let Some(v) = cst_object_selector_value(&elem_obj, key)
                {
                    *counts.entry(v).or_default() += 1;
                }
            }
            let dups: Vec<String> = counts
                .iter()
                .filter(|(_, c)| **c > 1)
                .map(|(v, c)| format!("{v}×{c}"))
                .collect();
            if !dups.is_empty() {
                bail!(
                    "ambiguous wildcard at {seg}: duplicate {key} values: {}",
                    dups.join(", ")
                );
            }
            for el in &elements {
                let Some(elem_obj) = el.as_object() else {
                    continue;
                };
                let Some(value) = cst_object_selector_value(&elem_obj, key) else {
                    continue;
                };
                let mut branch_id = identity.clone();
                branch_id.push(value.clone());
                let mut branch_resolved = resolved.clone();
                branch_resolved.push(Segment {
                    name: seg.name.clone(),
                    select: Some(ItemSelector::Pinned {
                        key: key.clone(),
                        value,
                    }),
                });
                json_expand_walk(&elem_obj, rest, branch_id, branch_resolved, out)?;
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use toml_edit::value;

    use super::{Document, TomlDocument};
    use crate::path::{FieldPath, SelectorValue};

    fn s(v: &str) -> SelectorValue {
        SelectorValue::String(v.to_string())
    }

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
    fn pinned_get_with_int_selector() {
        let doc = TomlDocument {
            doc: r#"
[[servers]]
port = 8080
host = "alpha"

[[servers]]
port = 9090
host = "beta"
"#
            .parse()
            .unwrap(),
        };
        let path = FieldPath::parse("servers[port=9090].host").unwrap();
        assert_eq!(
            doc.get(&path).unwrap().as_value().unwrap().as_str(),
            Some("beta")
        );
    }

    #[test]
    fn pinned_get_with_bool_selector() {
        let doc = TomlDocument {
            doc: r#"
[[entries]]
primary = true
host = "alpha"

[[entries]]
primary = false
host = "beta"
"#
            .parse()
            .unwrap(),
        };
        let path = FieldPath::parse("entries[primary=true].host").unwrap();
        assert_eq!(
            doc.get(&path).unwrap().as_value().unwrap().as_str(),
            Some("alpha")
        );
    }

    #[test]
    fn pinned_selector_is_type_strict() {
        // [k=8080] (int) does not match k = "8080" (string).
        let doc = TomlDocument {
            doc: r#"
[[servers]]
port = "8080"
host = "alpha"
"#
            .parse()
            .unwrap(),
        };
        assert!(
            doc.get(&FieldPath::parse("servers[port=8080].host").unwrap())
                .is_none()
        );
        assert!(
            doc.get(&FieldPath::parse("servers[port=\"8080\"].host").unwrap())
                .is_some()
        );
    }

    #[test]
    fn pinned_set_seeds_int_pinning_key_for_new_item() {
        let mut doc = TomlDocument::empty();
        let path = FieldPath::parse("servers[port=8080].host").unwrap();
        doc.set(&path, value("alpha")).unwrap();

        assert_eq!(
            doc.get(&path).unwrap().as_value().unwrap().as_str(),
            Some("alpha")
        );
        let port_path = FieldPath::parse("servers[port=8080].port").unwrap();
        assert_eq!(
            doc.get(&port_path)
                .unwrap()
                .as_value()
                .unwrap()
                .as_integer(),
            Some(8080)
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
        assert_eq!(resolved[0].identity, vec![s("github")]);
        assert_eq!(
            resolved[0].path.to_string(),
            "mcp_servers[name=\"github\"].enabled"
        );
        assert_eq!(resolved[1].identity, vec![s("linear")]);
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
        assert_eq!(resolved[0].identity, vec![s("gpt-4")]);
        assert_eq!(
            resolved[0].path.to_string(),
            "providers[name=\"openai\"].models[id=\"gpt-4\"].enabled"
        );
        assert_eq!(resolved[1].identity, vec![s("gpt-5")]);
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
    fn conflict_prefix_escapes_pinned_value_quotes() {
        // The container at `arr` is a scalar, so the path-conflict prefix
        // includes the selector verbatim. The selector value contains a
        // literal `"` which must be backslash-escaped so the prefix string
        // round-trips back through the parser.
        let doc = TomlDocument {
            doc: r#"arr = "scalar""#.parse().unwrap(),
        };
        let path = FieldPath::parse(r#"arr[name="he said \"hi\""].field"#).unwrap();
        let conflict = doc.table_conflict(&path).expect("expected conflict");
        assert_eq!(conflict.path, r#"arr[name="he said \"hi\""]"#);
        assert!(
            FieldPath::parse(&conflict.path).is_ok(),
            "prefix {:?} must reparse",
            conflict.path
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
    fn table_conflict_warns_for_single_segment_selector_against_scalar() {
        // Whole-item sync `arr[name]` (selector at the only segment) needs the
        // same array-shape warning as `arr[name].field`. Previously the early
        // return on `segments.len() <= 1` swallowed the check.
        let doc = TomlDocument {
            doc: r#"arr = "scalar""#.parse().unwrap(),
        };
        let pinned = FieldPath::parse(r#"arr[name="github"]"#).unwrap();
        let wildcard = FieldPath::parse("arr[name]").unwrap();
        assert!(doc.table_conflict(&pinned).is_some());
        assert!(doc.table_conflict(&wildcard).is_some());
    }

    #[test]
    fn table_conflict_does_not_warn_for_single_plain_key_segment() {
        // Plain leaf access — `tui` against `tui = "x"` is fine, leaf can be
        // any value type.
        let doc = TomlDocument {
            doc: r#"tui = "monokai""#.parse().unwrap(),
        };
        let path = FieldPath::parse("tui").unwrap();
        assert!(doc.table_conflict(&path).is_none());
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
    fn expand_wildcard_at_last_segment_yields_whole_items() {
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
        let pattern = FieldPath::parse("mcp_servers[name]").unwrap();
        let mut resolved = doc.expand(&pattern).unwrap();
        resolved.sort_by(|a, b| a.identity.cmp(&b.identity));
        assert_eq!(resolved.len(), 2);
        assert_eq!(resolved[0].identity, vec![s("github")]);
        assert_eq!(resolved[0].path.to_string(), "mcp_servers[name=\"github\"]");

        // The resolved path returns the whole item when used with `get`.
        let item = doc.get(&resolved[0].path).unwrap();
        let table = item.as_table().expect("table");
        assert_eq!(table.get("enabled").and_then(|i| i.as_bool()), Some(true));
    }

    #[test]
    fn expand_combines_wildcard_then_pinned_segments() {
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
id = "gpt-4"
enabled = true
"#
            .parse()
            .unwrap(),
        };
        // Wildcard then Pinned: every provider, but only its `gpt-4` model.
        let pattern = FieldPath::parse("providers[name].models[id=\"gpt-4\"].enabled").unwrap();
        let mut resolved = doc.expand(&pattern).unwrap();
        resolved.sort_by(|a, b| a.identity.cmp(&b.identity));
        assert_eq!(resolved.len(), 2);
        assert_eq!(resolved[0].identity, vec![s("anthropic")]);
        assert_eq!(
            resolved[0].path.to_string(),
            "providers[name=\"anthropic\"].models[id=\"gpt-4\"].enabled"
        );
        assert_eq!(resolved[1].identity, vec![s("openai")]);
    }

    #[test]
    fn expand_combines_wildcard_then_wildcard_segments() {
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
        // Wildcard then Wildcard: identity is a 2-tuple of (provider, model).
        let pattern = FieldPath::parse("providers[name].models[id].enabled").unwrap();
        let mut resolved = doc.expand(&pattern).unwrap();
        resolved.sort_by(|a, b| a.identity.cmp(&b.identity));
        assert_eq!(resolved.len(), 3);
        assert_eq!(resolved[0].identity, vec![s("anthropic"), s("opus")]);
        assert_eq!(resolved[1].identity, vec![s("openai"), s("gpt-4")]);
        assert_eq!(resolved[2].identity, vec![s("openai"), s("gpt-5")]);
        assert_eq!(
            resolved[0].path.to_string(),
            "providers[name=\"anthropic\"].models[id=\"opus\"].enabled"
        );
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
        assert_eq!(resolved[0].identity, vec![s("github")]);
    }

    // =================================================================
    // JsonDocument tests
    // =================================================================

    use serde_json::{Value as JsonValue, json};

    use super::JsonDocument;

    fn json_doc(content: &str) -> JsonDocument {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("doc.json");
        std::fs::write(&path, content).unwrap();
        JsonDocument::load(&path, false).unwrap()
    }

    #[test]
    fn json_load_treats_empty_file_as_empty_object() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("doc.json");
        std::fs::write(&path, "").unwrap();
        let doc = JsonDocument::load(&path, false).unwrap();
        assert_eq!(doc.render(), "{}\n");
    }

    #[test]
    fn json_load_rejects_non_object_root() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("doc.json");
        std::fs::write(&path, "[1, 2, 3]").unwrap();
        assert!(JsonDocument::load(&path, false).is_err());
    }

    #[test]
    fn json_set_and_get_nested_values() {
        let mut doc = JsonDocument::empty();
        let path = FieldPath::parse("tui.theme").unwrap();
        doc.set(&path, json!("monokai")).unwrap();
        assert_eq!(doc.get(&path).unwrap(), json!("monokai"));
    }

    #[test]
    fn json_get_distinguishes_explicit_null_from_missing() {
        let doc = json_doc(r#"{"explicit": null}"#);
        let exp = FieldPath::parse("explicit").unwrap();
        let miss = FieldPath::parse("missing").unwrap();
        assert_eq!(doc.get(&exp), Some(JsonValue::Null));
        assert_eq!(doc.get(&miss), None);
    }

    #[test]
    fn json_render_preserves_key_order() {
        let doc = json_doc(r#"{"z": 1, "a": 2, "m": 3}"#);
        let rendered = doc.render();
        let z = rendered.find("\"z\"").unwrap();
        let a = rendered.find("\"a\"").unwrap();
        let m = rendered.find("\"m\"").unwrap();
        assert!(z < a && a < m, "expected z<a<m order; got {rendered}");
    }

    #[test]
    fn json_render_uses_pretty_with_trailing_newline() {
        let mut doc = JsonDocument::empty();
        doc.set(&FieldPath::parse("a.b").unwrap(), json!(1))
            .unwrap();
        let rendered = doc.render();
        assert!(rendered.ends_with('\n'));
        assert!(
            rendered.contains("  "),
            "expected 2-space indent: {rendered}"
        );
    }

    #[test]
    fn json_render_preserves_four_space_indent_from_source() {
        let doc = json_doc("{\n    \"a\": 1,\n    \"b\": {\n        \"c\": 2\n    }\n}\n");
        let rendered = doc.render();
        // First nested level keeps 4 spaces.
        assert!(
            rendered.contains("\n    \"a\": 1"),
            "expected 4-space indent, got: {rendered}"
        );
        // Deeper level keeps 8 spaces.
        assert!(
            rendered.contains("\n        \"c\": 2"),
            "expected 8-space nested indent, got: {rendered}"
        );
    }

    #[test]
    fn json_render_preserves_tab_indent_from_source() {
        let doc = json_doc("{\n\t\"a\": 1\n}\n");
        let rendered = doc.render();
        assert!(
            rendered.contains("\n\t\"a\": 1"),
            "expected tab indent, got: {rendered:?}"
        );
    }

    #[test]
    fn json_render_preserves_minified_format() {
        // CST round-trip preserves whatever format the source uses —
        // minified input stays minified, the renderer doesn't impose
        // pretty-print on a file that wasn't pretty-printed.
        let doc = json_doc(r#"{"a":1,"b":{"c":2}}"#);
        let rendered = doc.render();
        assert!(
            !rendered.contains("\n  \"a\""),
            "minified input should not be re-pretty-printed: {rendered}"
        );
        assert!(rendered.starts_with(r#"{"a":1"#), "got: {rendered}");
    }

    #[test]
    fn json_render_keeps_indent_after_set_writes() {
        // Sniff once on load, then a write through `set` must not lose
        // the sniffed style.
        let mut doc = json_doc("{\n    \"a\": 1\n}\n");
        doc.set(&FieldPath::parse("b").unwrap(), json!(2)).unwrap();
        let rendered = doc.render();
        assert!(
            rendered.contains("\n    \"b\": 2"),
            "expected 4-space indent preserved after set, got: {rendered}"
        );
    }

    #[test]
    fn json_pinned_get_with_string_selector() {
        let doc = json_doc(
            r#"{"mcpServers": [{"name": "github", "enabled": true}, {"name": "linear", "enabled": false}]}"#,
        );
        let path = FieldPath::parse("mcpServers[name=\"github\"].enabled").unwrap();
        assert_eq!(doc.get(&path).unwrap(), json!(true));
    }

    #[test]
    fn json_pinned_get_with_int_selector() {
        let doc = json_doc(
            r#"{"servers": [{"port": 8080, "host": "alpha"}, {"port": 9090, "host": "beta"}]}"#,
        );
        let path = FieldPath::parse("servers[port=9090].host").unwrap();
        assert_eq!(doc.get(&path).unwrap(), json!("beta"));
    }

    #[test]
    fn json_pinned_get_with_bool_selector() {
        let doc = json_doc(
            r#"{"entries": [{"primary": true, "host": "alpha"}, {"primary": false, "host": "beta"}]}"#,
        );
        let path = FieldPath::parse("entries[primary=true].host").unwrap();
        assert_eq!(doc.get(&path).unwrap(), json!("alpha"));
    }

    #[test]
    fn json_pinned_selector_is_type_strict() {
        let doc = json_doc(r#"{"servers": [{"port": "8080", "host": "alpha"}]}"#);
        assert!(
            doc.get(&FieldPath::parse("servers[port=8080].host").unwrap())
                .is_none()
        );
        assert_eq!(
            doc.get(&FieldPath::parse("servers[port=\"8080\"].host").unwrap())
                .unwrap(),
            json!("alpha")
        );
    }

    #[test]
    fn json_pinned_set_appends_with_int_pinning_key() {
        let mut doc = JsonDocument::empty();
        let path = FieldPath::parse("servers[port=8080].host").unwrap();
        doc.set(&path, json!("alpha")).unwrap();
        assert_eq!(doc.get(&path).unwrap(), json!("alpha"));
        // The seeded pinning key must round-trip as a JSON Number, not a
        // String. Equality against `json!(8080)` already enforces this
        // (Value's PartialEq is type-strict), but spell out the type
        // expectation at the call site for parity with the TOML test.
        let port_path = FieldPath::parse("servers[port=8080].port").unwrap();
        let port = doc.get(&port_path).unwrap();
        assert_eq!(port.as_i64(), Some(8080), "port should be JSON Number");
        assert!(port.is_number(), "port should not be stringified: {port:?}");
    }

    #[test]
    fn json_pinned_set_updates_existing_and_preserves_siblings() {
        let mut doc = json_doc(
            r#"{"mcpServers": [{"name": "github", "enabled": true, "url": "https://api.github.com"}, {"name": "linear", "enabled": false}]}"#,
        );
        let path = FieldPath::parse("mcpServers[name=\"github\"].enabled").unwrap();
        doc.set(&path, json!(false)).unwrap();
        assert_eq!(doc.get(&path).unwrap(), json!(false));
        let url = FieldPath::parse("mcpServers[name=\"github\"].url").unwrap();
        assert_eq!(doc.get(&url).unwrap(), json!("https://api.github.com"));
        let linear = FieldPath::parse("mcpServers[name=\"linear\"].enabled").unwrap();
        assert_eq!(doc.get(&linear).unwrap(), json!(false));
    }

    #[test]
    fn json_table_conflict_reports_non_object_prefix() {
        let doc = json_doc(r#"{"settings": "plain"}"#);
        let path = FieldPath::parse("settings.theme").unwrap();
        let conflict = doc.table_conflict(&path).expect("expected conflict");
        assert_eq!(conflict.path, "settings");
        assert_eq!(conflict.kind, "string");
    }

    #[test]
    fn json_expand_wildcard_fans_out_with_typed_identity() {
        let doc = json_doc(
            r#"{"mcpServers": [{"name": "github", "enabled": true}, {"name": "linear", "enabled": false}]}"#,
        );
        let pattern = FieldPath::parse("mcpServers[name].enabled").unwrap();
        let mut resolved = doc.expand(&pattern).unwrap();
        resolved.sort_by(|a, b| a.identity.cmp(&b.identity));
        assert_eq!(resolved.len(), 2);
        assert_eq!(resolved[0].identity, vec![s("github")]);
        assert_eq!(resolved[1].identity, vec![s("linear")]);
    }

    #[test]
    fn json_expand_errors_on_wildcard_duplicate_identifier() {
        let doc = json_doc(
            r#"{"mcpServers": [{"name": "github", "enabled": true}, {"name": "github", "enabled": false}]}"#,
        );
        let pattern = FieldPath::parse("mcpServers[name].enabled").unwrap();
        let err = doc.expand(&pattern).unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains("ambiguous wildcard"), "msg: {msg}");
        assert!(msg.contains("\"github\""), "msg: {msg}");
    }
}
