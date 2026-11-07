//! A small wrapped input editor. Redraw all visible rows explicitly so erase
//! works across line boundaries without relying on canonical terminal editing.
use crate::{Result, check_interrupt, ui};
use std::fs::{File, OpenOptions};
use std::io::{self, IsTerminal, Write};
use std::os::fd::{AsRawFd, RawFd};
use std::os::unix::fs::OpenOptionsExt;
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

pub(super) struct Terminal {
    mode: libc::termios,
    input: File,
}
impl Drop for Terminal {
    fn drop(&mut self) {
        // SAFETY: restore terminal settings before closing our input descriptor,
        // including on cancellation.
        unsafe {
            libc::tcsetattr(self.input.as_raw_fd(), libc::TCSANOW, &self.mode);
        }
    }
}
impl Terminal {
    pub(super) fn enter() -> io::Result<Self> {
        // A separate open description avoids making stdout/stderr nonblocking
        // when a PTY launcher gave all three streams the same underlying handle.
        let input = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NONBLOCK)
            .open("/dev/tty")?;
        let fd = input.as_raw_fd();
        // SAFETY: termios is initialized by tcgetattr before use.
        unsafe {
            let mut mode = std::mem::zeroed();
            if libc::tcgetattr(fd, &mut mode) < 0 {
                return Err(io::Error::last_os_error());
            }
            let restore = Self { mode, input };
            let mut editing = mode;
            // Keep ISIG: Ctrl-C still uses the application's cancellation handler.
            editing.c_lflag &= !(libc::ICANON | libc::ECHO | libc::IEXTEN);
            editing.c_iflag &= !(libc::IXON | libc::ICRNL | libc::INLCR);
            editing.c_cc[libc::VMIN] = 1;
            editing.c_cc[libc::VTIME] = 0;
            if libc::tcsetattr(fd, libc::TCSANOW, &editing) < 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(restore)
        }
    }
}

fn dimensions() -> (usize, usize) {
    // SAFETY: ioctl initializes size on success; stderr is only borrowed.
    unsafe {
        let mut size: libc::winsize = std::mem::zeroed();
        if libc::ioctl(2, libc::TIOCGWINSZ, &mut size) == 0 && size.ws_col > 1 {
            (usize::from(size.ws_col), usize::from(size.ws_row).max(2))
        } else {
            (80, 24)
        }
    }
}

fn wrapped_rows(text: &str, width: usize) -> Vec<String> {
    let mut rows = vec![String::new()];
    let mut used = 0;
    for grapheme in text.graphemes(true) {
        // A wide grapheme cannot fit in a one-column terminal viewport.
        let grapheme = if grapheme.width() > width {
            "�"
        } else {
            grapheme
        };
        let cells = grapheme.width();
        if used + cells > width {
            rows.push(String::new());
            used = 0;
        }
        rows.last_mut().unwrap().push_str(grapheme);
        used += cells;
    }
    rows
}

fn redraw(
    out: &mut impl Write,
    text: &str,
    size: (usize, usize),
    painted_rows: &mut usize,
    styled: bool,
) -> io::Result<()> {
    redraw_cursor(out, text, text.len(), size, painted_rows, styled)
}

fn redraw_cursor(
    out: &mut impl Write,
    text: &str,
    cursor: usize,
    size: (usize, usize),
    painted_rows: &mut usize,
    styled: bool,
) -> io::Result<()> {
    // Reserve the last column to avoid the emulator's pending soft-wrap state.
    let rows = wrapped_rows(text, size.0 - 1);
    let mut remaining = cursor;
    let mut cursor_row = 0;
    for row in &rows {
        if remaining <= row.len() {
            break;
        }
        remaining -= row.len();
        cursor_row += 1;
    }
    let column = rows[cursor_row][..remaining].width();
    let start = cursor_row.saturating_sub(size.1 - 2);
    let visible = &rows[start..rows.len().min(start + size.1 - 1)];
    write!(out, "\r")?;
    let up = painted_rows.saturating_sub(1).min(size.1 - 1);
    if up > 0 {
        write!(out, "\x1b[{up}A")?;
    }
    write!(
        out,
        "\x1b[J{}",
        ui::styled(&visible.join("\r\n"), ui::Style::Input, styled)
    )?;
    let below = visible.len() - 1 - (cursor_row - start);
    if below > 0 {
        write!(out, "\x1b[{below}A")?;
    }
    if cursor != text.len() {
        write!(out, "\r")?;
        if column > 0 {
            write!(out, "\x1b[{column}C")?;
        }
    }
    *painted_rows = cursor_row - start + 1;
    out.flush()
}

