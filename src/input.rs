//! A small wrapped input editor. Redraw all visible rows explicitly so erase
//! works across line boundaries without relying on canonical terminal editing.
use super::{Result, check_interrupt};
use std::fs::{File, OpenOptions};
use std::io::{self, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::OpenOptionsExt;
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

struct Terminal {
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
    fn enter() -> io::Result<Self> {
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
) -> io::Result<()> {
    // Reserve the last column to avoid the emulator's pending soft-wrap state.
    let rows = wrapped_rows(text, size.0 - 1);
    let visible = &rows[rows.len().saturating_sub(size.1 - 1)..];
    write!(out, "\r")?;
    let up = painted_rows.saturating_sub(1).min(size.1 - 1);
    if up > 0 {
        write!(out, "\x1b[{up}A")?;
    }
    write!(out, "\x1b[J{}", visible.join("\r\n"))?;
    *painted_rows = visible.len();
    out.flush()
}

pub fn read_line(prompt: &str) -> Result<String> {
    check_interrupt()?;
    let restore = Terminal::enter()?;
    let input_fd = restore.input.as_raw_fd();
    let mut out = io::stderr().lock();
    let mut line = String::new();
    let mut pending = Vec::new();
    let mut escape = 0;
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
            let text = format!("{prompt}{line}");
            redraw(&mut out, &text, size, &mut painted_rows)?;
            dirty = false;
            last_size = size;
            painted = std::time::Instant::now();
        }
        let mut fd = libc::pollfd {
            fd: input_fd,
            events: libc::POLLIN,
            revents: 0,
        };
        // SAFETY: poll borrows a valid stack value; read borrows a one-byte buffer.
        let ready = unsafe { libc::poll(&mut fd, 1, 25) };
        if ready < 0 {
            let e = io::Error::last_os_error();
            if e.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return Err(e.into());
        }
        if ready == 0 {
            continue;
        }
        let mut byte = 0u8;
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
        // Ignore terminal key sequences rather than inserting their printable tail.
        if escape != 0 {
            if escape == 1 && matches!(byte, b'[' | b'O') {
                escape = 2;
            } else if escape == 1 || (0x40..=0x7e).contains(&byte) {
                escape = 0;
            }
            continue;
        }
        match byte {
            b'\r' | b'\n' => {
                let text = format!("{prompt}{line}");
                redraw(&mut out, &text, size, &mut painted_rows)?;
                writeln!(out)?;
                out.flush()?;
                return Ok(format!("{line}\n"));
            }
            4 if line.is_empty() => return Ok(String::new()),
            8 | 127 => {
                pending.clear();
                if let Some((i, _)) = line.grapheme_indices(true).next_back() {
                    line.truncate(i);
                }
            }
            21 => line.clear(), // Ctrl-U
            23 => {
                // Ctrl-W
                let trimmed = line.trim_end();
                let end = trimmed.rfind(char::is_whitespace).map_or(0, |i| i + 1);
                line.truncate(end);
            }
            27 => {
                escape = 1;
                continue;
            }
            0..=31 => continue,
            _ => {
                pending.push(byte);
                match std::str::from_utf8(&pending) {
                    Ok(s) => {
                        line.push_str(s);
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
        redraw(&mut out, "> abcdef", (6, 24), &mut painted).unwrap();
        assert_eq!(out, b"\r\x1b[J> abc\r\ndef");
        assert_eq!(painted, 2);
        out.clear();
        redraw(&mut out, "> ab", (6, 24), &mut painted).unwrap();
        assert_eq!(out, b"\r\x1b[1A\x1b[J> ab");
        assert_eq!(painted, 1);
    }

    #[test]
    fn long_input_keeps_the_tail_within_screen_height() {
        let mut out = Vec::new();
        let mut painted = 0;
        redraw(&mut out, "> abcdefghijk", (6, 3), &mut painted).unwrap();
        assert_eq!(out, b"\r\x1b[Jdefgh\r\nijk");
        assert_eq!(painted, 2);
    }
}
