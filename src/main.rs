mod auth;
mod input;
mod skills;
mod ui;

use llm_api::{Client, Message, Provider, Request, Response, Result};
use serde_json::{Value, json};
use std::collections::HashSet;
use std::fs::{self, File};
use std::io::{self, IsTerminal, Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::thread;
use std::time::{Duration, Instant};

const FILE_LIMIT: usize = 1024 * 1024;
const OUTPUT_LIMIT: usize = 32 * 1024;
static INTERRUPTED: AtomicBool = AtomicBool::new(false);
extern "C" fn interrupt(_: libc::c_int) {
    INTERRUPTED.store(true, Ordering::Relaxed);
}
fn check_interrupt() -> Result<()> {
    if INTERRUPTED.load(Ordering::Relaxed) {
        return Err(llm_api::Cancelled.into());
    }
    Ok(())
}

fn input_line(prompt: &str) -> Result<String> {
    input::read_line(prompt)
}

fn text(role: &str, value: String) -> Message {
    Message {
        role: role.into(),
        content: vec![json!({"type":"text","text":value})],
        replay: None,
    }
}

// Escape terminal control sequences from models, files, and commands, including OSC/CSI.
fn display(value: &str) -> String {
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

fn limited(value: &str) -> String {
    let mut end = value.len().min(OUTPUT_LIMIT);
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    if end == value.len() {
        value.into()
    } else {
        format!("{}\n[truncated]", &value[..end])
    }
}

fn field<'a>(args: &'a Value, key: &str) -> Result<&'a str> {
    args.get(key)
        .and_then(Value::as_str)
        .ok_or_else(|| format!("missing string {key}").into())
}

fn tools() -> Vec<Value> {
    [
        ("load_skill", "Load an installed skill's instructions by exact catalog name. Read-only; includes its directory for relative references.", vec!["name"], json!({"name":{"type":"string"}})),
        ("read_file", "Read UTF-8 text, at most 32 KiB per result. Optional 1-based start_line and line_count (default 200).", vec!["path"], json!({"path":{"type":"string"},"start_line":{"type":"integer","minimum":1},"line_count":{"type":"integer","minimum":1}})),
        ("write_file", "Create or overwrite a UTF-8 file after approval; parent must exist.", vec!["path","content"], json!({"path":{"type":"string"},"content":{"type":"string"}})),
        ("edit_file", "Replace exactly one occurrence of old_string with new_string after approval.", vec!["path","old_string","new_string"], json!({"path":{"type":"string"},"old_string":{"type":"string"},"new_string":{"type":"string"}})),
        ("shell", "Run /bin/sh in the workspace after approval; 60s timeout, bounded output. No sandbox.", vec!["command"], json!({"command":{"type":"string"}})),
    ].into_iter().map(|(name, description, required, properties)| json!({
        "name":name,"description":description,"parameters":{
            "type":"object","properties":properties,"required":required,"additionalProperties":false
        }
    })).collect()
}

struct Workspace {
    root: PathBuf,
    yes: bool,
    verbose: bool,
}

impl Workspace {
    fn path(&self, name: &str) -> Result<PathBuf> {
        let candidate = self.root.join(name);
        let path = if candidate.exists() || candidate.symlink_metadata().is_ok() {
            candidate.canonicalize()?
        } else {
            candidate
                .parent()
                .ok_or("missing parent")?
                .canonicalize()?
                .join(candidate.file_name().ok_or("missing file name")?)
        };
        if !path.starts_with(&self.root) {
            return Err("file path escapes workspace".into());
        }
        Ok(path)
    }

    fn read(&self, path: &Path) -> Result<String> {
        if !fs::metadata(path)?.is_file() {
            return Err("expected regular file".into());
        }
        let file = File::open(path)?;
        if !file.metadata()?.is_file() {
            return Err("expected regular file".into());
        }
        let mut bytes = Vec::new();
        file.take((FILE_LIMIT + 1) as u64).read_to_end(&mut bytes)?;
        if bytes.len() > FILE_LIMIT {
            return Err("file exceeds 1 MiB; use a narrow shell command".into());
        }
        Ok(String::from_utf8(bytes)?)
    }

