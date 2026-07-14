//! crossterm 生命周期边界；RAII 确保任何错误路径都恢复终端。

use std::{io, io::Write, time::Duration};

use anyhow::{Context, Result};
use crossterm::{
    cursor::{Hide, Show},
    event::{self, DisableMouseCapture, EnableMouseCapture, Event},
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use ratatui::{Terminal, backend::CrosstermBackend};

use crate::{
    state::ConsoleState,
    ui::{UiState, handle_key, render},
};

const INPUT_POLL_INTERVAL: Duration = Duration::from_millis(50);

pub(crate) trait TerminalOps {
    fn enable_raw(&mut self) -> io::Result<()>;
    fn enter_alternate(&mut self) -> io::Result<()>;
    fn enable_mouse(&mut self) -> io::Result<()>;
    fn hide_cursor(&mut self) -> io::Result<()>;
    fn show_cursor(&mut self) -> io::Result<()>;
    fn disable_mouse(&mut self) -> io::Result<()>;
    fn leave_alternate(&mut self) -> io::Result<()>;
    fn disable_raw(&mut self) -> io::Result<()>;
}

struct CrosstermOps;

impl TerminalOps for CrosstermOps {
    fn enable_raw(&mut self) -> io::Result<()> {
        enable_raw_mode()
    }

    fn enter_alternate(&mut self) -> io::Result<()> {
        execute!(io::stdout(), EnterAlternateScreen)
    }

    fn enable_mouse(&mut self) -> io::Result<()> {
        execute!(io::stdout(), EnableMouseCapture)
    }

    fn hide_cursor(&mut self) -> io::Result<()> {
        execute!(io::stdout(), Hide)
    }

    fn show_cursor(&mut self) -> io::Result<()> {
        execute!(io::stdout(), Show)
    }

    fn disable_mouse(&mut self) -> io::Result<()> {
        execute!(io::stdout(), DisableMouseCapture)
    }

    fn leave_alternate(&mut self) -> io::Result<()> {
        execute!(io::stdout(), LeaveAlternateScreen)
    }

    fn disable_raw(&mut self) -> io::Result<()> {
        disable_raw_mode()
    }
}

pub(crate) struct TerminalRestore<O: TerminalOps> {
    ops: O,
    raw: bool,
    alternate: bool,
    mouse: bool,
    hidden: bool,
}

impl TerminalRestore<CrosstermOps> {
    fn enter() -> Result<Self> {
        Self::enter_with(CrosstermOps)
    }
}

impl<O: TerminalOps> TerminalRestore<O> {
    pub(crate) fn enter_with(ops: O) -> Result<Self> {
        let mut guard = Self {
            ops,
            raw: false,
            alternate: false,
            mouse: false,
            hidden: false,
        };
        guard
            .ops
            .enable_raw()
            .context("failed to enable terminal raw mode")?;
        guard.raw = true;

        // transition 可能在写出控制序列后报错；先标记才能保证 Drop 仍执行逆操作。
        guard.alternate = true;
        guard
            .ops
            .enter_alternate()
            .context("failed to enter alternate terminal screen")?;
        guard.mouse = true;
        guard
            .ops
            .enable_mouse()
            .context("failed to enable terminal mouse capture")?;
        guard.hidden = true;
        guard
            .ops
            .hide_cursor()
            .context("failed to hide terminal cursor")?;
        Ok(guard)
    }
}

impl<O: TerminalOps> Drop for TerminalRestore<O> {
    fn drop(&mut self) {
        if self.hidden {
            let _ = self.ops.show_cursor();
        }
        if self.mouse {
            let _ = self.ops.disable_mouse();
        }
        if self.alternate {
            let _ = self.ops.leave_alternate();
        }
        if self.raw {
            let _ = self.ops.disable_raw();
        }
    }
}

pub(crate) fn run_interactive(mut state: ConsoleState) -> Result<()> {
    let _restore = TerminalRestore::enter()?;
    let backend = CrosstermBackend::new(io::stdout());
    let mut terminal = Terminal::new(backend).context("failed to initialize terminal backend")?;
    terminal.clear().context("failed to clear terminal")?;
    let mut ui = UiState::default();

    let result = loop {
        state.poll_controller_events();
        state.close_expired_streams(std::time::Instant::now());
        if let Err(error) = terminal.draw(|frame| render(frame, &state, &ui)) {
            break Err(anyhow::Error::new(error).context("terminal draw failed"));
        }
        match event::poll(INPUT_POLL_INTERVAL) {
            Ok(true) => match event::read() {
                Ok(Event::Key(key)) if handle_key(key, &mut state, &mut ui) => break Ok(()),
                Ok(_) => {}
                Err(error) => break Err(anyhow::Error::new(error).context("terminal input failed")),
            },
            Ok(false) => {}
            Err(error) => break Err(anyhow::Error::new(error).context("terminal poll failed")),
        }
    };
    state.shutdown();
    result
}

/// fatal error 在 RAII restore 完成后写到普通 stderr，便于 CI/support 留存。
pub fn write_fatal_error(
    writer: &mut impl Write,
    error: &(dyn std::error::Error + 'static),
) -> io::Result<()> {
    writeln!(writer, "Camera Toolbox TUI error: {error}")
}
