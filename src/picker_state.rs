//! Selection state machine for the interactive `add` picker.
//!
//! Decoupled from ratatui so the toggle / expand / cursor logic can be
//! unit-tested without a terminal. The TUI layer in `picker.rs` only
//! handles rendering and event mapping.
//!
//! ## Three-state cycle on container nodes
//!
//! Pressing space on a container cycles through:
//!
//! - `[ ]` — nothing selected
//! - `[x]` — whole-subtree mode: container's path emitted as one sync entry
//! - `[*]` — individual mode: every descendant leaf emitted separately
//! - back to `[ ]`
//!
//! `[~]` (mixed) is reachable when the user manually toggled some-but-not-all
//! leaves under a parent. Pressing space on `[~]` resets to `[ ]`.
//!
//! Toggling a leaf directly clears any whole-subtree ancestor — the two
//! states are incompatible by design (parent in whole mode covers its
//! leaves; explicitly checking a leaf breaks that coverage).
//!
//! Virtual groups (no `path`) skip the `[x]` step in the cycle —
//! `[ ]` → `[*]` → `[ ]`. They have no path of their own to emit.

use crate::discovery::{FieldNode, FieldNodeKind, FieldTree};
use crate::path::FieldPath;

/// Flattened, indexable tree node. Every original FieldNode becomes one
/// FlatNode; parent / children relationships are preserved as indices
/// into the flat vector for cheap navigation.
#[derive(Debug, Clone)]
pub struct FlatNode {
    pub display: String,
    pub kind: FieldNodeKind,
    pub path: Option<FieldPath>,
    pub depth: usize,
    pub parent: Option<usize>,
    pub children: Vec<usize>,
}

/// Per-node display state used by the renderer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CheckState {
    /// Nothing selected at this node or under it.
    Empty,
    /// Whole-subtree mode: this container's path is in the sync list.
    /// Descendants are *covered* and not individually selected.
    Whole,
    /// Individual mode: every descendant leaf is selected; emits
    /// per-leaf paths. Container's own path is NOT in the list.
    Individual,
    /// Some-but-not-all descendant leaves selected.
    Mixed,
}

/// Picker state: flat tree + selection + expansion + cursor.
pub struct PickerState {
    pub nodes: Vec<FlatNode>,
    selected: Vec<bool>,
    expanded: Vec<bool>,
    cursor: usize,
}

impl PickerState {
    /// Build picker state from a FieldTree. Top-level nodes start
    /// expanded; deeper levels start collapsed (per the design decision
    /// to default to a single visible level).
    pub fn from_tree(tree: FieldTree) -> Self {
        let mut nodes = Vec::new();
        for root in tree.roots {
            flatten(root, 0, None, &mut nodes);
        }
        let n = nodes.len();
        let mut expanded = vec![false; n];
        // Expand top-level (depth 0) parents only.
        for (i, node) in nodes.iter().enumerate() {
            if node.depth == 0 && !node.children.is_empty() {
                expanded[i] = true;
            }
        }
        Self {
            nodes,
            selected: vec![false; n],
            expanded,
            cursor: 0,
        }
    }

    pub fn cursor(&self) -> usize {
        self.cursor
    }

    pub fn is_expanded(&self, idx: usize) -> bool {
        self.expanded[idx]
    }

    /// Whether the node at `idx` is in the underlying selected set.
    /// Note: prefer [`Self::check_state`] for display purposes — that
    /// surfaces the tri-state including `Mixed` and `Individual` modes.
    /// Public for unit-test introspection.
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn is_selected(&self, idx: usize) -> bool {
        self.selected[idx]
    }

    /// Indices of nodes the user can see, in display order. A node is
    /// visible iff every ancestor is `expanded`.
    pub fn visible(&self) -> Vec<usize> {
        let mut out = Vec::new();
        for (i, node) in self.nodes.iter().enumerate() {
            let mut visible = true;
            let mut cur = node.parent;
            while let Some(p) = cur {
                if !self.expanded[p] {
                    visible = false;
                    break;
                }
                cur = self.nodes[p].parent;
            }
            if visible {
                out.push(i);
            }
        }
        out
    }

    /// Display state for a node, computed from the underlying boolean
    /// flags + descendant aggregates.
    pub fn check_state(&self, idx: usize) -> CheckState {
        let node = &self.nodes[idx];
        if matches!(node.kind, FieldNodeKind::Leaf) {
            return if self.selected[idx] {
                CheckState::Whole
            } else {
                CheckState::Empty
            };
        }
        if self.selected[idx] {
            return CheckState::Whole;
        }
        let leaves = self.descendant_leaves(idx);
        if leaves.is_empty() {
            // Virtual group with no leaves underneath — degenerate, treat as Empty.
            return CheckState::Empty;
        }
        let checked = leaves.iter().filter(|l| self.selected[**l]).count();
        if checked == 0 {
            CheckState::Empty
        } else if checked == leaves.len() {
            CheckState::Individual
        } else {
            CheckState::Mixed
        }
    }

