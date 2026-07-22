//! Key handling. Dispatches by mode first, then (in Normal mode) by focused pane.
//! Every git operation that can fail is funneled through `report`, which turns an
//! error into a dismissible message popup so the TUI never crashes mid-op.

use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};

use crate::app::{App, Dir, DiffSource, Editor, Focus, Mode};
use crate::git::{self, Res, Section};

pub fn handle_key(app: &mut App, key: KeyEvent) {
    if key.kind != KeyEventKind::Press {
        return;
    }
    // Ctrl+C exits immediately from any mode, abandoning whatever was in
    // flight (a half-typed commit message is discarded, not committed).
    if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
        app.should_quit = true;
        return;
    }
    // Match on a lightweight tag so we don't hold a borrow of `app.mode`.
    match tag(&app.mode) {
        ModeTag::Normal => normal(app, key),
        ModeTag::Input => input(app, key),
        ModeTag::Message => {
            // Any key dismisses.
            app.mode = Mode::Normal;
        }
    }
}

enum ModeTag {
    Normal,
    Input,
    Message,
}

fn tag(mode: &Mode) -> ModeTag {
    match mode {
        Mode::Normal => ModeTag::Normal,
        Mode::Input { .. } => ModeTag::Input,
        Mode::Message(_) => ModeTag::Message,
    }
}

// ---------------------------------------------------------------------------
// Normal mode
// ---------------------------------------------------------------------------

fn normal(app: &mut App, key: KeyEvent) {
    // Pane movement first: Shift + h/j/k/l (or Shift + arrows).
    if let Some(dir) = pane_dir(&key) {
        app.focus_dir(dir);
        return;
    }
    match key.code {
        // --- global controls ---
        KeyCode::Char('q') => app.should_quit = true,
        KeyCode::Tab => app.set_focus(app.focus.next()),
        KeyCode::BackTab => app.set_focus(app.focus.prev()),
        KeyCode::Char('?') => app.mode = Mode::Message(HELP.to_string()),
        KeyCode::Char('r') => {
            let res = app.refresh();
            report(app, res, |_| "Refreshed".to_string());
        }
        KeyCode::Char('c') => open_commit(app),
        KeyCode::Char('p') => {
            let res = git::push(&app.repo);
            remote(app, res);
        }
        KeyCode::Char('P') => {
            let res = git::pull(&app.repo);
            remote(app, res);
        }
        KeyCode::Char('f') => {
            let res = git::fetch(&app.repo);
            remote(app, res);
        }

        // --- everything else is pane-specific ---
        _ => match app.focus {
            Focus::Diff => diff_keys(app, key),
            _ => list_keys(app, key),
        },
    }
}

