//! Terminal frontend. Escaping, styling, prompts and input stay out of the engine.
mod cli;
mod input;
use crate::engine::{Frontend, check_interrupt, limited};
#[cfg(test)]
pub(crate) use cli::chat;
pub(crate) use cli::run;
use llm_api::Result;
use serde_json::Value;
use std::io::{self, IsTerminal};
use std::time::Duration;

// Escape terminal control sequences from models, files, and commands, including OSC/CSI.
pub(crate) fn display(value: &str) -> String {
    value
        .chars()
        .flat_map(|c| {
            if c.is_control() && c != '\n' && c != '\t' {
                c.escape_default().collect::<Vec<_>>()
            } else {
                vec![c]
            }
        })
        .collect()
}

pub(crate) struct TerminalFrontend {
    pub(crate) verbose: bool,
}

impl Frontend for TerminalFrontend {
    fn approve(&self, name: &str, args: &Value) -> Result<()> {
        // Approval always exposes the full proposed effect, even in compact mode.
        eprintln!("\n{} {}", display(name), display(&args.to_string()));
        if !io::stdin().is_terminal() {
            return Err(
                "approval requires a terminal; omit --ask or pass --yes to allow automatically"
                    .into(),
            );
        }
        let answer = input::read_line("Allow with full user privileges? [y/N] ")?;
        check_interrupt()?;
        if answer.trim() != "y" {
            return Err("denied by user".into());
        }
        Ok(())
    }

    fn tool_started(&self, number: usize, name: &str, args: &Value) {
        let label = if self.verbose {
            format!(
                "[{number}] {} {}",
                display(name),
                display(&args.to_string())
            )
        } else {
            format!("[{number}] {}", label(name, args))
        };
        tool_activity(&label);
    }

    fn tool_finished(
        &self,
        number: usize,
        name: &str,
        output: &str,
        is_error: bool,
        elapsed: Duration,
    ) {
        let summary = if self.verbose {
            display(&limited(output))
        } else {
            format!(
                "[{number}] {} · {:.1}s",
                outcome(name, output, is_error),
                elapsed.as_secs_f64()
            )
        };
        tool_activity(&summary);
    }

    fn diagnostic(&self, message: &str) {
        eprintln!("skills: {}", display(message));
    }
}

pub fn styles_enabled(terminal: bool) -> bool {
    terminal && std::env::var_os("NO_COLOR").is_none()
}

#[derive(Clone, Copy)]
pub enum Style {
    Input,
    Tool,
}

/// Wrap already-escaped text, resetting every fragment to avoid style leakage.
pub fn styled(text: &str, style: Style, enabled: bool) -> String {
    if !enabled {
        return text.into();
    }
    let code = match style {
        Style::Input => "0;36",
        Style::Tool => "0;33",
    };
    format!("\x1b[{code}m{text}\x1b[0m")
}

pub fn tool_activity(text: &str) {
    eprintln!(
        "{}",
        styled(
            text,
            Style::Tool,
            styles_enabled(io::stderr().is_terminal())
        )
    );
}

fn short(value: &str, width: usize) -> String {
    let safe = display(value).replace('\n', "\\n").replace('\t', "\\t");
    let mut chars = safe.chars();
    let head: String = chars.by_ref().take(width).collect();
    if chars.next().is_some() {
        format!("{head}…")
    } else {
        head
    }
}

pub fn label(name: &str, args: &Value) -> String {
    let target = args[if name == "shell" { "command" } else { "path" }]
        .as_str()
        .unwrap_or("?");
    match name {
        "read_file" => format!(
            "read {} (line {}, up to {})",
            short(target, 90),
            args["start_line"].as_u64().unwrap_or(1),
            args["line_count"].as_u64().unwrap_or(200).min(2000)
        ),
        "edit_file" => format!("edit {}", short(target, 90)),
        "write_file" => format!("write {}", short(target, 90)),
        "shell" => format!("$ {}", short(target, 100)),
        _ => short(name, 90),
    }
}

fn preview(value: &str) -> String {
    let lines: Vec<_> = value.lines().collect();
    let mut result = String::new();
    for line in lines.iter().take(3) {
        result.push_str(&format!("\n    {}", short(line, 120)));
    }
    if lines.len() > 3 {
        result.push_str(&format!(
            "\n    … {} more lines (--verbose for details)",
            lines.len() - 3
        ));
    }
    result
}

