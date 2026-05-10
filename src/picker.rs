//! Interactive TUI picker for `dot-sync add`.
//!
//! Uses ratatui's built-in `init()` / `restore()` which handle raw mode,
//! the alternate screen, and a panic hook that restores the terminal
//! before propagating. The state machine lives in `picker_state`; this
//! module is rendering + event mapping only.

use std::io;

use anyhow::{Context, Result, bail};
use ratatui::DefaultTerminal;
use ratatui::crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::layout::{Constraint, Layout};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, List, ListItem, ListState, Paragraph};

use crate::discovery::{FieldNodeKind, FieldTree};
use crate::path::FieldPath;
use crate::picker_state::{CheckState, PickerState};

/// Outcome of the picker.
pub enum PickerOutcome {
    /// User pressed Enter — emit selected paths.
    Confirmed(Vec<FieldPath>),
    /// User pressed q / Esc / Ctrl-C — no selection.
    Cancelled,
}

/// Run the interactive picker for the given field tree, blocking until
/// the user confirms or cancels. Refuses to run when stdin / stdout
/// isn't a TTY — caller must check up-front and provide a non-interactive
/// path (`--field` flags) when running headless.
pub fn run(title: &str, tree: FieldTree) -> Result<PickerOutcome> {
    if !is_terminal() {
        bail!(
            "interactive picker requires a TTY; pass --field arguments to use \
             dot-sync add non-interactively"
        );
    }
    let mut terminal = ratatui::try_init().context("failed to enter raw terminal mode")?;
    let result = run_loop(&mut terminal, title, tree);
    ratatui::restore();
    result
}

/// True when stdout is connected to a terminal — that's where ratatui
/// renders. Crossterm reads keyboard events from `/dev/tty` directly
/// rather than stdin, so a piped stdin (e.g. `echo "" | dot-sync add`)
/// is fine as long as stdout is still a TTY.
fn is_terminal() -> bool {
    use std::io::IsTerminal;
    io::stdout().is_terminal()
}

fn run_loop(terminal: &mut DefaultTerminal, title: &str, tree: FieldTree) -> Result<PickerOutcome> {
    let mut state = PickerState::from_tree(tree);
    if state.nodes.is_empty() {
        // Nothing to pick — return confirmed empty rather than spawning
        // an empty picker. Caller will treat as "no fields selected".
        return Ok(PickerOutcome::Confirmed(Vec::new()));
    }

    loop {
        terminal
            .draw(|frame| draw(frame, title, &state))
            .context("failed to render picker frame")?;

        // Block on the next event. ratatui::DefaultTerminal handles
        // SIGWINCH / resize naturally because we redraw each loop.
        match event::read().context("failed to read terminal event")? {
            Event::Key(key) if key.kind == KeyEventKind::Press => {
                if let Some(outcome) = handle_key(&mut state, key) {
                    return Ok(outcome);
                }
            }
            // Ignore everything else (mouse, paste, focus, releases…).
            _ => {}
        }
    }
}

fn handle_key(state: &mut PickerState, key: KeyEvent) -> Option<PickerOutcome> {
    // Ctrl-C is treated as cancel even outside the named hotkeys.
    if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
        return Some(PickerOutcome::Cancelled);
    }
    match key.code {
        KeyCode::Up | KeyCode::Char('k') => {
            state.cursor_up();
            None
        }
        KeyCode::Down | KeyCode::Char('j') => {
            state.cursor_down();
            None
        }
        KeyCode::Left | KeyCode::Char('h') => {
            // Collapse current node if it's an expanded container; else
            // jump cursor to its parent. Same convention as `tree` UIs.
            let idx = state.cursor();
            if !state.nodes[idx].children.is_empty() && state.is_expanded(idx) {
                state.collapse(idx);
            } else if let Some(parent) = state.nodes[idx].parent {
                state.set_cursor(parent);
            }
            None
        }
        KeyCode::Right | KeyCode::Char('l') => {
            // Expand current node if it has collapsed children.
            let idx = state.cursor();
            if !state.nodes[idx].children.is_empty() && !state.is_expanded(idx) {
                state.expand(idx);
            }
            None
        }
        KeyCode::Char(' ') => {
            state.toggle(state.cursor());
            None
        }
        KeyCode::Enter => Some(PickerOutcome::Confirmed(state.selected_paths())),
        KeyCode::Esc | KeyCode::Char('q') => Some(PickerOutcome::Cancelled),
        _ => None,
    }
}

