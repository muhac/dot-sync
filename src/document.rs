use std::fs;
use std::path::Path;

use anyhow::{Context, Result, bail};
use toml_edit::{DocumentMut, InlineTable, Item, Table, TableLike, Value};

use crate::path::FieldPath;

#[derive(Debug, Clone)]
pub struct TableConflict {
    pub path: String,
    pub kind: String,
}

pub trait Document {
    fn get(&self, path: &FieldPath) -> Option<Item>;
    fn set(&mut self, path: &FieldPath, item: Item) -> Result<()>;
    fn clear(&mut self);
    fn table_conflict(&self, path: &FieldPath) -> Option<TableConflict>;
    fn to_string(&self) -> String;
}

pub enum AnyDocument {
    Toml(TomlDocument),
}

impl AnyDocument {
    pub fn validate_format(format: &str) -> Result<()> {
        match format {
            "toml" => Ok(()),
            "json" => bail!("format json is recognized but not implemented yet"),
            other => bail!("unsupported format: {other}; supported formats: toml"),
        }
    }

    pub fn load(format: &str, path: &Path, allow_missing: bool) -> Result<Self> {
        Self::validate_format(format)?;
        match format {
            "toml" => Ok(Self::Toml(TomlDocument::load(path, allow_missing)?)),
            _ => unreachable!("format was validated"),
        }
    }

    pub fn empty(format: &str) -> Result<Self> {
        Self::validate_format(format)?;
        match format {
            "toml" => Ok(Self::Toml(TomlDocument::empty())),
            _ => unreachable!("format was validated"),
        }
    }
}

impl Document for AnyDocument {
    fn get(&self, path: &FieldPath) -> Option<Item> {
        match self {
            Self::Toml(doc) => doc.get(path),
        }
    }

    fn set(&mut self, path: &FieldPath, item: Item) -> Result<()> {
        match self {
            Self::Toml(doc) => doc.set(path, item),
        }
    }

    fn clear(&mut self) {
        match self {
            Self::Toml(doc) => doc.clear(),
        }
    }

    fn table_conflict(&self, path: &FieldPath) -> Option<TableConflict> {
        match self {
            Self::Toml(doc) => doc.table_conflict(path),
        }
    }

    fn to_string(&self) -> String {
        match self {
            Self::Toml(doc) => doc.to_string(),
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

    pub fn load(path: &Path, allow_missing: bool) -> Result<Self> {
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
}

impl Document for TomlDocument {
    fn get(&self, path: &FieldPath) -> Option<Item> {
        get_from_table(self.doc.as_table(), path.segments()).cloned()
    }

    fn set(&mut self, path: &FieldPath, item: Item) -> Result<()> {
        set_in_table(self.doc.as_table_mut(), path.segments(), item)
    }

    fn clear(&mut self) {
        self.doc = DocumentMut::new();
    }

    fn table_conflict(&self, path: &FieldPath) -> Option<TableConflict> {
        table_conflict_in_table(self.doc.as_table(), path.segments())
    }

    fn to_string(&self) -> String {
        self.doc.to_string()
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
            });
        };
        current = next;
    }
    None
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
        let mut doc = TomlDocument {
            doc: toml_edit::DocumentMut::new(),
        };
        let path = FieldPath::parse("tui.theme").unwrap();
        doc.set(&path, value("monokai")).unwrap();
        assert_eq!(
            doc.get(&path).unwrap().as_value().unwrap().as_str(),
            Some("monokai")
        );
    }

    #[test]
    fn handles_quoted_segments() {
        let mut doc = TomlDocument {
            doc: toml_edit::DocumentMut::new(),
        };
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