pub fn outcome(name: &str, output: &str, is_error: bool) -> String {
    if is_error {
        return format!("error{}", preview(output));
    }
    match name {
        "read_file" => format!(
            "done · {} lines, {} bytes{}",
            output.lines().count(),
            output.len(),
            if output.contains("\n[truncated]") {
                " (truncated)"
            } else {
                ""
            }
        ),
        "write_file" | "edit_file" => "done · written".into(),
        "shell" => {
            if let Some((status, streams)) = output.split_once("\nstdout:\n") {
                if let Some((stdout, stderr)) = streams.split_once("\nstderr:\n") {
                    let mut result = short(status, 120);
                    for (label, stream) in [("stdout", stdout), ("stderr", stderr)] {
                        if !stream.trim().is_empty() {
                            result
                                .push_str(&format!("\n    {label}:{}", preview(stream.trim_end())));
                        }
                    }
                    return result;
                }
            }
            format!("done{}", preview(output))
        }
        _ => format!("done{}", preview(output)),
    }
}

/// Stream plain output; buffer each terminal role for Markdown parsing.
/// Flush buffered text even on cancellation/errors, without changing the transcript.
pub struct StreamDisplay {
    styled: bool,
    reasoning: Option<bool>,
    pending: String,
}

impl StreamDisplay {
    pub fn new(styled: bool) -> Self {
        Self {
            styled,
            reasoning: None,
            pending: String::new(),
        }
    }

    pub fn delta(&mut self, event: &Value) -> String {
        let reasoning = match event["type"].as_str() {
            Some("reasoning_delta") => true,
            Some("text_delta") => false,
            _ => return String::new(),
        };
        let text = event["delta"].as_str().unwrap_or("");
        if text.is_empty() {
            return String::new();
        }
        let mut out = String::new();
        if self.reasoning != Some(reasoning) {
            out.push_str(&self.flush_pending());
            if self.reasoning.is_some() {
                out.push_str("\n\n");
            }
            // Plain output needs a label; styled reasoning is already distinct.
            if reasoning && !self.styled {
                out.push_str("Thinking:\n");
            }
        }
        self.reasoning = Some(reasoning);
        if self.styled {
            self.pending.push_str(text);
        } else {
            out.push_str(&display(text));
        }
        out
    }

    fn flush_pending(&mut self) -> String {
        if self.pending.is_empty() {
            return String::new();
        }
        let safe = display(&std::mem::take(&mut self.pending));
        let mut skin = termimad::MadSkin::default();
        if self.reasoning == Some(true) {
            skin.set_fg(termimad::crossterm::style::Color::DarkMagenta);
        }
        skin.term_text(&safe).to_string()
    }