    /// Toggle a node per the cycle: leaves flip, containers walk
    /// `[ ] → [x] → [*] → [ ]` (virtual groups skip `[x]`), and `[~]`
    /// resets to `[ ]`.
    ///
    /// Invariant maintained: at any point, for any ancestor / descendant
    /// pair, at most one of them has `selected == true`. Toggling a
    /// parent always clears every descendant (containers + leaves), and
    /// toggling a leaf to `true` clears every ancestor. Without this, a
    /// stale child-container selection would leak through `selected_paths()`
    /// — e.g. cycling Whole → Individual on a parent could emit a child
    /// container's whole-subtree path instead of the per-leaf paths.
    pub fn toggle(&mut self, idx: usize) {
        let node = &self.nodes[idx];
        if matches!(node.kind, FieldNodeKind::Leaf) {
            self.selected[idx] = !self.selected[idx];
            if self.selected[idx] {
                // Leaf and whole-mode are mutually exclusive — clear
                // every ancestor's `selected` (and every other ancestor
                // through the walk, no matter how deep).
                let mut cur = self.nodes[idx].parent;
                while let Some(p) = cur {
                    self.selected[p] = false;
                    cur = self.nodes[p].parent;
                }
            }
            return;
        }
        let state = self.check_state(idx);
        let has_path = node.path.is_some();
        match state {
            CheckState::Empty => {
                if has_path {
                    self.set_whole(idx);
                } else {
                    // Virtual group: skip Whole, go straight to Individual.
                    self.set_individual(idx);
                }
            }
            CheckState::Whole => {
                self.set_individual(idx);
            }
            CheckState::Individual | CheckState::Mixed => {
                // [*] and [~] both reset to [ ] on the next press.
                self.clear_subtree(idx);
            }
        }
    }

    /// Set the node at `idx` to whole-subtree mode (`[x]`): own
    /// `selected` flag on, every descendant cleared so the whole-mode
    /// path is the only one emitted from this branch.
    fn set_whole(&mut self, idx: usize) {
        self.selected[idx] = true;
        for desc in self.descendants_of(idx) {
            self.selected[desc] = false;
        }
    }

    /// Set the node at `idx` to individual-leaves mode (`[*]`): own
    /// flag off, every descendant container cleared (so it doesn't
    /// cover its leaves), every descendant leaf set.
    fn set_individual(&mut self, idx: usize) {
        self.selected[idx] = false;
        for desc in self.descendants_of(idx) {
            let is_leaf = matches!(self.nodes[desc].kind, FieldNodeKind::Leaf);
            self.selected[desc] = is_leaf;
        }
    }

    /// Reset the subtree rooted at `idx` to fully empty (`[ ]`).
    fn clear_subtree(&mut self, idx: usize) {
        self.selected[idx] = false;
        for desc in self.descendants_of(idx) {
            self.selected[desc] = false;
        }
    }

    /// All descendant indices (containers + leaves) in flat order.
    fn descendants_of(&self, idx: usize) -> Vec<usize> {
        let mut out = Vec::new();
        for &child in &self.nodes[idx].children {
            out.push(child);
            out.extend(self.descendants_of(child));
        }
        out
    }

    /// Expand a container. No-op for leaves and already-expanded nodes.
    pub fn expand(&mut self, idx: usize) {
        if !self.nodes[idx].children.is_empty() {
            self.expanded[idx] = true;
        }
    }

    /// Collapse a container.
    pub fn collapse(&mut self, idx: usize) {
        if !self.nodes[idx].children.is_empty() {
            self.expanded[idx] = false;
        }
    }

    pub fn cursor_down(&mut self) {
        let visible = self.visible();
        if visible.is_empty() {
            return;
        }
        let pos = visible.iter().position(|&i| i == self.cursor).unwrap_or(0);
        let next = (pos + 1).min(visible.len() - 1);
        self.cursor = visible[next];
    }

    pub fn cursor_up(&mut self) {
        let visible = self.visible();
        if visible.is_empty() {
            return;
        }
        let pos = visible.iter().position(|&i| i == self.cursor).unwrap_or(0);
        let prev = pos.saturating_sub(1);
        self.cursor = visible[prev];
    }

