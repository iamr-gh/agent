//! Terminal-only summaries. Never shorten the tool results sent to the model.
use super::display;
use serde_json::Value;
use std::io::{self, IsTerminal};

pub fn styles_enabled(terminal: bool) -> bool {
    terminal && std::env::var_os("NO_COLOR").is_none()
}

#[derive(Clone, Copy)]
pub enum Style {
    Input,
    Tool,
    Reasoning,
    Answer,
}

/// Wrap already-escaped text, resetting every fragment to avoid style leakage.
pub fn styled(text: &str, style: Style, enabled: bool) -> String {
    if !enabled {
        return text.into();
    }
    let code = match style {
        Style::Input => "0;36",
        Style::Tool => "0;2",
        Style::Reasoning => "0;2;3",
        Style::Answer => "0;39",
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

/// Small streaming renderer: keep reasoning distinct without buffering it.
/// Style each chunk separately so cancellation/errors cannot leave the terminal dim.
pub struct StreamDisplay {
    styled: bool,
    reasoning: Option<bool>,
}

impl StreamDisplay {
    pub fn new(styled: bool) -> Self {
        Self {
            styled,
            reasoning: None,
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
            if self.reasoning.is_some() {
                out.push_str("\n\n");
            }
            if reasoning {
                out.push_str("Thinking:\n");
            }
        }
        self.reasoning = Some(reasoning);
        out.push_str(&display(text));
        styled(
            &out,
            if reasoning {
                Style::Reasoning
            } else {
                Style::Answer
            },
            self.styled,
        )
    }

    pub fn finish(&mut self) -> &'static str {
        if self.reasoning.take().is_some() {
            "\n"
        } else {
            ""
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn roles_have_distinct_styles_and_plain_output_is_unchanged() {
        let styles = [Style::Input, Style::Tool, Style::Reasoning, Style::Answer];
        let codes = ["0;36", "0;2", "0;2;3", "0;39"];
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
    fn streaming_styles_are_reset_and_untrusted_controls_are_escaped() {
        let mut stream = StreamDisplay::new(true);
        assert_eq!(
            stream.delta(&json!({"type":"reasoning_delta","delta":""})),
            ""
        );
        assert_eq!(stream.finish(), "");
        let chunk = stream.delta(&json!({"type":"reasoning_delta","delta":"hi\u{1b}[0m"}));
        assert_eq!(chunk, "\x1b[0;2;3mThinking:\nhi\\u{1b}[0m\x1b[0m");
        assert_eq!(stream.finish(), "\n");
        assert_eq!(
            stream.delta(&json!({"type":"text_delta","delta":"Normal"})),
            "\x1b[0;39mNormal\x1b[0m"
        );
        assert_eq!(
            stream.delta(&json!({"type":"reasoning_delta","delta":"Again"})),
            "\x1b[0;2;3m\n\nThinking:\nAgain\x1b[0m"
        );
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