fn draw(frame: &mut ratatui::Frame, title: &str, state: &PickerState) {
    let chunks = Layout::vertical([
        Constraint::Length(3), // header
        Constraint::Min(1),    // tree
        Constraint::Length(1), // hints
    ])
    .split(frame.area());

    // Header
    let header_text = format!("dot-sync add — pick fields ({title})");
    let header = Paragraph::new(header_text).block(Block::default().borders(Borders::ALL));
    frame.render_widget(header, chunks[0]);

    // Tree as a stateful List, with the cursor's row highlighted.
    let visible = state.visible();
    let items: Vec<ListItem> = visible.iter().map(|&i| render_row(state, i)).collect();

    let list = List::new(items)
        .highlight_style(
            Style::default()
                .bg(Color::Indexed(238)) // dim grey for cursor row
                .add_modifier(Modifier::BOLD),
        )
        .highlight_symbol("▌ ");

    let cursor_pos = visible
        .iter()
        .position(|&i| i == state.cursor())
        .unwrap_or(0);
    let mut list_state = ListState::default();
    list_state.select(Some(cursor_pos));
    frame.render_stateful_widget(list, chunks[1], &mut list_state);

    // Hint line at the bottom.
    let hints = Paragraph::new(
        "↑/↓ move · ←/→ collapse/expand · space toggle · enter confirm · q/esc cancel",
    )
    .style(Style::default().fg(Color::DarkGray));
    frame.render_widget(hints, chunks[2]);
}

