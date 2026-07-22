//! Application state and the focus/mode state machine.
//!
//! Three panes: the staging area (Changes), the commit history (Log), and a
//! diff pane that always shows the diff of whatever is selected in the last
//! focused left pane. The diff pane has its own cursor and a visual-selection
//! anchor for line-level staging.

use git2::{Oid, Repository};
use ratatui::widgets::ListState;

use crate::git::{self, CommitInfo, DiffLine, HeadInfo, Res, Section, StatusEntry};

/// Which panel currently receives navigation/action keys.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Focus {
    Status,
    Log,
    Diff,
}

impl Focus {
    pub fn next(self) -> Focus {
        match self {
            Focus::Status => Focus::Log,
            Focus::Log => Focus::Diff,
            Focus::Diff => Focus::Status,
        }
    }

    pub fn prev(self) -> Focus {
        match self {
            Focus::Status => Focus::Diff,
            Focus::Log => Focus::Status,
            Focus::Diff => Focus::Log,
        }
    }
}

/// A pane-movement direction (Shift+H/J/K/L).
#[derive(Clone, Copy)]
pub enum Dir {
    Left,
    Right,
    Up,
    Down,
}

/// The interaction mode. `Normal` routes keys to the focused panel; the others
/// are modal popups that capture all input until dismissed.
pub enum Mode {
    Normal,
    /// Typing a commit message.
    Input { editor: Editor },
    Message(String),
}

/// A small multi-line text editor backing the commit-message popup: a list of
/// lines plus a cursor at (row, col), where col is a *char* index (not bytes).
pub struct Editor {
    pub lines: Vec<String>,
    pub row: usize,
    pub col: usize,
}

impl Editor {
    pub fn new() -> Editor {
        Editor {
            lines: vec![String::new()],
            row: 0,
            col: 0,
        }
    }

    fn line(&self) -> &str {
        &self.lines[self.row]
    }

    fn line_chars(&self) -> usize {
        self.line().chars().count()
    }

    /// Byte offset of the cursor within the current line.
    fn byte_col(&self) -> usize {
        byte_at(self.line(), self.col)
    }

    pub fn insert(&mut self, c: char) {
        let b = self.byte_col();
        self.lines[self.row].insert(b, c);
        self.col += 1;
    }

    pub fn newline(&mut self) {
        let b = self.byte_col();
        let rest = self.lines[self.row].split_off(b);
        self.lines.insert(self.row + 1, rest);
        self.row += 1;
        self.col = 0;
    }

    pub fn backspace(&mut self) {
        if self.col > 0 {
            self.col -= 1;
            let b = self.byte_col();
            self.lines[self.row].remove(b);
        } else if self.row > 0 {
            // At the start of a line: join it onto the previous one.
            let cur = self.lines.remove(self.row);
            self.row -= 1;
            self.col = self.line_chars();
            self.lines[self.row].push_str(&cur);
        }
    }

    pub fn delete(&mut self) {
        if self.col < self.line_chars() {
            let b = self.byte_col();
            self.lines[self.row].remove(b);
        } else if self.row + 1 < self.lines.len() {
            let next = self.lines.remove(self.row + 1);
            self.lines[self.row].push_str(&next);
        }
    }

    pub fn left(&mut self) {
        if self.col > 0 {
            self.col -= 1;
        } else if self.row > 0 {
            self.row -= 1;
            self.col = self.line_chars();
        }
    }

    pub fn right(&mut self) {
        if self.col < self.line_chars() {
            self.col += 1;
        } else if self.row + 1 < self.lines.len() {
            self.row += 1;
            self.col = 0;
        }
    }

    pub fn up(&mut self) {
        if self.row > 0 {
            self.row -= 1;
            self.col = self.col.min(self.line_chars());
        }
    }

    pub fn down(&mut self) {
        if self.row + 1 < self.lines.len() {
            self.row += 1;
            self.col = self.col.min(self.line_chars());
        }
    }

