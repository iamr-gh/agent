//! Searchable, workspace-local session switcher. No model calls or tool replay.
use super::{display, input};
use crate::{Result, check_interrupt, session::Summary};
use std::io::{self, Write};
use std::time::{Duration, Instant};
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

struct Picker<'a> {
    entries: &'a [Summary],
    query: String,
    selected: usize,
}

impl Picker<'_> {
    fn matches(&self) -> Vec<usize> {
        let query = self.query.to_lowercase();
        self.entries
            .iter()
            .enumerate()
            .filter_map(|(i, entry)| {
                format!("{} {} {}", entry.title, entry.model, entry.id)
                    .to_lowercase()
                    .contains(&query)
                    .then_some(i)
            })
            .collect()
    }

    fn move_selection(&mut self, down: bool) {
        let count = self.matches().len();
        self.selected = if down {
            (self.selected + 1).min(count.saturating_sub(1))
        } else {
            self.selected.saturating_sub(1)
        };
    }

    fn rows(&self, current: Option<&str>, size: (usize, usize)) -> Vec<String> {
        let matches = self.matches();
        let capacity = size.1.saturating_sub(3).max(1);
        let start = self.selected.saturating_sub(capacity - 1);
        let mut rows = vec![format!(
            "Resume > {}  ({} sessions)",
            self.query,
            matches.len()
        )];
        for (position, &index) in matches.iter().enumerate().skip(start).take(capacity) {
            let entry = &self.entries[index];
            let age = entry.modified.elapsed().unwrap_or_default().as_secs();
            let age = if age < 60 {
                "just now".into()
            } else if age < 3600 {
                format!("{}m ago", age / 60)
            } else if age < 86400 {
                format!("{}h ago", age / 3600)
            } else {
                format!("{}d ago", age / 86400)
            };
            rows.push(format!(
                "{} {}{}  [{} · {}]",
                if position == self.selected { ">" } else { " " },
                if current == Some(entry.id.as_str()) {
                    "(current) "
                } else {
                    ""
                },
                entry.title,
                entry.model,
                age
            ));
        }
        if matches.is_empty() {
            rows.push("  No matching sessions".into());
        }
        if size.1 > 3 {
            rows.push("↑/↓ select · type to filter · Enter resume · Esc cancel".into());
        }
        rows.into_iter()
            .take(size.1.saturating_sub(1).max(1))
            .map(|row| clip(&row, size.0.saturating_sub(1)))
            .collect()
    }
}

// Saved prompts are untrusted terminal content. Keep each entry on one row,
// escape control sequences, and truncate by display cells, not bytes.
fn clip(text: &str, width: usize) -> String {
    let text = display(text)
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    let mut result = String::new();
    let mut used = 0;
    for g in text.graphemes(true) {
        if used + g.width() > width {
            break;
        }
        used += g.width();
        result.push_str(g);
    }
    result
}

struct Screen(io::Stderr);
impl Screen {
    fn draw(&mut self, rows: &[String]) -> io::Result<()> {
        write!(self.0, "\r\x1b[J{}", rows.join("\r\n"))?;
        if rows.len() > 1 {
            write!(self.0, "\x1b[{}A", rows.len() - 1)?;
        }
        write!(self.0, "\r")?;
        self.0.flush()
    }
}
impl Drop for Screen {
    fn drop(&mut self) {
        let _ = write!(self.0, "\r\x1b[J");
        let _ = self.0.flush();
    }
}

