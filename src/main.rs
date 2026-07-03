//! git-viz — an interactive terminal git client.
//!
//! `main.rs` owns only the terminal lifecycle and the draw/input loop; the real
//! work lives in the modules:
//!   - `git`   — all git2 access, returning owned data
//!   - `app`   — application state + the focus/mode state machine
//!   - `event` — key dispatch and operation wiring
//!   - `ui`    — rendering

mod app;
mod event;
mod git;
mod ui;

use std::error::Error;

use crossterm::event::{read, Event};

use app::App;

fn main() -> Result<(), Box<dyn Error>> {
    let mut app = App::load()?;

    let mut terminal = ratatui::init();
    let result = run(&mut terminal, &mut app);
    ratatui::restore();
    result
}

fn run(terminal: &mut ratatui::DefaultTerminal, app: &mut App) -> Result<(), Box<dyn Error>> {
    while !app.should_quit {
        terminal.draw(|frame| ui::ui(frame, app))?;

        if let Event::Key(key) = read()? {
            event::handle_key(app, key);
        }
    }
    Ok(())
}