    pub fn home(&mut self) {
        self.col = 0;
    }

    pub fn end(&mut self) {
        self.col = self.line_chars();
    }

    /// Ctrl+W: delete the word (plus any trailing spaces) before the cursor.
    pub fn delete_word_back(&mut self) {
        while self.col > 0 && self.prev_is_space() {
            self.backspace();
        }
        while self.col > 0 && !self.prev_is_space() {
            self.backspace();
        }
    }

    fn prev_is_space(&self) -> bool {
        self.line().chars().nth(self.col - 1) == Some(' ')
    }

    /// Ctrl+U: delete from the start of the line to the cursor.
    pub fn delete_to_start(&mut self) {
        let b = self.byte_col();
        self.lines[self.row].replace_range(..b, "");
        self.col = 0;
    }

    /// Display column of the cursor (unicode-width aware), for placing the
    /// terminal cursor and scrolling the view.
    pub fn display_col(&self) -> usize {
        use unicode_width::UnicodeWidthStr;
        self.line()[..self.byte_col()].width()
    }

    pub fn text(&self) -> String {
        self.lines.join("\n")
    }
}

fn byte_at(s: &str, col: usize) -> usize {
    s.char_indices().nth(col).map(|(i, _)| i).unwrap_or(s.len())
}

/// What the diff pane is currently showing.
#[derive(Clone, PartialEq)]
pub enum DiffSource {
    Nothing,
    File {
        path: String,
        staged: bool,
        untracked: bool,
    },
    Commit(Oid),
}

/// The diff pane: a document with a cursor and an optional highlight anchor.
pub struct DiffView {
    pub source: DiffSource,
    pub title: String,
    pub lines: Vec<DiffLine>,
    pub state: ListState,
    /// Visual-selection anchor (Shift+Space); the highlight spans anchor..=cursor.
    pub anchor: Option<usize>,
    /// Inner height of the pane at last render — used for half-page jumps.
    pub viewport: u16,
}

impl DiffView {
    fn empty() -> DiffView {
        DiffView {
            source: DiffSource::Nothing,
            title: "Diff".to_string(),
            lines: vec![DiffLine::info("(nothing selected)")],
            state: ListState::default().with_selected(Some(0)),
            anchor: None,
            viewport: 0,
        }
    }

    pub fn cursor(&self) -> usize {
        self.state.selected().unwrap_or(0)
    }

    pub fn move_cursor(&mut self, delta: i32) {
        move_in(&mut self.state, self.lines.len(), delta);
    }

    pub fn jump(&mut self, end: bool) {
        if !self.lines.is_empty() {
            self.state.select(Some(if end { self.lines.len() - 1 } else { 0 }));
        }
    }

    /// Highlighted display range (lo, hi), if a selection is active.
    pub fn selection(&self) -> Option<(usize, usize)> {
        self.anchor.map(|a| {
            let c = self.cursor();
            (a.min(c), a.max(c))
        })
    }
}

/// All application state that must survive from one frame to the next.
pub struct App {
    pub repo: Repository,
    pub commits: Vec<CommitInfo>,
    pub status: Vec<StatusEntry>,
    /// Current branch + ahead/behind vs origin (None on an empty repo).
    pub head: Option<HeadInfo>,
    pub log_state: ListState,
    pub status_state: ListState,
    pub focus: Focus,
    /// Which left pane (Status or Log) last drove the diff pane.
    pub last_left: Focus,
    pub diff: DiffView,
    pub mode: Mode,
    /// Transient one-line feedback shown in the bottom bar.
    pub status_line: String,
    pub should_quit: bool,
}

impl App {
    /// Open the repo at or above the current directory and load every panel.
    pub fn load() -> Res<App> {
        let repo = Repository::discover(".")?;
        let mut app = App {
            repo,
            commits: Vec::new(),
            status: Vec::new(),
            head: None,
            log_state: ListState::default(),
            status_state: ListState::default(),
            focus: Focus::Status,
            last_left: Focus::Status,
            diff: DiffView::empty(),
            mode: Mode::Normal,
            status_line: "Welcome to git-viz — press ? for help".to_string(),
            should_quit: false,
        };
        app.refresh()?;
        Ok(app)
    }

