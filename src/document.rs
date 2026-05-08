use std::fs;
use std::path::Path;

use anyhow::{Context, Result, bail};
use toml_edit::{DocumentMut, Item, Table};

use crate::path::FieldPath;

pub trait Document {
    fn get(&self, path: &FieldPath) -> Option<Item>;
    fn set(&mut self, path: &FieldPath, item: Item) -> Result<()>;
    fn clear(&mut self);
    fn contains(&self, path: &FieldPath) -> bool {
        self.get(path).is_some()
    }
    fn to_string(&self) -> String;
}

pub enum AnyDocument {
    Toml(TomlDocument),
}

impl AnyDocument {
    pub fn load(format: &str, path: &Path, allow_missing: bool) -> Result<Self> {
        match format {
            "toml" => Ok(Self::Toml(TomlDocument::load(path, allow_missing)?)),
            "json" => bail!("format json is not implemented yet"),
            other => bail!("unsupported format: {other}"),
        }
    }

    pub fn empty(format: &str) -> Result<Self> {
        match format {
            "toml" => Ok(Self::Toml(TomlDocument::empty())),
            "json" => bail!("format json is not implemented yet"),
            other => bail!("unsupported format: {other}"),
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

    fn to_string(&self) -> String {
        self.doc.to_string()
    }
}

fn get_from_table<'a>(table: &'a Table, segments: &[String]) -> Option<&'a Item> {
    let (first, rest) = segments.split_first()?;
    let item = table.get(first)?;
    if rest.is_empty() {
        return Some(item);
    }
    let table = item.as_table()?;
    get_from_table(table, rest)
}

fn set_in_table(table: &mut Table, segments: &[String], item: Item) -> Result<()> {
    let Some((first, rest)) = segments.split_first() else {
        bail!("path must not be empty");
    };

    if rest.is_empty() {
        table.insert(first, item);
        return Ok(());
    }

    let child = match table.get_mut(first) {
        Some(existing) if existing.as_table().is_some() => existing,
        _ => {
            let mut child = Table::new();
            child.set_implicit(true);
            table.insert(first, Item::Table(child));
            table.get_mut(first).expect("inserted table")
        }
    };

    let child_table = child
        .as_table_mut()
        .expect("child was checked or inserted as table");
    set_in_table(child_table, rest, item)
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
}