pub(super) fn choose(entries: &[Summary], current: Option<&str>) -> Result<Option<String>> {
    if entries.is_empty() {
        eprintln!("No saved sessions for this workspace.");
        return Ok(None);
    }
    let terminal = input::Terminal::enter()?;
    let mut screen = Screen(io::stderr());
    let mut picker = Picker {
        entries,
        query: String::new(),
        selected: 0,
    };
    let mut pending = Vec::new();
    let mut escape = Vec::new();
    let mut escape_started = Instant::now();
    let mut last_size = (0, 0);
    let mut dirty = true;
    loop {
        check_interrupt()?;
        let size = input::dimensions();
        if dirty || size != last_size {
            screen.draw(&picker.rows(current, size))?;
            dirty = false;
            last_size = size;
        }
        let byte = match terminal.read_byte() {
            Ok(Some(byte)) => byte,
            Ok(None) => {
                // Distinguish a standalone Escape from an arrow-key prefix.
                if !escape.is_empty() && escape_started.elapsed() >= Duration::from_millis(100) {
                    return Ok(None);
                }
                continue;
            }
            Err(e)
                if matches!(
                    e.kind(),
                    io::ErrorKind::Interrupted | io::ErrorKind::WouldBlock
                ) =>
            {
                continue;
            }
            Err(e) => return Err(e.into()),
        };
        if !escape.is_empty() {
            if byte == 27 {
                return Ok(None);
            }
            escape.push(byte);
            if escape.len() == 2 && matches!(byte, b'[' | b'O') {
                continue;
            }
            if (0x40..=0x7e).contains(&byte) || escape.len() > 32 {
                match &escape[1..] {
                    b"[A" | b"OA" => picker.move_selection(false),
                    b"[B" | b"OB" => picker.move_selection(true),
                    _ => {}
                }
                escape.clear();
                dirty = true;
            }
            continue;
        }
        match byte {
            4 => return Ok(None),
            27 => {
                pending.clear();
                escape.push(byte);
                escape_started = Instant::now();
                continue;
            }
            b'\r' | b'\n' => {
                if let Some(&index) = picker.matches().get(picker.selected) {
                    let id = &entries[index].id;
                    return Ok((current != Some(id.as_str())).then(|| id.clone()));
                }
                continue;
            }
            14 => picker.move_selection(true),
            16 => picker.move_selection(false),
            8 | 127 => {
                pending.clear();
                if let Some((i, _)) = picker.query.grapheme_indices(true).next_back() {
                    picker.query.truncate(i);
                }
                picker.selected = 0;
            }
            21 => {
                picker.query.clear();
                picker.selected = 0;
            }
            0..=31 => continue,
            _ => {
                pending.push(byte);
                match std::str::from_utf8(&pending) {
                    Ok(text) => {
                        picker.query.push_str(text);
                        pending.clear();
                        picker.selected = 0;
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
    use std::time::SystemTime;

    #[test]
    fn search_navigation_and_safe_bounded_rows() {
        let entries = [
            Summary {
                id: "1".into(),
                title: "Fix 界\n\x1b[31m".into(),
                model: "model-a".into(),
                modified: SystemTime::now(),
            },
            Summary {
                id: "2".into(),
                title: "Other task".into(),
                model: "model-b".into(),
                modified: SystemTime::now(),
            },
        ];
        let mut picker = Picker {
            entries: &entries,
            query: String::new(),
            selected: 0,
        };
        picker.move_selection(false);
        assert_eq!(picker.selected, 0);
        picker.move_selection(true);
        picker.move_selection(true);
        assert_eq!(picker.selected, 1);
        picker.query = "FIX 界".into();
        picker.selected = 0;
        assert_eq!(picker.matches(), [0]);
        let rows = picker.rows(Some("1"), (80, 5));
        assert!(rows[1].contains("(current)"));
        assert!(!rows[1].contains(['\n', '\x1b']));
        assert!(rows[1].contains("\\u{1b}"));
        picker.query = "model-B".into();
        assert_eq!(picker.matches(), [1]);
        picker.query = "no match".into();
        picker.move_selection(true);
        assert!(picker.matches().is_empty());
        assert!(picker.rows(None, (40, 5))[1].contains("No matching"));
        assert_eq!(clip("ab界cd", 5), "ab界c");
        assert!(picker.rows(None, (8, 2)).iter().all(|row| row.width() <= 7));
    }
}
