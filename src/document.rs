use std::collections::BTreeMap;
use std::fs;
use std::path::Path;

use anyhow::{Context, Result, bail};
use toml_edit::{ArrayOfTables, DocumentMut, InlineTable, Item, Table, TableLike, Value};

use crate::discovery::{FieldNode, FieldTree, detect_identifier_key};
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

    /// Walk the document and produce a `FieldTree` describing every
    /// selectable sync path. Used by the interactive `add` picker so
    /// users can pick fields without knowing path syntax up front.
    ///
    /// Tree shape:
    /// - Object keys become container nodes; their leaves are scalar
    ///   children.
    /// - Arrays of objects with a detectable identifier (`name` / `id` /
    ///   `key` / `slug` priority) become a parent node containing a
    ///   virtual `[name=*]` wildcard cluster plus one pinned-item
    ///   container per concrete element.
    /// - Arrays without a detectable identifier and arrays of scalars
    ///   are skipped — the user has to write those paths manually.
    fn discover_field_tree(&self) -> FieldTree;
}

/// Supported document formats. Adding a variant prompts the compiler to
/// flag every dispatch site as non-exhaustive — that is the whole point.
/// Do not paper over with a wildcard arm.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Format {
    Toml,
    Json,
    GitConfig,
}

/// Parse the format string from `.sync.yaml` into the typed `Format` enum,
/// failing fast (before any file I/O) with a list of supported format names.
///
/// `json` and `jsonc` are aliases — both go through the JSONC-aware parser.
/// `jsonc` lets a user with VS Code / `tsconfig` files self-document the
/// fact that comments and trailing commas are expected, even though the
/// underlying handling is identical to `json`.
pub fn parse_format(format: &str) -> Result<Format> {
    match format {
        "toml" => Ok(Format::Toml),
        "json" | "jsonc" => Ok(Format::Json),
        "gitconfig" => Ok(Format::GitConfig),
        other => {
            bail!("unsupported format: {other}; supported formats: toml, json, jsonc, gitconfig")
        }
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

    fn discover_field_tree(&self) -> FieldTree {
        let mut roots = Vec::new();
        toml_walk_table_for_discovery(self.doc.as_table(), &[], &mut roots);
        FieldTree { roots }
    }
}

// ----- TOML field-tree discovery -----
//
// Walks an `&dyn TableLike` recursively, producing FieldNodes per key.
// Path tracking is via ancestors (a slice of Segments built up from the
// root) so each emitted FieldPath is fully qualified.
//
// Arrays of objects with a detectable identifier produce a container
// (the array key itself) plus a virtual `[name=*]` wildcard group and
// one pinned-item container per concrete entry. Arrays without a
// detectable identifier are skipped — the user has to write those
// paths manually since we can't fabricate stable identity.

fn toml_walk_table_for_discovery(
    table: &dyn TableLike,
    ancestors: &[Segment],
    out: &mut Vec<FieldNode>,
) {
    for (key, item) in table.iter() {
        let mut seg_ancestors = ancestors.to_vec();
        seg_ancestors.push(Segment {
            name: key.to_string(),
            select: None,
        });
        let path = FieldPath::from_segments(seg_ancestors.clone());

        if let Some(child_table) = item.as_table_like() {
            // Plain object container — recurse for its keys.
            let mut children = Vec::new();
            toml_walk_table_for_discovery(child_table, &seg_ancestors, &mut children);
            out.push(FieldNode::object(key, path, children));
            continue;
        }

        match item {
            Item::ArrayOfTables(arr) => {
                let items: Vec<&Table> = arr.iter().collect();
                if let Some(node) = toml_array_of_objects_node(
                    key,
                    &items
                        .iter()
                        .map(|t| *t as &dyn TableLike)
                        .collect::<Vec<_>>(),
                    &seg_ancestors,
                ) {
                    out.push(node);
                }
            }
            Item::Value(Value::Array(arr)) => {
                let inline_tables: Vec<&InlineTable> =
                    arr.iter().filter_map(|v| v.as_inline_table()).collect();
                if inline_tables.len() == arr.len() && !inline_tables.is_empty() {
                    let as_table_like: Vec<&dyn TableLike> =
                        inline_tables.iter().map(|t| *t as &dyn TableLike).collect();
                    if let Some(node) =
                        toml_array_of_objects_node(key, &as_table_like, &seg_ancestors)
                    {
                        out.push(node);
                    }
                }
                // Arrays of scalars / mixed: skip — user writes manually.
            }
            _ => {
                // Plain scalar leaf — value is whatever (string / int / bool / ...).
                out.push(FieldNode::leaf(key, path));
            }
        }
    }
}

/// Build a FieldNode for an array of objects. Returns `None` when no
/// usable identifier exists across the items (silent skip — surface a
/// hint to the user via picker output if useful later).
fn toml_array_of_objects_node(
    key: &str,
    items: &[&dyn TableLike],
    ancestors: &[Segment],
) -> Option<FieldNode> {
    let id_key = detect_identifier_key(items.len(), |i, k| {
        items[i]
            .get(k)
            .and_then(|item| item.as_value())
            .and_then(|v| v.as_str())
            .map(|s| s.to_string())
    })?;

    // Wildcard cluster: virtual group with one leaf per shared scalar
    // key across items (excluding the identifier itself, which is
    // implicit in the wildcard's `[name=*]` form).
    let mut wildcard_leaves = Vec::new();
    let shared_leaf_keys = toml_shared_scalar_leaf_keys(items, &id_key);
    for leaf_key in &shared_leaf_keys {
        let mut wc_ancestors = ancestors.to_vec();
        // Replace the trailing segment's select with Wildcard.
        let last = wc_ancestors.last_mut().expect("array key on stack");
        last.select = Some(ItemSelector::Wildcard {
            key: id_key.clone(),
        });
        wc_ancestors.push(Segment {
            name: leaf_key.clone(),
            select: None,
        });
        let wc_path = FieldPath::from_segments(wc_ancestors);
        wildcard_leaves.push(FieldNode::leaf(leaf_key, wc_path));
    }
    let wildcard_group = FieldNode::virtual_group(format!("[{id_key}=*]"), wildcard_leaves);

    // Per-item pinned containers.
    let mut pinned_items = Vec::new();
    for item in items {
        let id_value = item
            .get(&id_key)
            .and_then(|i| i.as_value())
            .and_then(|v| v.as_str())
            .map(String::from);
        let Some(id_value) = id_value else { continue };

        let mut pin_ancestors = ancestors.to_vec();
        let last = pin_ancestors.last_mut().expect("array key on stack");
        last.select = Some(ItemSelector::Pinned {
            key: id_key.clone(),
            value: SelectorValue::String(id_value.clone()),
        });
        let pin_path = FieldPath::from_segments(pin_ancestors.clone());

        let mut item_children = Vec::new();
        toml_walk_table_for_discovery(*item, &pin_ancestors, &mut item_children);

        pinned_items.push(FieldNode::pinned_item(
            format!("[{id_key}=\"{id_value}\"]"),
            pin_path,
            item_children,
        ));
    }

    let mut children = Vec::new();
    children.push(wildcard_group);
    children.extend(pinned_items);

    // The array key itself gets a "whole array sync" path (path =
    // ancestors as-is, no selector). It's a container with the wildcard
    // group + per-item children.
    let array_path = FieldPath::from_segments(ancestors.to_vec());
    Some(FieldNode::object(key, array_path, children))
}

/// Collect scalar leaf keys (string / int / bool / float / etc.) that
/// appear in *every* item, excluding the identifier key itself. These
/// are the keys the wildcard `[name=*].leaf` form can address.
fn toml_shared_scalar_leaf_keys(items: &[&dyn TableLike], id_key: &str) -> Vec<String> {
    if items.is_empty() {
        return Vec::new();
    }
    let mut common: Option<Vec<String>> = None;
    for item in items {
        let mut keys: Vec<String> = item
            .iter()
            .filter_map(|(k, v)| {
                if k == id_key {
                    return None;
                }
                let is_scalar = v.as_value().is_some_and(|val| {
                    val.as_str().is_some()
                        || val.as_integer().is_some()
                        || val.as_bool().is_some()
                        || val.as_float().is_some()
                        || val.as_datetime().is_some()
                });
                if is_scalar { Some(k.to_string()) } else { None }
            })
            .collect();
        keys.sort();
        common = Some(match common {
            None => keys,
            Some(prev) => prev.into_iter().filter(|k| keys.contains(k)).collect(),
        });
    }
    common.unwrap_or_default()
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
use jsonc_parser::cst::{CstArray, CstInputValue, CstNode, CstObject, CstObjectProp, CstRootNode};
use serde_json::Value as JsonValue;

/// Format-preserving JSON / JSONC document.
///
/// **Single-threaded only.** `CstRootNode` is `Rc<…>`-based with interior
/// mutability, so `JsonDocument` is intentionally `!Send + !Sync`. The
/// sync engine processes targets sequentially today; if that ever
/// changes, the storage needs to switch to an `Arc`-based variant or
/// each target needs its own document instance per thread (cheap — just
/// re-parse from the file).
pub struct JsonDocument {
    /// CST root. Holds the full source text plus parsed structure with
    /// all trivia (comments, whitespace) attached. `&mut self` on the
    /// trait methods is honored at the struct level even though the
    /// underlying CST mutates through `&self` accessors.
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

    fn discover_field_tree(&self) -> FieldTree {
        let mut roots = Vec::new();
        let root_obj = self
            .root
            .object_value()
            .expect("JsonDocument root is always an object");
        json_walk_object_for_discovery(&root_obj, &[], &mut roots);
        FieldTree { roots }
    }
}

// ----- JSON field-tree discovery -----

fn json_walk_object_for_discovery(
    obj: &CstObject,
    ancestors: &[Segment],
    out: &mut Vec<FieldNode>,
) {
    for prop in obj.properties() {
        let Some(name_node) = prop.name() else {
            continue;
        };
        let key = if let Some(sl) = name_node.as_string_lit() {
            match sl.decoded_value() {
                Ok(s) => s,
                Err(_) => continue,
            }
        } else if let Some(wl) = name_node.as_word_lit() {
            wl.to_string()
        } else {
            continue;
        };

        let Some(val_node) = prop.value() else {
            continue;
        };

        let mut seg_ancestors = ancestors.to_vec();
        seg_ancestors.push(Segment {
            name: key.clone(),
            select: None,
        });
        let path = FieldPath::from_segments(seg_ancestors.clone());

        if let Some(child_obj) = val_node.as_object() {
            let mut children = Vec::new();
            json_walk_object_for_discovery(&child_obj, &seg_ancestors, &mut children);
            out.push(FieldNode::object(key, path, children));
            continue;
        }

        if let Some(arr) = val_node.as_array() {
            // Filter to object elements only. `as_object()` returning
            // `Some` is sufficient post jsonc-parser 0.32.4 — the
            // phantom-string-lit bug that previously needed a separate
            // pre-filter is fixed upstream.
            let elements = arr.elements();
            let item_objs: Vec<CstObject> =
                elements.iter().filter_map(|el| el.as_object()).collect();
            if !item_objs.is_empty()
                && item_objs.len() == elements.len()
                && let Some(node) = json_array_of_objects_node(&key, &item_objs, &seg_ancestors)
            {
                out.push(node);
            }
            // Mixed / scalar / no-objects arrays: skip.
            continue;
        }

        // Scalar (string / number / bool / null) — leaf.
        out.push(FieldNode::leaf(key, path));
    }
}

fn json_array_of_objects_node(
    key: &str,
    items: &[CstObject],
    ancestors: &[Segment],
) -> Option<FieldNode> {
    let id_key = detect_identifier_key(items.len(), |i, k| {
        items[i]
            .get(k)
            .and_then(|p| p.value())
            .and_then(|v| v.as_string_lit())
            .and_then(|sl| sl.decoded_value().ok())
    })?;

    // Wildcard cluster.
    let mut wildcard_leaves = Vec::new();
    let shared_leaf_keys = json_shared_scalar_leaf_keys(items, &id_key);
    for leaf_key in &shared_leaf_keys {
        let mut wc_ancestors = ancestors.to_vec();
        let last = wc_ancestors.last_mut().expect("array key on stack");
        last.select = Some(ItemSelector::Wildcard {
            key: id_key.clone(),
        });
        wc_ancestors.push(Segment {
            name: leaf_key.clone(),
            select: None,
        });
        let wc_path = FieldPath::from_segments(wc_ancestors);
        wildcard_leaves.push(FieldNode::leaf(leaf_key, wc_path));
    }
    let wildcard_group = FieldNode::virtual_group(format!("[{id_key}=*]"), wildcard_leaves);

    // Per-item pinned containers.
    let mut pinned_items = Vec::new();
    for item in items {
        let id_value = item
            .get(&id_key)
            .and_then(|p| p.value())
            .and_then(|v| v.as_string_lit())
            .and_then(|sl| sl.decoded_value().ok());
        let Some(id_value) = id_value else { continue };

        let mut pin_ancestors = ancestors.to_vec();
        let last = pin_ancestors.last_mut().expect("array key on stack");
        last.select = Some(ItemSelector::Pinned {
            key: id_key.clone(),
            value: SelectorValue::String(id_value.clone()),
        });
        let pin_path = FieldPath::from_segments(pin_ancestors.clone());

        let mut item_children = Vec::new();
        json_walk_object_for_discovery(item, &pin_ancestors, &mut item_children);

        pinned_items.push(FieldNode::pinned_item(
            format!("[{id_key}=\"{id_value}\"]"),
            pin_path,
            item_children,
        ));
    }

    let mut children = Vec::new();
    children.push(wildcard_group);
    children.extend(pinned_items);

    let array_path = FieldPath::from_segments(ancestors.to_vec());
    Some(FieldNode::object(key, array_path, children))
}

fn json_shared_scalar_leaf_keys(items: &[CstObject], id_key: &str) -> Vec<String> {
    if items.is_empty() {
        return Vec::new();
    }
    let mut common: Option<Vec<String>> = None;
    for item in items {
        let mut keys: Vec<String> = item
            .properties()
            .iter()
            .filter_map(|prop| {
                let name_node = prop.name()?;
                let k = if let Some(sl) = name_node.as_string_lit() {
                    sl.decoded_value().ok()?
                } else if let Some(wl) = name_node.as_word_lit() {
                    wl.to_string()
                } else {
                    return None;
                };
                if k == id_key {
                    return None;
                }
                let val = prop.value()?;
                let is_scalar = val.as_string_lit().is_some()
                    || val.as_number_lit().is_some()
                    || val.as_boolean_lit().is_some()
                    || val.as_null_keyword().is_some();
                if is_scalar { Some(k) } else { None }
            })
            .collect();
        keys.sort();
        common = Some(match common {
            None => keys,
            Some(prev) => prev.into_iter().filter(|k| keys.contains(k)).collect(),
        });
    }
    common.unwrap_or_default()
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
/// Inspects leaves directly via `as_string_lit` / `as_number_lit` /
/// `as_boolean_lit` rather than going through `to_serde_value()`. This
/// avoids walking entire subtrees just to compare one scalar — a real
/// performance win on large arrays — and originally also worked around
/// a jsonc-parser 0.32.3 bug
/// (<https://github.com/dprint/jsonc-parser/issues/78>, fixed in 0.32.4).
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

/// Build a `TableConflict` from a CST node directly. We derive `kind`
/// from CST type accessors and `value` from `Display` rather than
/// going through `to_serde_value()`, which would recurse into the
/// entire subtree just to fill the report's two short fields.
/// Originally also worked around jsonc-parser 0.32.3's phantom
/// whitespace "string lit" bug after `append()`
/// (<https://github.com/dprint/jsonc-parser/issues/78>, fixed in 0.32.4).
fn cst_conflict_report(prefix: &[String], node: &CstNode) -> TableConflict {
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
    // Build the replacement property list, then *force* the pinning key
    // to the selector's value. Just checking that the key name exists
    // isn't enough: the replacement payload may carry the key with a
    // different value (e.g. cross-doc transfer where the source has
    // `name: "linear"` but the target path is `name="github"`). Without
    // this, the replaced item silently stops matching its own selector,
    // and the next sync pass appends a duplicate entry instead of finding
    // it.
    //
    // Override-in-place if the pin key is already in the payload —
    // preserving the payload's key order — and append at the end if
    // missing. The payload's order comes from `serde_json::Map` with
    // `preserve_order` (an `IndexMap`), so `.iter()` is insertion order.
    let mut props: Vec<(String, CstInputValue)> = map
        .iter()
        .map(|(k, v)| (k.clone(), value_to_cst_input(v)))
        .collect();
    let pin_input = selector_value_to_cst_input(pin_value);
    match props.iter().position(|(k, _)| k.as_str() == pin_key) {
        Some(i) => props[i].1 = pin_input,
        None => props.push((pin_key.to_string(), pin_input)),
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

// =====================================================================
// GitConfigDocument
// =====================================================================

/// gitconfig backend, wrapping `gix_config::File`. The library handles
/// format-preserving round-trip — comments, blank lines, tab/space
/// indentation, and `[section "subsection"]` quoting all survive
/// read-modify-write cycles. Multivar keys (multiple values for the
/// same key, e.g. several `remote.origin.fetch =` lines) round-trip in
/// memory, but dot-sync's surgical-sync model treats them as ambiguous
/// — see `get`/`set` for the policy.
///
/// Path semantics on this backend differ from the structured TOML/JSON
/// backends. A `FieldPath` of two segments addresses `section.key`; a
/// path of three addresses `section.subsection.key`. Anything deeper
/// is invalid because gitconfig has no nested objects under a key.
/// Subsections containing characters that need escaping in dot-sync's
/// path syntax (e.g. `[includeIf "gitdir:~/work/"]`) use the existing
/// quoted-segment form: `includeIf."gitdir:~/work/".path`.
pub struct GitConfigDocument {
    file: gix_config::File<'static>,
}

impl GitConfigDocument {
    pub fn empty() -> Self {
        Self {
            file: gix_config::File::new(gix_config::file::Metadata::default()),
        }
    }

    /// `true` iff some section in the file matches `key` byte-for-byte
    /// on section name + subsection name + has at least one value name
    /// matching `key.key` byte-for-byte. Used by `get` to refuse
    /// resolving paths whose case doesn't match the file.
    fn case_exact_match_exists(&self, key: &GitConfigKey) -> bool {
        for s in self.file.sections() {
            if !section_header_bytes_eq(s.header(), key) {
                continue;
            }
            for vn in s.body().value_names() {
                if bstr_bytes(vn) == key.key.as_bytes() {
                    return true;
                }
            }
        }
        false
    }

    /// Bail if any section / subsection / key in the file matches
    /// `key` case-insensitively but not byte-for-byte. A clean-miss
    /// (no case-insensitive overlap) is fine — that means we're
    /// creating a brand-new section / key with the path's own case.
    fn check_case_sensitivity(&self, key: &GitConfigKey, path: &FieldPath) -> Result<()> {
        for s in self.file.sections() {
            let h = s.header();
            let h_name_bytes = bstr_bytes_from_bstr(h.name());
            let h_sub_bytes = h.subsection_name().map(bstr_bytes_from_bstr);
            let key_sub_bytes = key.subsection.as_deref().map(str::as_bytes);

            let name_ci = h_name_bytes.eq_ignore_ascii_case(key.section.as_bytes());
            let name_exact = h_name_bytes == key.section.as_bytes();
            let sub_ci = match (h_sub_bytes, key_sub_bytes) {
                (Some(a), Some(b)) => a.eq_ignore_ascii_case(b),
                (None, None) => true,
                _ => false,
            };
            let sub_exact = h_sub_bytes == key_sub_bytes;
            if !(name_ci && sub_ci) {
                continue;
            }
            if !(name_exact && sub_exact) {
                bail!(
                    "gitconfig path '{path}' case-mismatches existing section header \
                     [{section}{sub_disp}] (dot-sync requires byte-exact section / \
                     subsection names; git itself is case-insensitive but dot-sync \
                     is not)",
                    section = h.name(),
                    sub_disp = match h.subsection_name() {
                        Some(b) => format!(" \"{b}\""),
                        None => String::new(),
                    }
                );
            }
            // Section + subsection are case-exact. Now check value names
            // in this section's body.
            for vn in s.body().value_names() {
                let vn_bytes = bstr_bytes(vn);
                if vn_bytes.eq_ignore_ascii_case(key.key.as_bytes())
                    && vn_bytes != key.key.as_bytes()
                {
                    bail!(
                        "gitconfig path '{path}' case-mismatches existing key '{}' \
                         in section [{section}{sub_disp}]",
                        bstr::BStr::new(vn_bytes),
                        section = h.name(),
                        sub_disp = match h.subsection_name() {
                            Some(b) => format!(" \"{b}\""),
                            None => String::new(),
                        }
                    );
                }
            }
        }
        Ok(())
    }
}

fn section_header_bytes_eq(h: &gix_config::parse::section::Header<'_>, key: &GitConfigKey) -> bool {
    if bstr_bytes_from_bstr(h.name()) != key.section.as_bytes() {
        return false;
    }
    match (h.subsection_name(), key.subsection.as_deref()) {
        (Some(a), Some(b)) => bstr_bytes_from_bstr(a) == b.as_bytes(),
        (None, None) => true,
        _ => false,
    }
}

fn bstr_bytes_from_bstr(b: &bstr::BStr) -> &[u8] {
    b
}

fn bstr_bytes<'a>(vn: &'a gix_config::parse::section::ValueName<'_>) -> &'a [u8] {
    // ValueName derefs to BStr which derefs to [u8].
    let bs: &bstr::BStr = vn;
    bs
}

/// Resolved gitconfig key parts from a `FieldPath`.
///
/// `section` and `key` are mandatory, `subsection` is optional. We pass
/// these to gix-config's `_by`-suffixed APIs to avoid having gix-config
/// re-parse a dotted form — that re-parse would mis-handle subsections
/// containing dots (e.g. `[includeIf "gitdir:~/work/"]`).
struct GitConfigKey {
    section: String,
    subsection: Option<String>,
    key: String,
}

fn path_to_gitconfig_key(path: &FieldPath) -> Result<GitConfigKey> {
    if path.segments().iter().any(|s| s.select.is_some()) {
        bail!(
            "array selectors are not supported in gitconfig paths: {path} \
             — gitconfig has no arrays of objects"
        );
    }
    let names: Vec<&str> = path.segments().iter().map(|s| s.name.as_str()).collect();
    match names.as_slice() {
        [] => bail!("empty gitconfig path"),
        [_section] => bail!(
            "gitconfig path needs at least 2 segments (section.key): \
             '{path}' is missing a key"
        ),
        [section, key] => Ok(GitConfigKey {
            section: (*section).to_string(),
            subsection: None,
            key: (*key).to_string(),
        }),
        [section, subsection, key] => Ok(GitConfigKey {
            section: (*section).to_string(),
            subsection: Some((*subsection).to_string()),
            key: (*key).to_string(),
        }),
        _ => bail!(
            "gitconfig path has too many segments (max 3 — section.subsection.key): {path}"
        ),
    }
}

impl Document for GitConfigDocument {
    type Item = String;

    fn load(path: &Path, allow_missing: bool) -> Result<Self> {
        if !path.exists() {
            if allow_missing {
                return Ok(Self::empty());
            }
            bail!("file does not exist: {}", path.display());
        }
        // git allows backslash-continued multi-line values, but
        // gix-config 0.56 parses them incorrectly: trailing fragments
        // become spurious empty-value keys, and write-back mangles the
        // line layout. Detect the marker bytes (`\` immediately before
        // `\n`) at the file level and refuse to load such files rather
        // than silently corrupt them. Drop this guard once gitoxide
        // ships a fix.
        let content = fs::read_to_string(path)
            .with_context(|| format!("failed to read {}", path.display()))?;
        if content.contains("\\\n") {
            bail!(
                "gitconfig file {} uses backslash-continued multi-line values, \
                 which dot-sync's gitconfig backend does not support yet \
                 (gix-config mangles them on round-trip). Inline the value \
                 onto a single line or remove the continuation.",
                path.display()
            );
        }
        let file =
            gix_config::File::from_path_no_includes(path.to_path_buf(), gix_config::Source::Local)
                .with_context(|| format!("failed to parse gitconfig {}", path.display()))?;
        Ok(Self { file })
    }

    fn get(&self, path: &FieldPath) -> Option<String> {
        // Invalid paths (selector segments, wrong arity) can never resolve
        // to a value, so swallow the error here. `set` surfaces the same
        // error properly when the caller actually tries to write.
        let key = path_to_gitconfig_key(path).ok()?;
        // Case-sensitive matching diverges from git's native case-
        // insensitive behavior on purpose — dot-sync's path syntax is
        // case-sensitive across all backends, and silently letting
        // `User.email` resolve through `[user]` would be a footgun for
        // users hand-writing `.sync.yaml` paths. If the path's case
        // doesn't byte-equal an existing section / subsection / key,
        // treat it as absent.
        if !self.case_exact_match_exists(&key) {
            return None;
        }
        let sub = key.subsection.as_deref().map(bstr::BStr::new);
        let value = self.file.string_by(&key.section, sub, &key.key)?;
        Some(value.to_string())
    }

    fn set(&mut self, path: &FieldPath, item: String) -> Result<()> {
        let key = path_to_gitconfig_key(path)?;
        // Case-sensitive guard: if the file has a section / subsection
        // / key that case-insensitively matches but does not byte-match
        // exactly, that's almost certainly a typo in `.sync.yaml`.
        // Surface it loudly rather than silently writing into the
        // existing case-different entry.
        self.check_case_sensitivity(&key, path)?;

        let sub = key.subsection.as_deref().map(bstr::BStr::new);
        // Multivar guard: if the destination key already has more than
        // one value (e.g. multiple `remote.origin.fetch =` lines),
        // surgical sync can't pick which one to overwrite. Bail rather
        // than silently mangle the file. Same "data corruption" stance
        // that the array-selector backends take on duplicate
        // identifiers.
        if let Ok(values) = self.file.raw_values_by(key.section.as_str(), sub, key.key.as_str())
            && values.len() > 1
        {
            bail!(
                "gitconfig path '{path}' has {} values (multivar); \
                 dot-sync requires single-valued keys for surgical sync",
                values.len()
            );
        }
        // gix-config's `File<'static>` requires owned key + value
        // material. The library validates section / key names against
        // git's grammar (alphanumeric + dash, leading alpha) and
        // returns its own error; surface that verbatim.
        let value: &bstr::BStr = bstr::BStr::new(item.as_bytes());
        self.file
            .set_raw_value_by(key.section.as_str(), sub, key.key, value)
            .map_err(|e| anyhow::anyhow!("failed to set gitconfig path '{path}': {e}"))?;
        Ok(())
    }

    fn table_conflict(&self, _path: &FieldPath) -> Option<TableConflict> {
        // gitconfig keys cannot contain nested values — a key is always
        // a leaf scalar — so writing a value at any valid path can never
        // clobber a "table" the way TOML/JSON can. Always None.
        None
    }

    fn render(&self) -> String {
        let bytes = self.file.to_bstring();
        // gix_config preserves the original input bytes, which for
        // pre-existing files keeps the trailing newline status the user
        // had. For docs constructed via `empty()` + edits, the rendered
        // output may lack a trailing newline; force one for POSIX
        // cleanliness, matching the JSON backend.
        let mut s = bytes.to_string();
        if !s.is_empty() && !s.ends_with('\n') {
            s.push('\n');
        }
        s
    }

    fn items_equal(a: &String, b: &String) -> bool {
        a == b
    }

    fn summarize(item: Option<&String>) -> String {
        match item {
            None => "<missing>".to_string(),
            Some(v) => format!("\"{}\"", v),
        }
    }

    fn expand(&self, pattern: &FieldPath) -> Result<Vec<ResolvedPath>> {
        // gitconfig has no arrays of objects — the only multi-value
        // construct is multivar (multiple lines with the same key),
        // which is handled at `set` time. Selector segments
        // (`arr[name="x"]` or `arr[name]`) can never resolve here.
        if pattern.segments().iter().any(|s| s.select.is_some()) {
            bail!(
                "gitconfig has no arrays of objects; selector path '{pattern}' is not supported"
            );
        }
        Ok(vec![ResolvedPath {
            identity: Vec::new(),
            path: pattern.clone(),
        }])
    }

    fn discover_field_tree(&self) -> FieldTree {
        // Walk every section in the file and bucket its keys by
        // (section name, optional subsection name). Sections / keys
        // that appear more than once collapse — git-config allows
        // splitting a section across multiple `[user]` headers, but
        // for picker purposes we just want a deduped list of paths.
        let mut by_section: BTreeMap<String, BTreeMap<Option<String>, Vec<String>>> =
            BTreeMap::new();
        for section in self.file.sections() {
            let header = section.header();
            let section_name = header.name().to_string();
            let subsection_name = header.subsection_name().map(|b| b.to_string());
            let body = section.body();
            let bucket = by_section
                .entry(section_name.clone())
                .or_default()
                .entry(subsection_name)
                .or_default();

            // value_names() may yield the same key multiple times for
            // multivar lines. Dedup, then drop the multivar entries
            // entirely — they won't round-trip through dot-sync's
            // single-value sync model, and surfacing them in the
            // picker would be a footgun.
            let mut seen: std::collections::BTreeSet<String> =
                std::collections::BTreeSet::new();
            for value_name in body.value_names() {
                let key = value_name.to_string();
                if !seen.insert(key.clone()) {
                    continue;
                }
                if body.values(&key).len() > 1 {
                    continue;
                }
                if !bucket.contains(&key) {
                    bucket.push(key);
                }
            }
        }

        let mut roots = Vec::new();
        for (section_name, subsections) in by_section {
            let mut section_children: Vec<FieldNode> = Vec::new();
            for (sub_opt, keys) in subsections {
                match sub_opt {
                    None => {
                        for key in keys {
                            let path = leaf_path(&[&section_name, &key]);
                            section_children.push(FieldNode::leaf(key, path));
                        }
                    }
                    Some(sub) => {
                        let mut sub_children = Vec::new();
                        for key in keys {
                            let path = leaf_path(&[&section_name, &sub, &key]);
                            sub_children.push(FieldNode::leaf(key, path));
                        }
                        // Subsection has no path of its own — gitconfig
                        // can't address `[remote "origin"]` as a value,
                        // only its keys. VirtualGroup matches the
                        // "select children individually, no whole" cycle.
                        section_children.push(FieldNode::virtual_group(
                            format!("\"{sub}\""),
                            sub_children,
                        ));
                    }
                }
            }
            if !section_children.is_empty() {
                // Section header itself has no associated value either
                // — same reasoning as for subsections. VirtualGroup so
                // `[x]` (whole-subtree) is correctly unavailable.
                roots.push(FieldNode::virtual_group(section_name, section_children));
            }
        }
        FieldTree { roots }
    }
}

/// Build a `FieldPath` from raw segment names, no selectors. Subsection
/// names containing characters that would confuse the path parser
/// (dots, brackets, quotes) round-trip through `Segment.name` directly,
/// not through `parse` — so we use `from_segments` instead of building
/// and reparsing a string.
fn leaf_path(parts: &[&str]) -> FieldPath {
    let segments = parts
        .iter()
        .map(|name| Segment {
            name: (*name).to_string(),
            select: None,
        })
        .collect();
    FieldPath::from_segments(segments)
}

#[cfg(test)]
mod tests {
    use toml_edit::value;

    use super::{Document, Format, TomlDocument, parse_format};
    use crate::path::{FieldPath, SelectorValue};

    fn s(v: &str) -> SelectorValue {
        SelectorValue::String(v.to_string())
    }

    #[test]
    fn parse_format_accepts_known_names() {
        assert_eq!(parse_format("toml").unwrap(), Format::Toml);
        assert_eq!(parse_format("json").unwrap(), Format::Json);
        // jsonc is an alias for json — same backend, more honest naming.
        assert_eq!(parse_format("jsonc").unwrap(), Format::Json);
    }

    #[test]
    fn parse_format_rejects_unknown_names() {
        let err = parse_format("yaml").unwrap_err().to_string();
        assert!(err.contains("yaml"), "msg: {err}");
        assert!(err.contains("toml, json, jsonc, gitconfig"), "msg: {err}");
    }

    #[test]
    fn parse_format_accepts_gitconfig() {
        assert_eq!(parse_format("gitconfig").unwrap(), Format::GitConfig);
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
    fn json_empty_renders_as_object_with_newline() {
        // Direct constructor — covers the `expect("hardcoded {} parses
        // cleanly")` path. Lock the basic invariant so a jsonc-parser
        // upgrade that breaks `{}` parsing surfaces here, not in the
        // engine's bootstrap path.
        let doc = JsonDocument::empty();
        assert_eq!(doc.render(), "{}\n");
    }

    #[test]
    fn json_empty_supports_set_then_get() {
        // empty() returns a usable document — set / get round-trips
        // through the CST without going through file I/O.
        let mut doc = JsonDocument::empty();
        let path = FieldPath::parse("a.b").unwrap();
        doc.set(&path, json!(1)).unwrap();
        assert_eq!(doc.get(&path), Some(json!(1)));
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
    fn json_get_returns_float_value_as_number() {
        // Float source values must round-trip without truncation.
        // `Number::from_f64` rejects NaN/Inf — those remain rare in
        // dotfiles and aren't tested here, but plain finite floats
        // (1.5, -2.25, 0.0) must come back as Number, not int or string.
        let doc = json_doc(r#"{"a": 1.5, "b": -2.25, "c": 0.0}"#);
        let a = doc.get(&FieldPath::parse("a").unwrap()).unwrap();
        let b = doc.get(&FieldPath::parse("b").unwrap()).unwrap();
        let c = doc.get(&FieldPath::parse("c").unwrap()).unwrap();
        assert!(a.is_number(), "a should be number: {a:?}");
        assert_eq!(a.as_f64(), Some(1.5));
        assert!(b.is_number(), "b should be number: {b:?}");
        assert_eq!(b.as_f64(), Some(-2.25));
        // 0.0 may parse as i64 first (we try i64 before f64 in the
        // safe walker) — accept either, the value just needs to be 0.
        assert!(c.is_number(), "c should be number: {c:?}");
        assert_eq!(c.as_f64(), Some(0.0));
    }

    #[test]
    fn json_set_then_render_preserves_float_value() {
        // Setting a float value through the trait — round-trip through
        // `value_to_cst_input` (which uses Number(n.to_string())) must
        // keep the float literal intact in the rendered output.
        let mut doc = JsonDocument::empty();
        doc.set(&FieldPath::parse("temperature").unwrap(), json!(0.7))
            .unwrap();
        let rendered = doc.render();
        assert!(
            rendered.contains("0.7"),
            "rendered should contain 0.7 literally: {rendered}"
        );
        // Get back returns the same float.
        assert_eq!(
            doc.get(&FieldPath::parse("temperature").unwrap()),
            Some(json!(0.7))
        );
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
    fn json_pinned_leaf_set_forces_pin_key_to_selector_value() {
        // Pinned-leaf write where the replacement payload carries the pin
        // key with the *wrong* value (e.g. sourced from a doc that uses a
        // different name for the same role). The matched item must still
        // match the selector after the write — otherwise the next sync
        // pass appends a duplicate instead of finding this entry.
        let mut doc = json_doc(
            r#"{"mcpServers": [{"name": "github", "enabled": true, "url": "https://api.github.com"}]}"#,
        );
        let path = FieldPath::parse("mcpServers[name=\"github\"]").unwrap();
        // Replacement object's `name` is wrong; force_pin_key must override.
        doc.set(&path, json!({ "name": "linear", "enabled": false }))
            .unwrap();

        // The item still matches the original `name="github"` selector.
        let enabled = FieldPath::parse("mcpServers[name=\"github\"].enabled").unwrap();
        assert_eq!(doc.get(&enabled).unwrap(), json!(false));
        // And does NOT now match `name="linear"`.
        let linear = FieldPath::parse("mcpServers[name=\"linear\"].enabled").unwrap();
        assert!(doc.get(&linear).is_none());
    }

    #[test]
    fn json_pinned_leaf_set_preserves_payload_key_order() {
        // The pin key in the replacement payload is in the middle. The
        // override must keep payload order intact (a, name, b), not
        // reorder name to the front. Locks the in-place override behavior.
        let mut doc = json_doc(r#"{"servers": [{"name": "github"}]}"#);
        let path = FieldPath::parse("servers[name=\"github\"]").unwrap();
        doc.set(&path, json!({ "a": 1, "name": "ignored", "b": 2 }))
            .unwrap();

        let rendered = doc.render();
        let a_pos = rendered.find("\"a\"").expect("a present");
        let name_pos = rendered.find("\"name\"").expect("name present");
        let b_pos = rendered.find("\"b\"").expect("b present");
        assert!(
            a_pos < name_pos && name_pos < b_pos,
            "expected payload order a < name < b, got: {rendered}"
        );
        // Pin key value still canonical despite payload override attempt.
        let name = FieldPath::parse("servers[name=\"github\"].name").unwrap();
        assert_eq!(doc.get(&name).unwrap(), json!("github"));
    }

    #[test]
    fn json_pinned_leaf_set_appends_pin_key_when_payload_lacks_it() {
        // Payload has no pin key at all; must append at end so the matched
        // item still satisfies the selector after the write.
        let mut doc = json_doc(r#"{"servers": [{"name": "github"}]}"#);
        let path = FieldPath::parse("servers[name=\"github\"]").unwrap();
        doc.set(&path, json!({ "host": "api.github.com" })).unwrap();

        let rendered = doc.render();
        let host_pos = rendered.find("\"host\"").expect("host present");
        let name_pos = rendered.find("\"name\"").expect("name present");
        assert!(
            host_pos < name_pos,
            "payload's host should come before appended pin key name: {rendered}"
        );
        let host = FieldPath::parse("servers[name=\"github\"].host").unwrap();
        assert_eq!(doc.get(&host).unwrap(), json!("api.github.com"));
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

    // ----- JSON parity tests with the TOML side -----
    //
    // The engine is generic over `Document`, but get/set/expand/table_conflict
    // dispatch through backend-specific helpers. Each TOML test below has a
    // corresponding JSON test exercising the same edge case so a regression in
    // either backend's helper code surfaces with the same expected behavior.

    #[test]
    fn json_pinned_set_creates_array_when_missing() {
        // Pinned write into a doc that has no array key at all — the
        // helper must seed the array, not fail.
        let mut doc = JsonDocument::empty();
        let path = FieldPath::parse("mcpServers[name=\"github\"].enabled").unwrap();
        doc.set(&path, json!(true)).unwrap();
        assert_eq!(doc.get(&path), Some(json!(true)));
    }

    #[test]
    fn json_pinned_get_returns_none_when_no_match() {
        // Array exists but no element satisfies the selector — get must
        // return None, not panic or pick a wrong element.
        let doc = json_doc(r#"{"mcpServers": [{"name": "linear"}]}"#);
        let path = FieldPath::parse("mcpServers[name=\"github\"].enabled").unwrap();
        assert!(doc.get(&path).is_none());
    }

    #[test]
    fn json_expand_returns_pattern_unchanged_for_no_wildcard() {
        // No wildcard segment means the pattern is its own resolution —
        // single ResolvedPath with empty identity, equal to the input.
        let doc = JsonDocument::empty();
        let path = FieldPath::parse("tui.theme").unwrap();
        let resolved = doc.expand(&path).unwrap();
        assert_eq!(resolved.len(), 1);
        assert!(resolved[0].identity.is_empty());
        assert_eq!(resolved[0].path, path);
    }

    #[test]
    fn json_expand_returns_empty_when_array_missing() {
        // Wildcard against a missing array — empty resolution, no error.
        let doc = JsonDocument::empty();
        let pattern = FieldPath::parse("mcpServers[name].enabled").unwrap();
        let resolved = doc.expand(&pattern).unwrap();
        assert!(resolved.is_empty());
    }

    #[test]
    fn json_expand_errors_on_pinned_multi_match() {
        // Two array items share the same selector value — surgical sync
        // requires unambiguous identity, so this is an error.
        let doc = json_doc(
            r#"{"mcpServers": [{"name": "github", "enabled": true}, {"name": "github", "enabled": false}]}"#,
        );
        let pattern = FieldPath::parse("mcpServers[name=\"github\"].enabled").unwrap();
        let err = doc.expand(&pattern).unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains("ambiguous pinned"), "msg: {msg}");
        assert!(msg.contains("name=\"github\""), "msg: {msg}");
        assert!(msg.contains("2 items"), "msg: {msg}");
    }

    #[test]
    fn json_expand_combines_pinned_and_wildcard_segments() {
        let doc = json_doc(
            r#"{
              "providers": [
                {"name": "openai", "models": [{"id": "gpt-4", "enabled": true}, {"id": "gpt-5", "enabled": false}]},
                {"name": "anthropic", "models": [{"id": "opus", "enabled": true}]}
              ]
            }"#,
        );
        // Pinned then Wildcard: only openai's models, but every model.
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
    fn json_expand_combines_wildcard_then_pinned_segments() {
        let doc = json_doc(
            r#"{
              "providers": [
                {"name": "openai", "models": [{"id": "gpt-4", "enabled": true}, {"id": "gpt-5", "enabled": false}]},
                {"name": "anthropic", "models": [{"id": "gpt-4", "enabled": true}]}
              ]
            }"#,
        );
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
    fn json_expand_combines_wildcard_then_wildcard_segments() {
        let doc = json_doc(
            r#"{
              "providers": [
                {"name": "openai", "models": [{"id": "gpt-4", "enabled": true}, {"id": "gpt-5", "enabled": false}]},
                {"name": "anthropic", "models": [{"id": "opus", "enabled": true}]}
              ]
            }"#,
        );
        // Wildcard then Wildcard: identity is a 2-tuple (provider, model).
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
    fn json_expand_wildcard_at_last_segment_yields_whole_items() {
        // Pattern ends in a wildcard with no further descent — each
        // resolved path returns the whole matched object via `get`.
        let doc = json_doc(
            r#"{"mcpServers": [{"name": "github", "enabled": true}, {"name": "linear", "enabled": false}]}"#,
        );
        let pattern = FieldPath::parse("mcpServers[name]").unwrap();
        let mut resolved = doc.expand(&pattern).unwrap();
        resolved.sort_by(|a, b| a.identity.cmp(&b.identity));
        assert_eq!(resolved.len(), 2);
        assert_eq!(resolved[0].identity, vec![s("github")]);
        assert_eq!(resolved[0].path.to_string(), "mcpServers[name=\"github\"]");

        // The resolved path returns the whole item when used with `get`.
        let item = doc.get(&resolved[0].path).expect("item present");
        let obj = item.as_object().expect("object");
        assert_eq!(obj.get("enabled"), Some(&json!(true)));
    }

    #[test]
    fn json_expand_skips_items_lacking_the_identifier_key() {
        // An item without the wildcard's identifier key drops out of the
        // expansion silently (no error) — same policy as the TOML side.
        let doc =
            json_doc(r#"{"mcpServers": [{"name": "github", "enabled": true}, {"enabled": true}]}"#);
        let pattern = FieldPath::parse("mcpServers[name].enabled").unwrap();
        let resolved = doc.expand(&pattern).unwrap();
        assert_eq!(resolved.len(), 1);
        assert_eq!(resolved[0].identity, vec![s("github")]);
    }

    #[test]
    fn json_table_conflict_warns_when_wildcard_target_is_not_an_array() {
        // Wildcard against a non-array value reports the conflict at
        // `<key>[<id>]` so the user can see the offending position
        // including the selector marker.
        let doc = json_doc(r#"{"mcpServers": "not an array"}"#);
        let path = FieldPath::parse("mcpServers[name].enabled").unwrap();
        let conflict = doc.table_conflict(&path).expect("expected conflict");
        assert_eq!(conflict.path, "mcpServers[name]");
        assert!(
            conflict.value.contains("not an array"),
            "value should include the offender's text: {conflict:?}"
        );
    }

    #[test]
    fn json_table_conflict_warns_for_single_segment_selector_against_scalar() {
        // Whole-item sync `arr[name="x"]` and `arr[name]` (selector at the
        // only segment) need the same array-shape warning as
        // `arr[name].field`. Mirrors the TOML regression test.
        let doc = json_doc(r#"{"arr": "scalar"}"#);
        let pinned = FieldPath::parse(r#"arr[name="github"]"#).unwrap();
        let wildcard = FieldPath::parse("arr[name]").unwrap();
        assert!(doc.table_conflict(&pinned).is_some());
        assert!(doc.table_conflict(&wildcard).is_some());
    }

    #[test]
    fn json_table_conflict_does_not_warn_for_single_plain_key_segment() {
        // Plain leaf access — `tui` against `"tui": "monokai"` is fine,
        // leaf can be any value type.
        let doc = json_doc(r#"{"tui": "monokai"}"#);
        let path = FieldPath::parse("tui").unwrap();
        assert!(doc.table_conflict(&path).is_none());
    }

    #[test]
    fn json_conflict_prefix_escapes_pinned_value_quotes() {
        // The container at `arr` is a scalar, so the conflict prefix
        // includes the selector verbatim. The selector value contains a
        // literal `"` which must be backslash-escaped so the prefix
        // round-trips back through the parser.
        let doc = json_doc(r#"{"arr": "scalar"}"#);
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
    fn json_pinned_set_then_get_works_through_nested_arrays() {
        // Set deep through pinned + pinned, then get back. Locks the
        // descend_set / descend_get pairing on a multi-level path.
        let mut doc = json_doc(
            r#"{
              "providers": [
                {"name": "openai", "models": [{"id": "gpt-4", "enabled": false}]}
              ]
            }"#,
        );
        let path =
            FieldPath::parse("providers[name=\"openai\"].models[id=\"gpt-4\"].enabled").unwrap();
        doc.set(&path, json!(true)).unwrap();
        assert_eq!(doc.get(&path), Some(json!(true)));
    }

    #[test]
    fn json_get_on_container_after_array_append_does_not_panic() {
        // Regression guard for jsonc-parser's phantom-string-lit bug
        // (https://github.com/dprint/jsonc-parser/issues/78). 0.32.3
        // would expose a whitespace "string lit" in arr.elements() after
        // append(), and to_serde_value() on the parent array would
        // panic walking through it. Fixed in 0.32.4 — kept as a
        // regression guard so a future dep regression surfaces here.
        let mut doc = json_doc(r#"{"servers": [{"name": "a"}, {"name": "b"}]}"#);
        // Append a third entry — triggers the phantom condition.
        doc.set(
            &FieldPath::parse("servers[name=\"c\"].host").unwrap(),
            json!("c.example"),
        )
        .unwrap();

        // Now get the whole array as a Value. Without the safe walker
        // this panics inside parse_string with "Expected \", was Some(' ')".
        let arr = doc
            .get(&FieldPath::parse("servers").unwrap())
            .expect("array present");
        let arr = arr.as_array().expect("got array");
        // Three real elements, phantom filtered.
        assert_eq!(arr.len(), 3);
        assert_eq!(arr[0]["name"], json!("a"));
        assert_eq!(arr[1]["name"], json!("b"));
        assert_eq!(arr[2]["name"], json!("c"));
        assert_eq!(arr[2]["host"], json!("c.example"));
    }

    #[test]
    fn json_get_on_object_after_set_returns_full_subtree() {
        // Get on a non-leaf (object) path after a set returns the
        // fully-converted subtree, going through the safe walker for
        // every nested value.
        let mut doc = JsonDocument::empty();
        doc.set(&FieldPath::parse("a.b.c").unwrap(), json!(1))
            .unwrap();
        doc.set(&FieldPath::parse("a.b.d").unwrap(), json!("x"))
            .unwrap();

        let a = doc.get(&FieldPath::parse("a").unwrap()).expect("a present");
        assert_eq!(a, json!({"b": {"c": 1, "d": "x"}}));
    }

    // =================================================================
    // Field-tree discovery tests
    // =================================================================

    use crate::discovery::FieldNodeKind;

    #[test]
    fn toml_discover_emits_leaves_and_object_containers() {
        let doc = TomlDocument {
            doc: r#"
project_doc_max_bytes = 65536

[tui]
theme = "monokai"
status_line = true
"#
            .parse()
            .unwrap(),
        };
        let tree = doc.discover_field_tree();
        let names: Vec<&str> = tree.roots.iter().map(|n| n.display.as_str()).collect();
        assert!(names.contains(&"project_doc_max_bytes"));
        assert!(names.contains(&"tui"));

        let tui = tree.roots.iter().find(|n| n.display == "tui").unwrap();
        assert_eq!(tui.kind, FieldNodeKind::Object);
        let tui_keys: Vec<&str> = tui.children.iter().map(|c| c.display.as_str()).collect();
        assert!(tui_keys.contains(&"theme"));
        assert!(tui_keys.contains(&"status_line"));
        let theme = tui.children.iter().find(|c| c.display == "theme").unwrap();
        assert_eq!(theme.kind, FieldNodeKind::Leaf);
        assert_eq!(theme.path.as_ref().unwrap().to_string(), "tui.theme");
    }

    #[test]
    fn toml_discover_array_of_objects_creates_wildcard_and_pinned_groups() {
        let doc = TomlDocument {
            doc: r#"
[[mcp_servers]]
name = "github"
enabled = true
url = "https://api.github.com"

[[mcp_servers]]
name = "linear"
enabled = false
url = "https://linear.app"
"#
            .parse()
            .unwrap(),
        };
        let tree = doc.discover_field_tree();
        let mcp = tree
            .roots
            .iter()
            .find(|n| n.display == "mcp_servers")
            .expect("mcp_servers present");
        assert_eq!(mcp.kind, FieldNodeKind::Object);

        // First child: wildcard virtual group.
        let wildcard = &mcp.children[0];
        assert_eq!(wildcard.display, "[name=*]");
        assert_eq!(wildcard.kind, FieldNodeKind::VirtualGroup);
        assert!(wildcard.path.is_none());
        let wc_leaf_keys: Vec<&str> = wildcard
            .children
            .iter()
            .map(|c| c.display.as_str())
            .collect();
        assert!(wc_leaf_keys.contains(&"enabled"));
        assert!(wc_leaf_keys.contains(&"url"));
        // Identifier key (`name`) excluded from the wildcard's leaves.
        assert!(!wc_leaf_keys.contains(&"name"));

        // Wildcard leaf paths use [name] form, not pinned.
        let wc_enabled = wildcard
            .children
            .iter()
            .find(|c| c.display == "enabled")
            .unwrap();
        assert_eq!(
            wc_enabled.path.as_ref().unwrap().to_string(),
            "mcp_servers[name].enabled"
        );

        // Per-item pinned containers follow.
        let pinned_displays: Vec<&str> = mcp.children[1..]
            .iter()
            .map(|c| c.display.as_str())
            .collect();
        assert_eq!(
            pinned_displays,
            vec!["[name=\"github\"]", "[name=\"linear\"]"]
        );
        let github = mcp
            .children
            .iter()
            .find(|c| c.display == "[name=\"github\"]")
            .unwrap();
        assert_eq!(github.kind, FieldNodeKind::PinnedArrayItem);
        assert_eq!(
            github.path.as_ref().unwrap().to_string(),
            "mcp_servers[name=\"github\"]"
        );
        let github_enabled = github
            .children
            .iter()
            .find(|c| c.display == "enabled")
            .unwrap();
        assert_eq!(
            github_enabled.path.as_ref().unwrap().to_string(),
            "mcp_servers[name=\"github\"].enabled"
        );
    }

    #[test]
    fn toml_discover_skips_array_with_no_detectable_identifier() {
        // Two items both `name = "x"` — duplicates can't disambiguate.
        let doc = TomlDocument {
            doc: r#"
[[items]]
name = "x"

[[items]]
name = "x"
"#
            .parse()
            .unwrap(),
        };
        let tree = doc.discover_field_tree();
        assert!(
            tree.roots.iter().all(|n| n.display != "items"),
            "items should be skipped: {:?}",
            tree.roots.iter().map(|n| &n.display).collect::<Vec<_>>()
        );
    }

    #[test]
    fn json_discover_emits_leaves_and_object_containers() {
        let doc = json_doc(
            r#"{
              "max_bytes": 65536,
              "tui": {
                "theme": "monokai",
                "status_line": true
              }
            }"#,
        );
        let tree = doc.discover_field_tree();
        let names: Vec<&str> = tree.roots.iter().map(|n| n.display.as_str()).collect();
        assert!(names.contains(&"max_bytes"));
        assert!(names.contains(&"tui"));

        let tui = tree.roots.iter().find(|n| n.display == "tui").unwrap();
        assert_eq!(tui.kind, FieldNodeKind::Object);
        let theme = tui.children.iter().find(|c| c.display == "theme").unwrap();
        assert_eq!(theme.path.as_ref().unwrap().to_string(), "tui.theme");
    }

    #[test]
    fn json_discover_array_of_objects_creates_wildcard_and_pinned_groups() {
        let doc = json_doc(
            r#"{
              "mcpServers": [
                {"name": "github", "enabled": true, "url": "https://api.github.com"},
                {"name": "linear", "enabled": false, "url": "https://linear.app"}
              ]
            }"#,
        );
        let tree = doc.discover_field_tree();
        let mcp = tree
            .roots
            .iter()
            .find(|n| n.display == "mcpServers")
            .unwrap();
        assert_eq!(mcp.kind, FieldNodeKind::Object);

        let wildcard = &mcp.children[0];
        assert_eq!(wildcard.display, "[name=*]");
        assert_eq!(wildcard.kind, FieldNodeKind::VirtualGroup);
        let wc_enabled = wildcard
            .children
            .iter()
            .find(|c| c.display == "enabled")
            .unwrap();
        assert_eq!(
            wc_enabled.path.as_ref().unwrap().to_string(),
            "mcpServers[name].enabled"
        );

        let github = mcp
            .children
            .iter()
            .find(|c| c.display == "[name=\"github\"]")
            .unwrap();
        let github_url = github.children.iter().find(|c| c.display == "url").unwrap();
        assert_eq!(
            github_url.path.as_ref().unwrap().to_string(),
            "mcpServers[name=\"github\"].url"
        );
    }

    #[test]
    fn json_discover_skips_array_of_scalars() {
        let doc = json_doc(r#"{"things": [1, 2, 3]}"#);
        let tree = doc.discover_field_tree();
        assert!(tree.roots.iter().all(|n| n.display != "things"));
    }

    #[test]
    fn json_discover_returns_empty_tree_for_empty_object() {
        // Empty document — no roots. Picker code special-cases an empty
        // tree to "Confirmed(empty)" so the user sees a "no fields"
        // message rather than an empty TUI.
        let doc = JsonDocument::empty();
        let tree = doc.discover_field_tree();
        assert!(tree.roots.is_empty(), "expected empty tree from empty doc");
    }

    #[test]
    fn toml_discover_returns_empty_tree_for_empty_doc() {
        let doc = TomlDocument::empty();
        let tree = doc.discover_field_tree();
        assert!(tree.roots.is_empty(), "expected empty tree from empty doc");
    }
}

// =====================================================================
// GitConfigDocument tests
// =====================================================================

#[cfg(test)]
mod gitconfig_tests {
    use super::{Document, GitConfigDocument};

    fn write_fixture(content: &str) -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("config"), content).unwrap();
        dir
    }

    #[test]
    fn loads_and_renders_unchanged_input_byte_for_byte() {
        // gix-config preserves comments / blank lines / tab indentation /
        // [section "subsection"] quoting on a no-op load → render. Keep
        // this contract under test — the whole gitconfig backend depends
        // on it.
        let original = "\
# top-level header

[user]
\tname = Alice
\t# inline comment
\temail = alice@example.com

; semicolon comment
[remote \"origin\"]
\turl = https://example.com/foo
\tfetch = +refs/heads/*:refs/remotes/origin/*
\tfetch = +refs/tags/*:refs/tags/*
";
        let dir = write_fixture(original);
        let doc = GitConfigDocument::load(&dir.path().join("config"), false).unwrap();
        assert_eq!(doc.render(), original);
    }

    #[test]
    fn empty_doc_renders_to_empty_string() {
        let doc = GitConfigDocument::empty();
        assert_eq!(doc.render(), "");
    }

    #[test]
    fn allow_missing_returns_empty() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("does-not-exist");
        let doc = GitConfigDocument::load(&missing, true).unwrap();
        assert_eq!(doc.render(), "");
    }

    #[test]
    fn missing_without_allow_errors() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("does-not-exist");
        let result = GitConfigDocument::load(&missing, false);
        let err = match result {
            Ok(_) => panic!("expected an error for missing file"),
            Err(e) => e,
        };
        assert!(err.to_string().contains("does not exist"));
    }

    fn doc_from(content: &str) -> (tempfile::TempDir, GitConfigDocument) {
        let dir = write_fixture(content);
        let doc = GitConfigDocument::load(&dir.path().join("config"), false).unwrap();
        (dir, doc)
    }

    use crate::path::FieldPath;

    #[test]
    fn get_reads_section_key() {
        let (_dir, doc) = doc_from("[user]\n\temail = alice@example.com\n");
        let path = FieldPath::parse("user.email").unwrap();
        assert_eq!(doc.get(&path), Some("alice@example.com".to_string()));
    }

    #[test]
    fn get_reads_section_subsection_key() {
        let (_dir, doc) = doc_from(
            "[remote \"origin\"]\n\turl = https://example.com/foo\n",
        );
        let path = FieldPath::parse("remote.origin.url").unwrap();
        assert_eq!(doc.get(&path), Some("https://example.com/foo".to_string()));
    }

    #[test]
    fn get_reads_quoted_subsection_with_special_chars() {
        // Subsections may contain almost anything; dot-sync expresses
        // them via the existing quoted-segment syntax. The key under
        // [includeIf "gitdir:~/work/"] is reachable as
        // includeIf."gitdir:~/work/".path.
        let (_dir, doc) = doc_from(
            "[includeIf \"gitdir:~/work/\"]\n\tpath = ~/.gitconfig-work\n",
        );
        let path = FieldPath::parse("includeIf.\"gitdir:~/work/\".path").unwrap();
        assert_eq!(doc.get(&path), Some("~/.gitconfig-work".to_string()));
    }

    #[test]
    fn get_returns_none_for_absent_key() {
        let (_dir, doc) = doc_from("[user]\n\temail = a@b\n");
        let path = FieldPath::parse("user.name").unwrap();
        assert_eq!(doc.get(&path), None);
    }

    #[test]
    fn get_returns_none_for_absent_section() {
        let (_dir, doc) = doc_from("[user]\n\temail = a@b\n");
        let path = FieldPath::parse("core.editor").unwrap();
        assert_eq!(doc.get(&path), None);
    }

    #[test]
    fn get_returns_none_for_invalid_path_arity() {
        // Single-segment path can't address anything in gitconfig (no
        // top-level keys without a section). `get` swallows the error
        // and returns None — `set` surfaces it.
        let (_dir, doc) = doc_from("[user]\n\temail = a@b\n");
        let too_short = FieldPath::parse("user").unwrap();
        let too_long = FieldPath::parse("a.b.c.d").unwrap();
        assert_eq!(doc.get(&too_short), None);
        assert_eq!(doc.get(&too_long), None);
    }

    // ----- set -----

    #[test]
    fn set_updates_existing_scalar_in_place() {
        // The byte-exact assertion locks in that gix-config replaces only
        // the value bytes — surrounding comments, blank lines, and tab
        // indentation are untouched.
        let original = "\
# header
[user]
\tname = Alice
\t# inline
\temail = old@example.com
";
        let (_dir, mut doc) = doc_from(original);
        let path = FieldPath::parse("user.email").unwrap();
        doc.set(&path, "new@example.com".to_string()).unwrap();

        let expected = "\
# header
[user]
\tname = Alice
\t# inline
\temail = new@example.com
";
        assert_eq!(doc.render(), expected);
    }

    #[test]
    fn set_updates_value_inside_subsection() {
        let original = "\
[remote \"origin\"]
\turl = https://old.example.com
";
        let (_dir, mut doc) = doc_from(original);
        let path = FieldPath::parse("remote.origin.url").unwrap();
        doc.set(&path, "https://new.example.com".to_string()).unwrap();
        assert_eq!(
            doc.render(),
            "[remote \"origin\"]\n\turl = https://new.example.com\n",
        );
    }

    #[test]
    fn set_inserts_new_key_into_existing_section() {
        let original = "\
[core]
\teditor = vim
";
        let (_dir, mut doc) = doc_from(original);
        let path = FieldPath::parse("core.autocrlf").unwrap();
        doc.set(&path, "input".to_string()).unwrap();
        // Round-trip: the read-back value matches what we just wrote.
        assert_eq!(doc.get(&path), Some("input".to_string()));
        // Original section header + existing key both preserved.
        let rendered = doc.render();
        assert!(rendered.contains("[core]"), "got: {rendered}");
        assert!(rendered.contains("editor = vim"), "got: {rendered}");
        assert!(rendered.contains("autocrlf = input"), "got: {rendered}");
    }

    #[test]
    fn set_inserts_new_section_when_missing() {
        let (_dir, mut doc) = doc_from("[user]\n\temail = a@b\n");
        let path = FieldPath::parse("alias.co").unwrap();
        doc.set(&path, "checkout".to_string()).unwrap();
        let rendered = doc.render();
        // Existing content preserved.
        assert!(rendered.contains("[user]"), "got: {rendered}");
        assert!(rendered.contains("email = a@b"), "got: {rendered}");
        // New section + key appended.
        assert!(rendered.contains("[alias]"), "got: {rendered}");
        assert!(rendered.contains("co = checkout"), "got: {rendered}");
        // Round-trip read.
        assert_eq!(doc.get(&path), Some("checkout".to_string()));
    }

    #[test]
    fn set_inserts_new_subsection_when_missing() {
        let original = "[remote \"origin\"]\n\turl = https://example.com/foo\n";
        let (_dir, mut doc) = doc_from(original);
        let path = FieldPath::parse("remote.upstream.url").unwrap();
        doc.set(&path, "https://example.com/up".to_string())
            .unwrap();
        let rendered = doc.render();
        assert!(rendered.contains("[remote \"origin\"]"), "got: {rendered}");
        assert!(rendered.contains("[remote \"upstream\"]"), "got: {rendered}");
        assert_eq!(doc.get(&path), Some("https://example.com/up".to_string()));
    }

    #[test]
    fn set_works_on_empty_document() {
        // Bootstrapping path: an `allow_missing` load of an absent file
        // produces an empty document; subsequent sets must work.
        let mut doc = GitConfigDocument::empty();
        let path = FieldPath::parse("user.email").unwrap();
        doc.set(&path, "bob@example.com".to_string()).unwrap();
        assert_eq!(doc.get(&path), Some("bob@example.com".to_string()));
    }

    #[test]
    fn set_rejects_path_with_too_few_segments() {
        let mut doc = GitConfigDocument::empty();
        let path = FieldPath::parse("orphan").unwrap();
        let err = doc.set(&path, "x".to_string()).unwrap_err();
        let s = err.to_string();
        assert!(s.contains("section.key") || s.contains("missing a key"), "msg: {s}");
    }

    #[test]
    fn set_rejects_path_with_too_many_segments() {
        let mut doc = GitConfigDocument::empty();
        let path = FieldPath::parse("a.b.c.d").unwrap();
        let err = doc.set(&path, "x".to_string()).unwrap_err();
        assert!(err.to_string().contains("too many segments"), "msg: {err}");
    }

    // ----- multivar + selector rejection -----

    #[test]
    fn set_rejects_destination_with_multivar() {
        // Target file already has two `fetch` lines under [remote "origin"].
        // Trying to sync that path is data corruption: which line do we
        // overwrite? Bail rather than guess.
        let original = "\
[remote \"origin\"]
\tfetch = +refs/heads/*:refs/remotes/origin/*
\tfetch = +refs/tags/*:refs/tags/*
";
        let (_dir, mut doc) = doc_from(original);
        let path = FieldPath::parse("remote.origin.fetch").unwrap();
        let err = doc.set(&path, "+refs/replace/*".to_string()).unwrap_err();
        let s = err.to_string();
        assert!(s.contains("multivar"), "msg: {s}");
        assert!(s.contains("2 values"), "msg: {s}");
    }

    #[test]
    fn set_rejects_path_with_array_selector() {
        let mut doc = GitConfigDocument::empty();
        let path = FieldPath::parse("a[name=\"foo\"].b").unwrap();
        let err = doc.set(&path, "x".to_string()).unwrap_err();
        assert!(err.to_string().contains("selectors"), "msg: {err}");
    }

    #[test]
    fn expand_passes_through_clean_paths() {
        let doc = GitConfigDocument::empty();
        let path = FieldPath::parse("user.email").unwrap();
        let resolved = doc.expand(&path).unwrap();
        assert_eq!(resolved.len(), 1);
        assert_eq!(resolved[0].path, path);
        assert!(resolved[0].identity.is_empty());
    }

    #[test]
    fn expand_rejects_paths_with_selectors() {
        let doc = GitConfigDocument::empty();
        let pinned = FieldPath::parse("arr[k=\"v\"].field").unwrap();
        let err = doc.expand(&pinned).unwrap_err();
        assert!(err.to_string().contains("no arrays of objects"), "msg: {err}");

        let wildcard = FieldPath::parse("arr[k].field").unwrap();
        let err = doc.expand(&wildcard).unwrap_err();
        assert!(err.to_string().contains("no arrays of objects"), "msg: {err}");
    }

    // ----- discover_field_tree -----

    use crate::discovery::FieldNodeKind;

    fn paths_in(tree: &crate::discovery::FieldTree) -> Vec<String> {
        let mut out = Vec::new();
        fn walk(node: &crate::discovery::FieldNode, out: &mut Vec<String>) {
            if let Some(p) = &node.path {
                out.push(p.to_string());
            }
            for c in &node.children {
                walk(c, out);
            }
        }
        for r in &tree.roots {
            walk(r, &mut out);
        }
        out
    }

    #[test]
    fn discover_returns_empty_tree_for_empty_doc() {
        let doc = GitConfigDocument::empty();
        let tree = doc.discover_field_tree();
        assert!(tree.roots.is_empty());
    }

    #[test]
    fn discover_emits_section_keys_as_leaves() {
        let (_dir, doc) = doc_from(
            "\
[user]
\tname = Alice
\temail = a@b
",
        );
        let tree = doc.discover_field_tree();
        let paths = paths_in(&tree);
        assert!(paths.contains(&"user.name".to_string()), "{paths:?}");
        assert!(paths.contains(&"user.email".to_string()), "{paths:?}");
        // Section is a VirtualGroup so the picker offers [*] but not [x].
        assert_eq!(tree.roots.len(), 1);
        assert_eq!(tree.roots[0].kind, FieldNodeKind::VirtualGroup);
        assert!(tree.roots[0].path.is_none(), "section should have no path");
    }

    #[test]
    fn discover_nests_subsections_as_virtual_groups() {
        let (_dir, doc) = doc_from(
            "\
[remote \"origin\"]
\turl = https://example.com/origin
[remote \"upstream\"]
\turl = https://example.com/up
",
        );
        let tree = doc.discover_field_tree();
        let paths = paths_in(&tree);
        assert!(paths.contains(&"remote.origin.url".to_string()), "{paths:?}");
        assert!(paths.contains(&"remote.upstream.url".to_string()), "{paths:?}");
        // remote → [origin (vgroup), upstream (vgroup)]
        assert_eq!(tree.roots.len(), 1);
        let remote = &tree.roots[0];
        assert_eq!(remote.kind, FieldNodeKind::VirtualGroup);
        assert!(remote.path.is_none());
        assert_eq!(remote.children.len(), 2);
        for sub in &remote.children {
            assert_eq!(sub.kind, FieldNodeKind::VirtualGroup);
            assert!(sub.path.is_none());
        }
    }

    #[test]
    fn discover_skips_multivar_keys() {
        // [remote "origin"] has both a single-valued `url` and a
        // multivar `fetch`. Only `url` should appear in the tree —
        // multivar can't round-trip through dot-sync.
        let (_dir, doc) = doc_from(
            "\
[remote \"origin\"]
\turl = https://example.com
\tfetch = +refs/heads/*:refs/remotes/origin/*
\tfetch = +refs/tags/*:refs/tags/*
",
        );
        let tree = doc.discover_field_tree();
        let paths = paths_in(&tree);
        assert!(paths.contains(&"remote.origin.url".to_string()), "{paths:?}");
        assert!(
            !paths.contains(&"remote.origin.fetch".to_string()),
            "multivar fetch should be hidden, got {paths:?}"
        );
    }

    #[test]
    fn discover_handles_subsection_without_special_chars() {
        // `gitdir:~/work/` has no characters that the path parser
        // treats specially (no . [ ] " or whitespace), so its Segment
        // serializes unquoted. The path still round-trips.
        let (_dir, doc) = doc_from(
            "\
[includeIf \"gitdir:~/work/\"]
\tpath = ~/.gitconfig-work
",
        );
        let tree = doc.discover_field_tree();
        let paths = paths_in(&tree);
        assert_eq!(paths, vec!["includeIf.gitdir:~/work/.path".to_string()]);
        let parsed = FieldPath::parse(&paths[0]).unwrap();
        assert_eq!(doc.get(&parsed), Some("~/.gitconfig-work".to_string()));
    }

    #[test]
    fn discover_quotes_subsections_with_dots() {
        // Subsection contains a `.` — Segment.name carries it
        // verbatim and the path's Display impl quotes the segment so
        // re-parsing stays unambiguous.
        let (_dir, doc) = doc_from(
            "\
[branch \"feature.x\"]
\tremote = origin
",
        );
        let tree = doc.discover_field_tree();
        let paths = paths_in(&tree);
        assert_eq!(paths, vec!["branch.\"feature.x\".remote".to_string()]);
        let parsed = FieldPath::parse(&paths[0]).unwrap();
        assert_eq!(doc.get(&parsed), Some("origin".to_string()));
    }

    // ----- case-sensitive matching -----

    #[test]
    fn get_returns_none_when_section_case_differs() {
        // File has lowercase [user]; path queries `User.email`. git
        // would resolve this case-insensitively; dot-sync deliberately
        // does not — paths are case-sensitive across all backends.
        let (_dir, doc) = doc_from("[user]\n\temail = a@b\n");
        let path = FieldPath::parse("User.email").unwrap();
        assert_eq!(doc.get(&path), None);
        // Case-exact still works.
        let path = FieldPath::parse("user.email").unwrap();
        assert_eq!(doc.get(&path), Some("a@b".to_string()));
    }

    #[test]
    fn get_returns_none_when_subsection_case_differs() {
        let (_dir, doc) = doc_from("[remote \"Origin\"]\n\turl = u\n");
        // Path subsection is "origin"; file has "Origin".
        let mismatched = FieldPath::parse("remote.origin.url").unwrap();
        assert_eq!(doc.get(&mismatched), None);
        let exact = FieldPath::parse("remote.Origin.url").unwrap();
        assert_eq!(doc.get(&exact), Some("u".to_string()));
    }

    #[test]
    fn get_returns_none_when_key_case_differs() {
        let (_dir, doc) = doc_from("[user]\n\tEmail = a@b\n");
        let mismatched = FieldPath::parse("user.email").unwrap();
        assert_eq!(doc.get(&mismatched), None);
        let exact = FieldPath::parse("user.Email").unwrap();
        assert_eq!(doc.get(&exact), Some("a@b".to_string()));
    }

    #[test]
    fn set_bails_when_section_case_differs() {
        let (_dir, mut doc) = doc_from("[user]\n\temail = a@b\n");
        let path = FieldPath::parse("User.email").unwrap();
        let err = doc.set(&path, "new@x".to_string()).unwrap_err();
        let s = err.to_string();
        assert!(s.contains("case-mismatches"), "msg: {s}");
        assert!(s.contains("[user]"), "msg: {s}");
    }

    #[test]
    fn set_bails_when_subsection_case_differs() {
        let (_dir, mut doc) = doc_from("[remote \"Origin\"]\n\turl = u\n");
        let path = FieldPath::parse("remote.origin.url").unwrap();
        let err = doc.set(&path, "new".to_string()).unwrap_err();
        assert!(err.to_string().contains("case-mismatches"), "msg: {err}");
    }

    #[test]
    fn set_bails_when_key_case_differs() {
        let (_dir, mut doc) = doc_from("[user]\n\tEmail = a@b\n");
        let path = FieldPath::parse("user.email").unwrap();
        let err = doc.set(&path, "new@x".to_string()).unwrap_err();
        let s = err.to_string();
        assert!(s.contains("case-mismatches"), "msg: {s}");
        assert!(s.contains("Email"), "msg: {s}");
    }

    #[test]
    fn set_creates_new_section_when_no_case_overlap() {
        // No `user` section at all → set creates a new `[user]` with
        // path's case. No case-mismatch error.
        let (_dir, mut doc) = doc_from("[core]\n\teditor = vim\n");
        let path = FieldPath::parse("user.email").unwrap();
        doc.set(&path, "a@b".to_string()).unwrap();
        assert_eq!(doc.get(&path), Some("a@b".to_string()));
    }

    #[test]
    fn set_succeeds_with_case_exact_match() {
        let (_dir, mut doc) = doc_from("[user]\n\temail = old\n");
        let path = FieldPath::parse("user.email").unwrap();
        doc.set(&path, "new".to_string()).unwrap();
        assert_eq!(doc.get(&path), Some("new".to_string()));
    }

    // ----- items_equal / summarize / table_conflict -----

    #[test]
    fn items_equal_compares_byte_for_byte() {
        // String comparison — no boolean polysemy. git treats `true` /
        // `yes` / `on` / `1` as the same bool, but dot-sync compares
        // literal bytes. Document the contract explicitly so future
        // refactors can't loosen it without breaking this.
        assert!(GitConfigDocument::items_equal(
            &"yes".to_string(),
            &"yes".to_string()
        ));
        assert!(!GitConfigDocument::items_equal(
            &"yes".to_string(),
            &"true".to_string()
        ));
        assert!(!GitConfigDocument::items_equal(
            &"true".to_string(),
            &"True".to_string()
        ));
    }

    #[test]
    fn summarize_quotes_present_value_and_marks_missing() {
        assert_eq!(
            GitConfigDocument::summarize(Some(&"vim".to_string())),
            "\"vim\""
        );
        assert_eq!(GitConfigDocument::summarize(None), "<missing>");
    }

    #[test]
    fn table_conflict_always_none_for_gitconfig() {
        // gitconfig keys can't contain nested values, so no path
        // can ever resolve to a "would clobber a table" condition.
        let (_dir, doc) = doc_from("[user]\n\temail = a@b\n");
        let path = FieldPath::parse("user.email").unwrap();
        assert!(doc.table_conflict(&path).is_none());
        let mut empty = GitConfigDocument::empty();
        let new_path = FieldPath::parse("alias.co").unwrap();
        empty.set(&new_path, "x".to_string()).unwrap();
        assert!(empty.table_conflict(&new_path).is_none());
    }

    // ----- multiple same-name sections -----

    #[test]
    fn discover_dedupes_keys_across_repeated_section_headers() {
        // git allows the same section to appear multiple times — the
        // file is logically the union of all of them. The picker
        // shouldn't show duplicates.
        let (_dir, doc) = doc_from(
            "\
[user]
\tname = Alice
[core]
\teditor = vim
[user]
\temail = a@b
[user]
\tname = Alice
",
        );
        let tree = doc.discover_field_tree();
        let paths = paths_in(&tree);
        // user.name appears in two `[user]` sections — show once.
        let user_name_count = paths.iter().filter(|p| p == &"user.name").count();
        assert_eq!(user_name_count, 1, "{paths:?}");
        // user.email is split off in a separate header — still counted.
        assert!(paths.contains(&"user.email".to_string()), "{paths:?}");
    }

    #[test]
    fn get_finds_value_in_repeated_section() {
        // Even when split across two headers, get must find the value.
        let (_dir, doc) = doc_from(
            "\
[user]
\tname = Alice
[core]
\teditor = vim
[user]
\temail = a@b
",
        );
        let path = FieldPath::parse("user.email").unwrap();
        assert_eq!(doc.get(&path), Some("a@b".to_string()));
        let path = FieldPath::parse("user.name").unwrap();
        assert_eq!(doc.get(&path), Some("Alice".to_string()));
    }

    // ----- value content edge cases -----

    #[test]
    fn round_trips_value_with_spaces() {
        // Aliases routinely have spaces: `co = checkout HEAD --quiet`.
        let original = "[alias]\n\tco = checkout HEAD --quiet\n";
        let (_dir, doc) = doc_from(original);
        let path = FieldPath::parse("alias.co").unwrap();
        assert_eq!(
            doc.get(&path),
            Some("checkout HEAD --quiet".to_string())
        );
        // Round-trip render is byte-stable.
        assert_eq!(doc.render(), original);
    }

    #[test]
    fn set_preserves_spaces_in_value() {
        let (_dir, mut doc) = doc_from("[alias]\n\tco = checkout\n");
        let path = FieldPath::parse("alias.co").unwrap();
        doc.set(&path, "checkout HEAD --quiet".to_string()).unwrap();
        assert_eq!(
            doc.get(&path),
            Some("checkout HEAD --quiet".to_string())
        );
    }

    #[test]
    fn empty_value_round_trips_as_empty_string_or_none() {
        // git allows `key =` with nothing after the equals sign.
        // gix-config exposes this through `value_implicit`; the
        // user-facing comfort accessor `string_by` flattens it. We
        // pin down the actual behavior here so a future gix-config
        // bump that changes the flatten rule shows up as a test diff
        // rather than slipping in as silent semantic change.
        let (_dir, doc) = doc_from("[core]\n\teditor =\n");
        let path = FieldPath::parse("core.editor").unwrap();
        // Either Some("") or None is defensible; lock whatever
        // gix-config currently does.
        let actual = doc.get(&path);
        assert!(
            matches!(&actual, Some(s) if s.is_empty()) || actual.is_none(),
            "expected empty or None, got {actual:?}"
        );
    }

    // ----- section name validation -----

    #[test]
    fn set_surfaces_gitconfig_section_name_validation_error() {
        // git's section grammar rejects names with underscores. We
        // pass section names straight through to gix-config; its
        // error must surface to the user with our path context.
        let mut doc = GitConfigDocument::empty();
        let path = FieldPath::parse("new_section.key").unwrap();
        let err = doc.set(&path, "value".to_string()).unwrap_err();
        let s = err.to_string();
        assert!(
            s.contains("new_section.key") || s.contains("section"),
            "msg: {s}"
        );
    }

    #[test]
    fn set_surfaces_gitconfig_key_name_validation_error() {
        // Keys must start with an alphabetic character per git's
        // grammar. A leading digit gets rejected by gix-config.
        let mut doc = GitConfigDocument::empty();
        let path = FieldPath::parse("section.0bad").unwrap();
        let err = doc.set(&path, "value".to_string()).unwrap_err();
        assert!(
            err.to_string().contains("section.0bad") || err.to_string().contains("key"),
            "msg: {err}"
        );
    }

    // ----- gix-config edge behavior: multi-line / mixed indent / EOL comments -----

    #[test]
    fn load_rejects_files_with_backslash_continuation_values() {
        // git allows multi-line values via `\<newline>`, but gix-config
        // 0.56 parses them incorrectly. Refuse to load rather than
        // silently corrupt on the first write.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config");
        std::fs::write(
            &path,
            "[alias]\n\tmultiline = !echo first; \\\necho second\n",
        )
        .unwrap();
        let result = GitConfigDocument::load(&path, false);
        let err = match result {
            Ok(_) => panic!("expected error for multi-line value"),
            Err(e) => e,
        };
        let s = err.to_string();
        assert!(s.contains("multi-line") || s.contains("continuation"), "msg: {s}");
    }

    #[test]
    fn round_trips_mixed_tab_and_space_indentation() {
        // First section uses tab, second uses 4 spaces. gix-config
        // preserves each section's local style — the rendered output
        // is byte-identical to the input.
        let original = "\
[user]
\tname = tab-indented
[core]
    editor = space-indented
";
        let (_dir, doc) = doc_from(original);
        assert_eq!(doc.render(), original);

        // get reads through both styles transparently.
        let user_name = FieldPath::parse("user.name").unwrap();
        assert_eq!(doc.get(&user_name), Some("tab-indented".to_string()));
        let core_editor = FieldPath::parse("core.editor").unwrap();
        assert_eq!(doc.get(&core_editor), Some("space-indented".to_string()));
    }

    #[test]
    fn end_of_line_comments_are_preserved_and_stripped_from_value() {
        // git allows trailing `;` or `#` comments on a value line.
        // Two contracts to lock:
        //   1. `get` returns the value with the comment stripped — the
        //      user's `email` is `a@b`, not `a@b ; trailing comment`.
        //   2. `render` preserves the comment byte-exactly so the file
        //      still shows the user's annotation after a no-op write.
        let original = "\
[user]
\temail = a@b ; trailing semicolon comment
\tname = Alice  # trailing hash comment
";
        let (_dir, doc) = doc_from(original);

        let email = FieldPath::parse("user.email").unwrap();
        assert_eq!(doc.get(&email), Some("a@b".to_string()));
        let name = FieldPath::parse("user.name").unwrap();
        assert_eq!(doc.get(&name), Some("Alice".to_string()));

        assert_eq!(doc.render(), original);
    }

    #[test]
    fn set_after_eol_comment_keeps_comment() {
        // After updating the value, the trailing comment must still
        // be there. This is the case-of-record for "user annotates
        // their gitconfig and dot-sync respects it".
        let original = "[user]\n\temail = old ; my email\n";
        let (_dir, mut doc) = doc_from(original);
        let path = FieldPath::parse("user.email").unwrap();
        doc.set(&path, "new".to_string()).unwrap();
        let rendered = doc.render();
        assert!(rendered.contains("email = new"), "got: {rendered}");
        assert!(rendered.contains("; my email"), "got: {rendered}");
    }

    #[test]
    fn discover_paths_round_trip_to_get() {
        // Every leaf path the picker emits must actually fetch a value
        // when handed back to `get`. This is the picker's foundational
        // contract.
        let (_dir, doc) = doc_from(
            "\
[user]
\tname = Alice
[remote \"origin\"]
\turl = https://example.com/o
",
        );
        let tree = doc.discover_field_tree();
        for path_str in paths_in(&tree) {
            let path = FieldPath::parse(&path_str).unwrap();
            assert!(doc.get(&path).is_some(), "no value at picker path {path_str}");
        }
    }
}
