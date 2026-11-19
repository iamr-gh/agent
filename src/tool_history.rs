//! Optional request-only paging of old, large tool outputs. The saved request is authoritative.
use llm_api::{Request, Result};
use serde_json::{Value, json};

const BATCH: usize = 16;
const EDGE: usize = 2 * 1024;
const MIN_OUTPUT: usize = 6 * 1024;
const PAGE: usize = 8 * 1024;
pub(crate) const TOOL: &str = "read_tool_output";

pub(crate) fn enabled() -> bool {
    std::env::var("AGENT_PAGE_TOOL_OUTPUT").as_deref() == Ok("1")
}

fn schema() -> Value {
    json!({"name":TOOL,"description":"Read a page of an original saved tool result by the reference in its shortened preview. Use the returned next_offset to continue. Does not rerun any tool.","parameters":{"type":"object","properties":{"reference":{"type":"string"},"offset":{"type":"integer","minimum":0}},"required":["reference"],"additionalProperties":false}})
}

fn floor_boundary(text: &str, mut n: usize) -> usize {
    while !text.is_char_boundary(n) {
        n -= 1;
    }
    n
}

fn eligible<'a>(request: &'a Request, message: usize, block: &Value) -> Option<&'a str> {
    let call = request.messages.get(message.checked_sub(1)?)?;
    if call.role != "assistant" || request.messages.get(message)?.role != "user" {
        return None;
    }
    call.content.iter().find_map(|item| {
        (item["type"] == "tool_call" && item["id"].is_string() && item["id"] == block["id"])
            .then(|| item["name"].as_str())
            .flatten()
    })
}

pub(crate) fn project(request: &Request) -> Option<Request> {
    // Update old prefixes in steps, not on every model call. Always keep at least 16
    // messages verbatim; an assistant's native replay is never touched.
    let cutoff = (request.messages.len() / BATCH).saturating_sub(1) * BATCH;
    let mut projected: Option<Request> = None;
    for (i, message) in request.messages[..cutoff].iter().enumerate() {
        for (j, block) in message.content.iter().enumerate() {
            if block["type"] != "tool_result" || block["is_error"] != false {
                continue;
            }
            let Some(output) = block["output"].as_str().filter(|s| s.len() >= MIN_OUTPUT) else {
                continue;
            };
            match eligible(request, i, block) {
                Some("read_file") => {}
                Some("shell") if output.starts_with("status: exit status: 0\n") => {}
                _ => continue,
            }
            let head = floor_boundary(output, EDGE);
            let tail = floor_boundary(output, output.len() - EDGE);
            let shortened = format!(
                "{}\n[Older tool output: omitted bytes {}–{}. To recover the original, call {TOOL} with reference \"{i}:{j}\" and offset {head}.]\n{}",
                &output[..head],
                head,
                tail,
                &output[tail..]
            );
            projected
                .get_or_insert_with(|| {
                    let mut result = request.clone();
                    result.tools.push(schema());
                    result
                })
                .messages[i]
                .content[j]["output"] = json!(shortened);
        }
    }
    projected
}

