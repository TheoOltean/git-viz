//! Key handling. Dispatches by mode first, then (in Normal mode) by focused panel.
//! Every git operation that can fail is funneled through `report`, which turns an
//! error into a dismissible message popup so the TUI never crashes mid-op.

use crossterm::event::{KeyCode, KeyEvent, KeyEventKind};

use crate::app::{App, Focus, Mode};
use crate::git::{self, Res, Section};

pub fn handle_key(app: &mut App, key: KeyEvent) {
    if key.kind != KeyEventKind::Press {
        return;
    }
    // Match on a lightweight tag so we don't hold a borrow of `app.mode`.
    match tag(&app.mode) {
        ModeTag::Normal => normal(app, key),
        ModeTag::Input => input(app, key),
        ModeTag::Diff => diff(app, key),
        ModeTag::Message => {
            // Any key dismisses.
            app.mode = Mode::Normal;
        }
    }
}

enum ModeTag {
    Normal,
    Input,
    Diff,
    Message,
}

fn tag(mode: &Mode) -> ModeTag {
    match mode {
        Mode::Normal => ModeTag::Normal,
        Mode::Input { .. } => ModeTag::Input,
        Mode::Diff { .. } => ModeTag::Diff,
        Mode::Message(_) => ModeTag::Message,
    }
}

// ---------------------------------------------------------------------------
// Normal mode
// ---------------------------------------------------------------------------

fn normal(app: &mut App, key: KeyEvent) {
    match key.code {
        // --- global controls ---
        KeyCode::Char('q') => app.should_quit = true,
        KeyCode::Tab | KeyCode::BackTab => app.focus = app.focus.toggle(),
        KeyCode::Char('?') => app.mode = Mode::Message(HELP.to_string()),
        KeyCode::Char('r') => {
            let res = app.refresh();
            report(app, res, |_| "Refreshed".to_string());
        }
        KeyCode::Char('p') => remote(app, git::push()),
        KeyCode::Char('P') => remote(app, git::pull()),
        KeyCode::Char('f') => remote(app, git::fetch()),

        // --- navigation (moves whichever panel is focused) ---
        KeyCode::Char('j') | KeyCode::Down => app.move_selection(1),
        KeyCode::Char('k') | KeyCode::Up => app.move_selection(-1),

        // --- staging actions (work from either panel) ---
        KeyCode::Char('s') => stage_selected(app),
        KeyCode::Char('u') => unstage_selected(app),
        KeyCode::Char('a') => {
            let res = stage_all(app);
            report(app, res, |_| "Staged all changes".to_string());
        }
        KeyCode::Char('c') => open_commit(app),

        // --- contextual: Enter/d diff the selected commit or file ---
        KeyCode::Enter | KeyCode::Char('d') => match app.focus {
            Focus::Status => diff_selected_file(app),
            Focus::Log => diff_selected_commit(app),
        },
        _ => {}
    }
}

fn open_commit(app: &mut App) {
    let staged = app.status.iter().filter(|e| e.section == Section::Staged).count();
    if staged == 0 {
        app.notify("Nothing staged to commit — press s or a first");
        return;
    }
    app.mode = Mode::Input {
        buffer: String::new(),
    };
}

fn stage_selected(app: &mut App) {
    match app
        .selected_status()
        .map(|e| (e.path.clone(), e.section, e.deleted))
    {
        Some((_, Section::Staged, _)) => app.notify("Already staged — use u to unstage"),
        Some((path, _, deleted)) => {
            let res = git::stage(&app.repo, &path, deleted);
            report(app, res, |_| format!("Staged {path}"));
        }
        None => app.notify("Nothing to stage"),
    }
}

fn unstage_selected(app: &mut App) {
    match app.selected_status().map(|e| (e.path.clone(), e.section)) {
        Some((path, Section::Staged)) => {
            let res = git::unstage(&app.repo, &path);
            report(app, res, |_| format!("Unstaged {path}"));
        }
        Some(_) => app.notify("That change isn't staged"),
        None => app.notify("Nothing to unstage"),
    }
}

/// Stage every changed path currently shown in the status panel.
fn stage_all(app: &mut App) -> Res<()> {
    // Clone the work-list first so we're not borrowing `app.status` during mutation.
    let entries: Vec<(String, bool)> = app
        .status
        .iter()
        .filter(|e| e.section != Section::Staged)
        .map(|e| (e.path.clone(), e.deleted))
        .collect();
    for (path, deleted) in entries {
        git::stage(&app.repo, &path, deleted)?;
    }
    Ok(())
}

