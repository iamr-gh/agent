//! Agent and tool logic. No terminal I/O or presentation dependencies.
use crate::skills;
use llm_api::{Message, Request, Response, Result};
use serde_json::{Value, json};
use std::collections::HashSet;
use std::fs::{self, File};
use std::io::{self, Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::thread;
use std::time::{Duration, Instant};

pub(crate) const FILE_LIMIT: usize = 1024 * 1024;
pub(crate) const OUTPUT_LIMIT: usize = 32 * 1024;
pub(crate) static INTERRUPTED: AtomicBool = AtomicBool::new(false);
pub(crate) fn check_interrupt() -> Result<()> {
    if INTERRUPTED.load(Ordering::Relaxed) {
        return Err(llm_api::Cancelled.into());
    }
    Ok(())
}

/// Presentation/interaction port. Arguments and results are raw, not terminal-escaped.
/// A remote adapter can forward these events and await an approval reply.
pub(crate) trait Frontend {
    fn approve(&self, name: &str, args: &Value) -> Result<()>;
    fn tool_started(&self, number: usize, name: &str, args: &Value);
    fn tool_finished(
        &self,
        number: usize,
        name: &str,
        output: &str,
        is_error: bool,
        elapsed: Duration,
    );
    fn diagnostic(&self, message: &str);
}

pub(crate) fn text(role: &str, value: String) -> Message {
    Message {
        role: role.into(),
        content: vec![json!({"type":"text","text":value})],
        replay: None,
    }
}

pub(crate) fn limited(value: &str) -> String {
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

pub(crate) fn tools() -> Vec<Value> {
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

pub(crate) struct Workspace {
    pub(crate) root: PathBuf,
    pub(crate) yes: bool,
    pub(crate) frontend: Box<dyn Frontend>,
}

impl Workspace {
    pub(crate) fn path(&self, name: &str) -> Result<PathBuf> {
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

    pub(crate) fn read(&self, path: &Path) -> Result<String> {
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
        if !self.yes {
            self.frontend.approve(name, args)?;
        }
        check_interrupt()
    }

    pub(crate) fn execute(&self, name: &str, args: &Value) -> Result<String> {
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

pub(crate) fn atomic_write(path: &Path, content: &str) -> Result<()> {
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

pub(crate) fn shell(root: &Path, command: &str, timeout: Duration) -> Result<String> {
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

#[cfg(test)]
pub(crate) fn turn(
    request: &mut Request,
    workspace: &Workspace,
    max_turns: usize,
    max_context_bytes: usize,
    infer: impl FnMut(&Request) -> Result<Response>,
) -> Result<()> {
    turn_saved(
        request,
        workspace,
        max_turns,
        max_context_bytes,
        infer,
        |_| Ok(()),
    )
}

pub(crate) fn turn_saved(
    request: &mut Request,
    workspace: &Workspace,
    max_turns: usize,
    max_context_bytes: usize,
    mut infer: impl FnMut(&Request) -> Result<Response>,
    mut checkpoint: impl FnMut(&Request) -> Result<()>,
) -> Result<()> {
    let mut tool_number = 0;
    for _ in 0..max_turns {
        check_interrupt()?;
        if serde_json::to_vec(request)?.len() > max_context_bytes {
            return Err(format!("context exceeds {max_context_bytes} bytes; increase --max-context-kib or start a new conversation (no silent truncation)").into());
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
            if !calls.is_empty() || response.finish_reason != "stop" {
                return Err(format!(
                    "incomplete response: {} (no tools executed)",
                    response.finish_reason
                )
                .into());
            }
            request.messages.push(response.message);
            checkpoint(request)?;
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
        // Persist a settled placeholder for every call before any effects. A crash
        // can leave uncertainty, but never an executable call awaiting replay.
        let content = calls
            .iter()
            .map(|call| {
                json!({"type":"tool_result", "id":call["id"],
            "output":"Interrupted: tool not executed.", "is_error":true})
            })
            .collect();
        request.messages.push(Message {
            role: "user".into(),
            content,
            replay: None,
        });
        checkpoint(request)?;
        let results = request.messages.len() - 1;
        for (index, call) in calls.iter().enumerate() {
            tool_number += 1;
            let name = call["name"].as_str().unwrap_or("unknown");
            request.messages[results].content[index]["output"] = json!(
                "Interrupted: tool outcome unknown; effects may remain. Inspect state before retrying."
            );
            checkpoint(request)?; // Durable intent before approval/execution.
            workspace
                .frontend
                .tool_started(tool_number, name, &call["arguments"]);
            let started = Instant::now();
            let result =
                field(call, "name").and_then(|name| workspace.execute(name, &call["arguments"]));
            let is_error = result.is_err();
            let output = result.unwrap_or_else(|e| e.to_string());
            request.messages[results].content[index] = json!({"type":"tool_result", "id":call["id"], "output":output, "is_error":is_error});
            checkpoint(request)?; // Each result survives independently of the batch.
            workspace.frontend.tool_finished(
                tool_number,
                name,
                &output,
                is_error,
                started.elapsed(),
            );
        }
    }
    Err("turn limit reached; stopped without another model request".into())
}