pub(crate) fn page(request: &Request, args: &Value) -> Result<String> {
    let object = args.as_object().ok_or("expected an argument object")?;
    if object
        .keys()
        .any(|key| key != "reference" && key != "offset")
    {
        return Err("unknown argument".into());
    }
    let (i, j) = args["reference"]
        .as_str()
        .and_then(|s| s.split_once(':'))
        .and_then(|(i, j)| Some((i.parse::<usize>().ok()?, j.parse::<usize>().ok()?)))
        .ok_or("expected a saved tool result reference")?;
    let output = request
        .messages
        .get(i)
        .and_then(|m| m.content.get(j))
        .filter(|b| b["type"] == "tool_result" && b["is_error"] == false)
        .filter(|b| matches!(eligible(request, i, b), Some("read_file" | "shell")))
        .and_then(|b| b["output"].as_str())
        .ok_or("unknown tool result reference")?;
    let offset = match args.get("offset") {
        None => 0,
        Some(v) => v
            .as_u64()
            .and_then(|n| usize::try_from(n).ok())
            .ok_or("invalid offset")?,
    };
    if offset > output.len() || !output.is_char_boundary(offset) {
        return Err("offset must be a UTF-8 byte boundary inside the result".into());
    }
    let end = floor_boundary(output, offset.saturating_add(PAGE).min(output.len()));
    let next = if end < output.len() {
        end.to_string()
    } else {
        "none".into()
    };
    Ok(format!(
        "Saved output bytes {offset}..{end} of {}; next_offset: {next}\n{}",
        output.len(),
        &output[offset..end]
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tests::{call, request};
    use llm_api::{Message, Provider};

    fn assert_same(left: &Request, right: &Request) {
        assert_eq!(
            serde_json::to_value(left).unwrap(),
            serde_json::to_value(right).unwrap()
        );
    }

    #[test]
    fn projects_only_old_successful_output_and_recovers_utf8() {
        let mut req = request();
        let output = "☀️".repeat(3000);
        let cases = [
            ("read_file", false),
            ("load_skill", false),
            ("read_file", true),
            ("shell", false),
        ];
        for (i, (name, error)) in cases.into_iter().enumerate() {
            req.messages.push(Message {
                role: "assistant".into(),
                content: vec![call(&i.to_string(), name, json!({}))],
                replay: Some(json!({"untouched":i})),
            });
            req.messages.push(Message {role:"user".into(), content:vec![json!({"type":"tool_result","id":i.to_string(),"is_error":error,"output":output})], replay:None});
        }
        assert!(project(&req).is_none());
        let old = req.clone();
        while req.messages.len() < 32 {
            req.messages
                .push(crate::engine::text("user", "task".into()));
        }
        req.messages.extend(old.messages[1..3].iter().cloned());
        let orig = req.clone();
        let view = project(&req).unwrap();
        assert_same(&req, &orig);
        assert_eq!(view.tools.last().unwrap()["name"], TOOL);
        assert_eq!(view.messages[1].replay, orig.messages[1].replay);
        assert_eq!(view.messages[2].content[0]["id"], "0");
        assert!(
            view.messages[2].content[0]["output"]
                .as_str()
                .unwrap()
                .contains("2:0")
        );
        for i in [4, 6, 8, 33] {
            assert_eq!(view.messages[i].content, orig.messages[i].content);
        }
        let mut rebuilt = String::new();
        while rebuilt.len() < output.len() {
            let page = page(&req, &json!({"reference":"2:0","offset":rebuilt.len()})).unwrap();
            rebuilt.push_str(page.split_once('\n').unwrap().1);
        }
        assert_eq!(rebuilt, output);
        assert!(page(&req, &json!({"reference":"2:0","offset":1})).is_err());
        assert!(page(&req, &json!({"reference":"4:0"})).is_err());
    }

    // Offline trace replay: use the actual provider encoder; print sizes, never transcript content.
    #[test]
    #[ignore = "requires paths to private session snapshots in AGENT_TRACE_FILES"]
    fn replay_saved_traces() {
        let paths = std::env::var_os("AGENT_TRACE_FILES").expect("set AGENT_TRACE_FILES");
        for path in std::env::split_paths(&paths) {
            let saved: Value =
                serde_json::from_reader(std::fs::File::open(&path).unwrap()).unwrap();
            let provider: Provider = serde_json::from_value(saved["provider"].clone()).unwrap();
            let mut request: Request = serde_json::from_value(saved["request"].clone()).unwrap();
            let messages = std::mem::take(&mut request.messages);
            let (mut base, mut paged, mut changed, mut calls) = (0usize, 0usize, 0usize, 0usize);
            let size = |request: &Request| {
                serde_json::to_vec(&llm_api::encode(provider, request).unwrap())
                    .unwrap()
                    .len()
            };
            for message in messages {
                if message.role == "assistant" {
                    let n = size(&request);
                    base += n;
                    calls += 1;
                    if let Some(view) = project(&request) {
                        let m = size(&view);
                        assert!(m < n);
                        paged += m;
                        changed += 1;
                    } else {
                        paged += n;
                    }
                }
                request.messages.push(message);
            }
            println!(
                "{}: calls={calls}, changed={changed}, bytes={base}->{paged}",
                path.file_name().unwrap().to_string_lossy()
            );
        }
    }
}