fn wait_for_input(fd: RawFd) -> io::Result<bool> {
    if fd < 0 || fd as usize >= libc::FD_SETSIZE {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "terminal descriptor exceeds select capacity",
        ));
    }
    // macOS poll(/dev/tty) returns POLLNVAL immediately, even for a valid
    // controlling terminal. Reading after that returns EAGAIN and spins.
    // select supports this device and actually sleeps until input is ready.
    // Keep a bounded timeout for cancellation checks and terminal resizing.
    // SAFETY: the descriptor fits fd_set; select borrows initialized stack values.
    unsafe {
        let mut read_fds = std::mem::zeroed();
        libc::FD_ZERO(&mut read_fds);
        libc::FD_SET(fd, &mut read_fds);
        let mut timeout = libc::timeval {
            tv_sec: 0,
            tv_usec: 25_000,
        };
        let ready = libc::select(
            fd + 1,
            &mut read_fds,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            &mut timeout,
        );
        if ready < 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(ready > 0)
        }
    }
}

#[derive(Default)]
struct Editor {
    line: String,
    cursor: usize,
    history_index: usize,
    draft: String,
}

impl Editor {
    fn left(&mut self) {
        self.cursor = self.line[..self.cursor]
            .grapheme_indices(true)
            .next_back()
            .map_or(0, |(i, _)| i);
    }

    fn right(&mut self) {
        if let Some(g) = self.line[self.cursor..].graphemes(true).next() {
            self.cursor += g.len();
        }
    }

    fn insert(&mut self, text: &str) {
        self.line.insert_str(self.cursor, text);
        self.cursor += text.len();
        // Inserting a base before a combining mark can join graphemes.
        while self.cursor < self.line.len()
            && !self
                .line
                .grapheme_indices(true)
                .any(|(i, _)| i == self.cursor)
        {
            self.cursor += self.line[self.cursor..].chars().next().unwrap().len_utf8();
        }
    }

    fn history(&mut self, history: &[String], up: bool) {
        if up {
            if self.history_index == 0 {
                return;
            }
            if self.history_index == history.len() {
                self.draft = self.line.clone();
            }
            self.history_index -= 1;
        } else if self.history_index < history.len() {
            self.history_index += 1;
        } else {
            return;
        }
        self.line = history
            .get(self.history_index)
            .unwrap_or(&self.draft)
            .clone();
        self.cursor = self.line.len();
    }

    fn key(&mut self, sequence: &[u8], history: &[String]) {
        match sequence {
            b"[D" | b"OD" => self.left(),
            b"[C" | b"OC" => self.right(),
            b"[A" | b"OA" => self.history(history, true),
            b"[B" | b"OB" => self.history(history, false),
            b"[H" | b"OH" | b"[1~" | b"[7~" => self.cursor = 0,
            b"[F" | b"OF" | b"[4~" | b"[8~" => self.cursor = self.line.len(),
            b"[3~" => {
                let start = self.cursor;
                self.right();
                self.line.replace_range(start..self.cursor, "");
                self.cursor = start;
            }
            _ => {}
        }
    }
}

pub fn read_line(prompt: &str) -> Result<String> {
    read_line_with_history(prompt, &mut Vec::new())
}

pub(super) fn read_line_with_history(prompt: &str, history: &mut Vec<String>) -> Result<String> {
    let result = edit_line(prompt, history)?;
    let entry = result.trim_end_matches('\n');
    if !entry.is_empty() && history.last().is_none_or(|last| last != entry) {
        history.push(entry.to_owned());
    }
    Ok(result)
}

