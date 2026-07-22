//! Rendering. Three panes — the staging area (Changes) and commit history
//! (Log) stacked on the left, the diff pane on the right — plus a bottom bar
//! and modal popups.

use ratatui::{
    layout::{Constraint, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, List, ListItem, Paragraph, Wrap},
    Frame,
};

use crate::app::{App, Focus, Mode};
use crate::git::{LineKind, Section};

/// Border color for a panel: bright when focused, dim otherwise.
fn border_style(focused: bool) -> Style {
    if focused {
        Style::new().fg(Color::Cyan).add_modifier(Modifier::BOLD)
    } else {
        Style::new().fg(Color::DarkGray)
    }
}

fn panel(title: &str, focused: bool) -> Block<'_> {
    Block::default()
        .borders(Borders::ALL)
        .border_style(border_style(focused))
        .title(format!(" {title} "))
}

pub fn ui(frame: &mut Frame, app: &mut App) {
    // Outer: body + a feedback line + a legend.
    let outer = Layout::vertical([Constraint::Min(0), Constraint::Length(3)]).split(frame.area());
    let body =
        Layout::horizontal([Constraint::Percentage(36), Constraint::Min(0)]).split(outer[0]);
    let left = Layout::vertical([Constraint::Percentage(40), Constraint::Min(0)]).split(body[0]);

    render_status(frame, app, left[0]);
    render_log(frame, app, left[1]);
    render_diff(frame, app, body[1]);
    render_status_bar(frame, app, outer[1]);

    // Popups draw on top of the body.
    match &app.mode {
        Mode::Normal => {}
        Mode::Input { .. } => render_input(frame, app, outer[0]),
        Mode::Message(_) => render_message(frame, app, outer[0]),
    }
}

fn render_status(frame: &mut Frame, app: &mut App, area: Rect) {
    // Entries arrive grouped Staged → Unstaged → Untracked. Color + symbol make
    // the group obvious at a glance: green ● = will be committed.
    let items: Vec<ListItem> = app
        .status
        .iter()
        .map(|e| {
            let (symbol, color) = match e.section {
                Section::Staged => ("●", Color::Green),
                Section::Unstaged => ("●", Color::Yellow),
                Section::Untracked => ("?", Color::DarkGray),
            };
            let line = Line::from(vec![
                Span::styled(format!("{symbol} "), Style::new().fg(color)),
                Span::styled(format!("{:<2} ", e.code), Style::new().fg(color)),
                Span::raw(e.path.clone()),
            ]);
            ListItem::new(line)
        })
        .collect();

    let staged = app.status.iter().filter(|e| e.section == Section::Staged).count();
    let unstaged = app.status.iter().filter(|e| e.section == Section::Unstaged).count();
    let untracked = app.status.iter().filter(|e| e.section == Section::Untracked).count();
    // Compact, adaptive title: only nonzero groups.
    let mut parts = Vec::new();
    if staged > 0 {
        parts.push(format!("{staged} staged"));
    }
    if unstaged > 0 {
        parts.push(format!("{unstaged} mod"));
    }
    if untracked > 0 {
        parts.push(format!("{untracked} new"));
    }
    let title = if parts.is_empty() {
        "Changes (clean)".to_string()
    } else {
        format!("Changes: {}", parts.join(", "))
    };

    let list = List::new(items)
        .block(panel(&title, app.focus == Focus::Status))
        .highlight_style(Style::new().add_modifier(Modifier::REVERSED))
        .highlight_symbol("> ");
    frame.render_stateful_widget(list, area, &mut app.status_state);
}

fn render_log(frame: &mut Frame, app: &mut App, area: Rect) {
    let items: Vec<ListItem> = app
        .commits
        .iter()
        .map(|c| {
            let mut spans = vec![
                Span::styled(c.graph.clone(), Style::new().fg(Color::Cyan)),
                Span::styled(format!("{} ", c.short_id), Style::new().fg(Color::DarkGray)),
            ];
            // Ref badges (local green, origin magenta) show where each branch
            // sits — so you can see local commits sitting ahead of origin.
            for r in &c.refs {
                let color = if r.remote { Color::Magenta } else { Color::Green };
                spans.push(Span::styled(
                    format!("{} ", r.name),
                    Style::new().fg(color).add_modifier(Modifier::BOLD),
                ));
            }
            spans.push(Span::raw(c.summary.clone()));
            spans.push(Span::styled(
                format!("  ({})", c.author),
                Style::new().fg(Color::DarkGray),
            ));
            ListItem::new(Line::from(spans))
        })
        .collect();

    let title = log_title(app);
    let list = List::new(items)
        .block(panel(&title, app.focus == Focus::Log))
        .highlight_style(Style::new().add_modifier(Modifier::REVERSED))
        .highlight_symbol("> ");
    frame.render_stateful_widget(list, area, &mut app.log_state);
}