    fn approve(&self, name: &str, args: &Value) -> Result<()> {
        if self.yes {
            return check_interrupt();
        }
        // Approval always exposes the full proposed effect, even in compact mode.
        eprintln!("\n{} {}", name, display(&args.to_string()));
        if !io::stdin().is_terminal() {
            return Err(
                "approval requires a terminal; omit --ask or pass --yes to allow automatically"
                    .into(),
            );
        }
        let answer = input_line("Allow with full user privileges? [y/N] ")?;
        check_interrupt()?;
        if answer.trim() != "y" {
            return Err("denied by user".into());
        }
        Ok(())
    }

    fn execute(&self, name: &str, args: &Value) -> Result<String> {
        check_interrupt()?;
        let schema = tools()
            .into_iter()
            .find(|tool| tool["name"] == name)
            .ok_or("unknown tool")?;
        let object = args.as_object().ok_or("arguments must be an object")?;
        let props = schema["parameters"]["properties"].as_object().unwrap();
        for (key, value) in object {
            let spec = props.get(key).ok_or("unknown argument")?;
            if spec["type"] == "string" && !value.is_string()
                || spec["type"] == "integer" && value.as_u64().filter(|n| *n > 0).is_none()
            {
                return Err("invalid argument type or range".into());
            }
        }
        for key in schema["parameters"]["required"].as_array().unwrap() {
            field(args, key.as_str().unwrap())?;
        }
        if name == "load_skill" {
            return skills::load(self, field(args, "name")?);
        }
        if name == "shell" {
            self.approve(name, args)?;
            return shell(&self.root, field(args, "command")?, Duration::from_secs(60));
        }
        let path = self.path(field(args, "path")?)?;
        if name == "read_file" {
            let data = self.read(&path)?;
            let start = args["start_line"].as_u64().unwrap_or(1).saturating_sub(1);
            let count = args["line_count"].as_u64().unwrap_or(200).min(2000);
            let mut output = String::new();
            for (i, line) in data
                .lines()
                .enumerate()
                .skip(start.min(usize::MAX as u64) as usize)
                .take(count as usize)
            {
                output.push_str(&format!("{}\t{}\n", i + 1, line));
                if output.len() > OUTPUT_LIMIT {
                    break;
                }
            }
            return Ok(limited(&output));
        }
        let before = if path.exists() {
            Some(self.read(&path)?)
        } else {
            None
        };
        let content = if name == "edit_file" {
            let old = field(args, "old_string")?;
            let data = before.as_ref().ok_or("edit requires an existing file")?;
            let matches = data
                .char_indices()
                .filter(|(i, _)| data[*i..].starts_with(old))
                .take(2)
                .count();
            if old.is_empty() || matches != 1 {
                return Err("old_string must be nonempty and match exactly once".into());
            }
            data.replacen(old, field(args, "new_string")?, 1)
        } else {
            field(args, "content")?.into()
        };
        if content.len() > FILE_LIMIT {
            return Err("write exceeds 1 MiB".into());
        }
        self.approve(name, args)?;
        let current = if path.exists() {
            Some(self.read(&path)?)
        } else {
            None
        };
        if current != before {
            return Err("file changed while awaiting approval; read it again".into());
        }
        if self.path(field(args, "path")?)? != path {
            return Err("path changed during approval".into());
        }
        atomic_write(&path, &content)?;
        Ok("written".into())
    }
}