    /// Set cursor to a specific node index (used when re-rendering or
    /// jumping). Bounds-checked; out-of-range is ignored.
    pub fn set_cursor(&mut self, idx: usize) {
        if idx < self.nodes.len() {
            self.cursor = idx;
        }
    }

    /// Emit sync paths the user has chosen, in flattened order.
    ///
    /// Rule: a node contributes its own `path` iff
    /// 1. the node is selected, AND
    /// 2. no ancestor is also selected (covered by ancestor's whole-mode).
    pub fn selected_paths(&self) -> Vec<FieldPath> {
        let mut out = Vec::new();
        for idx in 0..self.nodes.len() {
            if !self.selected[idx] {
                continue;
            }
            // Skip if any ancestor is whole-selected — defensive; the
            // toggle rules already prevent this co-existence.
            let mut covered = false;
            let mut cur = self.nodes[idx].parent;
            while let Some(p) = cur {
                if self.selected[p] {
                    covered = true;
                    break;
                }
                cur = self.nodes[p].parent;
            }
            if covered {
                continue;
            }
            if let Some(path) = self.nodes[idx].path.clone() {
                out.push(path);
            }
        }
        out
    }

    /// All descendant leaf indices of `idx`, in flat order. Empty for
    /// leaves themselves. Used by the toggle logic and check_state.
    fn descendant_leaves(&self, idx: usize) -> Vec<usize> {
        let mut out = Vec::new();
        self.collect_leaves(idx, &mut out);
        out
    }

    fn collect_leaves(&self, idx: usize, out: &mut Vec<usize>) {
        let node = &self.nodes[idx];
        if matches!(node.kind, FieldNodeKind::Leaf) {
            out.push(idx);
            return;
        }
        for &child in &node.children {
            self.collect_leaves(child, out);
        }
    }
}

