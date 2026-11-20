//! Visible typeahead while the synchronous engine works. Only this reader owns
//! terminal input during a turn; approval temporarily hands it back to the editor.
use super::{input, styles_enabled};
use crate::{INTERRUPTED, Result};
use std::cell::RefCell;
use std::collections::VecDeque;
use std::io::{self, IsTerminal, Write};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
};
use std::thread::{self, JoinHandle};
use std::time::Duration;

thread_local! {
    static ACTIVE: RefCell<Option<Arc<Mutex<Screen>>>> = const { RefCell::new(None) };
}

#[derive(Default)]
pub(super) struct Typing {
    draft: input::Draft,
    history: Vec<String>,
    queued: VecDeque<String>,
}

impl Typing {
    pub(super) fn read_line(&mut self) -> Result<String> {
        if let Some(line) = self.queued.pop_front() {
            eprint!("> {}", super::display(&line));
            return Ok(line);
        }
        let result = input::read_line_with_draft("> ", &mut self.history, &mut self.draft);
        if result.is_err() {
            self.draft = input::Draft::default();
        }
        result
    }

    pub(super) fn busy(&mut self) -> Result<Option<Busy>> {
        // Do not send screen-control sequences into redirected output, or use a
        // scroll region on a terminal too small to leave room for model output.
        if !io::stdout().is_terminal() || !io::stderr().is_terminal() || input::dimensions().1 < 5 {
            return Ok(None);
        }
        let terminal = input::Terminal::enter()?;
        self.draft.history_end(&self.history);
        let mut screen = Screen {
            terminal,
            typing: std::mem::take(self),
            size: input::dimensions(),
            paused: false,
            error: None,
            accepting: true,
        };
        screen.enter(false)?;
        let screen = Arc::new(Mutex::new(screen));
        ACTIVE.with(|active| *active.borrow_mut() = Some(screen.clone()));
        let stop = Arc::new(AtomicBool::new(false));
        let reader_screen = screen.clone();
        let reader_stop = stop.clone();
        let reader = thread::spawn(move || {
            while !reader_stop.load(Ordering::Relaxed) {
                {
                    let mut screen = reader_screen.lock().unwrap();
                    if !screen.paused {
                        let result = screen.tick();
                        if let Err(error) = result {
                            screen.error = Some(error);
                            break;
                        }
                    }
                }
                thread::sleep(Duration::from_millis(16));
            }
        });
        Ok(Some(Busy {
            screen,
            stop,
            reader: Some(reader),
        }))
    }
}

pub(super) struct Busy {
    screen: Arc<Mutex<Screen>>,
    stop: Arc<AtomicBool>,
    reader: Option<JoinHandle<()>>,
}

impl Busy {
    fn stop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(reader) = self.reader.take() {
            let _ = reader.join();
        }
        ACTIVE.with(|active| *active.borrow_mut() = None);
    }

    pub(super) fn finish(mut self, typing: &mut Typing) -> Result<()> {
        self.stop();
        let mut screen = self.screen.lock().unwrap();
        screen.leave()?;
        *typing = std::mem::take(&mut screen.typing);
        if INTERRUPTED.load(Ordering::Relaxed) {
            // Ctrl-C must not accidentally submit a partially typed follow-up.
            typing.draft = input::Draft::default();
            typing.queued.clear();
        }
        if let Some(error) = screen.error.take() {
            return Err(error.into());
        }
        Ok(())
    }
}

impl Drop for Busy {
    fn drop(&mut self) {
        self.stop();
        let mut screen = self.screen.lock().unwrap();
        if !screen.paused {
            let _ = screen.leave();
        }
    }
}

struct Screen {
    terminal: input::Terminal,
    typing: Typing,
    size: (usize, usize),
    paused: bool,
    error: Option<io::Error>,
    accepting: bool,
}

impl Screen {
    fn height(&self) -> usize {
        // Keep at least one output row even after a drastic resize.
        self.size.1.saturating_sub(3).max(1)
    }

    fn enter(&mut self, resized: bool) -> io::Result<()> {
        let mut out = io::stderr().lock();
        if self.size.1 < 5 {
            // A one-row scroll region is invalid on VT terminals. Keep collecting
            // typeahead invisibly until there is room to restore the live area.
            write!(out, "\x1b[r\x1b[{};1H\x1b7", self.size.1)?;
        } else if resized {
            // The old saved cursor may now be outside the new scroll region.
            write!(
                out,
                "\x1b[1;{}r\x1b[{};1H\x1b7",
                self.height(),
                self.height()
            )?;
        } else {
            // Shift existing scrollback up to make room, preserving the logical
            // output cursor. ESC 7/8 are private to this compositor while active.
            write!(
                out,
                "\x1b7\x1b[3S\x1b[1;{}r\x1b8\x1b[3A\x1b7",
                self.height()
            )?;
        }
        self.paused = false;
        self.paint(&mut out)
    }