fn render_row(state: &PickerState, idx: usize) -> ListItem<'_> {
    let node = &state.nodes[idx];
    let indent = "  ".repeat(node.depth);

    // Triangle for expandable containers; blank slot for leaves to keep
    // checkbox columns aligned across rows.
    let arrow = if !node.children.is_empty() {
        if state.is_expanded(idx) {
            "▾ "
        } else {
            "▸ "
        }
    } else {
        "  "
    };

    let checkbox = match state.check_state(idx) {
        CheckState::Empty => "[ ]",
        CheckState::Whole => "[x]",
        CheckState::Individual => "[*]",
        CheckState::Mixed => "[~]",
    };
    let checkbox_style = match state.check_state(idx) {
        CheckState::Empty => Style::default(),
        CheckState::Whole => Style::default()
            .fg(Color::Green)
            .add_modifier(Modifier::BOLD),
        CheckState::Individual => Style::default()
            .fg(Color::Cyan)
            .add_modifier(Modifier::BOLD),
        CheckState::Mixed => Style::default().fg(Color::Yellow),
    };

    let label_style = match node.kind {
        FieldNodeKind::VirtualGroup => Style::default().fg(Color::DarkGray),
        FieldNodeKind::PinnedArrayItem => Style::default().fg(Color::Magenta),
        FieldNodeKind::Object => Style::default().fg(Color::Blue),
        FieldNodeKind::Leaf => Style::default(),
    };

    ListItem::new(Line::from(vec![
        Span::raw(indent),
        Span::raw(arrow),
        Span::styled(checkbox, checkbox_style),
        Span::raw(" "),
        Span::styled(node.display.clone(), label_style),
    ]))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::discovery::FieldNode;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    fn p(s: &str) -> FieldPath {
        FieldPath::parse(s).unwrap()
    }

    fn fixture_state() -> PickerState {
        let tui = FieldNode::object(
            "tui",
            p("tui"),
            vec![FieldNode::leaf("theme", p("tui.theme"))],
        );
        let model = FieldNode::leaf("model", p("model"));
        let tree = FieldTree {
            roots: vec![tui, model],
        };
        PickerState::from_tree(tree)
    }

    #[test]
    fn render_smoke_test_via_test_backend() {
        // Render the picker once and assert the buffer contains expected
        // text fragments. Locks the rendering pipeline against API drift.
        let backend = TestBackend::new(80, 10);
        let mut terminal = Terminal::new(backend).unwrap();
        let state = fixture_state();
        terminal.draw(|frame| draw(frame, "test", &state)).unwrap();
        let buf = terminal.backend().buffer();
        let dump = buffer_to_string(buf);
        assert!(dump.contains("dot-sync add"), "header missing: {dump}");
        assert!(dump.contains("tui"), "tui label missing: {dump}");
        assert!(dump.contains("theme"), "theme label missing: {dump}");
        assert!(dump.contains("model"), "model label missing: {dump}");
        assert!(dump.contains("[ ]"), "checkbox glyph missing: {dump}");
        assert!(dump.contains("space toggle"), "hint line missing: {dump}");
    }

    #[test]
    fn render_gitconfig_tree_via_test_backend() {
        // End-to-end: build a GitConfigDocument from a realistic
        // fixture, hand its discover_field_tree() to the picker, and
        // render the result. Locks two coupling points:
        //   1. discovery.rs's gitconfig walker emits the expected
        //      tree shape (sections / subsections as VirtualGroups,
        //      keys as Leaves).
        //   2. picker.rs renders that shape with the right glyph
        //      cycle — VirtualGroups don't have a `[x]` whole-mode,
        //      only `[ ]` ↔ `[*]`.
        use crate::document::{Document, GitConfigDocument};

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config");
        std::fs::write(
            &path,
            "\
[user]
\tname = Alice
\temail = a@b
[remote \"origin\"]
\turl = https://example.com/o
[alias]
\tco = checkout
",
        )
        .unwrap();
        let doc = GitConfigDocument::load(&path, false).unwrap();
        let tree = doc.discover_field_tree();
        let state = PickerState::from_tree(tree);

        let backend = TestBackend::new(80, 16);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|frame| draw(frame, "test", &state)).unwrap();
        let dump = buffer_to_string(terminal.backend().buffer());

        for label in ["user", "remote", "alias", "name", "email"] {
            assert!(dump.contains(label), "missing {label}:\n{dump}");
        }
        assert!(dump.contains("[ ]"), "checkbox glyph missing:\n{dump}");
        // Subsection "origin" appears with quotes (matches our
        // discovery output for non-special-char subsections).
        assert!(dump.contains("\"origin\""), "subsection label missing:\n{dump}");
    }

    #[test]
    fn render_gitconfig_individual_mode_uses_star_not_x() {
        // VirtualGroup nodes (sections in gitconfig) skip the `[x]`
        // whole-subtree state when the user cycles them — `[ ]` →
        // `[*]` → `[ ]`. This locks that behavior at the picker
        // level for the gitconfig discovery shape specifically. Note
        // that the leaves *under* an Individual-mode container do
        // render `[x]` (they're "selected as part of [*]"), which is
        // the intended visual.
        use crate::document::{Document, GitConfigDocument};

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config");
        std::fs::write(&path, "[user]\n\tname = Alice\n\temail = a@b\n").unwrap();
        let doc = GitConfigDocument::load(&path, false).unwrap();
        let tree = doc.discover_field_tree();
        let mut state = PickerState::from_tree(tree);

        // Toggle the [user] section (top-level VirtualGroup at idx 0).
        state.toggle(0);
        let backend = TestBackend::new(80, 12);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|frame| draw(frame, "test", &state)).unwrap();
        let dump = buffer_to_string(terminal.backend().buffer());

        // Find the line that contains the section label and assert it
        // shows `[*]` and not `[x]` — `[x]` on the row would mean the
        // VirtualGroup mistakenly entered Whole mode.
        let user_line = dump
            .lines()
            .find(|l| l.contains(" user"))
            .unwrap_or_else(|| panic!("user row missing:\n{dump}"));
        assert!(user_line.contains("[*]"), "user row: {user_line:?}");
        assert!(
            !user_line.contains("[x]"),
            "VirtualGroup row must not show [x]: {user_line:?}"
        );
    }

    #[test]
    fn render_reflects_tri_state_glyphs() {
        let mut state = fixture_state();
        // Toggle tui (Object container) → Whole.
        let tui_idx = 0;
        state.toggle(tui_idx);

        let backend = TestBackend::new(80, 10);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|frame| draw(frame, "test", &state)).unwrap();
        let dump = buffer_to_string(terminal.backend().buffer());
        assert!(dump.contains("[x]"), "Whole glyph missing: {dump}");

        // Cycle → Individual.
        state.toggle(tui_idx);
        terminal.draw(|frame| draw(frame, "test", &state)).unwrap();
        let dump = buffer_to_string(terminal.backend().buffer());
        assert!(dump.contains("[*]"), "Individual glyph missing: {dump}");
    }

    fn buffer_to_string(buf: &ratatui::buffer::Buffer) -> String {
        let mut out = String::new();
        for y in 0..buf.area.height {
            for x in 0..buf.area.width {
                let cell = &buf[(x, y)];
                out.push_str(cell.symbol());
            }
            out.push('\n');
        }
        out
    }
}