fn edit_line(prompt: &str, history: &[String]) -> Result<String> {
    check_interrupt()?;
    let restore = Terminal::enter()?;
    let input_fd = restore.input.as_raw_fd();
    let styled = ui::styles_enabled(io::stderr().is_terminal());
    let mut out = io::stderr().lock();
    let mut editor = Editor {
        history_index: history.len(),
        ..Editor::default()
    };
    let mut pending = Vec::new();
    let mut escape = Vec::new();
    let mut last_size = (0, 0);
    let mut painted_rows = 0;
    let mut dirty = true;
    let mut painted = std::time::Instant::now();
    loop {
        check_interrupt()?;
        let size = dimensions();
        // Coalesce bursts (paste/key repeat) instead of flooding the terminal.
        if size != last_size || (dirty && painted.elapsed() >= std::time::Duration::from_millis(16))
        {
            let text = format!("{prompt}{}", editor.line);
            redraw_cursor(
                &mut out,
                &text,
                prompt.len() + editor.cursor,
                size,
                &mut painted_rows,
                styled,
            )?;
            dirty = false;
            last_size = size;
            painted = std::time::Instant::now();
        }
        match wait_for_input(input_fd) {
            Ok(true) => {}
            Ok(false) => continue,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e.into()),
        }
        let mut byte = 0u8;
        // SAFETY: read borrows a live descriptor and a one-byte buffer.
        let n = unsafe { libc::read(input_fd, (&mut byte as *mut u8).cast(), 1) };
        if n < 0 {
            let e = io::Error::last_os_error();
            if matches!(
                e.kind(),
                io::ErrorKind::Interrupted | io::ErrorKind::WouldBlock
            ) {
                continue;
            }
            return Err(e.into());
        }
        if n == 0 {
            return Ok(String::new());
        }
        check_interrupt()?;
        if !escape.is_empty() {
            escape.push(byte);
            if escape.len() == 2 && matches!(byte, b'[' | b'O') {
                continue;
            }
            if (0x40..=0x7e).contains(&byte) || escape.len() > 32 {
                editor.key(&escape[1..], history);
                escape.clear();
                dirty = true;
            }
            continue;
        }
        match byte {
            b'\r' | b'\n' => {
                let text = format!("{prompt}{}", editor.line);
                redraw(&mut out, &text, size, &mut painted_rows, styled)?;
                writeln!(out)?;
                out.flush()?;
                return Ok(format!("{}\n", editor.line));
            }
            4 if editor.line.is_empty() => return Ok(String::new()),
            1 => editor.cursor = 0,                 // Ctrl-A
            5 => editor.cursor = editor.line.len(), // Ctrl-E
            8 | 127 => {
                pending.clear();
                let end = editor.cursor;
                editor.left();
                editor.line.replace_range(editor.cursor..end, "");
            }
            21 => {
                editor.line.replace_range(..editor.cursor, "");
                editor.cursor = 0;
            }
            23 => {
                // Ctrl-W deletes the word before the cursor, preserving the suffix.
                let trimmed = editor.line[..editor.cursor].trim_end();
                let start = trimmed.rfind(char::is_whitespace).map_or(0, |i| i + 1);
                editor.line.replace_range(start..editor.cursor, "");
                editor.cursor = start;
            }
            27 => {
                pending.clear();
                escape.push(27);
                continue;
            }
            0..=31 => continue,
            _ => {
                pending.push(byte);
                match std::str::from_utf8(&pending) {
                    Ok(s) => {
                        editor.insert(s);
                        pending.clear();
                    }
                    Err(e) if e.error_len().is_none() => continue,
                    Err(_) => {
                        pending.clear();
                        continue;
                    }
                }
            }
        }
        dirty = true;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn idle_controlling_terminal_waits_without_spinning() {
        use std::os::fd::FromRawFd;
        use std::os::unix::process::CommandExt;
        use std::process::Command;
        use std::time::{Duration, Instant};

        const HELPER: &str = "AGENT_IDLE_TERMINAL_TEST";
        if std::env::var_os(HELPER).is_some() {
            let terminal = Terminal::enter().unwrap();
            // SAFETY: getrusage initializes the supplied stack values.
            let cpu_time = || unsafe {
                let mut usage = std::mem::zeroed();
                assert_eq!(libc::getrusage(libc::RUSAGE_SELF, &mut usage), 0);
                let seconds = |t: libc::timeval| t.tv_sec as f64 + t.tv_usec as f64 / 1_000_000.0;
                seconds(usage.ru_utime) + seconds(usage.ru_stime)
            };
            let cpu = cpu_time();
            let start = Instant::now();
            for _ in 0..10 {
                assert!(!wait_for_input(terminal.input.as_raw_fd()).unwrap());
            }
            assert!(start.elapsed() >= Duration::from_millis(200));
            assert!(
                cpu_time() - cpu < 0.1,
                "idle terminal consumed excessive CPU"
            );
            return;
        }

        let (mut master, mut slave) = (-1, -1);
        // SAFETY: openpty initializes the two descriptors on success.
        assert_eq!(
            unsafe {
                libc::openpty(
                    &mut master,
                    &mut slave,
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                )
            },
            0
        );
        // SAFETY: successful openpty transfers ownership of unique descriptors.
        let master = unsafe { File::from_raw_fd(master) };
        let slave = unsafe { File::from_raw_fd(slave) };
        // Do not let the child inherit its own terminal master.
        // SAFETY: master is live and its existing descriptor flags are preserved.
        unsafe {
            let flags = libc::fcntl(master.as_raw_fd(), libc::F_GETFD);
            assert!(flags >= 0);
            assert_eq!(
                libc::fcntl(master.as_raw_fd(), libc::F_SETFD, flags | libc::FD_CLOEXEC),
                0
            );
        }
        let mut command = Command::new(std::env::current_exe().unwrap());
        command
            .args([
                "--exact",
                "ui::input::tests::idle_controlling_terminal_waits_without_spinning",
                "--nocapture",
            ])
            .env(HELPER, "1")
            .stdin(slave);
        // SAFETY: only async-signal-safe syscalls execute between fork and exec.
        unsafe {
            command.pre_exec(|| {
                if libc::setsid() == -1 || libc::ioctl(0, libc::TIOCSCTTY as _, 0) == -1 {
                    return Err(io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let output = command.output().unwrap();
        assert!(
            output.status.success(),
            "{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }

    #[test]
    fn wait_rejects_descriptors_outside_fd_set() {
        for fd in [-1, libc::FD_SETSIZE as RawFd] {
            assert_eq!(
                wait_for_input(fd).unwrap_err().kind(),
                io::ErrorKind::InvalidInput
            );
        }
    }

    #[test]
    fn wraps_prompt_and_input_without_splitting_graphemes() {
        assert_eq!(wrapped_rows("> abcdef", 5), ["> abc", "def"]);
        assert_eq!(wrapped_rows("> 界e\u{301}", 3), ["> ", "界e\u{301}"]);
        assert_eq!(wrapped_rows("abcdef", 3), ["abc", "def"]);
        assert_eq!(wrapped_rows("", 3), [""]);
        assert_eq!(wrapped_rows("界", 1), ["�"]);
    }

    #[test]
    fn redraw_clears_old_rows_after_backspace_or_clear() {
        let mut out = Vec::new();
        let mut painted = 0;
        redraw(&mut out, "> abcdef", (6, 24), &mut painted, false).unwrap();
        assert_eq!(out, b"\r\x1b[J> abc\r\ndef");
        assert_eq!(painted, 2);
        out.clear();
        redraw(&mut out, "> ab", (6, 24), &mut painted, false).unwrap();
        assert_eq!(out, b"\r\x1b[1A\x1b[J> ab");
        assert_eq!(painted, 1);
    }

    #[test]
    fn colored_input_wraps_by_visible_width_and_resets_style() {
        let mut out = Vec::new();
        let mut painted = 0;
        redraw(&mut out, "> 界abcd", (6, 24), &mut painted, true).unwrap();
        assert_eq!(
            String::from_utf8(out).unwrap(),
            "\r\x1b[J\x1b[0;36m> 界a\r\nbcd\x1b[0m"
        );
        assert_eq!(painted, 2);
    }

    #[test]
    fn cursor_redraw_tracks_wrapped_rows_and_scrolls_back_to_the_start() {
        let mut out = Vec::new();
        let mut painted = 0;
        redraw(&mut out, "> abcdefghijk", (6, 3), &mut painted, false).unwrap();
        out.clear();
        redraw_cursor(&mut out, "> abcdefghijk", 3, (6, 3), &mut painted, false).unwrap();
        assert_eq!(out, b"\r\x1b[1A\x1b[J> abc\r\ndefgh\x1b[1A\r\x1b[3C");
        assert_eq!(painted, 1);
        out.clear();
        redraw_cursor(&mut out, "> abcdefghijk", 8, (6, 3), &mut painted, false).unwrap();
        assert_eq!(out, b"\r\x1b[J> abc\r\ndefgh\r\x1b[3C");
        assert_eq!(painted, 2);
    }

    #[test]
    fn long_input_keeps_the_tail_within_screen_height() {
        let mut out = Vec::new();
        let mut painted = 0;
        redraw(&mut out, "> abcdefghijk", (6, 3), &mut painted, false).unwrap();
        assert_eq!(out, b"\r\x1b[Jdefgh\r\nijk");
        assert_eq!(painted, 2);
    }
}