fn diff_selected_commit(app: &mut App) {
    if let Some((oid, short)) = app.selected_commit().map(|c| (c.oid, c.short_id.clone())) {
        match git::diff_commit(&app.repo, oid) {
            Ok(lines) => {
                app.mode = Mode::Diff {
                    title: format!("diff {short}"),
                    lines,
                    scroll: 0,
                }
            }
            Err(e) => app.mode = Mode::Message(e.to_string()),
        }
    }
}

fn diff_selected_file(app: &mut App) {
    if let Some((path, staged)) = app.selected_status().map(|e| (e.path.clone(), e.staged())) {
        match git::diff_file(&app.repo, &path, staged) {
            Ok(lines) => {
                app.mode = Mode::Diff {
                    title: format!("diff {path}"),
                    lines,
                    scroll: 0,
                }
            }
            Err(e) => app.mode = Mode::Message(e.to_string()),
        }
    } else {
        app.notify("No file selected");
    }
}

// ---------------------------------------------------------------------------
// Input mode (commit message)
// ---------------------------------------------------------------------------

fn input(app: &mut App, key: KeyEvent) {
    match key.code {
        KeyCode::Esc => {
            app.mode = Mode::Normal;
            app.notify("Commit cancelled");
        }
        KeyCode::Enter => submit_commit(app),
        KeyCode::Backspace => {
            if let Mode::Input { buffer } = &mut app.mode {
                buffer.pop();
            }
        }
        KeyCode::Char(c) => {
            if let Mode::Input { buffer } = &mut app.mode {
                buffer.push(c);
            }
        }
        _ => {}
    }
}

fn submit_commit(app: &mut App) {
    let mode = std::mem::replace(&mut app.mode, Mode::Normal);
    let Mode::Input { buffer } = mode else {
        return;
    };
    let text = buffer.trim().to_string();
    if text.is_empty() {
        app.notify("Aborted: empty commit message");
        return;
    }
    let res = git::commit(&app.repo, &text);
    report(app, res, |oid| {
        format!("Committed {}", &oid.to_string()[..7])
    });
}

// ---------------------------------------------------------------------------
// Diff mode
// ---------------------------------------------------------------------------

fn diff(app: &mut App, key: KeyEvent) {
    match key.code {
        KeyCode::Char('q') | KeyCode::Esc => app.mode = Mode::Normal,
        KeyCode::Char('j') | KeyCode::Down => scroll_diff(app, 1),
        KeyCode::Char('k') | KeyCode::Up => scroll_diff(app, -1),
        _ => {}
    }
}

fn scroll_diff(app: &mut App, delta: i32) {
    if let Mode::Diff { scroll, lines, .. } = &mut app.mode {
        let max = lines.len().saturating_sub(1) as i32;
        *scroll = (*scroll as i32 + delta).clamp(0, max) as u16;
    }
}

// ---------------------------------------------------------------------------
// Shared helpers
// ---------------------------------------------------------------------------

/// Run an op result: on success, refresh panels and set the status line; on
/// failure, pop up the error. Keeps the draw loop free of `Result`s.
fn report<T>(app: &mut App, res: Res<T>, ok: impl FnOnce(&T) -> String) {
    match res {
        Ok(v) => {
            let msg = ok(&v);
            let _ = app.refresh();
            app.notify(msg);
        }
        Err(e) => app.mode = Mode::Message(e.to_string()),
    }
}

/// Remote ops always show their (multi-line) CLI output in a message popup.
fn remote(app: &mut App, res: Res<String>) {
    let _ = app.refresh();
    app.mode = match res {
        Ok(out) => Mode::Message(out),
        Err(e) => Mode::Message(e.to_string()),
    };
}

const HELP: &str = "\
git-viz — keys

  General
    Tab               switch panel (Log / Changes)
    j / k  (↓ / ↑)    move selection
    r                 refresh
    p / P / f         push / pull / fetch
    ?                 this help
    q                 quit

  Commit history (Log)
    Enter / d         view commit diff

  Staging area (Changes)
    s / u             stage / unstage selected file
    a                 stage all
    c                 commit staged changes
    Enter / d         view file diff

  Popups
    Esc               cancel   ·   any key closes messages";