/// Log title carries the local-vs-origin summary: branch name + ahead/behind.
fn log_title(app: &App) -> String {
    let n = app.commits.len();
    match &app.head {
        Some(h) => {
            let mut sync = String::new();
            if h.ahead > 0 {
                sync.push_str(&format!("↑{} ", h.ahead));
            }
            if h.behind > 0 {
                sync.push_str(&format!("↓{} ", h.behind));
            }
            if h.ahead == 0 && h.behind == 0 && h.upstream.is_some() {
                sync.push_str("✓ ");
            }
            format!("{}  {}— {n} commits", h.branch, sync)
        }
        None => format!("git-viz — {n} commits"),
    }
}

/// The diff pane: syntax-colored patch lines with a cursor and, while
/// highlighting, a visibly selected range.
fn render_diff(frame: &mut Frame, app: &mut App, area: Rect) {
    let focused = app.focus == Focus::Diff;
    let sel = app.diff.selection();

    let items: Vec<ListItem> = app
        .diff
        .lines
        .iter()
        .enumerate()
        .map(|(i, l)| {
            let (marker, style) = match l.kind {
                LineKind::Add => ("+", Style::new().fg(Color::Green)),
                LineKind::Del => ("-", Style::new().fg(Color::Red)),
                LineKind::Ctx => (" ", Style::new()),
                LineKind::Hunk => ("", Style::new().fg(Color::Cyan)),
                LineKind::File => ("", Style::new().fg(Color::DarkGray).add_modifier(Modifier::BOLD)),
                LineKind::Meta => ("", Style::new().fg(Color::DarkGray)),
                LineKind::Msg => ("", Style::new()),
            };
            let mut line = Line::from(Span::styled(format!("{marker}{}", l.text), style));
            if sel.is_some_and(|(lo, hi)| i >= lo && i <= hi) {
                line = line.style(Style::new().bg(Color::DarkGray));
            }
            ListItem::new(line)
        })
        .collect();

    let mut title = format!("Diff: {}", app.diff.title);
    if let Some((lo, hi)) = sel {
        title.push_str(&format!("  [{} highlighted]", hi - lo + 1));
    }

    let list = List::new(items)
        .block(panel(&title, focused))
        .highlight_style(Style::new().add_modifier(Modifier::REVERSED));

    app.diff.viewport = area.height.saturating_sub(2);
    frame.render_stateful_widget(list, area, &mut app.diff.state);
}

fn render_status_bar(frame: &mut Frame, app: &App, area: Rect) {
    let hdr = |name: &str, color: Color| {
        Span::styled(
            format!(" {name} "),
            Style::new()
                .fg(Color::Black)
                .bg(color)
                .add_modifier(Modifier::BOLD),
        )
    };
    let keys = |s: &str| Span::styled(s.to_string(), Style::new().fg(Color::Gray));
    let gap = || Span::raw("   ");

    // Line 1: transient feedback (last action / error).
    let feedback = Line::from(Span::styled(
        format!(" {} ", app.status_line),
        Style::new().fg(Color::Black).bg(Color::Cyan),
    ));

    // Line 2: movement.
    let general = Line::from(vec![
        hdr("Panes", Color::Cyan),
        keys(" ⇧H/J/K/L move · Tab cycle · j/k select · Enter diff · q/^C quit · ? help"),
    ]);

    // Line 3: actions.
    let actions = Line::from(vec![
        hdr("Diff", Color::Blue),
        keys(" ⇧Space highlight · s/u stage/unstage lines"),
        gap(),
        hdr("Files", Color::Green),
        keys(" s/u/a stage · c commit"),
        gap(),
        hdr("Sync", Color::Magenta),
        keys(" p/P/f"),
    ]);

    frame.render_widget(Paragraph::new(vec![feedback, general, actions]), area);
}

