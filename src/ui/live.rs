//! Visible pending messages and cooperative steering for the terminal frontend.
//! Only this reader owns input during a turn; approvals temporarily suspend it.
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
use std::time::{Duration, Instant};

thread_local! {
    static ACTIVE: RefCell<Option<Arc<Mutex<Screen>>>> = const { RefCell::new(None) };
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Delivery {
    Queued,
    Steering,
    Delivering,
}
struct Pending {
    text: String,
    delivery: Delivery,
}

#[derive(Default)]
pub(super) struct Typing {
    draft: input::Draft,
    history: Vec<String>,
    pending: VecDeque<Pending>,
    delivered: VecDeque<String>,
    selected: Option<usize>,
    held: bool,
    ready: Option<String>,
}

impl Typing {
    pub(super) fn interrupted(&self) -> bool {
        self.held
    }

    fn delivered(&mut self, text: String) {
        self.delivered.push_back(text);
        while self.delivered.len() > 3 {
            self.delivered.pop_front();
        }
        self.selected = None;
    }

    fn send_selected(&mut self) {
        if let Some(index) = self.selected.take() {
            if self.pending[index].delivery == Delivery::Delivering {
                return;
            }
            let message = self.pending.remove(index).unwrap();
            self.ready = Some(message.text.clone());
            self.delivered(message.text);
            self.held &= !self.pending.is_empty();
        }
    }

    fn submit(&mut self, line: String, steer: bool, working: bool) {
        input::remember(&mut self.history, &line);
        self.draft.history_end(&self.history);
        self.selected = None;
        if !working {
            if !line.trim().is_empty() || line.is_empty() {
                self.held &= !self.pending.is_empty();
            }
            self.ready = Some(line);
        } else if !line.trim().is_empty() || line.is_empty() {
            // Slash commands belong to the CLI, never to engine-level steering.
            let delivery = if steer && !line.trim_start().starts_with('/') {
                Delivery::Steering
            } else {
                Delivery::Queued
            };
            self.pending.push_back(Pending {
                text: line,
                delivery,
            });
        }
    }

    fn key(&mut self, byte: u8, working: bool) {
        if byte == b'\t' {
            if let Some(index) = self.selected.take() {
                if !working {
                    self.selected = Some(index);
                    self.send_selected();
                } else if self.pending[index].delivery == Delivery::Queued
                    && !self.pending[index].text.trim_start().starts_with('/')
                {
                    self.pending[index].delivery = Delivery::Steering;
                }
            } else if !self.draft.editor.line.trim().is_empty() {
                let line = self.draft.key(b'\r', &self.history).unwrap();
                self.submit(line, true, working);
            }
            return;
        }
        if self.selected.is_some() {
            match byte {
                b'\r' | b'\n' => {
                    if !working {
                        self.send_selected();
                    }
                }
                b'e' => {
                    let index = self.selected.take().unwrap();
                    if self.pending[index].delivery != Delivery::Delivering {
                        let message = self.pending.remove(index).unwrap();
                        for byte in message.text.trim_end_matches('\n').bytes() {
                            self.draft.key(byte, &self.history);
                        }
                    }
                }
                _ => {
                    self.selected = None;
                    self.key(byte, working);
                }
            }
            return;
        }
        if let Some(line) = self.draft.key(byte, &self.history) {
            self.submit(line, false, working);
        }
    }

    fn sequence(&mut self, sequence: &[u8]) {
        if self.draft.editor.line.is_empty() && !self.pending.is_empty() {
            match sequence {
                b"[A" | b"OA" => {
                    self.selected = Some(
                        self.selected
                            .map_or(self.pending.len() - 1, |i| i.saturating_sub(1)),
                    );
                    return;
                }
                b"[B" | b"OB" => {
                    self.selected = self
                        .selected
                        .and_then(|i| (i + 1 < self.pending.len()).then_some(i + 1));
                    return;
                }
                b"[3~" if self.selected.is_some() => {
                    let index = self.selected.take().unwrap();
                    if self.pending[index].delivery != Delivery::Delivering {
                        self.pending.remove(index);
                    }
                    return;
                }
                _ => {}
            }
        }
        self.draft.key(27, &self.history);
        for byte in sequence {
            self.draft.key(*byte, &self.history);
        }
    }

    pub(super) fn read_line(&mut self) -> Result<String> {
        if !self.held {
            if let Some(message) = self.pending.pop_front() {
                eprint!("> {}", super::display(&message.text));
                self.delivered(message.text.clone());
                return Ok(message.text);
            }
        }
        if self.held && Self::supported() {
            let busy = self.start(false)?;
            loop {
                let ready = busy.screen.lock().unwrap().typing.ready.take();
                if ready.is_some() || INTERRUPTED.load(Ordering::Relaxed) {
                    busy.finish(self)?;
                    crate::check_interrupt()?;
                    return Ok(ready.unwrap());
                }
                if busy.screen.lock().unwrap().error.is_some() {
                    busy.finish(self)?;
                    unreachable!();
                }
                thread::sleep(Duration::from_millis(16));
            }
        }
        let result = input::read_line_with_draft("> ", &mut self.history, &mut self.draft);
        if result.is_err() {
            self.draft = input::Draft::default();
            self.pending.clear();
        } else if result.as_ref().is_ok_and(|line| !line.trim().is_empty()) {
            self.held = false;
        }
        result
    }

    fn supported() -> bool {
        io::stdout().is_terminal() && io::stderr().is_terminal() && input::dimensions().1 >= 5
    }

    pub(super) fn busy(&mut self) -> Result<Option<Busy>> {
        if !Self::supported() {
            return Ok(None);
        }
        self.start(true).map(Some)
    }

    fn start(&mut self, working: bool) -> Result<Busy> {
        let terminal = input::Terminal::enter()?;
        self.draft.history_end(&self.history);
        let mut screen = Screen {
            terminal,
            typing: std::mem::take(self),
            size: input::dimensions(),
            paused: false,
            error: None,
            accepting: true,
            working,
            escape: Vec::new(),
            escape_at: Instant::now(),
            escaped: false,
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
                        if let Err(error) = screen.tick() {
                            screen.error = Some(error);
                            break;
                        }
                    }
                }
                thread::sleep(Duration::from_millis(16));
            }
        });
        Ok(Busy {
            screen,
            stop,
            reader: Some(reader),
        })
    }

    fn acknowledge(&mut self, count: usize) {
        for _ in 0..count {
            if let Some(index) = self
                .pending
                .iter()
                .position(|m| m.delivery == Delivery::Delivering)
            {
                let message = self.pending.remove(index).unwrap();
                self.delivered(message.text);
            }
        }
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
        // Resolve an Esc that arrived just before the turn finished. Incomplete
        // cursor sequences are returned to the ordinary draft parser instead.
        if screen.escape == [27] {
            screen.interrupt();
        } else if !screen.escape.is_empty() {
            let sequence = std::mem::take(&mut screen.escape);
            screen.typing.sequence(&sequence[1..]);
        }
        screen.leave()?;
        *typing = std::mem::take(&mut screen.typing);
        if INTERRUPTED.load(Ordering::Relaxed) && !screen.escaped {
            typing.draft = input::Draft::default();
            typing.pending.clear();
            typing.selected = None;
            typing.held = false;
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
    working: bool,
    escape: Vec<u8>,
    escape_at: Instant,
    escaped: bool,
}

impl Screen {
    fn reserved(&self) -> usize {
        self.size.1.saturating_sub(5).clamp(3, 9)
    }
    fn height(&self) -> usize {
        self.size.1.saturating_sub(self.reserved()).max(1)
    }

    fn enter(&mut self, resized: bool) -> io::Result<()> {
        let mut out = io::stderr().lock();
        if self.size.1 < 5 {
            write!(out, "\x1b[r\x1b[{};1H\x1b7", self.size.1)?;
        } else if resized {
            write!(
                out,
                "\x1b[1;{}r\x1b[{};1H\x1b7",
                self.height(),
                self.height()
            )?;
        } else {
            write!(
                out,
                "\x1b7\x1b[{}S\x1b[1;{}r\x1b8\x1b[{}A\x1b7",
                self.reserved(),
                self.height(),
                self.reserved()
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

    fn interrupt(&mut self) {
        self.escape.clear();
        if self.working {
            self.escaped = true;
            self.typing.held = true;
            for message in &mut self.typing.pending {
                if message.delivery == Delivery::Steering {
                    message.delivery = Delivery::Queued;
                }
            }
            self.typing.selected = None;
            self.accepting = false;
            INTERRUPTED.store(true, Ordering::Relaxed);
        } else {
            self.typing.selected = None;
        }
    }

    fn drain(&mut self) -> io::Result<bool> {
        if !self.accepting {
            return Ok(false);
        }
        let mut dirty = false;
        for _ in 0..4096 {
            let Some(byte) = self.terminal.try_read_byte()? else {
                break;
            };
            dirty = true;
            if !self.escape.is_empty() {
                self.escape.push(byte);
                if self.escape.len() == 2 {
                    if matches!(byte, b'[' | b'O') {
                        continue;
                    }
                    self.interrupt();
                } else if (0x40..=0x7e).contains(&byte) || self.escape.len() > 32 {
                    let sequence = std::mem::take(&mut self.escape);
                    self.typing.sequence(&sequence[1..]);
                }
            } else if byte == 27 {
                self.escape.push(byte);
                self.escape_at = Instant::now();
            } else {
                self.typing.key(byte, self.working);
                if self.typing.ready.is_some()
                    || self
                        .typing
                        .pending
                        .back()
                        .is_some_and(|m| m.text.is_empty())
                {
                    self.accepting = false;
                }
            }
            if !self.accepting {
                break;
            }
        }
        if self.escape == [27] && self.escape_at.elapsed() >= Duration::from_millis(60) {
            self.interrupt();
            dirty = true;
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
        let label = if self.typing.selected.is_some() {
            Some("Tab: steer | e: edit | Del: remove")
        } else if !self.working {
            Some("Paused | ↑: select pending | Enter/Tab: send")
        } else {
            Some("Enter: queue · Tab: steer · Esc: interrupt")
        };
        let footer_rows = if label.is_none() {
            0
        } else if self.reserved() >= 6 {
            2
        } else {
            1
        };
        let editor_slots = if self.reserved() >= 6 { 2 } else { 1 };
        let list_slots = self.reserved() - footer_rows - editor_slots;
        write!(out, "\x1b[{top};1H\x1b[J")?;
        if !self.working || !self.typing.pending.is_empty() || !self.typing.delivered.is_empty() {
            let list = self.typing.list_rows(width, list_slots);
            for (index, row) in list.iter().enumerate() {
                write!(out, "\x1b[{};1H{row}", top + index)?;
            }
        }
        if let Some(label) = label {
            for (index, row) in input::wrapped_rows(label, width)
                .iter()
                .take(footer_rows)
                .enumerate()
            {
                write!(out, "\x1b[{};1H{row}", top + list_slots + index)?;
            }
        }
        let editor_top = top + list_slots + footer_rows;
        let text = format!("> {}", self.typing.draft.editor.line);
        let rows = input::wrapped_rows(&text, width);
        let (cursor_row, column) =
            input::wrapped_cursor(&text, 2 + self.typing.draft.editor.cursor, width);
        let start = cursor_row.saturating_sub(editor_slots - 1);
        let end = rows.len().min(start + editor_slots);
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

impl Typing {
    fn list_rows(&self, width: usize, slots: usize) -> Vec<String> {
        let mut rows = Vec::new();
        let mut selected_row = None;
        for (index, message) in self.pending.iter().enumerate() {
            let selected = self.selected == Some(index);
            if selected {
                selected_row = Some(rows.len());
            }
            let status = if message.delivery == Delivery::Queued {
                "Queued"
            } else {
                "Steering pending"
            };
            let safe = super::display(message.text.trim_end()).replace(['\n', '\t'], " ");
            let text = format!("{} {}: {safe}", if selected { "▶" } else { "·" }, status);
            let wrapped = input::wrapped_rows(&text, width);
            let limit = if selected { slots } else { 2.min(slots) };
            let truncated = wrapped.len() > limit;
            rows.extend(wrapped.into_iter().take(limit));
            if truncated {
                if let Some(last) = rows.last_mut() {
                    use unicode_segmentation::UnicodeSegmentation;
                    use unicode_width::UnicodeWidthStr;
                    while last.width() >= width {
                        let end = last.grapheme_indices(true).next_back().unwrap().0;
                        last.truncate(end);
                    }
                    last.push('…');
                }
            }
        }
        let pending_rows = rows.len();
        for text in self.delivered.iter().rev() {
            let safe = super::display(text.trim_end()).replace(['\n', '\t'], " ");
            rows.push(input::wrapped_rows(&format!("✓ Delivered: {safe}"), width).remove(0));
        }
        if rows.is_empty() {
            rows.push("No pending messages".chars().take(width).collect());
        }
        let start = selected_row.map_or_else(
            || pending_rows.saturating_sub(slots),
            |row| row.min(rows.len().saturating_sub(slots)),
        );
        rows.into_iter().skip(start).take(slots).collect()
    }
}

pub(super) fn steering() -> Result<Vec<String>> {
    ACTIVE.with(|active| {
        let Some(screen) = active.borrow().clone() else {
            return Ok(Vec::new());
        };
        let mut screen = screen.lock().unwrap();
        if screen.paused {
            return Ok(Vec::new());
        }
        screen.drain()?;
        crate::check_interrupt()?;
        let mut messages = Vec::new();
        for message in &mut screen.typing.pending {
            if message.delivery != Delivery::Queued {
                message.delivery = Delivery::Delivering;
                messages.push(message.text.clone());
            }
        }
        Ok(messages)
    })
}

pub(super) fn steering_delivered(count: usize) {
    ACTIVE.with(|active| {
        if let Some(screen) = active.borrow().as_ref() {
            let mut screen = screen.lock().unwrap();
            screen.typing.acknowledge(count);
            if let Err(error) = screen.paint(&mut io::stderr().lock()) {
                screen.error = Some(error);
            }
        }
    });
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

/// Approvals use a fresh editor, not pending messages or the live draft.
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

#[cfg(test)]
mod tests {
    use super::*;

    fn type_text(typing: &mut Typing, text: &str, working: bool) {
        for byte in text.bytes() {
            typing.key(byte, working);
        }
    }

    #[test]
    fn queued_messages_stay_visible_and_promote_without_duplicate_submission() {
        let mut typing = Typing::default();
        type_text(&mut typing, "first\nsecond\n", true);
        let rows = typing.list_rows(80, 5).join("\n");
        assert!(rows.contains("Queued: first"));
        assert!(rows.contains("Queued: second"));
        typing.sequence(b"[A");
        typing.key(b'\t', true);
        assert!(typing.pending[1].delivery == Delivery::Steering);
        assert_eq!(typing.pending.len(), 2);
        assert!(
            typing
                .list_rows(80, 5)
                .join("\n")
                .contains("Steering pending: second")
        );
        type_text(&mut typing, "third\t", true);
        assert_eq!(typing.pending[2].text, "third\n");
        assert!(typing.pending[2].delivery == Delivery::Steering);
    }

    #[test]
    fn acknowledgement_does_not_consume_a_message_promoted_during_checkpoint() {
        let mut typing = Typing::default();
        type_text(&mut typing, "older\naccepted\t", true);
        typing.pending[1].delivery = Delivery::Delivering;
        typing.sequence(b"[A");
        typing.sequence(b"[A");
        typing.key(b'\t', true);
        typing.acknowledge(1);
        assert_eq!(typing.pending.len(), 1);
        assert_eq!(typing.pending[0].text, "older\n");
        assert!(typing.pending[0].delivery == Delivery::Steering);
        assert_eq!(typing.delivered[0], "accepted\n");
    }

    #[test]
    fn paused_selection_edits_removes_or_explicitly_sends_and_commands_stay_queued() {
        let mut typing = Typing::default();
        type_text(&mut typing, "first\nsecond\n/quit\t", true);
        assert!(typing.pending[2].delivery == Delivery::Queued);
        typing.held = true;
        typing.sequence(b"[A");
        typing.sequence(b"[3~");
        assert_eq!(typing.pending.len(), 2);
        typing.sequence(b"[A");
        typing.key(b'e', false);
        assert_eq!(typing.draft.editor.line, "second");
        type_text(&mut typing, " edited\n", false);
        assert_eq!(typing.ready.take().unwrap(), "second edited\n");
        assert!(typing.held);
        typing.held = true;
        typing.sequence(b"[A");
        typing.key(b'\t', false);
        assert_eq!(typing.ready.unwrap(), "first\n");
        assert!(typing.pending.is_empty());
    }

    #[test]
    fn pending_rows_fit_tiny_widths_without_splitting_unicode() {
        use unicode_width::UnicodeWidthStr;
        let mut typing = Typing::default();
        type_text(&mut typing, "界e\u{301}🙂abcdef\n", true);
        for width in 1..20 {
            let rows = typing.list_rows(width, 1);
            assert_eq!(rows.len(), 1);
            assert!(rows[0].width() <= width, "{rows:?}");
        }
    }
}