fn flatten(node: FieldNode, depth: usize, parent: Option<usize>, out: &mut Vec<FlatNode>) {
    let my_idx = out.len();
    out.push(FlatNode {
        display: node.display,
        kind: node.kind,
        path: node.path,
        depth,
        parent,
        children: Vec::new(),
    });
    let mut child_indices = Vec::new();
    for child in node.children {
        let child_idx = out.len();
        child_indices.push(child_idx);
        flatten(child, depth + 1, Some(my_idx), out);
    }
    out[my_idx].children = child_indices;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::discovery::FieldNode;

    fn p(s: &str) -> FieldPath {
        FieldPath::parse(s).unwrap()
    }

    /// Build a small fixture tree for state machine tests:
    /// ```
    /// tui (object, path=tui)
    ///   theme (leaf, path=tui.theme)
    ///   status_line (leaf, path=tui.status_line)
    /// model (leaf, path=model)
    /// servers (object, path=servers)
    ///   [name=*] (virtual group)
    ///     enabled (leaf, path=servers[name].enabled)
    ///   [name="github"] (pinned, path=servers[name="github"])
    ///     enabled (leaf, path=servers[name="github"].enabled)
    /// ```
    fn fixture() -> PickerState {
        let tui = FieldNode::object(
            "tui",
            p("tui"),
            vec![
                FieldNode::leaf("theme", p("tui.theme")),
                FieldNode::leaf("status_line", p("tui.status_line")),
            ],
        );
        let model = FieldNode::leaf("model", p("model"));
        let wildcard = FieldNode::virtual_group(
            "[name=*]",
            vec![FieldNode::leaf("enabled", p("servers[name].enabled"))],
        );
        let github = FieldNode::pinned_item(
            "[name=\"github\"]",
            p("servers[name=\"github\"]"),
            vec![FieldNode::leaf(
                "enabled",
                p("servers[name=\"github\"].enabled"),
            )],
        );
        let servers = FieldNode::object("servers", p("servers"), vec![wildcard, github]);
        let tree = FieldTree {
            roots: vec![tui, model, servers],
        };
        PickerState::from_tree(tree)
    }

    fn idx_of(state: &PickerState, display: &str) -> usize {
        state
            .nodes
            .iter()
            .position(|n| n.display == display)
            .unwrap_or_else(|| panic!("no node with display {display}"))
    }

    #[test]
    fn fresh_state_has_no_selection_and_top_level_expanded() {
        let s = fixture();
        for i in 0..s.nodes.len() {
            assert!(!s.selected[i]);
        }
        // Top-level containers expanded; deeper containers collapsed.
        let tui = idx_of(&s, "tui");
        let servers = idx_of(&s, "servers");
        assert!(s.is_expanded(tui));
        assert!(s.is_expanded(servers));
        // The wildcard group lives under `servers` at depth 1, default collapsed.
        let wildcard = idx_of(&s, "[name=*]");
        assert!(!s.is_expanded(wildcard));
    }

    #[test]
    fn toggle_leaf_cycles_self() {
        let mut s = fixture();
        let theme = idx_of(&s, "theme");
        s.toggle(theme);
        assert!(s.is_selected(theme));
        assert_eq!(s.check_state(theme), CheckState::Whole);
        s.toggle(theme);
        assert!(!s.is_selected(theme));
        assert_eq!(s.check_state(theme), CheckState::Empty);
    }

    #[test]
    fn toggle_object_container_walks_whole_individual_empty() {
        let mut s = fixture();
        let tui = idx_of(&s, "tui");
        s.toggle(tui);
        assert_eq!(s.check_state(tui), CheckState::Whole);
        assert_eq!(s.selected_paths(), vec![p("tui")]);

        s.toggle(tui);
        assert_eq!(s.check_state(tui), CheckState::Individual);
        let mut paths = s.selected_paths();
        paths.sort_by_key(|p| p.to_string());
        assert_eq!(paths, vec![p("tui.status_line"), p("tui.theme")]);

        s.toggle(tui);
        assert_eq!(s.check_state(tui), CheckState::Empty);
        assert!(s.selected_paths().is_empty());
    }

    #[test]
    fn virtual_group_skips_whole_step() {
        let mut s = fixture();
        let wildcard = idx_of(&s, "[name=*]");
        s.toggle(wildcard);
        // Goes straight to Individual (no Whole step for virtual groups).
        assert_eq!(s.check_state(wildcard), CheckState::Individual);
        assert_eq!(s.selected_paths(), vec![p("servers[name].enabled")]);
        s.toggle(wildcard);
        assert_eq!(s.check_state(wildcard), CheckState::Empty);
    }

    #[test]
    fn manually_toggled_leaf_under_unselected_parent_yields_mixed() {
        let mut s = fixture();
        let theme = idx_of(&s, "theme");
        s.toggle(theme);
        let tui = idx_of(&s, "tui");
        assert_eq!(s.check_state(tui), CheckState::Mixed);
        assert_eq!(s.selected_paths(), vec![p("tui.theme")]);
    }

    #[test]
    fn space_on_mixed_resets_to_empty() {
        let mut s = fixture();
        let theme = idx_of(&s, "theme");
        s.toggle(theme);
        let tui = idx_of(&s, "tui");
        assert_eq!(s.check_state(tui), CheckState::Mixed);
        s.toggle(tui);
        assert_eq!(s.check_state(tui), CheckState::Empty);
        assert!(s.selected_paths().is_empty());
    }

    #[test]
    fn leaf_select_clears_ancestor_whole_mode() {
        let mut s = fixture();
        let tui = idx_of(&s, "tui");
        s.toggle(tui);
        assert_eq!(s.check_state(tui), CheckState::Whole);
        // Now manually toggle a leaf — ancestor's whole-mode must clear.
        let theme = idx_of(&s, "theme");
        s.toggle(theme);
        assert!(!s.is_selected(tui));
        assert_eq!(s.check_state(tui), CheckState::Mixed);
        assert_eq!(s.selected_paths(), vec![p("tui.theme")]);
    }

    #[test]
    fn whole_mode_emits_only_parent_path_skipping_descendants() {
        let mut s = fixture();
        let tui = idx_of(&s, "tui");
        s.toggle(tui); // Whole
        // Even if a descendant is somehow selected, the parent's whole-mode
        // takes precedence in the path emission. (Toggle clears descendants
        // automatically, so this is a defensive check.)
        let paths = s.selected_paths();
        assert_eq!(paths, vec![p("tui")]);
    }

    #[test]
    fn selected_paths_combines_independent_branches() {
        let mut s = fixture();
        let tui = idx_of(&s, "tui");
        let model = idx_of(&s, "model");
        s.toggle(tui); // Whole
        s.toggle(model); // leaf
        let mut paths = s.selected_paths();
        paths.sort_by_key(|p| p.to_string());
        assert_eq!(paths, vec![p("model"), p("tui")]);
    }

    #[test]
    fn expand_collapse_changes_visible_set() {
        let mut s = fixture();
        let visible_before: Vec<&str> = s
            .visible()
            .iter()
            .map(|i| s.nodes[*i].display.as_str())
            .collect();
        // Default visible: top-level + their direct children (since top-level
        // is expanded), but NOT grandchildren since `wildcard` is collapsed.
        assert!(visible_before.contains(&"theme"));
        assert!(!visible_before.contains(&"enabled")); // under collapsed [name=*]

        let wildcard = idx_of(&s, "[name=*]");
        s.expand(wildcard);
        let visible_after: Vec<&str> = s
            .visible()
            .iter()
            .map(|i| s.nodes[*i].display.as_str())
            .collect();
        let count = visible_after.iter().filter(|d| **d == "enabled").count();
        assert_eq!(count, 1, "expanded wildcard reveals its leaf");
    }

    #[test]
    fn cursor_navigates_only_visible_rows() {
        let mut s = fixture();
        // Cursor starts at 0 (first node = tui).
        assert_eq!(s.nodes[s.cursor].display, "tui");

        // Walk through visible rows: tui → theme → status_line → model → servers.
        s.cursor_down();
        assert_eq!(s.nodes[s.cursor].display, "theme");
        s.cursor_down();
        assert_eq!(s.nodes[s.cursor].display, "status_line");
        s.cursor_down();
        assert_eq!(s.nodes[s.cursor].display, "model");
        s.cursor_down();
        assert_eq!(s.nodes[s.cursor].display, "servers");

        // Next: wildcard group (visible because servers is expanded), but
        // its own children are NOT visible since the wildcard is collapsed.
        s.cursor_down();
        assert_eq!(s.nodes[s.cursor].display, "[name=*]");

        // One more — to the pinned [name="github"] container (also collapsed).
        s.cursor_down();
        assert_eq!(s.nodes[s.cursor].display, "[name=\"github\"]");
    }

    #[test]
    fn cursor_up_at_top_stays_put() {
        let mut s = fixture();
        s.cursor_up();
        assert_eq!(s.cursor, 0);
    }

    /// Regression: cycling a parent's tri-state must not leave a stale
    /// child-container selection that hijacks `selected_paths()`.
    ///
    /// Bug shape (fixed): user toggled a child container (e.g. the
    /// pinned `[name="github"]` inside `servers`) → child becomes
    /// Whole. Then user toggled the grand-parent `servers` to Whole,
    /// then cycled to Individual. Without descendant-container cleanup,
    /// `[name="github"].selected` persisted and `selected_paths`
    /// emitted `servers[name="github"]` (whole) instead of the per-leaf
    /// paths the `[*]` display promised.
    #[test]
    fn cycling_parent_to_individual_clears_nested_container_selections() {
        let mut s = fixture();
        let github = idx_of(&s, "[name=\"github\"]");
        let servers = idx_of(&s, "servers");

        // 1. User selects child container as Whole.
        s.toggle(github);
        assert_eq!(s.check_state(github), CheckState::Whole);

        // 2. User toggles parent → Whole (the previously-selected
        //    nested container must get cleared).
        s.toggle(servers);
        assert_eq!(s.check_state(servers), CheckState::Whole);
        assert!(
            !s.is_selected(github),
            "nested container selection must be cleared when parent enters Whole"
        );

        // 3. User cycles parent to Individual. selected_paths must emit
        //    per-leaf paths, NOT a nested whole-subtree path.
        s.toggle(servers);
        assert_eq!(s.check_state(servers), CheckState::Individual);
        assert!(
            !s.is_selected(github),
            "nested container must stay cleared in Individual mode"
        );

        let mut paths: Vec<String> = s.selected_paths().iter().map(|p| p.to_string()).collect();
        paths.sort();
        // Lexicographic: `[` < `\"`, so `[name="github"]` sorts before `[name]`.
        assert_eq!(
            paths,
            vec![
                "servers[name=\"github\"].enabled".to_string(),
                "servers[name].enabled".to_string(),
            ],
            "Individual mode must emit per-leaf paths, not nested whole-subtree paths"
        );
    }

    /// Direct selection of a nested container while the parent stays
    /// empty must not leak into the parent's Whole behavior — toggling
    /// the parent next clears the nested selection cleanly.
    #[test]
    fn parent_whole_clears_pre_existing_nested_container_selection() {
        let mut s = fixture();
        let github = idx_of(&s, "[name=\"github\"]");
        let servers = idx_of(&s, "servers");

        s.toggle(github);
        assert!(s.is_selected(github));
        // selected_paths from this state alone yields just the nested path.
        assert_eq!(
            s.selected_paths()
                .iter()
                .map(|p| p.to_string())
                .collect::<Vec<_>>(),
            vec!["servers[name=\"github\"]".to_string()],
        );

        // Now press space on parent — should override.
        s.toggle(servers);
        assert_eq!(s.check_state(servers), CheckState::Whole);
        assert!(!s.is_selected(github), "nested must be cleared");
        assert_eq!(
            s.selected_paths()
                .iter()
                .map(|p| p.to_string())
                .collect::<Vec<_>>(),
            vec!["servers".to_string()],
        );
    }
}
