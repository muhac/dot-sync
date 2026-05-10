use std::fs;
use std::path::Path;

use anyhow::{Context, Result, bail};
use toml_edit::{DocumentMut, InlineTable, Item, Table, TableLike, Value};

use crate::path::{FieldPath, Segment};

/// Reduce a FieldPath to a flat list of key names if every segment is a plain
/// key (no array selector). Returns None otherwise — callers that don't yet
/// handle selectors fall back to "no value at this path".
fn plain_segment_names(path: &FieldPath) -> Option<Vec<String>> {
    path.segments()
        .iter()
        .map(|seg: &Segment| match &seg.select {
            None => Some(seg.name.clone()),
            Some(_) => None,
        })
        .collect()
}

#[derive(Debug, Clone)]
pub struct TableConflict {
    pub path: String,
    pub kind: String,
    pub value: String,
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
/// - **Path semantics**: `FieldPath` segments are dotted keys for object
///   navigation. Array indexing (`a[0]`, `a.*.b`) is **not** supported by the
///   path syntax today; impls do not need to handle it.
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
        let names = plain_segment_names(path)?;
        get_from_table(self.doc.as_table(), &names).cloned()
    }

    fn set(&mut self, path: &FieldPath, item: Item) -> Result<()> {
        let Some(names) = plain_segment_names(path) else {
            bail!("array selectors in {path:?} are not yet wired through TomlDocument");
        };
        set_in_table(self.doc.as_table_mut(), &names, item)
    }

    fn table_conflict(&self, path: &FieldPath) -> Option<TableConflict> {
        let names = plain_segment_names(path)?;
        table_conflict_in_table(self.doc.as_table(), &names)
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
}

fn get_from_table<'a>(table: &'a Table, segments: &[String]) -> Option<&'a Item> {
    get_from_table_like(table, segments)
}

fn get_from_table_like<'a>(table: &'a dyn TableLike, segments: &[String]) -> Option<&'a Item> {
    let (first, rest) = segments.split_first()?;
    let item = table.get(first)?;
    if rest.is_empty() {
        return Some(item);
    }
    let table = item.as_table_like()?;
    get_from_table_like(table, rest)
}

fn table_conflict_in_table(table: &Table, segments: &[String]) -> Option<TableConflict> {
    table_conflict_in_table_like(table, segments)
}

fn table_conflict_in_table_like(
    table: &dyn TableLike,
    segments: &[String],
) -> Option<TableConflict> {
    let mut current = table;
    let mut prefix = Vec::new();
    for segment in segments.iter().take(segments.len().saturating_sub(1)) {
        prefix.push(segment.clone());
        let item = current.get(segment)?;
        let Some(next) = item.as_table_like() else {
            return Some(TableConflict {
                path: prefix.join("."),
                kind: item.type_name().to_string(),
                value: summarize_toml_item(item),
            });
        };
        current = next;
    }
    None
}

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

fn set_in_table(table: &mut Table, segments: &[String], item: Item) -> Result<()> {
    let Some((first, rest)) = segments.split_first() else {
        bail!("path must not be empty");
    };

    if rest.is_empty() {
        table.insert(first, item);
        return Ok(());
    }

    if !matches!(table.get(first), Some(existing) if existing.as_table_like().is_some()) {
        let mut child = Table::new();
        child.set_implicit(true);
        table.insert(first, Item::Table(child));
    }

    let child = table.get_mut(first).expect("inserted table");
    set_in_child(child, rest, item)
}

fn set_in_inline_table(table: &mut InlineTable, segments: &[String], item: Item) -> Result<()> {
    let Some((first, rest)) = segments.split_first() else {
        bail!("path must not be empty");
    };

    if rest.is_empty() {
        let value = match item.into_value() {
            Ok(value) => value,
            Err(item) => bail!("cannot insert {} into inline table", item.type_name()),
        };
        table.insert(first, value);
        return Ok(());
    }

    if !matches!(TableLike::get(table, first), Some(existing) if existing.as_table_like().is_some())
    {
        TableLike::insert(
            table,
            first,
            Item::Value(Value::InlineTable(InlineTable::new())),
        );
    }

    let child = TableLike::get_mut(table, first).expect("inserted inline table");
    set_in_child(child, rest, item)
}

fn set_in_child(child: &mut Item, segments: &[String], item: Item) -> Result<()> {
    match child {
        Item::Table(table) => set_in_table(table, segments, item),
        Item::Value(Value::InlineTable(table)) => set_in_inline_table(table, segments, item),
        _ => unreachable!("child was checked or inserted as table-like"),
    }
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
}