// ---------------------------------------------------------------------------
// Popups
// ---------------------------------------------------------------------------

fn render_input(frame: &mut Frame, app: &App, area: Rect) {
    let Mode::Input { editor } = &app.mode else {
        return;
    };
    let staged = app.status.iter().filter(|e| e.section == Section::Staged).count();

    // The popup grows with the message (subject + body), within reason.
    let body_h = (editor.lines.len() as u16).clamp(1, area.height.saturating_sub(6).max(1));
    let rect = centered_h(area, 70, body_h + 2);
    let inner_w = rect.width.saturating_sub(2).max(1);
    let inner_h = rect.height.saturating_sub(2).max(1);

    // Scroll so the cursor always stays in view — long lines shift the text
    // left instead of letting typing vanish past the border.
    let cx = editor.display_col().min(u16::MAX as usize) as u16;
    let cy = editor.row.min(u16::MAX as usize) as u16;
    let scroll_x = cx.saturating_sub(inner_w - 1);
    let scroll_y = cy.saturating_sub(inner_h - 1);

    frame.render_widget(Clear, rect);
    let text: Vec<Line> = editor.lines.iter().map(|l| Line::raw(l.clone())).collect();
    let para = Paragraph::new(text)
        .scroll((scroll_y, scroll_x))
        .block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(Style::new().fg(Color::Yellow))
                .title(format!(" Commit message — {staged} file(s) staged "))
                .title_bottom(" Enter commit · Ctrl+N newline · Esc cancel "),
        );
    frame.render_widget(para, rect);

    // The real terminal cursor sits at the edit point, blinking like any
    // normal input field.
    frame.set_cursor_position((rect.x + 1 + cx - scroll_x, rect.y + 1 + cy - scroll_y));
}

fn render_message(frame: &mut Frame, app: &App, area: Rect) {
    let Mode::Message(text) = &app.mode else {
        return;
    };
    let line_count = text.lines().count().max(1) as u16;
    let height = (line_count + 2).min(area.height.saturating_sub(2)).max(3);
    let rect = centered_h(area, 80, height);
    frame.render_widget(Clear, rect);
    let para = Paragraph::new(text.clone())
        .wrap(Wrap { trim: false })
        .block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(Style::new().fg(Color::Yellow))
                .title(" Message (press any key) "),
        );
    frame.render_widget(para, rect);
}

// ---------------------------------------------------------------------------
// Geometry helpers
// ---------------------------------------------------------------------------

/// Centered horizontally by percentage, with a fixed height in rows.
fn centered_h(area: Rect, pct_w: u16, height: u16) -> Rect {
    let v = Layout::vertical([
        Constraint::Min(0),
        Constraint::Length(height),
        Constraint::Min(0),
    ])
    .split(area);
    Layout::horizontal([
        Constraint::Percentage((100 - pct_w) / 2),
        Constraint::Percentage(pct_w),
        Constraint::Percentage((100 - pct_w) / 2),
    ])
    .split(v[1])[1]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::{App, Editor};
    use ratatui::{backend::TestBackend, Terminal};

    /// Render every mode and focus into a headless backend across a range of
    /// sizes; the point is that layout math and popup sizing never panic.
    #[test]
    fn renders_all_modes_without_panicking() {
        let mut app = App::load().expect("open repo");

        let make_modes = || {
            // A long line (forces horizontal scroll) plus a multi-line body
            // (forces vertical growth), cursor mid-text.
            let editor = Editor {
                lines: vec![
                    "a commit subject long enough to scroll past a narrow popup border".into(),
                    String::new(),
                    "body line".into(),
                ],
                row: 2,
                col: 4,
            };
            vec![
                Mode::Normal,
                Mode::Input { editor },
                Mode::Message("line1\nline2\nline3".to_string()),
            ]
        };

        for (w, h) in [(80u16, 24u16), (20, 8), (200, 60)] {
            let mut terminal = Terminal::new(TestBackend::new(w, h)).unwrap();
            for focus in [Focus::Status, Focus::Log, Focus::Diff] {
                app.set_focus(focus);
                // Exercise the highlight path too.
                app.diff.anchor = Some(0);
                app.diff.jump(true);
                for mode in make_modes() {
                    app.mode = mode;
                    terminal.draw(|f| ui(f, &mut app)).expect("draw");
                }
                app.diff.anchor = None;
            }
        }
    }
}
