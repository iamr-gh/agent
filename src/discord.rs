//! Frontend-neutral JSON-lines subprocess protocol (legacy filename).
use crate::{
    compaction,
    engine::{Frontend, INTERRUPTED, Workspace, text},
    runner,
    session::{self, Store},
};
use llm_api::{Provider, Result};
use serde_json::{Value, json};
use std::io::{self, BufRead, Read, Write};
use std::time::Duration;

fn emit(value: Value) -> Result<()> {
    let mut out = io::stdout().lock();
    serde_json::to_writer(&mut out, &value)?;
    writeln!(out)?;
    out.flush()?;
    Ok(())
}

fn receive() -> Result<Value> {
    let mut line = String::new();
    if io::stdin().lock().take(1024 * 1024).read_line(&mut line)? == 0 {
        return Err("adapter disconnected".into());
    }
    Ok(serde_json::from_str(&line)?)
}

fn is_approval(answer: &Value) -> bool {
    answer == &json!({"approve":true})
}

fn policy(input: &Value) -> Result<(bool, usize)> {
    let auto_approve = match input.get("auto_approve") {
        None => false,
        Some(value) => value.as_bool().ok_or("auto_approve must be a boolean")?,
    };
    let max_turns = match input.get("max_turns") {
        None => 25,
        Some(value) => value
            .as_u64()
            .filter(|n| (1..=100).contains(n))
            .ok_or("max_turns must be an integer from 1 to 100")? as usize,
    };
    Ok((auto_approve, max_turns))
}

struct ProtocolFrontend;
impl Frontend for ProtocolFrontend {
    fn approve(&self, name: &str, args: &Value) -> Result<()> {
        emit(json!({"type":"approval", "name":name, "args":args}))?;
        let answer = receive()?;
        if is_approval(&answer) {
            Ok(())
        } else {
            Err("denied by frontend or approval expired".into())
        }
    }
    fn tool_started(&self, _: usize, name: &str, _: &Value) {
        let _ = emit(json!({"type":"tool", "name":name}));
    }
    fn tool_finished(&self, index: usize, name: &str, _: &str, error: bool, elapsed: Duration) {
        let _ = emit(json!({"type":"tool_finished", "index":index, "name":name,
            "error":error, "elapsed_ms":elapsed.as_millis()}));
    }
    fn diagnostic(&self, message: &str) {
        eprintln!("{message}");
    }
}

pub(crate) fn run() -> Result<()> {
    let input = receive()?;
    let prompt = input["prompt"]
        .as_str()
        .filter(|s| !s.trim().is_empty())
        .ok_or("missing prompt")?;
    let (auto_approve, max_turns) = policy(&input)?;
    let workspace = Workspace {
        root: std::env::current_dir()?.canonicalize()?,
        yes: auto_approve,
        frontend: Box::new(ProtocolFrontend),
    };
    let root = session::directory()?;
    let (mut session, provider) = if let Some(id) = input["session"].as_str() {
        Store::load(&root, &workspace.root, id)?
    } else {
        let provider: Provider = serde_json::from_value(json!(
            std::env::var("AGENT_PROVIDER").unwrap_or("codex".into())
        ))?;
        (
            runner::create(
                &workspace,
                provider,
                std::env::var("AGENT_MODEL").unwrap_or(crate::models::DEFAULT_MODEL.into()),
            )?,
            provider,
        )
    };
    let client = runner::connect(provider)?;
    emit(json!({"type":"session", "id":session.store.as_ref().unwrap().id}))?;
    // Presentation belongs to the adapter, not persisted core instructions.
    let presentation = input
        .get("presentation")
        .and_then(Value::as_str)
        .unwrap_or("");
    session.request.messages.push(text("user", prompt.into()));
    session.save(true)?;
    let store = session.store.as_ref().unwrap();
    let result = runner::turn(
        &mut session.request,
        &workspace,
        max_turns,
        4 * 1024 * 1024,
        compaction::automatic(&client, Some(store), compaction::threshold()?, |_| {}),
        |request| {
            let mut presented = request.clone();
            presented.system.push_str(presentation);
            let response = client.run_cancellable(&presented, &INTERRUPTED, |_| true)?;
            for block in &response.message.content {
                if response.finish_reason == "stop" && block["type"] == "text" {
                    emit(json!({"type":"answer", "text":block["text"]}))?;
                }
            }
            Ok(response)
        },
        |request| store.save(request, true),
    );
    if let Err(error) = result {
        if error.to_string() != "turn limit reached; stopped without another model request" {
            return Err(error);
        }
        emit(json!({"type":"limit", "max_turns":max_turns}))?;
    }
    session.save(false)?;
    emit(json!({"type":"done"}))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn worker_policy_defaults_fail_closed_and_bounds_turns() {
        assert_eq!(policy(&json!({})).unwrap(), (false, 25));
        assert_eq!(
            policy(&json!({"auto_approve":true, "max_turns":1})).unwrap(),
            (true, 1)
        );
        assert_eq!(
            policy(&json!({"auto_approve":false, "max_turns":100})).unwrap(),
            (false, 100)
        );
        for input in [
            json!({"auto_approve":"true"}),
            json!({"auto_approve":1}),
            json!({"max_turns":0}),
            json!({"max_turns":101}),
            json!({"max_turns":-1}),
            json!({"max_turns":1.5}),
            json!({"max_turns":"25"}),
        ] {
            assert!(policy(&input).is_err());
        }
    }
    #[test]
    fn automatic_shell_tools_stop_at_the_model_call_cap() {
        let (yes, max_turns) = policy(&json!({"auto_approve":true, "max_turns":2})).unwrap();
        let workspace = Workspace {
            root: std::env::current_dir().unwrap(),
            yes,
            frontend: Box::new(ProtocolFrontend),
        };
        let mut request = crate::tests::request();
        let mut calls = 0;
        let error = crate::engine::turn_saved(
            &mut request,
            &workspace,
            max_turns,
            1024 * 1024,
            |_| {
                calls += 1;
                Ok(crate::tests::response(
                    vec![crate::tests::call(
                        &calls.to_string(),
                        "shell",
                        json!({"command":"printf ok"}),
                    )],
                    "tool_calls",
                ))
            },
            |_| Ok(()),
        )
        .unwrap_err();
        assert_eq!(calls, 2);
        assert_eq!(
            error.to_string(),
            "turn limit reached; stopped without another model request"
        );
        assert!(
            request
                .messages
                .last()
                .unwrap()
                .content
                .iter()
                .all(|block| block["type"] == "tool_result" && block["is_error"] == false)
        );
    }

    #[test]
    fn manual_approval_requires_an_exact_boolean_grant() {
        assert!(is_approval(&json!({"approve":true})));
        for answer in [
            json!({"approve":"true"}),
            json!({}),
            json!({"approve":false}),
            json!({"approve":true,"extra":1}),
        ] {
            assert!(!is_approval(&answer));
        }
    }
}
