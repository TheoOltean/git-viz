//! Rendering. Two panels — the staging area (Changes) on the left and the
//! commit history (Log) on the right — plus a bottom bar and modal popups.

use ratatui::{
    layout::{Constraint, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, List, ListItem, Paragraph, Wrap},
    Frame,
};

use crate::app::{App, Focus, Mode};
use crate::git::Section;

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
    let body = Layout::horizontal([Constraint::Length(34), Constraint::Min(0)]).split(outer[0]);

    render_status(frame, app, body[0]);
    render_log(frame, app, body[1]);
    render_status_bar(frame, app, outer[1]);

    // Popups draw on top of the body.
    match &app.mode {
        Mode::Normal => {}
        Mode::Input { .. } => render_input(frame, app, outer[0]),
        Mode::Diff { .. } => render_diff(frame, app, outer[0]),
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

    // Line 2: general controls.
    let general = Line::from(vec![
        hdr("General", Color::Cyan),
        keys(" Tab switch panel · j/k ↑↓ move · r refresh · ? help · q quit"),
    ]);

    // Line 3: actions. Green ● = staged (committed), yellow ● = unstaged, magenta = origin.
    let actions = Line::from(vec![
        hdr("Stage", Color::Green),
        keys(" s/u stage · a all · c commit · d diff"),
        gap(),
        hdr("Sync", Color::Magenta),
        keys(" p/P/f push·pull·fetch"),
    ]);

    frame.render_widget(Paragraph::new(vec![feedback, general, actions]), area);
}

// ---------------------------------------------------------------------------
// Popups
// ---------------------------------------------------------------------------

fn render_input(frame: &mut Frame, app: &App, area: Rect) {
    let Mode::Input { buffer } = &app.mode else {
        return;
    };
    let staged = app.status.iter().filter(|e| e.section == Section::Staged).count();

    let rect = centered_h(area, 70, 3);
    frame.render_widget(Clear, rect);
    let para = Paragraph::new(buffer.as_str()).block(
        Block::default()
            .borders(Borders::ALL)
            .border_style(Style::new().fg(Color::Yellow))
            .title(format!(
                " Commit message — {staged} file(s) · Enter = commit · Esc = cancel "
            )),
    );
    frame.render_widget(para, rect);

    // Place the real terminal cursor after the typed text so it's clearly
    // visible and blinks like any normal input field.
    let max_x = rect.x + rect.width.saturating_sub(2);
    let cursor_x = (rect.x + 1 + buffer.chars().count() as u16).min(max_x);
    frame.set_cursor_position((cursor_x, rect.y + 1));
}

fn render_diff(frame: &mut Frame, app: &App, area: Rect) {
    let Mode::Diff { title, lines, scroll } = &app.mode else {
        return;
    };
    let rect = centered(area, 90, 80);
    frame.render_widget(Clear, rect);
    let styled: Vec<Line> = lines.iter().map(|l| diff_line(l)).collect();
    let para = Paragraph::new(styled)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(Style::new().fg(Color::Cyan))
                .title(format!(" {title}  (j/k scroll, q close) ")),
        )
        .scroll((*scroll, 0));
    frame.render_widget(para, rect);
}

/// Colorize a diff line by its leading marker.
fn diff_line(l: &str) -> Line<'static> {
    let style = match l.chars().next() {
        Some('+') => Style::new().fg(Color::Green),
        Some('-') => Style::new().fg(Color::Red),
        _ if l.starts_with("@@") => Style::new().fg(Color::Cyan),
        _ if l.starts_with("diff ") || l.starts_with("index ") => {
            Style::new().fg(Color::DarkGray).add_modifier(Modifier::BOLD)
        }
        _ => Style::new(),
    };
    Line::from(Span::styled(l.to_string(), style))
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

/// A rectangle centered in `area`, sized as a percentage of width and height.
fn centered(area: Rect, pct_w: u16, pct_h: u16) -> Rect {
    let v = Layout::vertical([
        Constraint::Percentage((100 - pct_h) / 2),
        Constraint::Percentage(pct_h),
        Constraint::Percentage((100 - pct_h) / 2),
    ])
    .split(area);
    Layout::horizontal([
        Constraint::Percentage((100 - pct_w) / 2),
        Constraint::Percentage(pct_w),
        Constraint::Percentage((100 - pct_w) / 2),
    ])
    .split(v[1])[1]
}

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
    use crate::app::App;
    use ratatui::{backend::TestBackend, Terminal};

    /// Render every mode into a headless backend across a range of sizes; the
    /// point is that layout math and popup sizing never panic.
    #[test]
    fn renders_all_modes_without_panicking() {
        let mut app = App::load().expect("open repo");

        let make_modes = || {
            vec![
                Mode::Normal,
                Mode::Input {
                    buffer: "a commit message".to_string(),
                },
                Mode::Diff {
                    title: "diff".to_string(),
                    lines: vec!["@@ hunk".into(), "+added".into(), "-removed".into()],
                    scroll: 1,
                },
                Mode::Message("line1\nline2\nline3".to_string()),
            ]
        };

        for (w, h) in [(80u16, 24u16), (20, 8), (200, 60)] {
            let mut terminal = Terminal::new(TestBackend::new(w, h)).unwrap();
            for mode in make_modes() {
                app.mode = mode;
                app.focus = if matches!(app.focus, Focus::Log) {
                    Focus::Status
                } else {
                    Focus::Log
                };
                terminal.draw(|f| ui(f, &mut app)).expect("draw");
            }
        }
    }
}