    /// Reload the panels from the repo and clamp selections to the new sizes.
    /// Call after every mutation so the UI always reflects the real repo state.
    pub fn refresh(&mut self) -> Res<()> {
        self.commits = git::load_commits(&self.repo)?;
        self.status = git::load_status(&self.repo)?;
        self.head = git::head_info(&self.repo);
        clamp(&mut self.log_state, self.commits.len());
        clamp(&mut self.status_state, self.status.len());
        self.rebuild_diff();
        Ok(())
    }

    // --- navigation -------------------------------------------------------

    /// Move the selection in the focused left panel by `delta`, clamped to
    /// range, and retarget the diff pane.
    pub fn move_selection(&mut self, delta: i32) {
        let (state, len) = match self.focus {
            Focus::Log => (&mut self.log_state, self.commits.len()),
            _ => (&mut self.status_state, self.status.len()),
        };
        move_in(state, len, delta);
        self.rebuild_diff();
    }

    /// Jump the focused left panel's selection to the first or last entry.
    pub fn jump_selection(&mut self, end: bool) {
        let (state, len) = match self.focus {
            Focus::Log => (&mut self.log_state, self.commits.len()),
            _ => (&mut self.status_state, self.status.len()),
        };
        if len > 0 {
            state.select(Some(if end { len - 1 } else { 0 }));
        }
        self.rebuild_diff();
    }

    pub fn set_focus(&mut self, f: Focus) {
        if self.focus == f {
            return;
        }
        self.focus = f;
        // When the driving left pane changes, the diff pane follows it.
        if matches!(f, Focus::Status | Focus::Log) && self.last_left != f {
            self.last_left = f;
            self.rebuild_diff();
        }
    }

    /// Directional pane movement (Shift + h/j/k/l).
    pub fn focus_dir(&mut self, d: Dir) {
        let next = match (self.focus, d) {
            (Focus::Status, Dir::Down) => Focus::Log,
            (Focus::Log, Dir::Up) => Focus::Status,
            (Focus::Status | Focus::Log, Dir::Right) => Focus::Diff,
            (Focus::Diff, Dir::Left) => self.last_left,
            _ => return,
        };
        self.set_focus(next);
    }

    pub fn selected_commit(&self) -> Option<&CommitInfo> {
        self.log_state.selected().and_then(|i| self.commits.get(i))
    }

    pub fn selected_status(&self) -> Option<&StatusEntry> {
        self.status_state.selected().and_then(|i| self.status.get(i))
    }

    // --- diff pane --------------------------------------------------------

    /// Point the diff pane at whatever the driving left pane has selected.
    /// Keeps the cursor when the source is unchanged (e.g. after a partial
    /// stage); resets it when the document changes.
    pub fn rebuild_diff(&mut self) {
        let (source, title) = match self.last_left {
            Focus::Log => match self.selected_commit() {
                Some(c) => (
                    DiffSource::Commit(c.oid),
                    format!("commit {} — {}", c.short_id, c.summary),
                ),
                None => (DiffSource::Nothing, "Diff".to_string()),
            },
            _ => match self.selected_status() {
                Some(e) => {
                    let flavor = match e.section {
                        Section::Staged => "staged",
                        Section::Unstaged => "unstaged",
                        Section::Untracked => "untracked",
                    };
                    (
                        DiffSource::File {
                            path: e.path.clone(),
                            staged: e.staged(),
                            untracked: e.section == Section::Untracked,
                        },
                        format!("{} — {flavor}", e.path),
                    )
                }
                None => (DiffSource::Nothing, "Diff".to_string()),
            },
        };

        let lines = match &source {
            DiffSource::Nothing => vec![DiffLine::info("(nothing selected — pick a file or commit)")],
            DiffSource::File { path, staged, untracked } => {
                git::diff_file(&self.repo, path, *staged, *untracked)
                    .unwrap_or_else(|e| vec![DiffLine::info(e.to_string())])
            }
            DiffSource::Commit(oid) => git::diff_commit(&self.repo, *oid)
                .unwrap_or_else(|e| vec![DiffLine::info(e.to_string())]),
        };

        let same = source == self.diff.source;
        if same {
            let cursor = self.diff.cursor().min(lines.len().saturating_sub(1));
            self.diff.state.select(Some(cursor));
        } else {
            // New document: fresh state so the scroll offset resets too.
            self.diff.state = ListState::default().with_selected(Some(0));
        }
        self.diff.source = source;
        self.diff.title = title;
        self.diff.lines = lines;
        self.diff.anchor = None;
    }