fn atomic_write(path: &Path, content: &str) -> Result<()> {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let temp = path.parent().ok_or("missing parent")?.join(format!(
        ".agent-{}-{}.tmp",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&temp)?;
    let result = (|| -> Result<()> {
        file.write_all(content.as_bytes())?;
        if let Ok(meta) = fs::metadata(path) {
            file.set_permissions(meta.permissions())?;
        }
        file.sync_all()?;
        fs::rename(&temp, path)?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(temp);
    }
    result
}

fn drain(reader: &mut dyn Read, kept: &mut Vec<u8>) -> io::Result<()> {
    let mut buffer = [0; 8192];
    // Bounded work even for commands continuously writing output.
    for _ in 0..16 {
        let n = match reader.read(&mut buffer) {
            Ok(n) => n,
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        };
        if n == 0 {
            break;
        }
        let keep = n.min((OUTPUT_LIMIT + 1).saturating_sub(kept.len()));
        kept.extend_from_slice(&buffer[..keep]);
    }
    Ok(())
}

fn shell(root: &Path, command: &str, timeout: Duration) -> Result<String> {
    let mut child = Command::new("/bin/sh")
        .args(["-c", command])
        .current_dir(root)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0)
        .spawn()?;
    let pid = child.id() as i32;
    let mut stdout = child.stdout.take().unwrap();
    let mut stderr = child.stderr.take().unwrap();
    let mut out = Vec::new();
    let mut err = Vec::new();
    let result = (|| -> Result<_> {
        for fd in [stdout.as_raw_fd(), stderr.as_raw_fd()] {
            // SAFETY: both descriptors are live, owned pipes; fcntl retains ownership.
            if unsafe { libc::fcntl(fd, libc::F_SETFL, libc::O_NONBLOCK) } == -1 {
                return Err(io::Error::last_os_error().into());
            }
        }
        let start = Instant::now();
        loop {
            drain(&mut stdout, &mut out)?;
            drain(&mut stderr, &mut err)?;
            if let Some(status) = child.try_wait()? {
                return Ok(Some(status));
            }
            check_interrupt()?;
            if start.elapsed() >= timeout {
                return Ok(None);
            }
            thread::sleep(Duration::from_millis(10));
        }
    })();
    // SAFETY: target only the process group created above. Detached jobs may survive.
    unsafe {
        libc::kill(-pid, libc::SIGKILL);
    }
    child.wait()?;
    let status = result?;
    drain(&mut stdout, &mut out)?;
    drain(&mut stderr, &mut err)?;
    Ok(format!(
        "status: {}\nstdout:\n{}\nstderr:\n{}",
        status
            .map(|s| s.to_string())
            .unwrap_or("timeout (killed)".into()),
        limited(&String::from_utf8_lossy(&out)),
        limited(&String::from_utf8_lossy(&err))
    ))
}

fn turn(
    request: &mut Request,
    workspace: &Workspace,
    max_turns: usize,
    max_context_bytes: usize,
    mut infer: impl FnMut(&Request) -> Result<Response>,
) -> Result<()> {
    let mut tool_number = 0;
    for _ in 0..max_turns {
        check_interrupt()?;
        if serde_json::to_vec(request)?.len() > max_context_bytes {
            return Err(format!(
                "context exceeds {max_context_bytes} bytes; increase --max-context-kib or start a new conversation (no silent truncation)"
            ).into());
        }
        let response = infer(request)?;
        check_interrupt()?;
        let calls: Vec<_> = response
            .message
            .content
            .iter()
            .filter(|block| block["type"] == "tool_call")
            .cloned()
            .collect();
        if response.finish_reason != "tool_calls" {
            request.messages.push(response.message);
            if !calls.is_empty() || response.finish_reason != "stop" {
                return Err(format!(
                    "incomplete response: {} (no tools executed)",
                    response.finish_reason
                )
                .into());
            }
            return Ok(());
        }
        if calls.is_empty() || calls.len() > 32 {
            return Err("expected 1–32 tool calls per response (no tools executed)".into());
        }
        let mut ids = HashSet::new();
        for call in &calls {
            if field(call, "id")?.is_empty() || !ids.insert(field(call, "id")?) {
                return Err("empty or duplicate tool call ID (no tools executed)".into());
            }
        }
        request.messages.push(response.message); // Preserve native reasoning replay intact.
        let content = calls
            .iter()
            .map(|call| {
                tool_number += 1;
                let name = call["name"].as_str().unwrap_or("unknown");
                if workspace.verbose {
                    eprintln!(
                        "[{tool_number}] {} {}",
                        display(name),
                        display(&call["arguments"].to_string())
                    );
                } else {
                    eprintln!("[{tool_number}] {}", ui::label(name, &call["arguments"]));
                }
                let started = Instant::now();
                let result = field(call, "name")
                    .and_then(|name| workspace.execute(name, &call["arguments"]));
                let is_error = result.is_err();
                let output = result.unwrap_or_else(|e| e.to_string());
                if workspace.verbose {
                    eprintln!("{}", display(&limited(&output)));
                } else {
                    eprintln!(
                        "[{tool_number}] {} · {:.1}s",
                        ui::outcome(name, &output, is_error),
                        started.elapsed().as_secs_f64()
                    );
                }
                json!({"type":"tool_result","id":call["id"],"output":output,"is_error":is_error})
            })
            .collect();
        request.messages.push(Message {
            role: "user".into(),
            content,
            replay: None,
        });
    }
    Err("turn limit reached; stopped without another model request".into())
}

fn run() -> Result<()> {
    let mut provider = std::env::var("AGENT_PROVIDER").unwrap_or("codex".into());
    let mut model = std::env::var("AGENT_MODEL").unwrap_or("gpt-6.1-sol".into());
    let mut yes = true;
    let mut verbose = false;
    let mut max_turns = 500;
    let mut max_context_kib: usize = 4096;
    let mut prompt = Vec::new();
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--list-skills" => {
                let workspace = Workspace {
                    root: std::env::current_dir()?.canonicalize()?,
                    yes: false,
                    verbose: false,
                };
                println!("{}", skills::catalog(&skills::discover(&workspace)));
                return Ok(());
            }
            "--verbose" => verbose = true,
            "--yes" => yes = true,
            "--ask" => yes = false,
            "--provider" => provider = args.next().ok_or("--provider needs a value")?,
            "--model" => model = args.next().ok_or("--model needs a value")?,
            "--max-turns" => max_turns = args.next().ok_or("--max-turns needs a value")?.parse()?,
            "--max-context-kib" => {
                max_context_kib = args
                    .next()
                    .ok_or("--max-context-kib needs a value")?
                    .parse()?;
            }
            "--help" | "-h" => {
                println!(
                    "agent [--provider openai|anthropic|openrouter|codex] [--model MODEL] [--ask|--yes] [--verbose] [--list-skills] [--max-turns N] [--max-context-kib N] [--] [PROMPT...]\nDefaults: provider codex, model gpt-6.1-sol (overridable with AGENT_PROVIDER/AGENT_MODEL).\nLimits: 500 model calls per prompt, 4096 KiB serialized context (not a token limit; provider limits still apply).\nNo prompt: interactive conversation. /quit exits, /clear resets history.\nTool calls are allowed without confirmation by default, including unsandboxed shell commands with full user privileges.\n--ask requires confirmation for shell commands and file writes; --yes restores automatic approval.\nTool activity is compact by default; --verbose shows full arguments and bounded results."
                );
                return Ok(());
            }
            "--" => {
                prompt.extend(args);
                break;
            }
            _ if arg.starts_with('-') => return Err(format!("unknown option {arg}").into()),
            _ => {
                prompt.push(arg);
                prompt.extend(args);
                break;
            }
        }
    }
    if model.is_empty() || max_turns == 0 {
        return Err("set --model/AGENT_MODEL and a positive --max-turns".into());
    }
    let max_context_bytes = max_context_kib
        .checked_mul(1024)
        .filter(|bytes| *bytes > 0)
        .ok_or("--max-context-kib must be positive and fit in bytes")?;
    let provider: Provider = serde_json::from_value(json!(provider))?;
    let client = match provider {
        Provider::Codex => auth::codex_client()?,
        _ => {
            let key = match provider {
                Provider::Anthropic => "ANTHROPIC_API_KEY",
                Provider::Openrouter => "OPENROUTER_API_KEY",
                _ => "OPENAI_API_KEY",
            };
            Client::new(provider, std::env::var(key)?)
        }
    };
    let workspace = Workspace {
        root: std::env::current_dir()?.canonicalize()?,
        yes,
        verbose,
    };
    let mut request = Request {
        model,
        messages: Vec::new(),
        system: format!(
            "{}\nWorkspace: {}",
            include_str!("prompt.txt"),
            workspace.root.display()
        ),
        tools: tools(),
        json_schema: None,
        max_tokens: 8192,
        options: if provider == Provider::Codex {
            // Codex otherwise may return only opaque replay, with no visible summary.
            serde_json::from_value(json!({"reasoning":{"summary":"auto"}}))?
        } else {
            Default::default()
        },
    };
    request
        .system
        .push_str(&skills::catalog(&skills::discover(&workspace)));
    let instructions = workspace.root.join("AGENTS.md");
    if instructions.exists() {
        request.system.push_str(&format!(
            "\nRoot AGENTS.md:\n{}",
            workspace.read(&workspace.path("AGENTS.md")?)?
        ));
    }
    chat(
        client,
        workspace,
        request,
        prompt,
        max_turns,
        max_context_bytes,
    )
}

