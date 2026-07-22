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
use std::io::stdout;

use crossterm::event::{
    read, DisableBracketedPaste, EnableBracketedPaste, Event, KeyboardEnhancementFlags,
    PopKeyboardEnhancementFlags, PushKeyboardEnhancementFlags,
};
use crossterm::execute;

use app::App;

fn main() -> Result<(), Box<dyn Error>> {
    let mut app = App::load()?;

    let mut terminal = ratatui::init();
    // Bracketed paste delivers pasted text as one event, so a multi-line
    // commit message pastes cleanly instead of submitting at the first \r.
    let _ = execute!(stdout(), EnableBracketedPaste);
    // Opt into the kitty keyboard protocol where the terminal supports it so
    // modifier combos like Shift+Space arrive with their modifiers intact.
    // Everywhere else the bindings degrade gracefully (plain Space works too).
    let enhanced = crossterm::terminal::supports_keyboard_enhancement().unwrap_or(false);
    if enhanced {
        let _ = execute!(
            stdout(),
            PushKeyboardEnhancementFlags(KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES)
        );
    }
    let result = run(&mut terminal, &mut app);
    if enhanced {
        let _ = execute!(stdout(), PopKeyboardEnhancementFlags);
    }
    let _ = execute!(stdout(), DisableBracketedPaste);
    ratatui::restore();
    result
}

fn run(terminal: &mut ratatui::DefaultTerminal, app: &mut App) -> Result<(), Box<dyn Error>> {
    while !app.should_quit {
        terminal.draw(|frame| ui::ui(frame, app))?;

        match read()? {
            Event::Key(key) => event::handle_key(app, key),
            Event::Paste(data) => event::handle_paste(app, &data),
            _ => {}
        }
    }
    Ok(())
}
