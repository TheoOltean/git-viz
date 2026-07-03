//! Application state and the focus/mode state machine.
//!
//! Stripped to the basics: a commit-history view, the staging area, and a
//! local-vs-origin summary. No branch management.

use git2::Repository;
use ratatui::widgets::ListState;

use crate::git::{self, CommitInfo, HeadInfo, Res, StatusEntry};

/// Which panel currently receives navigation/action keys.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Focus {
    Log,
    Status,
}

impl Focus {
    /// There are only two panels, so next and prev both just toggle.
    pub fn toggle(self) -> Focus {
        match self {
            Focus::Log => Focus::Status,
            Focus::Status => Focus::Log,
        }
    }
}

/// The interaction mode. `Normal` routes keys to the focused panel; the others
/// are modal popups that capture all input until dismissed.
pub enum Mode {
    Normal,
    /// Typing a commit message.
    Input { buffer: String },
    Diff { title: String, lines: Vec<String>, scroll: u16 },
    Message(String),
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
    pub mode: Mode,
    /// Transient one-line feedback shown in the bottom bar.
    pub status_line: String,
    pub should_quit: bool,
}

impl App {
    /// Open the repo in the current directory and load every panel.
    pub fn load() -> Res<App> {
        let repo = Repository::open(".")?;
        let mut app = App {
            repo,
            commits: Vec::new(),
            status: Vec::new(),
            head: None,
            log_state: ListState::default(),
            status_state: ListState::default(),
            focus: Focus::Log,
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
        Ok(())
    }

    // --- navigation -------------------------------------------------------

    /// Move the selection in the focused panel by `delta` (±1), clamped to range.
    pub fn move_selection(&mut self, delta: i32) {
        let (state, len) = match self.focus {
            Focus::Log => (&mut self.log_state, self.commits.len()),
            Focus::Status => (&mut self.status_state, self.status.len()),
        };
        move_in(state, len, delta);
    }

    pub fn selected_commit(&self) -> Option<&CommitInfo> {
        self.log_state.selected().and_then(|i| self.commits.get(i))
    }

    pub fn selected_status(&self) -> Option<&StatusEntry> {
        self.status_state.selected().and_then(|i| self.status.get(i))
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