    fn leave(&mut self) -> io::Result<()> {
        let mut out = io::stderr().lock();
        if self.size.1 >= 5 {
            write!(out, "\x1b[{};1H\x1b[J", self.height() + 1)?;
        }
        write!(out, "\x1b[r\x1b8")?;
        self.paused = true;
        out.flush()
    }

    fn drain(&mut self) -> io::Result<bool> {
        if !self.accepting {
            return Ok(false);
        }
        let mut dirty = false;
        // Bound each pass so a large paste cannot starve model output.
        for _ in 0..4096 {
            let Some(byte) = self.terminal.try_read_byte()? else {
                break;
            };
            dirty = true;
            if let Some(line) = self.typing.draft.key(byte, &self.typing.history) {
                if line.trim().is_empty() && !line.is_empty() {
                    continue;
                }
                input::remember(&mut self.typing.history, &line);
                self.typing.draft.history_end(&self.typing.history);
                self.accepting = !line.is_empty();
                self.typing.queued.push_back(line);
                if !self.accepting {
                    break;
                }
            }
        }
        Ok(dirty)
    }

    fn tick(&mut self) -> io::Result<()> {
        let size = input::dimensions();
        if size != self.size {
            self.size = size;
            self.enter(true)?;
        }
        if !INTERRUPTED.load(Ordering::Relaxed) && self.drain()? {
            self.paint(&mut io::stderr().lock())?;
        }
        Ok(())
    }

    fn paint(&self, out: &mut impl Write) -> io::Result<()> {
        if self.size.1 < 5 {
            return out.flush();
        }
        let top = self.height() + 1;
        let width = self.size.0.saturating_sub(1).max(1);
        // Show the newest queued messages above the draft without changing FIFO order.
        let queue = self.typing.queued.iter().filter(|line| !line.is_empty());
        let visible: Vec<_> = queue.rev().take(2).collect();
        let editor_top = top + visible.len();
        let slots = self.size.1 - editor_top + 1;
        let text = format!("> {}", self.typing.draft.editor.line);
        let rows = input::wrapped_rows(&text, width);
        let (cursor_row, column) =
            input::wrapped_cursor(&text, 2 + self.typing.draft.editor.cursor, width);
        let start = cursor_row.saturating_sub(slots - 1);
        let end = rows.len().min(start + slots);
        write!(out, "\x1b[{top};1H\x1b[J")?;
        for (index, line) in visible.into_iter().rev().enumerate() {
            let safe = super::display(line.trim_end_matches('\n')).replace('\t', " ");
            let preview = input::wrapped_rows(&format!("Queued: {safe}"), width).remove(0);
            write!(out, "\x1b[{};1H{preview}", top + index)?;
        }
        for (index, row) in rows[start..end].iter().enumerate() {
            write!(
                out,
                "\x1b[{};1H{}",
                editor_top + index,
                super::styled(row, super::Style::Input, styles_enabled(true))
            )?;
        }
        write!(
            out,
            "\x1b[{};{}H",
            editor_top + cursor_row - start,
            column + 1
        )?;
        out.flush()
    }

    fn output(&self, text: &str, stderr: bool) -> io::Result<()> {
        let mut terminal = io::stderr().lock();
        terminal.write_all(b"\x1b8")?;
        terminal.flush()?;
        if stderr {
            terminal.write_all(text.as_bytes())?;
        } else {
            let mut stdout = io::stdout().lock();
            stdout.write_all(text.as_bytes())?;
            stdout.flush()?;
        }
        terminal.write_all(b"\x1b7")?;
        self.paint(&mut terminal)
    }
}

pub(super) fn output(text: &str, stderr: bool) -> io::Result<()> {
    ACTIVE.with(|active| {
        if let Some(screen) = active.borrow().as_ref() {
            let screen = screen.lock().unwrap();
            if !screen.paused {
                return screen.output(text, stderr);
            }
        }
        if stderr {
            let mut out = io::stderr().lock();
            out.write_all(text.as_bytes())?;
            out.flush()
        } else {
            let mut out = io::stdout().lock();
            out.write_all(text.as_bytes())?;
            out.flush()
        }
    })
}

/// Approvals use a fresh editor and cannot consume already queued messages or
/// the unfinished live draft. The background reader is paused under its lock.
pub(super) fn suspend<T>(run: impl FnOnce() -> Result<T>) -> Result<T> {
    let screen = ACTIVE.with(|active| active.borrow().clone());
    if let Some(screen) = &screen {
        let mut screen = screen.lock().unwrap();
        while screen.drain()? {}
        screen.leave()?;
    }
    let result = run();
    if let Some(screen) = screen {
        let mut screen = screen.lock().unwrap();
        screen.size = input::dimensions();
        screen.enter(false)?;
    }
    result
}
