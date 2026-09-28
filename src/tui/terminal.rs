use std::io::{self, Stdout};

use crossterm::cursor::{Hide, Show};
use crossterm::event::{DisableBracketedPaste, EnableBracketedPaste};
use crossterm::execute;
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;

pub(crate) struct TerminalSession {
    terminal: Terminal<CrosstermBackend<Stdout>>,
    active: bool,
}

impl TerminalSession {
    pub(crate) fn enter() -> io::Result<Self> {
        // Resolved once, before the first frame: the terminal's colour
        // capability cannot change under us, and every surface reads the
        // palette rather than asking the environment again.
        super::theme::set_palette(super::theme::Palette::resolve(
            super::theme::ColorCapability::from_environment(),
            super::theme::ThemeChoice::from_environment(),
        ));
        // Same reasoning for movement: what the user asked for is read once,
        // and from then on every surface asks the policy rather than the
        // environment.
        super::motion::set_motion_setting(super::motion::MotionSetting::from_environment());
        enable_raw_mode()?;
        let mut stdout = io::stdout();
        if let Err(error) = execute!(stdout, EnterAlternateScreen, EnableBracketedPaste, Hide) {
            let _ = disable_raw_mode();
            return Err(error);
        }
        match Terminal::new(CrosstermBackend::new(stdout)) {
            Ok(terminal) => Ok(Self {
                terminal,
                active: true,
            }),
            Err(error) => {
                restore_terminal();
                Err(error)
            }
        }
    }

    pub(crate) fn terminal_mut(&mut self) -> &mut Terminal<CrosstermBackend<Stdout>> {
        &mut self.terminal
    }

    pub(crate) fn restore(&mut self) {
        if !self.active {
            return;
        }
        let _ = self.terminal.show_cursor();
        restore_terminal();
        self.active = false;
    }
}

impl Drop for TerminalSession {
    fn drop(&mut self) {
        self.restore();
    }
}

pub(crate) fn install_panic_restore_hook() {
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |panic| {
        restore_terminal();
        previous(panic);
    }));
}

fn restore_terminal() {
    let _ = disable_raw_mode();
    let _ = execute!(
        io::stdout(),
        Show,
        DisableBracketedPaste,
        LeaveAlternateScreen
    );
}
