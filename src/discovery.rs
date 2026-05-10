//! Field tree discovery for the interactive `add` picker.
//!
//! Each `Document` impl produces a [`FieldTree`] representing the document's
//! shape as a hierarchy of selectable nodes. The picker renders the tree,
//! tracks per-node selection state, and emits sync paths from the user's
//! choices.
//!
//! Three node kinds:
//!
//! - **Leaf** — a scalar value (string / number / bool / null) at a known
//!   path. Selecting it adds that single path to the sync list.
//! - **Container** — an object or pinned array element. Has a path of its
//!   own (selecting it `[x]` syncs the whole subtree as one entity) plus
//!   children that can also be selected individually (`[*]`).
//! - **VirtualGroup** — a synthetic grouping that doesn't correspond to a
//!   real path on its own. Used for the wildcard `arr[name=*]` cluster
//!   where children are wildcard-flavored leaves like `arr[name].field`.
//!   Cannot be `[x]`-selected; only `[*]` (all children).

use crate::path::FieldPath;

/// A discovered hierarchy of paths in a document.
#[derive(Debug, Clone)]
pub struct FieldTree {
    /// Top-level nodes. The root itself isn't represented as a node — the
    /// picker treats `roots` as the visible top level.
    pub roots: Vec<FieldNode>,
}

/// One node in the field tree.
#[derive(Debug, Clone)]
pub struct FieldNode {
    /// Display label for *this segment only* (e.g. `tui`, `[name=*]`,
    /// `[name="github"]`, `enabled`). The picker renders ancestry via
    /// indentation, not by including ancestor names here.
    pub display: String,
    /// Full sync path produced when the user selects this node directly
    /// (`[x]` whole-subtree mode for containers, plain selection for
    /// leaves). `None` for virtual groups that have no real path.
    pub path: Option<FieldPath>,
    pub kind: FieldNodeKind,
    /// Child nodes. Empty for true leaves; non-empty for containers and
    /// virtual groups. Order is the order the picker displays.
    pub children: Vec<FieldNode>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FieldNodeKind {
    /// Scalar leaf — its `path` is what gets synced.
    Leaf,
    /// Object container. Both `[x]` (sync whole object) and `[*]`
    /// (sync each leaf individually) are valid.
    Object,
    /// Pinned array item, e.g. `arr[name="github"]`. Same selection
    /// semantics as `Object`.
    PinnedArrayItem,
    /// Virtual group with no path of its own (e.g. the wildcard cluster
    /// `arr[name=*]`). Only `[*]` selecting all children makes sense.
    VirtualGroup,
}

impl FieldNode {
    pub fn leaf(display: impl Into<String>, path: FieldPath) -> Self {
        Self {
            display: display.into(),
            path: Some(path),
            kind: FieldNodeKind::Leaf,
            children: Vec::new(),
        }
    }

    pub fn object(display: impl Into<String>, path: FieldPath, children: Vec<FieldNode>) -> Self {
        Self {
            display: display.into(),
            path: Some(path),
            kind: FieldNodeKind::Object,
            children,
        }
    }

    pub fn pinned_item(
        display: impl Into<String>,
        path: FieldPath,
        children: Vec<FieldNode>,
    ) -> Self {
        Self {
            display: display.into(),
            path: Some(path),
            kind: FieldNodeKind::PinnedArrayItem,
            children,
        }
    }

    pub fn virtual_group(display: impl Into<String>, children: Vec<FieldNode>) -> Self {
        Self {
            display: display.into(),
            path: None,
            kind: FieldNodeKind::VirtualGroup,
            children,
        }
    }
}

/// Identifier-key auto-detection for arrays of objects. Priority order:
/// `name` → `id` → `key` → `slug` → any common scalar key whose values
/// are all unique strings across items. Returns `None` when no usable
/// identifier exists (no shared key, duplicate values, non-string types
/// only) — the array is rendered as a virtual group of pinned items
/// without a wildcard suggestion.
///
/// Caller passes a closure so this helper stays backend-agnostic; each
/// backend supplies its own way to query "does item N have a string at
/// key K, and what is it".
pub fn detect_identifier_key<F>(item_count: usize, mut probe: F) -> Option<String>
where
    F: FnMut(usize, &str) -> Option<String>,
{
    if item_count == 0 {
        return None;
    }
    const PREFERRED: &[&str] = &["name", "id", "key", "slug"];
    for &candidate in PREFERRED {
        if let Some(key) = try_identifier(candidate, item_count, &mut probe) {
            return Some(key);
        }
    }
    None
}

fn try_identifier<F>(key: &str, item_count: usize, probe: &mut F) -> Option<String>
where
    F: FnMut(usize, &str) -> Option<String>,
{
    let mut values = Vec::with_capacity(item_count);
    for i in 0..item_count {
        let v = probe(i, key)?;
        values.push(v);
    }
    let mut sorted = values.clone();
    sorted.sort();
    sorted.dedup();
    if sorted.len() != values.len() {
        return None; // duplicates — can't disambiguate
    }
    Some(key.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detect_prefers_name_when_present_and_unique() {
        let probe = |i: usize, key: &str| -> Option<String> {
            if key == "name" {
                Some(format!("item-{i}"))
            } else if key == "id" {
                Some(format!("id-{i}"))
            } else {
                None
            }
        };
        assert_eq!(detect_identifier_key(3, probe), Some("name".to_string()));
    }

    #[test]
    fn detect_falls_through_to_id_when_name_missing() {
        let probe = |i: usize, key: &str| -> Option<String> {
            if key == "id" {
                Some(format!("id-{i}"))
            } else {
                None
            }
        };
        assert_eq!(detect_identifier_key(2, probe), Some("id".to_string()));
    }

    #[test]
    fn detect_returns_none_when_no_preferred_key_present() {
        let probe = |_: usize, _: &str| -> Option<String> { None };
        assert_eq!(detect_identifier_key(3, probe), None);
    }

    #[test]
    fn detect_returns_none_on_duplicate_values() {
        // Two items both name="github" — can't disambiguate.
        let probe = |_: usize, key: &str| -> Option<String> {
            if key == "name" {
                Some("github".to_string())
            } else {
                None
            }
        };
        assert_eq!(detect_identifier_key(2, probe), None);
    }

    #[test]
    fn detect_returns_none_when_a_single_item_lacks_the_key() {
        let probe = |i: usize, key: &str| -> Option<String> {
            if key == "name" && i != 1 {
                Some(format!("server-{i}"))
            } else {
                None
            }
        };
        assert_eq!(detect_identifier_key(3, probe), None);
    }

    #[test]
    fn field_node_constructors_set_kind_correctly() {
        let p = FieldPath::parse("a").unwrap();
        assert_eq!(FieldNode::leaf("a", p.clone()).kind, FieldNodeKind::Leaf);
        assert_eq!(
            FieldNode::object("a", p.clone(), vec![]).kind,
            FieldNodeKind::Object
        );
        assert_eq!(
            FieldNode::pinned_item("a", p, vec![]).kind,
            FieldNodeKind::PinnedArrayItem
        );
        assert_eq!(
            FieldNode::virtual_group("a", vec![]).kind,
            FieldNodeKind::VirtualGroup
        );
    }
}