    pub fn finish(&mut self) -> String {
        let mut out = self.flush_pending();
        if self.reasoning.take().is_some() && !out.ends_with('\n') {
            out.push('\n');
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn roles_have_distinct_styles_and_plain_output_is_unchanged() {
        let styles = [Style::Input, Style::Tool];
        let codes = ["0;36", "0;33"];
        for (style, code) in styles.into_iter().zip(codes) {
            assert_eq!(
                styled("text", style, true),
                format!("\x1b[{code}mtext\x1b[0m")
            );
            assert_eq!(styled("text", style, false), "text");
        }
        assert!(!styles_enabled(false));
        assert_eq!(styles_enabled(true), std::env::var_os("NO_COLOR").is_none());
    }

    #[test]
    fn reasoning_streams_separately_from_answers() {
        let mut stream = StreamDisplay::new(false);
        assert_eq!(
            stream.delta(&json!({"type":"reasoning_delta","delta":"First"})),
            "Thinking:\nFirst"
        );
        assert_eq!(
            stream.delta(&json!({"type":"reasoning_delta","delta":" step"})),
            " step"
        );
        assert_eq!(
            stream.delta(&json!({"type":"tool_call_delta","delta":"hidden"})),
            ""
        );
        assert_eq!(
            stream.delta(&json!({"type":"text_delta","delta":"Answer"})),
            "\n\nAnswer"
        );
        assert_eq!(stream.finish(), "\n");
        assert_eq!(stream.finish(), "");
    }

    #[test]
    fn reasoning_markdown_is_colored_without_forced_italics_and_flushes_on_role_change() {
        let mut stream = StreamDisplay::new(true);
        assert_eq!(stream.finish(), "");
        for text in ["plain **bo", "ld**\u{1b}[0m"] {
            assert_eq!(
                stream.delta(&json!({"type":"reasoning_delta","delta":text})),
                ""
            );
        }
        let reasoning = stream.delta(&json!({"type":"text_delta","delta":"Answer"}));
        assert!(reasoning.contains("plain "));
        assert!(reasoning.contains("bold"));
        assert!(!reasoning.contains("**"));
        assert!(reasoning.contains("\\u{1b}[0m"));
        assert!(reasoning.contains("\x1b[38;5;5m"), "{reasoning:?}");
        assert!(!reasoning.contains("\x1b[3m"));
        let answer = stream.delta(&json!({"type":"reasoning_delta","delta":"*italic*"}));
        assert!(answer.contains("Answer"));
        assert!(!answer.contains("\x1b[38;5;5m"));
        let final_reasoning = stream.finish();
        assert!(final_reasoning.contains("italic"));
        assert!(!final_reasoning.contains("*italic*"));
        assert!(final_reasoning.contains("\x1b[3m"));
        assert_eq!(stream.finish(), "");
    }

    #[test]
    fn terminal_answers_render_markdown_across_chunks_and_finish_once() {
        let mut stream = StreamDisplay::new(true);
        let markdown = "*italic* **bold**\n\n| A | B |\n|---|---|\n| one | two |\n\n```rust\nlet x = 1;\n```\n";
        for part in markdown.chars() {
            assert_eq!(
                stream.delta(&json!({"type":"text_delta","delta":part.to_string()})),
                ""
            );
        }
        let rendered = stream.finish();
        for text in ["italic", "bold", "one", "two", "let x = 1;"] {
            assert!(rendered.contains(text), "missing {text}: {rendered:?}");
        }
        assert!(!rendered.contains("**bold**"));
        assert!(!rendered.contains("```"));
        assert!(rendered.contains('\x1b'));
        assert_eq!(stream.finish(), "");
    }

    #[test]
    fn buffered_answers_escape_controls_and_preserve_plain_markdown() {
        let raw = "**bold**\u{1b}[31m\r";
        let mut plain = StreamDisplay::new(false);
        assert_eq!(
            plain.delta(&json!({"type":"text_delta","delta":raw})),
            display(raw)
        );
        let mut terminal = StreamDisplay::new(true);
        terminal.delta(&json!({"type":"reasoning_delta","delta":"Thinking"}));
        let reasoning = terminal.delta(&json!({"type":"text_delta","delta":raw}));
        assert!(reasoning.contains("Thinking"));
        assert!(reasoning.ends_with("\n\n"));
        let rendered = terminal.finish();
        assert!(rendered.contains("\\u{1b}[31m\\r"));
        assert!(!rendered.contains("\u{1b}[31m"));
        assert_eq!(terminal.finish(), "");
    }

    #[test]
    fn labels_are_short_safe_and_hide_file_bodies() {
        assert_eq!(
            label(
                "edit_file",
                &json!({"path":"a.rs", "new_string":"secret body"})
            ),
            "edit a.rs"
        );
        assert_eq!(
            label(
                "read_file",
                &json!({"path":"a.rs", "start_line":5, "line_count":2})
            ),
            "read a.rs (line 5, up to 2)"
        );
        let command = label(
            "shell",
            &json!({"command":format!("\u{1b}[31m\n{}", "🦀".repeat(200))}),
        );
        assert!(!command.contains('\u{1b}'));
        assert!(!command.contains('\n'));
        assert!(command.ends_with('…'));
        assert!(command.chars().count() < 110);
    }

    #[test]
    fn summaries_preserve_status_and_both_streams() {
        let output = "status: exit status: 1\nstdout:\none\ntwo\nthree\nfour\nstderr:\nfailed\n";
        let rendered = outcome("shell", output, false);
        assert!(rendered.contains("exit status: 1"));
        assert!(rendered.contains("1 more lines"));
        assert!(rendered.contains("stderr:\n    failed"));
        assert!(!rendered.contains("four"));
        assert!(outcome("shell", "denied by user", true).starts_with("error"));
        assert_eq!(
            outcome("read_file", "1\ta\n", false),
            "done · 1 lines, 4 bytes"
        );
        assert!(outcome("read_file", "1\ta\n[truncated]", false).contains("(truncated)"));
        assert!(outcome("shell", "x", false).contains('x'));
    }
}