fn chat(
    client: Client,
    workspace: Workspace,
    mut request: Request,
    prompt: Vec<String>,
    max_turns: usize,
    max_context_bytes: usize,
) -> Result<()> {
    let interactive = prompt.is_empty();
    if interactive && !io::stdin().is_terminal() {
        return Err("provide a prompt argument for noninteractive use".into());
    }
    loop {
        let input = if interactive {
            eprintln!();
            let input = match input_line("> ") {
                Ok(input) => input,
                Err(e) if e.is::<llm_api::Cancelled>() => {
                    INTERRUPTED.store(false, Ordering::Relaxed);
                    eprintln!("cancelled");
                    continue;
                }
                Err(e) => return Err(e),
            };
            if input.is_empty() {
                break;
            }
            match input.trim() {
                "/quit" => break,
                "/clear" => {
                    request.messages.clear();
                    continue;
                }
                "" => continue,
                _ => input,
            }
        } else {
            prompt.join(" ")
        };
        request.messages.push(text("user", input));
        let result = turn(
            &mut request,
            &workspace,
            max_turns,
            max_context_bytes,
            |req| {
                let styled = io::stdout().is_terminal() && std::env::var_os("NO_COLOR").is_none();
                let mut stream = ui::StreamDisplay::new(styled);
                let result = client.run_cancellable(req, &INTERRUPTED, |event| {
                    if check_interrupt().is_err() {
                        return false;
                    }
                    let chunk = stream.delta(&event);
                    if !chunk.is_empty() {
                        print!("{chunk}");
                        return io::stdout().flush().is_ok();
                    }
                    true
                });
                print!("{}", stream.finish());
                io::stdout().flush()?;
                result
            },
        );
        if interactive && INTERRUPTED.swap(false, Ordering::Relaxed) {
            request.messages.push(text("user", "Previous turn cancelled. Completed tool effects may remain; inspect state before retrying anything.".into()));
            eprintln!("cancelled; completed changes were not rolled back");
            continue;
        }
        result?;
        if !interactive {
            break;
        }
    }
    Ok(())
}

fn install_interrupt_handler() {
    // SAFETY: handler only sets a lock-free atomic flag and never calls Rust I/O.
    unsafe {
        libc::signal(libc::SIGINT, interrupt as *const () as libc::sighandler_t);
    }
}

fn main() {
    install_interrupt_handler();
    if let Err(error) = run() {
        eprintln!("agent: {}", display(&error.to_string()));
        std::process::exit(
            if INTERRUPTED.load(Ordering::Relaxed) || error.is::<llm_api::Cancelled>() {
                130
            } else {
                1
            },
        );
    }
}

#[cfg(test)]
#[path = "../tests/unit/mod.rs"]
mod tests;