    /// Record feedback shown in the bottom bar.
    pub fn notify(&mut self, msg: impl Into<String>) {
        self.status_line = msg.into();
    }
}

/// Clamp a list's selection so it stays valid (and non-empty lists start selected).
fn clamp(state: &mut ListState, len: usize) {
    match state.selected() {
        _ if len == 0 => state.select(None),
        Some(i) if i >= len => state.select(Some(len - 1)),
        None => state.select(Some(0)),
        _ => {}
    }
}

/// Shift a selection by `delta`, saturating at both ends.
fn move_in(state: &mut ListState, len: usize, delta: i32) {
    if len == 0 {
        return;
    }
    let current = state.selected().unwrap_or(0) as i32;
    let next = (current + delta).clamp(0, len as i32 - 1);
    state.select(Some(next as usize));
}

#[cfg(test)]
mod tests {
    use super::Editor;

    fn type_in(e: &mut Editor, s: &str) {
        for c in s.chars() {
            e.insert(c);
        }
    }

    #[test]
    fn editor_multiline_editing() {
        let mut e = Editor::new();
        type_in(&mut e, "fix: bug");
        e.newline();
        type_in(&mut e, "body");
        assert_eq!(e.text(), "fix: bug\nbody");

        // Backspace at column 0 joins onto the previous line.
        e.home();
        e.backspace();
        assert_eq!(e.text(), "fix: bugbody");
        assert_eq!((e.row, e.col), (0, 8));

        // Delete at end of line joins the next line up.
        e.newline();
        e.up();
        e.end();
        e.delete();
        assert_eq!(e.text(), "fix: bugbody");

        // Cursor movement crosses line boundaries: after the split the lines
        // are ["fix: bug", "body"], so ← from the start of "body" lands at the
        // end of "fix: bug" (col 8), and → returns.
        e.newline();
        assert_eq!((e.row, e.col), (1, 0));
        e.left();
        assert_eq!((e.row, e.col), (0, 8));
        e.right();
        assert_eq!((e.row, e.col), (1, 0));
    }

    #[test]
    fn editor_word_and_line_deletion() {
        let mut e = Editor::new();
        type_in(&mut e, "abc def ");
        e.delete_word_back();
        assert_eq!(e.text(), "abc ");
        e.delete_word_back();
        assert_eq!(e.text(), "");

        type_in(&mut e, "abc def");
        e.left();
        e.left();
        e.left();
        e.delete_to_start();
        assert_eq!(e.text(), "def");
        assert_eq!(e.col, 0);
    }

    #[test]
    fn editor_is_unicode_aware() {
        let mut e = Editor::new();
        type_in(&mut e, "héllo 汉");
        assert_eq!(e.text(), "héllo 汉");
        // 汉 is one char but two display columns.
        assert_eq!(e.col, 7);
        assert_eq!(e.display_col(), 8);
        e.backspace();
        assert_eq!(e.text(), "héllo ");
        // Editing in the middle of multi-byte text stays on char boundaries.
        e.home();
        e.right();
        e.insert('X');
        assert_eq!(e.text(), "hXéllo ");
    }
}