/// Shift + vim direction (or Shift + arrow) → a pane movement.
/// Shift+h arrives as an uppercase char on legacy terminals and as a
/// lowercase char + SHIFT under the kitty keyboard protocol — accept both.
fn pane_dir(key: &KeyEvent) -> Option<Dir> {
    let shift = key.modifiers.contains(KeyModifiers::SHIFT);
    match key.code {
        KeyCode::Char(c) => {
            let c = if shift { c.to_ascii_uppercase() } else { c };
            match c {
                'H' => Some(Dir::Left),
                'J' => Some(Dir::Down),
                'K' => Some(Dir::Up),
                'L' => Some(Dir::Right),
                _ => None,
            }
        }
        KeyCode::Left if shift => Some(Dir::Left),
        KeyCode::Right if shift => Some(Dir::Right),
        KeyCode::Up if shift => Some(Dir::Up),
        KeyCode::Down if shift => Some(Dir::Down),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// Changes / Log panes
// ---------------------------------------------------------------------------

fn list_keys(app: &mut App, key: KeyEvent) {
    match key.code {
        KeyCode::Char('j') | KeyCode::Down => app.move_selection(1),
        KeyCode::Char('k') | KeyCode::Up => app.move_selection(-1),
        KeyCode::Char('g') => app.jump_selection(false),
        KeyCode::Char('G') => app.jump_selection(true),
        KeyCode::Enter | KeyCode::Char('d') => app.set_focus(Focus::Diff),

        KeyCode::Char('s') => {
            if app.focus == Focus::Status {
                stage_selected(app);
            } else {
                app.notify("Staging acts on files — Shift+K to the Changes pane");
            }
        }
        KeyCode::Char('u') => {
            if app.focus == Focus::Status {
                unstage_selected(app);
            } else {
                app.notify("Unstaging acts on files — Shift+K to the Changes pane");
            }
        }
        KeyCode::Char('a') => {
            let res = stage_all(app);
            report(app, res, |_| "Staged all changes".to_string());
        }
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
        editor: Editor::new(),
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

// ---------------------------------------------------------------------------
// Diff pane — cursor, highlight, line-level stage/unstage
// ---------------------------------------------------------------------------

fn diff_keys(app: &mut App, key: KeyEvent) {
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    let half = (app.diff.viewport / 2).max(1) as i32;
    match key.code {
        KeyCode::Char('d') if ctrl => app.diff.move_cursor(half),
        KeyCode::Char('u') if ctrl => app.diff.move_cursor(-half),
        KeyCode::Char('j') | KeyCode::Down => app.diff.move_cursor(1),
        KeyCode::Char('k') | KeyCode::Up => app.diff.move_cursor(-1),
        KeyCode::Char('g') => app.diff.jump(false),
        KeyCode::Char('G') => app.diff.jump(true),
        // Shift+Space starts/stops the highlight. Terminals without the kitty
        // protocol report Shift+Space as a plain space — accept both.
        KeyCode::Char(' ') => toggle_anchor(app),
        KeyCode::Esc => {
            if app.diff.anchor.take().is_some() {
                app.notify("Highlight cleared");
            }
        }
        KeyCode::Char('s') => stage_in_diff(app),
        KeyCode::Char('u') => unstage_in_diff(app),
        _ => {}
    }
}

fn toggle_anchor(app: &mut App) {
    if app.diff.anchor.take().is_some() {
        app.notify("Highlight cleared");
        return;
    }
    if git::change_count(&app.diff.lines) == 0 {
        app.notify("Nothing to highlight in this diff");
        return;
    }
    app.diff.anchor = Some(app.diff.cursor());
    app.notify("Highlighting — j/k extend · s stage · u unstage · Space/Esc cancel");
}

/// The lines an s/u press acts on: the active highlight, or failing that the
/// hunk under the cursor.
fn active_range(app: &App) -> Option<(usize, usize)> {
    app.diff
        .selection()
        .or_else(|| git::hunk_at(&app.diff.lines, app.diff.cursor()))
}

fn stage_in_diff(app: &mut App) {
    let DiffSource::File { path, staged, untracked } = app.diff.source.clone() else {
        app.notify("Staging works on file diffs — pick a file in Changes");
        return;
    };
    if staged {
        app.notify("These lines are already staged — u unstages them");
        return;
    }
    let Some(range) = active_range(app) else {
        app.notify("Highlight lines with Shift+Space or move into a hunk first");
        return;
    };
    let Some((patch, kept)) = git::partial_patch(&app.diff.lines, range, false) else {
        app.notify("No +/− lines in the highlight");
        return;
    };
    // A deletion can't be split: a partial patch would need context lines
    // against /dev/null. (Untracked files are fine — intent-to-add makes the
    // index side real.)
    if kept < git::change_count(&app.diff.lines)
        && !untracked
        && git::whole_file_kind(&app.diff.lines) == Some("deleted")
    {
        app.notify("Deleted files stage as a whole — press s on the file in Changes");
        return;
    }
    let res = (|| -> Res<()> {
        if untracked {
            git::intent_to_add(&app.repo, &path)?;
        }
        git::apply_cached(&app.repo, &patch, false)
    })();
    report(app, res, |_| format!("Staged {kept} line(s) of {path}"));
}

fn unstage_in_diff(app: &mut App) {
    let DiffSource::File { path, staged, .. } = app.diff.source.clone() else {
        app.notify("Unstaging works on file diffs — pick a staged file in Changes");
        return;
    };
    if !staged {
        app.notify("These lines aren't staged yet — s stages them");
        return;
    }
    let Some(range) = active_range(app) else {
        app.notify("Highlight lines with Shift+Space or move into a hunk first");
        return;
    };
    let Some((patch, kept)) = git::partial_patch(&app.diff.lines, range, true) else {
        app.notify("No +/− lines in the highlight");
        return;
    };
    // Newly added or deleted files have no HEAD side to leave context against,
    // so they can only be unstaged whole.
    if kept < git::change_count(&app.diff.lines) && git::whole_file_kind(&app.diff.lines).is_some() {
        app.notify("New/deleted files unstage as a whole — press u on the file in Changes");
        return;
    }
    let res = git::apply_cached(&app.repo, &patch, true);
    report(app, res, |_| format!("Unstaged {kept} line(s) of {path}"));
}

// ---------------------------------------------------------------------------
// Input mode (commit message)
// ---------------------------------------------------------------------------

fn input(app: &mut App, key: KeyEvent) {
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    let alt = key.modifiers.contains(KeyModifiers::ALT);
    let shift = key.modifiers.contains(KeyModifiers::SHIFT);

    if key.code == KeyCode::Esc {
        app.mode = Mode::Normal;
        app.notify("Commit cancelled");
        return;
    }
    // Plain Enter commits; Alt+Enter (and Shift+Enter where the terminal can
    // report it) inserts a newline, handled below.
    if key.code == KeyCode::Enter && !alt && !shift {
        submit_commit(app);
        return;
    }

    let Mode::Input { editor } = &mut app.mode else {
        return;
    };
    match key.code {
        KeyCode::Enter => editor.newline(),
        KeyCode::Char('n') if ctrl => editor.newline(),
        KeyCode::Char('a') if ctrl => editor.home(),
        KeyCode::Char('e') if ctrl => editor.end(),
        KeyCode::Char('w') if ctrl => editor.delete_word_back(),
        KeyCode::Char('u') if ctrl => editor.delete_to_start(),
        KeyCode::Char(c) if !ctrl && !alt => editor.insert(c),
        KeyCode::Backspace => editor.backspace(),
        KeyCode::Delete => editor.delete(),
        KeyCode::Left => editor.left(),
        KeyCode::Right => editor.right(),
        KeyCode::Up => editor.up(),
        KeyCode::Down => editor.down(),
        KeyCode::Home => editor.home(),
        KeyCode::End => editor.end(),
        _ => {}
    }
}

/// Bracketed paste goes straight into the commit-message editor, newlines and
/// all — without this, the `\r`s in a pasted message would submit it early.
pub fn handle_paste(app: &mut App, data: &str) {
    let Mode::Input { editor } = &mut app.mode else {
        return;
    };
    let normalized = data.replace("\r\n", "\n").replace('\r', "\n");
    for ch in normalized.chars() {
        match ch {
            '\n' => editor.newline(),
            '\t' => {
                for _ in 0..4 {
                    editor.insert(' ');
                }
            }
            c if c.is_control() => {}
            c => editor.insert(c),
        }
    }
}

fn submit_commit(app: &mut App) {
    let mode = std::mem::replace(&mut app.mode, Mode::Normal);
    let Mode::Input { editor } = mode else {
        return;
    };
    let text = editor.text().trim().to_string();
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

  Panes
    Shift+H/J/K/L     move between panes (vim directions)
    Tab / Shift+Tab   cycle panes
    j / k  (↓ / ↑)    move selection / cursor
    g / G             jump to top / bottom
    Enter             jump to the diff pane
    r refresh   ·   q / Ctrl+C quit   ·   ? this help
                      (Ctrl+C works from anywhere and discards
                       any in-progress input)

  Changes pane
    s / u             stage / unstage the selected file
    a                 stage all
    c                 commit staged changes

  Commit message
    Enter             commit
    Ctrl+N            insert a newline (subject + body); Alt+Enter too
    arrows/Home/End   move the cursor  (Ctrl+A / Ctrl+E = line ends)
    Ctrl+W / Ctrl+U   delete word / to start of line
    Esc               cancel

  Diff pane  (follows the selected file or commit)
    Shift+Space       start/stop highlighting lines (plain Space works too)
    j / k             extend the highlight / move the cursor
    Ctrl+d / Ctrl+u   half-page down / up
    s                 stage highlighted lines (no highlight: hunk under cursor)
    u                 unstage highlighted lines (no highlight: hunk under cursor)
    Esc               clear the highlight

  Sync
    p / P / f         push / pull / fetch

  Popups
    Esc               cancel   ·   any key closes messages";

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    #[test]
    fn ctrl_c_quits_from_any_mode_without_committing() {
        let mut app = App::load().expect("open repo");
        let ctrl_c = KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL);

        let mut editor = Editor::new();
        for c in "half-typed message".chars() {
            editor.insert(c);
        }
        let modes = [
            Mode::Normal,
            Mode::Input { editor },
            Mode::Message("popup".to_string()),
        ];
        for mode in modes {
            app.should_quit = false;
            app.mode = mode;
            handle_key(&mut app, ctrl_c);
            assert!(app.should_quit, "Ctrl+C must quit from every mode");
        }

        // Plain c (no Ctrl) must keep meaning "commit", not quit.
        app.should_quit = false;
        app.mode = Mode::Normal;
        handle_key(&mut app, KeyEvent::new(KeyCode::Char('c'), KeyModifiers::NONE));
        assert!(!app.should_quit);
    }

    #[test]
    fn commit_editor_supports_multiline_and_cursor_editing() {
        let mut app = App::load().expect("open repo");
        app.mode = Mode::Input { editor: Editor::new() };

        let press = |app: &mut App, code: KeyCode, mods: KeyModifiers| {
            handle_key(app, KeyEvent::new(code, mods));
        };
        let type_str = |app: &mut App, s: &str| {
            for c in s.chars() {
                press(app, KeyCode::Char(c), KeyModifiers::NONE);
            }
        };

        type_str(&mut app, "subjct");
        // Fix the typo in place: ← ← then insert 'e'.
        press(&mut app, KeyCode::Left, KeyModifiers::NONE);
        press(&mut app, KeyCode::Left, KeyModifiers::NONE);
        press(&mut app, KeyCode::Char('e'), KeyModifiers::NONE);
        // Ctrl+N opens a body line without committing.
        press(&mut app, KeyCode::End, KeyModifiers::NONE);
        press(&mut app, KeyCode::Char('n'), KeyModifiers::CONTROL);
        type_str(&mut app, "body");
        // Alt+Enter also inserts a newline instead of committing.
        press(&mut app, KeyCode::Enter, KeyModifiers::ALT);
        type_str(&mut app, "more");

        let Mode::Input { editor } = &app.mode else {
            panic!("still in input mode");
        };
        assert_eq!(editor.text(), "subject\nbody\nmore");

        // A multi-line paste lands in the editor instead of submitting.
        handle_paste(&mut app, " pasted\r\ntail");
        let Mode::Input { editor } = &app.mode else {
            panic!("still in input mode");
        };
        assert_eq!(editor.text(), "subject\nbody\nmore pasted\ntail");

        // Esc abandons the whole thing.
        press(&mut app, KeyCode::Esc, KeyModifiers::NONE);
        assert!(matches!(app.mode, Mode::Normal));
    }
}
